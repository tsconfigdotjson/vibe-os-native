//! Read-only ext4 images, written from scratch (PRD: What's inside `linux`).
//!
//! `vibed` builds the managed layer on the Mac, which has no `mke2fs`, so
//! it writes the image itself. It's ext4 in the plain layout ext2 started
//! with: block maps rather than extents, and no journal, as nothing ever
//! writes to it. The guest's ext4 driver mounts it read-only, and
//! `e2fsck` finds nothing to fix (the tests check, where it's installed).
//!
//! Everything in an image is root's. The host file is sparse: blocks of
//! zeros, in a file or never used, are left as holes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// The block size: 4 KiB, as `mke2fs` picks for anything but tiny disks.
pub const BLOCK: u64 = 4096;

const BLOCKS_PER_GROUP: u64 = BLOCK * 8;
const INODE_SIZE: u64 = 256;
const INODES_PER_BLOCK: u64 = BLOCK / INODE_SIZE;
const DESC_SIZE: u64 = 32;
/// Inodes 1 to 10 are the filesystem's own; the root is 2.
const FIRST_INO: u32 = 11;
const ROOT_INO: u32 = 2;
const DIRECT: u64 = 12;
const PER_BLOCK: u64 = BLOCK / 4;

/// The largest file a block map with a double indirect block reaches.
pub const MAX_FILE: u64 = (DIRECT + PER_BLOCK + PER_BLOCK * PER_BLOCK) * BLOCK;

const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;

const INCOMPAT_FILETYPE: u32 = 0x0002;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const EXTRA_ISIZE: u16 = 32;

/// Where a file's contents come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file on the host, read as the image is written.
    Path(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Dir { mode: u16 },
    File { source: Source, mode: u16 },
    Symlink { target: String },
}

/// What goes in an image: directories, files and symlinks by path, relative
/// to its root. A directory on the way to anything is made, `0755`, unless
/// it's given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    entries: BTreeMap<String, Entry>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree::default()
    }

    pub fn dir(&mut self, path: &str, mode: u16) -> &mut Tree {
        self.add(path, Entry::Dir { mode })
    }

    pub fn file(&mut self, path: &str, source: Source, mode: u16) -> &mut Tree {
        self.add(path, Entry::File { source, mode })
    }

    pub fn symlink(&mut self, path: &str, target: &str) -> &mut Tree {
        self.add(
            path,
            Entry::Symlink {
                target: target.to_owned(),
            },
        )
    }

    fn add(&mut self, path: &str, entry: Entry) -> &mut Tree {
        self.entries
            .insert(path.trim_matches('/').to_owned(), entry);
        self
    }
}

/// One inode as it will be written.
#[derive(Debug)]
struct Node {
    ino: u32,
    kind: Kind,
    mode: u16,
    size: u64,
    /// Data blocks, then the blocks of the map that finds them.
    blocks: Vec<u32>,
    map: Vec<u32>,
    links: u16,
}

#[derive(Debug)]
enum Kind {
    /// Its entries: name, inode, type.
    Dir(Vec<(Vec<u8>, u32, u8)>),
    File(Source),
    Symlink(Vec<u8>),
}

impl Node {
    fn data_blocks(&self) -> u64 {
        match &self.kind {
            Kind::Symlink(t) if t.len() < 60 => 0,
            _ => self.size.div_ceil(BLOCK),
        }
    }
}

/// How many blocks the map of a file of `n` data blocks takes.
fn map_blocks(n: u64) -> io::Result<u64> {
    if n <= DIRECT {
        return Ok(0);
    }
    let n = n - DIRECT;
    if n <= PER_BLOCK {
        return Ok(1);
    }
    let n = n - PER_BLOCK;
    if n <= PER_BLOCK * PER_BLOCK {
        return Ok(2 + n.div_ceil(PER_BLOCK));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("a file over {MAX_FILE} bytes doesn't fit an ext4 block map"),
    ))
}

/// A directory's entries packed into blocks: each entry's record runs to
/// the next, and a block's last to its end.
fn dir_blocks(entries: &[(Vec<u8>, u32, u8)]) -> Vec<Vec<u8>> {
    let mut blocks = Vec::new();
    let mut block: Vec<u8> = Vec::new();
    let mut last = 0usize;
    for (name, ino, kind) in entries {
        let len = (8 + name.len()).next_multiple_of(4);
        if block.len() + len > BLOCK as usize {
            let rec = (BLOCK as usize - last) as u16;
            block[last + 4..last + 6].copy_from_slice(&rec.to_le_bytes());
            block.resize(BLOCK as usize, 0);
            blocks.push(std::mem::take(&mut block));
        }
        last = block.len();
        block.extend_from_slice(&ino.to_le_bytes());
        block.extend_from_slice(&(len as u16).to_le_bytes());
        block.push(name.len() as u8);
        block.push(*kind);
        block.extend_from_slice(name);
        block.resize(last + len, 0);
    }
    let rec = (BLOCK as usize - last) as u16;
    block[last + 4..last + 6].copy_from_slice(&rec.to_le_bytes());
    block.resize(BLOCK as usize, 0);
    blocks.push(block);
    blocks
}

/// The shape of the filesystem: its groups and what's in each.
#[derive(Debug)]
struct Geometry {
    groups: u64,
    inodes_per_group: u64,
    table_blocks: u64,
    desc_blocks: u64,
    blocks: u64,
}

impl Geometry {
    /// Blocks at the start of every group: a superblock, the descriptors,
    /// two bitmaps and the inode table.
    fn overhead(&self) -> u64 {
        1 + self.desc_blocks + 2 + self.table_blocks
    }

    fn group_len(&self, g: u64) -> u64 {
        (self.blocks - g * BLOCKS_PER_GROUP).min(BLOCKS_PER_GROUP)
    }

    /// The fewest groups that hold `data` blocks and `inodes` inodes.
    fn fit(data: u64, inodes: u64) -> io::Result<Geometry> {
        for groups in 1..=(1 << 15) {
            let inodes_per_group = inodes.div_ceil(groups).next_multiple_of(INODES_PER_BLOCK);
            let geometry = Geometry {
                groups,
                inodes_per_group,
                table_blocks: inodes_per_group / INODES_PER_BLOCK,
                desc_blocks: (groups * DESC_SIZE).div_ceil(BLOCK),
                blocks: 0,
            };
            let overhead = geometry.overhead();
            if inodes_per_group > BLOCK * 8 || overhead >= BLOCKS_PER_GROUP {
                continue;
            }
            let room = BLOCKS_PER_GROUP - overhead;
            if groups * room >= data {
                let last = overhead + (data - (groups - 1) * room).max(1);
                return Ok(Geometry {
                    blocks: (groups - 1) * BLOCKS_PER_GROUP + last,
                    ..geometry
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too much for one ext4 image",
        ))
    }
}

/// Writes `tree` as a new image at `out`, labelled `label` (up to 16
/// bytes), with every time stamp `time` (seconds since the epoch). Returns
/// the image's size in bytes. `out` must not exist yet.
pub fn write(tree: &Tree, out: &Path, label: &str, time: u32) -> io::Result<u64> {
    let mut nodes = plan(tree)?;
    let data: u64 = nodes
        .iter()
        .map(|n| {
            let d = n.data_blocks();
            map_blocks(d).map(|m| d + m)
        })
        .sum::<io::Result<u64>>()?;
    let used_inodes = u64::from(FIRST_INO) - 1 + (nodes.len() as u64 - 1);
    let geo = Geometry::fit(data, used_inodes)?;

    // Data blocks in order, skipping each group's own.
    let mut next = Allocator {
        geo: &geo,
        group: 0,
        at: geo.overhead(),
    };
    for n in &mut nodes {
        let d = n.data_blocks();
        n.blocks = (0..d).map(|_| next.take()).collect::<io::Result<_>>()?;
        n.map = (0..map_blocks(d)?)
            .map(|_| next.take())
            .collect::<io::Result<_>>()?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(out)?;
    let written = write_image(&file, &nodes, &geo, label, time);
    if written.is_err() {
        drop(file);
        let _ = fs::remove_file(out);
    }
    written.map(|()| geo.blocks * BLOCK)
}

struct Allocator<'a> {
    geo: &'a Geometry,
    group: u64,
    at: u64,
}

impl Allocator<'_> {
    fn take(&mut self) -> io::Result<u32> {
        while self.at >= self.geo.group_len(self.group) {
            self.group += 1;
            self.at = self.geo.overhead();
            if self.group >= self.geo.groups {
                return Err(io::Error::other("ran out of blocks"));
            }
        }
        let block = self.group * BLOCKS_PER_GROUP + self.at;
        self.at += 1;
        u32::try_from(block).map_err(|_| io::Error::other("block number too large"))
    }
}

/// The inodes, root first and lost+found second, then the tree's entries;
/// with every directory's entries.
fn plan(tree: &Tree) -> io::Result<Vec<Node>> {
    // Every path, with its parents made where they weren't given.
    let mut entries: BTreeMap<Vec<&str>, Entry> = BTreeMap::new();
    for (path, entry) in &tree.entries {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root can't be an entry",
            ));
        }
        for part in &parts {
            if *part == "." || *part == ".." || part.len() > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("`{path}` isn't a name ext4 can hold"),
                ));
            }
        }
        for i in 1..parts.len() {
            entries
                .entry(parts[..i].to_vec())
                .or_insert(Entry::Dir { mode: 0o755 });
        }
        entries.insert(parts, entry.clone());
    }
    for (parts, entry) in &entries {
        if parts.len() > 1
            && !matches!(
                entries.get(&parts[..parts.len() - 1]),
                Some(Entry::Dir { .. })
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is inside something that isn't a directory",
                    parts.join("/")
                ),
            ));
        }
        if let Entry::Symlink { target } = entry
            && (target.is_empty() || target.len() >= BLOCK as usize)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}'s target is too long or empty", parts.join("/")),
            ));
        }
    }

    let mut nodes = vec![
        Node {
            ino: ROOT_INO,
            kind: Kind::Dir(Vec::new()),
            mode: S_IFDIR | 0o755,
            size: 0,
            blocks: Vec::new(),
            map: Vec::new(),
            links: 2,
        },
        Node {
            ino: FIRST_INO,
            kind: Kind::Dir(Vec::new()),
            mode: S_IFDIR | 0o700,
            size: 0,
            blocks: Vec::new(),
            map: Vec::new(),
            links: 2,
        },
    ];
    let mut index: BTreeMap<Vec<&str>, usize> = BTreeMap::new();
    index.insert(Vec::new(), 0);
    for (parts, entry) in &entries {
        let ino = FIRST_INO + nodes.len() as u32 - 1;
        let (kind, mode, size) = match entry {
            Entry::Dir { mode } => (Kind::Dir(Vec::new()), S_IFDIR | (mode & 0o7777), 0),
            Entry::File { source, mode } => {
                let size = match source {
                    Source::Path(p) => fs::metadata(p)
                        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?
                        .len(),
                    Source::Bytes(b) => b.len() as u64,
                };
                if size > MAX_FILE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is too large", parts.join("/")),
                    ));
                }
                (Kind::File(source.clone()), S_IFREG | (mode & 0o7777), size)
            }
            Entry::Symlink { target } => (
                Kind::Symlink(target.as_bytes().to_vec()),
                S_IFLNK | 0o777,
                target.len() as u64,
            ),
        };
        let is_dir = matches!(kind, Kind::Dir(_));
        nodes.push(Node {
            ino,
            kind,
            mode,
            size,
            blocks: Vec::new(),
            map: Vec::new(),
            links: if is_dir { 2 } else { 1 },
        });
        let me = nodes.len() - 1;
        let parent = index[&parts[..parts.len() - 1]];
        let file_type = match entry {
            Entry::Dir { .. } => 2,
            Entry::File { .. } => 1,
            Entry::Symlink { .. } => 7,
        };
        if let Kind::Dir(children) = &mut nodes[parent].kind {
            children.push((parts[parts.len() - 1].as_bytes().to_vec(), ino, file_type));
        }
        if is_dir {
            nodes[parent].links += 1;
            index.insert(parts.clone(), me);
        }
    }
    // lost+found is the root's too.
    if let Kind::Dir(children) = &mut nodes[0].kind {
        children.insert(0, (b"lost+found".to_vec(), FIRST_INO, 2));
    }
    nodes[0].links += 1;

    // Each directory's own entries, then its size.
    let parents: BTreeMap<u32, u32> = nodes
        .iter()
        .flat_map(|n| match &n.kind {
            Kind::Dir(children) => children
                .iter()
                .filter(|c| c.2 == 2)
                .map(|c| (c.1, n.ino))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    for n in &mut nodes {
        let ino = n.ino;
        if let Kind::Dir(children) = &mut n.kind {
            let parent = parents.get(&ino).copied().unwrap_or(ROOT_INO);
            children.insert(0, (b"..".to_vec(), parent, 2));
            children.insert(0, (b".".to_vec(), ino, 2));
            n.size = dir_blocks(children).len() as u64 * BLOCK;
        }
    }
    Ok(nodes)
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn write_image(
    file: &File,
    nodes: &[Node],
    geo: &Geometry,
    label: &str,
    time: u32,
) -> io::Result<()> {
    file.set_len(geo.blocks * BLOCK)?;
    let at = |block: u64| block * BLOCK;
    let too_big = |what| io::Error::other(format!("{what} doesn't fit in 32 bits"));
    let u32_of = |v: u64, what| u32::try_from(v).map_err(|_| too_big(what));

    // Which inodes and blocks each group uses.
    let ipg = geo.inodes_per_group;
    let mut used_blocks = vec![Vec::<u64>::new(); geo.groups as usize];
    for n in nodes {
        for b in n.blocks.iter().chain(&n.map) {
            let b = u64::from(*b);
            used_blocks[(b / BLOCKS_PER_GROUP) as usize].push(b % BLOCKS_PER_GROUP);
        }
    }
    let mut used_inodes = vec![Vec::<u64>::new(); geo.groups as usize];
    let mut dirs = vec![0u16; geo.groups as usize];
    for ino in 1..FIRST_INO {
        used_inodes[0].push(u64::from(ino) - 1);
    }
    for n in nodes {
        let i = u64::from(n.ino) - 1;
        if n.ino >= FIRST_INO {
            used_inodes[(i / ipg) as usize].push(i % ipg);
        }
        if matches!(n.kind, Kind::Dir(_)) {
            dirs[(i / ipg) as usize] += 1;
        }
    }

    // The group descriptors, and the free counts they add up to.
    let overhead = geo.overhead();
    let mut descs = vec![0u8; (geo.desc_blocks * BLOCK) as usize];
    let (mut free_blocks, mut free_inodes) = (0u64, 0u64);
    for g in 0..geo.groups {
        let base = g * BLOCKS_PER_GROUP;
        let len = geo.group_len(g);
        let d = &mut descs[(g * DESC_SIZE) as usize..((g + 1) * DESC_SIZE) as usize];
        put32(d, 0, u32_of(base + 1 + geo.desc_blocks, "a bitmap")?);
        put32(d, 4, u32_of(base + 2 + geo.desc_blocks, "a bitmap")?);
        put32(d, 8, u32_of(base + 3 + geo.desc_blocks, "an inode table")?);
        let fb = len - overhead - used_blocks[g as usize].len() as u64;
        let fi = ipg - used_inodes[g as usize].len() as u64;
        put16(d, 12, fb as u16);
        put16(d, 14, fi as u16);
        put16(d, 16, dirs[g as usize]);
        free_blocks += fb;
        free_inodes += fi;

        // The bitmaps: what's used, and everything past the group's end.
        let mut bitmap = vec![0u8; BLOCK as usize];
        let set = |bits: &mut [u8], i: u64| bits[(i / 8) as usize] |= 1 << (i % 8);
        for i in 0..overhead {
            set(&mut bitmap, i);
        }
        for i in &used_blocks[g as usize] {
            set(&mut bitmap, *i);
        }
        for i in len..BLOCKS_PER_GROUP {
            set(&mut bitmap, i);
        }
        file.write_all_at(&bitmap, at(base + 1 + geo.desc_blocks))?;
        let mut bitmap = vec![0u8; BLOCK as usize];
        for i in &used_inodes[g as usize] {
            set(&mut bitmap, *i);
        }
        for i in ipg..BLOCK * 8 {
            set(&mut bitmap, i);
        }
        file.write_all_at(&bitmap, at(base + 2 + geo.desc_blocks))?;
    }

    let mut sb = vec![0u8; 1024];
    put32(&mut sb, 0, u32_of(geo.groups * ipg, "the inode count")?);
    put32(&mut sb, 4, u32_of(geo.blocks, "the block count")?);
    put32(&mut sb, 12, u32_of(free_blocks, "the free blocks")?);
    put32(&mut sb, 16, u32_of(free_inodes, "the free inodes")?);
    put32(&mut sb, 20, 0); // first data block: 0 for 4 KiB blocks
    put32(&mut sb, 24, 2); // log2(block size) - 10
    put32(&mut sb, 28, 2);
    put32(&mut sb, 32, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 36, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 40, ipg as u32);
    put32(&mut sb, 48, time); // last written
    put16(&mut sb, 54, 0xffff); // no mount-count checks
    put16(&mut sb, 56, 0xef53);
    put16(&mut sb, 58, 1); // clean
    put16(&mut sb, 60, 1); // on errors, continue
    put32(&mut sb, 64, time); // last checked
    put32(&mut sb, 76, 1); // dynamic revision
    put32(&mut sb, 84, FIRST_INO);
    put16(&mut sb, 88, INODE_SIZE as u16);
    put32(&mut sb, 96, INCOMPAT_FILETYPE);
    put32(&mut sb, 100, RO_COMPAT_LARGE_FILE | RO_COMPAT_EXTRA_ISIZE);
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&vibe_core::keys::random_bytes()?[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    sb[104..120].copy_from_slice(&uuid);
    let name = label.as_bytes();
    sb[120..120 + name.len().min(16)].copy_from_slice(&name[..name.len().min(16)]);
    put32(&mut sb, 264, time); // made
    put16(&mut sb, 348, EXTRA_ISIZE);
    put16(&mut sb, 350, EXTRA_ISIZE);
    // A superblock and the descriptors at the start of every group, as
    // without sparse_super.
    for g in 0..geo.groups {
        put16(&mut sb, 90, g as u16);
        let base = g * BLOCKS_PER_GROUP;
        file.write_all_at(&sb, at(base) + if g == 0 { 1024 } else { 0 })?;
        file.write_all_at(&descs, at(base + 1))?;
    }

    // The inodes, then what each holds.
    for n in nodes {
        let i = u64::from(n.ino) - 1;
        let table = (i / ipg) * BLOCKS_PER_GROUP + 3 + geo.desc_blocks;
        let mut inode = vec![0u8; INODE_SIZE as usize];
        put16(&mut inode, 0, n.mode);
        put32(&mut inode, 4, n.size as u32);
        put32(&mut inode, 8, time);
        put32(&mut inode, 12, time);
        put32(&mut inode, 16, time);
        put16(&mut inode, 26, n.links);
        let sectors = (n.blocks.len() + n.map.len()) as u64 * (BLOCK / 512);
        put32(&mut inode, 28, u32_of(sectors, "a file's sectors")?);
        put32(&mut inode, 108, (n.size >> 32) as u32);
        put16(&mut inode, 128, EXTRA_ISIZE);
        put32(&mut inode, 144, time); // made
        match &n.kind {
            Kind::Symlink(target) if n.blocks.is_empty() => {
                inode[40..40 + target.len()].copy_from_slice(target);
            }
            _ => block_map(&mut inode, file, &n.blocks, &n.map)?,
        }
        file.write_all_at(&inode, at(table) + (i % ipg) * INODE_SIZE)?;

        match &n.kind {
            Kind::Dir(children) => {
                for (block, bytes) in n.blocks.iter().zip(dir_blocks(children)) {
                    file.write_all_at(&bytes, at(u64::from(*block)))?;
                }
            }
            Kind::Symlink(target) => {
                if let Some(block) = n.blocks.first() {
                    file.write_all_at(target, at(u64::from(*block)))?;
                }
            }
            Kind::File(Source::Bytes(bytes)) => {
                for (block, chunk) in n.blocks.iter().zip(bytes.chunks(BLOCK as usize)) {
                    write_unless_zero(file, chunk, at(u64::from(*block)))?;
                }
            }
            Kind::File(Source::Path(path)) => copy_in(file, path, n, &at)?,
        }
    }
    // Holes where APFS wrote the gaps out (vibe_env::sparse).
    crate::sparse::punch_zeros(file)?;
    file.sync_all()
}

/// Fills `inode`'s block pointers, and writes the indirect blocks.
fn block_map(inode: &mut [u8], file: &File, blocks: &[u32], map: &[u32]) -> io::Result<()> {
    let ptr = |inode: &mut [u8], i: usize, v: u32| put32(inode, 40 + 4 * i, v);
    for (i, b) in blocks.iter().take(DIRECT as usize).enumerate() {
        ptr(inode, i, *b);
    }
    let rest = blocks.get(DIRECT as usize..).unwrap_or_default();
    if rest.is_empty() {
        return Ok(());
    }
    let write_ptrs = |block: u32, ptrs: &[u32]| {
        let mut buf = vec![0u8; BLOCK as usize];
        for (i, p) in ptrs.iter().enumerate() {
            put32(&mut buf, 4 * i, *p);
        }
        file.write_all_at(&buf, u64::from(block) * BLOCK)
    };
    let (single, rest) = rest.split_at(rest.len().min(PER_BLOCK as usize));
    ptr(inode, 12, map[0]);
    write_ptrs(map[0], single)?;
    if rest.is_empty() {
        return Ok(());
    }
    ptr(inode, 13, map[1]);
    let leaves: Vec<u32> = map[2..].to_vec();
    write_ptrs(map[1], &leaves)?;
    for (leaf, ptrs) in leaves.iter().zip(rest.chunks(PER_BLOCK as usize)) {
        write_ptrs(*leaf, ptrs)?;
    }
    Ok(())
}

fn write_unless_zero(file: &File, chunk: &[u8], offset: u64) -> io::Result<()> {
    if chunk.iter().all(|&b| b == 0) {
        return Ok(());
    }
    file.write_all_at(chunk, offset)
}

/// Copies a host file into its blocks, leaving blocks of zeros as holes.
fn copy_in(file: &File, path: &Path, n: &Node, at: &impl Fn(u64) -> u64) -> io::Result<()> {
    let mut src = File::open(path)?;
    let mut buf = vec![0u8; BLOCK as usize];
    let mut left = n.size;
    for block in &n.blocks {
        let want = left.min(BLOCK) as usize;
        src.read_exact(&mut buf[..want]).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{} changed as it was read: {e}", path.display()),
            )
        })?;
        write_unless_zero(file, &buf[..want], at(u64::from(*block)))?;
        left -= want as u64;
    }
    // A file that grew since it was measured would be cut short.
    let mut one = [0u8; 1];
    if src.read(&mut one)? != 0 {
        return Err(io::Error::other(format!(
            "{} grew as it was read",
            path.display()
        )));
    }
    Ok(())
}

/// Whether `head`, an image's first 2 KiB or more, holds an ext4
/// superblock, as the guest tells a layer's filesystem by.
pub fn is_ext4(head: &[u8]) -> bool {
    head.get(1080..1082) == Some(&[0x53, 0xef])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tool(name: &str) -> Option<PathBuf> {
        [
            "/sbin",
            "/usr/sbin",
            "/usr/bin",
            "/bin",
            "/opt/homebrew/opt/e2fsprogs/sbin",
        ]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
    }

    /// `e2fsck -fn`, if it's installed: true if it found nothing wrong.
    fn fsck(image: &Path) -> Option<String> {
        let e2fsck = tool("e2fsck")?;
        let out = Command::new(e2fsck).arg("-fn").arg(image).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "e2fsck found problems:\n{text}");
        Some(text)
    }

    fn debugfs(image: &Path, request: &str) -> Option<Vec<u8>> {
        let debugfs = tool("debugfs")?;
        let out = Command::new(debugfs)
            .args(["-R", request])
            .arg(image)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(out.stdout)
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn map_sizes() {
        assert_eq!(map_blocks(0).unwrap(), 0);
        assert_eq!(map_blocks(12).unwrap(), 0);
        assert_eq!(map_blocks(13).unwrap(), 1);
        assert_eq!(map_blocks(12 + 1024).unwrap(), 1);
        assert_eq!(map_blocks(12 + 1024 + 1).unwrap(), 3);
        assert_eq!(map_blocks(12 + 1024 + 1024).unwrap(), 3);
        assert_eq!(map_blocks(12 + 1024 + 1025).unwrap(), 4);
        assert!(map_blocks(MAX_FILE / BLOCK).is_ok());
        assert!(map_blocks(MAX_FILE / BLOCK + 1).is_err());
    }

    #[test]
    fn directory_entries_pack_into_blocks() {
        let entries: Vec<_> = (0..400)
            .map(|i| (format!("entry-number-{i:04}").into_bytes(), 12 + i, 1u8))
            .collect();
        let blocks = dir_blocks(&entries);
        assert!(blocks.len() > 1);
        // Walking each block's records reaches its end exactly, and finds
        // every entry once.
        let mut seen = 0;
        for b in &blocks {
            assert_eq!(b.len(), BLOCK as usize);
            let mut at = 0;
            while at < b.len() {
                let rec = u16::from_le_bytes([b[at + 4], b[at + 5]]) as usize;
                assert!(rec >= 8 && rec.is_multiple_of(4));
                seen += 1;
                at += rec;
            }
            assert_eq!(at, b.len());
        }
        assert_eq!(seen, entries.len());
    }

    #[test]
    fn an_image_ext4_checks_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        // A file big enough for a double indirect block, with a hole.
        let big = tmp.path().join("big");
        let mut bytes = pattern(6 << 20, 7);
        bytes[(1 << 20)..(2 << 20)].fill(0);
        fs::write(&big, &bytes).unwrap();
        let mut tree = Tree::new();
        tree.file("opt/vibeos/bin/claude", Source::Path(big.clone()), 0o755)
            .file("opt/vibeos/empty", Source::Bytes(Vec::new()), 0o644)
            .file(
                "opt/vibeos/medium",
                Source::Bytes(pattern(100 * 4096 + 5, 3)),
                0o600,
            )
            .file("etc/note", Source::Bytes(b"hello\n".to_vec()), 0o644)
            .dir("opt/private", 0o700)
            .symlink("usr/bin/claude", "/opt/vibeos/bin/claude")
            .symlink("usr/bin/long", &"x/".repeat(100));
        for i in 0..300 {
            tree.file(
                &format!("many/a-rather-long-file-name-number-{i:03}"),
                Source::Bytes(format!("{i}\n").into_bytes()),
                0o644,
            );
        }
        let image = tmp.path().join("layer.img");
        let size = write(&tree, &image, "managed", 1_800_000_000).unwrap();
        assert_eq!(fs::metadata(&image).unwrap().len(), size);
        let head = fs::read(&image).unwrap();
        assert!(is_ext4(&head));
        // Sparse on the host: well under its size, less the hole.
        use std::os::unix::fs::MetadataExt;
        assert!(fs::metadata(&image).unwrap().blocks() * 512 < size);

        let Some(report) = fsck(&image) else {
            eprintln!("no e2fsck here: only the layout was checked");
            return;
        };
        assert!(report.contains("managed"), "{report}");
        let read = |p: &str| debugfs(&image, &format!("cat {p}")).unwrap();
        assert_eq!(read("/opt/vibeos/bin/claude"), bytes);
        assert_eq!(read("/opt/vibeos/medium"), pattern(100 * 4096 + 5, 3));
        assert_eq!(read("/opt/vibeos/empty"), b"");
        assert_eq!(read("/etc/note"), b"hello\n");
        assert_eq!(read("/many/a-rather-long-file-name-number-299"), b"299\n");
        let stat =
            |p: &str| String::from_utf8(debugfs(&image, &format!("stat {p}")).unwrap()).unwrap();
        let claude = stat("/opt/vibeos/bin/claude");
        assert!(claude.contains("Mode:  0755"), "{claude}");
        assert!(claude.contains("User:     0"), "{claude}");
        assert!(stat("/opt/private").contains("Mode:  0700"));
        let link = stat("/usr/bin/claude");
        assert!(
            link.contains("Fast link dest: \"/opt/vibeos/bin/claude\""),
            "{link}"
        );
        let ls = String::from_utf8(debugfs(&image, "ls -l /usr/bin").unwrap()).unwrap();
        assert!(ls.contains("long"), "{ls}");
    }

    #[test]
    fn an_almost_empty_image_still_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("e.img");
        let mut tree = Tree::new();
        tree.dir("opt", 0o755);
        write(&tree, &image, "empty", 1).unwrap();
        fsck(&image);
    }

    #[test]
    fn several_groups_check() {
        // Over 128 MiB of data takes more than one group.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let f = File::create(&src).unwrap();
        f.set_len(200 << 20).unwrap();
        f.write_all_at(b"start", 0).unwrap();
        f.write_all_at(b"end", (200 << 20) - 3).unwrap();
        drop(f);
        let image = tmp.path().join("g.img");
        let mut tree = Tree::new();
        tree.file("a/big", Source::Path(src.clone()), 0o644).file(
            "z/after",
            Source::Bytes(b"after".to_vec()),
            0o644,
        );
        write(&tree, &image, "groups", 1).unwrap();
        if let Some(report) = fsck(&image) {
            assert!(report.contains("groups"), "{report}");
            let stats = String::from_utf8(debugfs(&image, "stats").unwrap()).unwrap();
            assert!(stats.contains("Group  1:"), "{stats}");
            assert_eq!(debugfs(&image, "cat /z/after").unwrap(), b"after");
            let got = debugfs(&image, "cat /a/big").unwrap();
            assert_eq!(got.len(), 200 << 20);
            assert_eq!(&got[..5], b"start");
            assert_eq!(&got[got.len() - 3..], b"end");
        }
    }

    #[test]
    fn bad_trees_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("x.img");
        let mut tree = Tree::new();
        tree.file("a", Source::Bytes(vec![1]), 0o644)
            .file("a/b", Source::Bytes(vec![2]), 0o644);
        assert!(write(&tree, &out, "x", 1).is_err());
        let mut tree = Tree::new();
        tree.file("a/../b", Source::Bytes(vec![1]), 0o644);
        assert!(write(&tree, &out, "x", 1).is_err());
        let mut tree = Tree::new();
        tree.symlink("l", "");
        assert!(write(&tree, &out, "x", 1).is_err());
        let mut tree = Tree::new();
        tree.file("missing", Source::Path(tmp.path().join("nope")), 0o644);
        assert!(write(&tree, &out, "x", 1).is_err());
        assert!(!out.exists(), "a refused image leaves nothing");
        // Nor does it replace a file.
        fs::write(&out, "keep").unwrap();
        let mut tree = Tree::new();
        tree.dir("d", 0o755);
        assert!(write(&tree, &out, "x", 1).is_err());
        assert_eq!(fs::read_to_string(&out).unwrap(), "keep");
    }
}
