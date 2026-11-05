//! The registry: one SQLite database, `vibe.db`, holding hosts, projects,
//! profiles, agents and API key hashes (PRD: Where state lives). Every row
//! that belongs to a host says which, from day one.
//!
//! The schema is versioned with `PRAGMA user_version` and only ever migrated
//! forward. A database from a newer `vibed` is refused rather than guessed
//! at.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    AdminNeed, AgentKind, AgentState, Environment, HostLink, Mode, Model, Profile, ProjectSource,
    Reasoning,
};
use crate::rpc::{ApiKey, HostInfo, KeyHash, Project};
use crate::time::rfc3339;

/// The URL outside MCP callers reach the external address at.
pub fn external_url(domain: &str) -> String {
    format!("https://{domain}/mcp")
}

/// Each entry moves the schema up one version.
const MIGRATIONS: &[&str] = &[
    // 1: the registry as #2 left it. Agents are filled in by #4.
    "
    CREATE TABLE hosts (
        name     TEXT PRIMARY KEY,
        os       TEXT NOT NULL,
        label    TEXT NOT NULL,
        -- The SSH destination of a Linux host (#16); NULL for this host.
        address  TEXT,
        added_at INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE projects (
        id       INTEGER PRIMARY KEY,
        host     TEXT NOT NULL REFERENCES hosts (name),
        name     TEXT NOT NULL,
        source   TEXT NOT NULL CHECK (source IN ('github', 'path')),
        repo     TEXT,
        path     TEXT NOT NULL,
        added_at INTEGER NOT NULL,
        UNIQUE (host, name),
        UNIQUE (host, path)
    ) STRICT;

    CREATE TABLE profiles (
        id          INTEGER PRIMARY KEY,
        project_id  INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        environment TEXT NOT NULL,
        model       TEXT NOT NULL,
        reasoning   TEXT NOT NULL,
        mode        TEXT NOT NULL,
        memory      INTEGER NOT NULL,
        UNIQUE (project_id, name)
    ) STRICT;

    -- One agent, one worktree: the worktree is a column, not a table.
    -- Removing a project with agents is refused by the foreign key too.
    CREATE TABLE agents (
        id          INTEGER PRIMARY KEY,
        host        TEXT NOT NULL REFERENCES hosts (name),
        ref         TEXT NOT NULL,
        project_id  INTEGER NOT NULL REFERENCES projects (id),
        profile     TEXT NOT NULL,
        environment TEXT NOT NULL,
        worktree    TEXT NOT NULL,
        branch      TEXT NOT NULL,
        session_id  TEXT NOT NULL,
        shim_socket TEXT,
        state       TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        UNIQUE (host, ref)
    ) STRICT;

    CREATE TABLE api_keys (
        id           INTEGER PRIMARY KEY,
        name         TEXT NOT NULL UNIQUE,
        hash         TEXT NOT NULL UNIQUE,
        prefix       TEXT NOT NULL,
        created_at   INTEGER NOT NULL,
        last_used_at INTEGER
    ) STRICT;
    ",
    // 2: what an agent was started with. A profile can change or go after
    // the start, but the agent keeps its settings for life, so a resume
    // launches the same Claude. Its compartment's handle, which only its
    // backend reads. And whether its start is still under way: a row is
    // added before anything else is made, so a start cut short by a crash
    // can be undone when vibed starts again.
    "
    ALTER TABLE agents ADD COLUMN model TEXT NOT NULL DEFAULT 'opus';
    ALTER TABLE agents ADD COLUMN reasoning TEXT NOT NULL DEFAULT 'high';
    ALTER TABLE agents ADD COLUMN mode TEXT NOT NULL DEFAULT 'auto';
    ALTER TABLE agents ADD COLUMN memory INTEGER NOT NULL DEFAULT 4294967296;
    ALTER TABLE agents ADD COLUMN compartment TEXT NOT NULL DEFAULT '{}';
    ALTER TABLE agents ADD COLUMN starting INTEGER NOT NULL DEFAULT 0;
    ",
    // 3: Linux hosts (#16). What each one said of itself when its link last
    // came up, and its agents as it last listed them, for while its link is
    // down. And the API keys a Mac pushed to this host, by their hashes:
    // apart from the host's own, as the Mac replaces them all at once.
    "
    ALTER TABLE hosts ADD COLUMN version TEXT;
    ALTER TABLE hosts ADD COLUMN missing TEXT NOT NULL DEFAULT '';
    ALTER TABLE hosts ADD COLUMN agents TEXT NOT NULL DEFAULT '[]';

    CREATE TABLE pushed_keys (
        id           INTEGER PRIMARY KEY,
        name         TEXT NOT NULL,
        hash         TEXT NOT NULL UNIQUE,
        prefix       TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        last_used_at INTEGER
    ) STRICT;
    ",
    // 4: external access (#18). The public name a host is the external
    // address at: on a Mac, for a Linux host it chose; on a Linux host, its
    // own (this host's row). NULL while it isn't.
    "
    ALTER TABLE hosts ADD COLUMN external TEXT;
    ",
    // 5: shared MCP services (#51): on for every profile, and every agent
    // already made, unless switched off.
    "
    ALTER TABLE profiles ADD COLUMN shared_services INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE agents ADD COLUMN shared_services INTEGER NOT NULL DEFAULT 1;
    ",
    // 6: Pi (#52). Which coding agent a profile runs and an agent was started
    // with, and Pi's model as `provider/id`. Everything before this is
    // Claude's.
    "
    ALTER TABLE profiles ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude';
    ALTER TABLE profiles ADD COLUMN pi_model TEXT;
    ALTER TABLE agents ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude';
    ALTER TABLE agents ADD COLUMN pi_model TEXT;
    ",
    // 7: pushed keys revoked on this host (#59), by their hashes, so that the Mac's
    // next push doesn't bring them back.
    "
    CREATE TABLE revoked_pushed_keys (
        hash TEXT PRIMARY KEY
    ) STRICT;
    ",
    // 8: what a Linux host last said it needs `sudo` for (#74), kept for while its
    // link is down.
    "
    ALTER TABLE hosts ADD COLUMN needs_admin TEXT NOT NULL DEFAULT '';
    ",
    // 9: a host shared by more than one Mac, and an external address that's the
    // host's own (#100). Which Mac pushed each key, so that one replaces only its
    // own, with '' for what came from a Mac too old to say. This install's id, for
    // saying so. On a Mac, when the choice it couldn't deliver yet was made. On a
    // Linux host's own row, when its external address changed and who asked.
    "
    ALTER TABLE pushed_keys ADD COLUMN source TEXT NOT NULL DEFAULT '';
    ALTER TABLE revoked_pushed_keys ADD COLUMN source TEXT NOT NULL DEFAULT '';
    CREATE TABLE install (id TEXT NOT NULL) STRICT;
    ALTER TABLE hosts ADD COLUMN external_asked_at INTEGER;
    ALTER TABLE hosts ADD COLUMN external_changed_at INTEGER;
    ALTER TABLE hosts ADD COLUMN external_changed_by TEXT;
    ALTER TABLE hosts ADD COLUMN external_chosen TEXT;
    ALTER TABLE hosts ADD COLUMN external_seen TEXT;
    ",
];

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The database was made by a newer `vibed`.
    NewerSchema {
        path: PathBuf,
        found: i64,
    },
    NotFound(String),
    Conflict(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "registry error: {e}"),
            StoreError::Io { path, source } => {
                write!(f, "can't open {}: {source}", path.display())
            }
            StoreError::NewerSchema { path, found } => write!(
                f,
                "{} is from a newer vibed (schema {found}; this one knows up to {SCHEMA_VERSION})",
                path.display()
            ),
            StoreError::NotFound(m) | StoreError::Conflict(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A project before it's in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    pub host: String,
    pub name: String,
    pub source: ProjectSource,
    pub repo: Option<String>,
    pub path: String,
}

/// An agent's row. A project with agents can't be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub host: String,
    pub agent_ref: String,
    pub project: String,
    /// The profile it was started from, by name. Its settings are copied
    /// below, so they hold even if the profile changes or goes.
    pub profile: String,
    pub environment: Environment,
    pub model: Model,
    pub reasoning: Reasoning,
    pub mode: Mode,
    pub memory: u64,
    pub worktree: String,
    pub branch: String,
    pub session_id: String,
    pub shim_socket: Option<String>,
    pub state: AgentState,
    pub created_at: i64,
    /// The compartment's handle as its backend wrote it (JSON); nothing
    /// else reads it.
    pub compartment: String,
    /// Its start hasn't finished. Once `vibed` is up, that means the start
    /// was cut short, and what it made is undone.
    pub starting: bool,
    /// Whether it got the shared MCP services as it started. Each launch goes by
    /// what its profile says then: this is for when that profile has gone.
    pub shared_services: bool,
    /// Which coding agent it runs, for life.
    pub agent: AgentKind,
    /// Pi's model, for one that runs Pi and whose profile named one.
    pub pi_model: Option<String>,
}

const AGENT_COLUMNS: &str = "a.host, a.ref, p.name, a.profile, a.environment, a.model, \
     a.reasoning, a.mode, a.memory, a.worktree, a.branch, a.session_id, a.shim_socket, a.state, \
     a.created_at, a.compartment, a.starting, a.shared_services, a.agent, a.pi_model";

fn agent_row(r: &Row<'_>) -> rusqlite::Result<AgentRecord> {
    let memory: i64 = r.get(8)?;
    Ok(AgentRecord {
        host: r.get(0)?,
        agent_ref: r.get(1)?,
        project: r.get(2)?,
        profile: r.get(3)?,
        environment: parse(r, 4)?,
        model: parse(r, 5)?,
        reasoning: parse(r, 6)?,
        mode: parse(r, 7)?,
        memory: u64::try_from(memory).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(e))
        })?,
        worktree: r.get(9)?,
        branch: r.get(10)?,
        session_id: r.get(11)?,
        shim_socket: r.get(12)?,
        state: parse(r, 13)?,
        created_at: r.get(14)?,
        compartment: r.get(15)?,
        starting: r.get(16)?,
        shared_services: r.get(17)?,
        agent: parse(r, 18)?,
        pi_model: r.get(19)?,
    })
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the registry at `path`, private to this user, and
    /// brings its schema up to date.
    pub fn open(path: &Path) -> Result<Store> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| StoreError::Io {
                path: path.to_owned(),
                source,
            })?;
        let conn = Connection::open(path)?;
        // WAL keeps readers and the writer out of each other's way; every
        // commit is still durable against a crash of the process.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        Store::init(conn, path)
    }

    /// A registry that lives only as long as this value, for tests.
    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?, Path::new(":memory:"))
    }

    fn init(conn: Connection, path: &Path) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn };
        store.migrate(path)?;
        Ok(store)
    }

    fn migrate(&mut self, path: &Path) -> Result<()> {
        let found: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > SCHEMA_VERSION as i64 {
            return Err(StoreError::NewerSchema {
                path: path.to_owned(),
                found,
            });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(found.max(0) as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    // Hosts.

    /// Records this host, or updates its OS and label if they changed.
    pub fn ensure_host(&mut self, name: &str, os: &str, label: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO hosts (name, os, label, added_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (name) DO UPDATE SET os = excluded.os, label = excluded.label",
            params![name, os, label, now],
        )?;
        Ok(())
    }

    /// Every host, this one first, then the Linux hosts added to it by
    /// name. Another host's link is down here, and nothing says whether it
    /// serves MCP, nor how its tunnel is: those are the daemon's to say.
    /// Whether it's the external address, and at what URL, is recorded.
    pub fn hosts(&self) -> Result<Vec<HostInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, os, label, address, version, missing, external, needs_admin FROM hosts \
                 ORDER BY address IS NOT NULL, name",
        )?;
        let hosts = stmt
            .query_map([], |r| {
                let address: Option<String> = r.get(3)?;
                let missing: String = r.get(5)?;
                let external: Option<String> = r.get(6)?;
                let needs_admin: String = r.get(7)?;
                Ok(HostInfo {
                    name: r.get(0)?,
                    os: r.get(1)?,
                    label: r.get(2)?,
                    link: if address.is_none() {
                        HostLink::Local
                    } else {
                        HostLink::Down
                    },
                    external: external.is_some(),
                    mcp: false,
                    address,
                    version: r.get(4)?,
                    missing: split_list(&missing),
                    detail: None,
                    url: external.as_deref().map(external_url),
                    tunnel: None,
                    external_changed_at: None,
                    external_note: None,
                    external_serves: None,
                    external_last: None,
                    external_detail: None,
                    memory: None,
                    // Any this build doesn't know are left out.
                    needs_admin: split_list(&needs_admin)
                        .iter()
                        .filter_map(|n| n.parse().ok())
                        .collect(),
                    upgrade_error: None,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(hosts)
    }

    /// One host, as [`Store::hosts`] gives it.
    pub fn host(&self, name: &str) -> Result<HostInfo> {
        self.hosts()?
            .into_iter()
            .find(|h| h.name == name)
            .ok_or_else(|| StoreError::NotFound(format!("no host named {name}")))
    }

    /// Records a Linux host reached over SSH at `address`.
    pub fn add_host(&mut self, host: &HostInfo, now: i64) -> Result<HostInfo> {
        let Some(address) = &host.address else {
            return Err(StoreError::Conflict(
                "a Linux host needs an SSH address".into(),
            ));
        };
        let tx = self.conn.transaction()?;
        let taken = tx
            .query_row("SELECT 1 FROM hosts WHERE name = ?1", [&host.name], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if taken {
            return Err(StoreError::Conflict(format!(
                "a host named {} already exists",
                host.name
            )));
        }
        tx.execute(
            "INSERT INTO hosts (name, os, label, address, added_at, version, missing, needs_admin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                host.name,
                host.os,
                host.label,
                address,
                now,
                host.version,
                host.missing.join(","),
                admin_needs_text(&host.needs_admin),
            ],
        )?;
        tx.commit()?;
        self.host(&host.name)
    }

    /// Updates what a Linux host says of itself: its OS, hostname, `vibed`
    /// version and missing tools.
    pub fn set_host_report(&mut self, host: &HostInfo) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE hosts SET os = ?2, label = ?3, version = ?4, missing = ?5, needs_admin = ?6
             WHERE name = ?1 AND address IS NOT NULL",
            params![
                host.name,
                host.os,
                host.label,
                host.version,
                host.missing.join(","),
                admin_needs_text(&host.needs_admin),
            ],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!(
                "no Linux host named {}",
                host.name
            )));
        }
        Ok(())
    }

    /// Forgets a Linux host, and its agents as it last listed them. This
    /// host can't be removed, nor one with projects or agents recorded here.
    pub fn remove_host(&mut self, name: &str) -> Result<HostInfo> {
        let host = self.host(name)?;
        if host.address.is_none() {
            return Err(StoreError::Conflict(format!(
                "{name} is this host, which can't be removed"
            )));
        }
        let used: i64 = self.conn.query_row(
            "SELECT (SELECT count(*) FROM projects WHERE host = ?1)
                  + (SELECT count(*) FROM agents WHERE host = ?1)",
            [name],
            |r| r.get(0),
        )?;
        if used > 0 {
            return Err(StoreError::Conflict(format!(
                "{name} has projects recorded on this host; remove them first"
            )));
        }
        self.conn
            .execute("DELETE FROM hosts WHERE name = ?1", [name])?;
        Ok(host)
    }

    /// The public name `name` is the external address at, if it is.
    pub fn host_external(&self, name: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT external FROM hosts WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("no host named {name}")))
    }

    /// On a Mac, when the choice for `name` that hasn't reached it yet was made.
    pub fn host_external_asked(&self, name: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT external_asked_at FROM hosts WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// On a Mac, notes that the choice for `name` is waiting to reach it, since `at`,
    /// or with `None` that it has nothing waiting.
    pub fn set_host_external_asked(&mut self, name: &str, at: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET external_asked_at = ?2 WHERE name = ?1",
            params![name, at],
        )?;
        Ok(())
    }

    /// On a Mac, the name someone chose for `name`'s external address that it has since
    /// stopped serving, to go back to if it serves that name again.
    pub fn host_external_chosen(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT external_chosen FROM hosts WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// On a Mac, notes or with `None` clears what [`Self::host_external_chosen`] says.
    pub fn set_host_external_chosen(&mut self, name: &str, domain: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET external_chosen = ?2 WHERE name = ?1",
            params![name, domain],
        )?;
        Ok(())
    }

    /// On a Mac, the name `name` last said it serves, known while its link is down.
    pub fn host_external_seen(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT external_seen FROM hosts WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// On a Mac, notes what [`Self::host_external_seen`] says.
    pub fn set_host_external_seen(&mut self, name: &str, domain: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET external_seen = ?2 WHERE name = ?1",
            params![name, domain],
        )?;
        Ok(())
    }

    /// On a Mac, forgets every such name: someone has chosen afresh.
    pub fn clear_host_external_chosen(&mut self) -> Result<()> {
        self.conn
            .execute("UPDATE hosts SET external_chosen = NULL", [])?;
        Ok(())
    }

    /// On a Linux host, when its external address last changed and who asked.
    pub fn host_external_changed(&self, name: &str) -> Result<Option<(i64, Option<String>)>> {
        let row: Option<(Option<i64>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT external_changed_at, external_changed_by FROM hosts WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(at, by)| at.map(|at| (at, by))))
    }

    /// On a Linux host, notes when its external address changed and who asked.
    pub fn set_host_external_changed(
        &mut self,
        name: &str,
        at: i64,
        by: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET external_changed_at = ?2, external_changed_by = ?3 WHERE name = ?1",
            params![name, at, by],
        )?;
        Ok(())
    }

    /// Records that `name` is the external address at `domain`, or with
    /// `None` that it isn't. On a Mac, at most one host is: choosing one
    /// turns any other off, in the same transaction.
    pub fn set_host_external(&mut self, name: &str, domain: Option<&str>) -> Result<()> {
        let tx = self.conn.transaction()?;
        if domain.is_some() {
            tx.execute("UPDATE hosts SET external = NULL WHERE name != ?1", [name])?;
        }
        let n = tx.execute(
            "UPDATE hosts SET external = ?2 WHERE name = ?1",
            params![name, domain],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("no host named {name}")));
        }
        tx.commit()?;
        Ok(())
    }

    /// A Linux host's agents as it last listed them (JSON), kept for while
    /// its link is down.
    pub fn host_agents(&self, name: &str) -> Result<String> {
        self.conn
            .query_row("SELECT agents FROM hosts WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("no host named {name}")))
    }

    pub fn set_host_agents(&mut self, name: &str, agents: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE hosts SET agents = ?2 WHERE name = ?1 AND address IS NOT NULL",
            params![name, agents],
        )?;
        Ok(())
    }

    pub fn has_host(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM hosts WHERE name = ?1", [name], |_| Ok(()))
            .optional()?
            .is_some())
    }

    // Projects.

    /// Adds a project and its first profiles, all or nothing.
    pub fn add_project(
        &mut self,
        p: &NewProject,
        profiles: &[Profile],
        now: i64,
    ) -> Result<Project> {
        let tx = self.conn.transaction()?;
        if project_id(&tx, &p.host, &p.name)?.is_some() {
            return Err(StoreError::Conflict(format!(
                "a project named {} already exists on {}",
                p.name, p.host
            )));
        }
        let same_path: Option<String> = tx
            .query_row(
                "SELECT name FROM projects WHERE host = ?1 AND path = ?2",
                params![p.host, p.path],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = same_path {
            return Err(StoreError::Conflict(format!(
                "{} is already the project {other}",
                p.path
            )));
        }
        tx.execute(
            "INSERT INTO projects (host, name, source, repo, path, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![p.host, p.name, p.source.as_str(), p.repo, p.path, now],
        )?;
        let id = tx.last_insert_rowid();
        for profile in profiles {
            insert_profile(&tx, id, profile)?;
        }
        tx.commit()?;
        self.project(&p.host, &p.name)
    }

    /// A host's projects, by name.
    pub fn projects(&self, host: &str) -> Result<Vec<Project>> {
        let names: Vec<String> = self
            .conn
            .prepare("SELECT name FROM projects WHERE host = ?1 ORDER BY name")?
            .query_map([host], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        names.iter().map(|n| self.project(host, n)).collect()
    }

    pub fn project(&self, host: &str, name: &str) -> Result<Project> {
        let row = self
            .conn
            .query_row(
                "SELECT id, source, repo, path, added_at FROM projects
                 WHERE host = ?1 AND name = ?2",
                params![host, name],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        parse::<ProjectSource>(r, 1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let (id, source, repo, path, added_at) = row.ok_or_else(|| no_project(host, name))?;
        Ok(Project {
            host: host.to_owned(),
            name: name.to_owned(),
            source,
            repo,
            path,
            added_at: rfc3339(added_at),
            profiles: profiles_of(&self.conn, id)?,
            deleted: false,
        })
    }

    /// Forgets a project and its profiles. Refused while it has agents.
    pub fn remove_project(&mut self, host: &str, name: &str) -> Result<Project> {
        let project = self.project(host, name)?;
        let tx = self.conn.transaction()?;
        let id = project_id(&tx, host, name)?.ok_or_else(|| no_project(host, name))?;
        let agents: i64 = tx.query_row(
            "SELECT count(*) FROM agents WHERE project_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if agents > 0 {
            return Err(StoreError::Conflict(format!(
                "{name} still has {agents} agent{}; delete {} first",
                if agents == 1 { "" } else { "s" },
                if agents == 1 { "it" } else { "them" }
            )));
        }
        tx.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(project)
    }

    // Profiles.

    /// A project's profiles, oldest first.
    pub fn profiles(&self, host: &str, project: &str) -> Result<Vec<Profile>> {
        let id = self.require_project(host, project)?;
        profiles_of(&self.conn, id)
    }

    pub fn profile(&self, host: &str, project: &str, name: &str) -> Result<Profile> {
        let id = self.require_project(host, project)?;
        self.conn
            .query_row(
                &format!(
                    "SELECT {PROFILE_COLUMNS} FROM profiles WHERE project_id = ?1 AND name = ?2"
                ),
                params![id, name],
                profile_row,
            )
            .optional()?
            .ok_or_else(|| no_profile(project, name))
    }

    pub fn create_profile(
        &mut self,
        host: &str,
        project: &str,
        profile: &Profile,
    ) -> Result<Profile> {
        let tx = self.conn.transaction()?;
        let id = project_id(&tx, host, project)?.ok_or_else(|| no_project(host, project))?;
        if profile_exists(&tx, id, &profile.name)? {
            return Err(profile_taken(project, &profile.name));
        }
        insert_profile(&tx, id, profile)?;
        tx.commit()?;
        Ok(profile.clone())
    }

    /// Replaces the profile named `name` with `profile`, which may rename it.
    pub fn replace_profile(
        &mut self,
        host: &str,
        project: &str,
        name: &str,
        profile: &Profile,
    ) -> Result<Profile> {
        let tx = self.conn.transaction()?;
        let id = project_id(&tx, host, project)?.ok_or_else(|| no_project(host, project))?;
        if !profile_exists(&tx, id, name)? {
            return Err(no_profile(project, name));
        }
        if profile.name != name && profile_exists(&tx, id, &profile.name)? {
            return Err(profile_taken(project, &profile.name));
        }
        tx.execute(
            "UPDATE profiles SET name = ?3, environment = ?4, model = ?5, reasoning = ?6,
                 mode = ?7, memory = ?8, shared_services = ?9, agent = ?10, pi_model = ?11
             WHERE project_id = ?1 AND name = ?2",
            params![
                id,
                name,
                profile.name,
                profile.environment.as_str(),
                profile.model.as_str(),
                profile.reasoning.as_str(),
                profile.mode.as_str(),
                memory_column(profile.memory)?,
                profile.shared_services,
                profile.agent.as_str(),
                profile.pi_model,
            ],
        )?;
        // Its agents are still its agents, and go by its settings at each launch.
        tx.execute(
            "UPDATE agents SET profile = ?3 WHERE project_id = ?1 AND profile = ?2",
            params![id, name, profile.name],
        )?;
        tx.commit()?;
        Ok(profile.clone())
    }

    /// Whether any profile runs Pi in the `linux` environment.
    pub fn has_linux_pi_profile(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM profiles WHERE agent = 'pi' AND environment = 'linux' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn delete_profile(&mut self, host: &str, project: &str, name: &str) -> Result<Profile> {
        let profile = self.profile(host, project, name)?;
        let id = self.require_project(host, project)?;
        self.conn.execute(
            "DELETE FROM profiles WHERE project_id = ?1 AND name = ?2",
            params![id, name],
        )?;
        Ok(profile)
    }

    fn require_project(&self, host: &str, name: &str) -> Result<i64> {
        project_id(&self.conn, host, name)?.ok_or_else(|| no_project(host, name))
    }

    // Agents.

    pub fn add_agent(&mut self, a: &AgentRecord) -> Result<()> {
        let tx = self.conn.transaction()?;
        let id =
            project_id(&tx, &a.host, &a.project)?.ok_or_else(|| no_project(&a.host, &a.project))?;
        let taken = tx
            .query_row(
                "SELECT 1 FROM agents WHERE host = ?1 AND ref = ?2",
                params![a.host, a.agent_ref],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if taken {
            return Err(StoreError::Conflict(format!(
                "an agent {} already exists on {}",
                a.agent_ref, a.host
            )));
        }
        tx.execute(
            "INSERT INTO agents (host, ref, project_id, profile, environment, model, reasoning,
                 mode, memory, worktree, branch, session_id, shim_socket, state, created_at,
                 compartment, starting, shared_services, agent, pi_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20)",
            params![
                a.host,
                a.agent_ref,
                id,
                a.profile,
                a.environment.as_str(),
                a.model.as_str(),
                a.reasoning.as_str(),
                a.mode.as_str(),
                memory_column(a.memory)?,
                a.worktree,
                a.branch,
                a.session_id,
                a.shim_socket,
                a.state.as_str(),
                a.created_at,
                a.compartment,
                a.starting,
                a.shared_services,
                a.agent.as_str(),
                a.pi_model,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A host's agents, oldest first.
    pub fn agents(&self, host: &str) -> Result<Vec<AgentRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a JOIN projects p ON p.id = a.project_id
             WHERE a.host = ?1 ORDER BY a.id"
        ))?;
        let agents = stmt
            .query_map([host], agent_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(agents)
    }

    pub fn agent(&self, host: &str, agent_ref: &str) -> Result<AgentRecord> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {AGENT_COLUMNS} FROM agents a JOIN projects p ON p.id = a.project_id
                     WHERE a.host = ?1 AND a.ref = ?2"
                ),
                params![host, agent_ref],
                agent_row,
            )
            .optional()?
            .ok_or_else(|| no_agent(host, agent_ref))
    }

    /// Records an agent's state, and returns whether it changed.
    pub fn set_agent_state(
        &mut self,
        host: &str,
        agent_ref: &str,
        state: AgentState,
    ) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT state FROM agents WHERE host = ?1 AND ref = ?2",
                params![host, agent_ref],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| no_agent(host, agent_ref))?;
        if found == state.as_str() {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE agents SET state = ?3 WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref, state.as_str()],
        )?;
        Ok(true)
    }

    /// Records an agent's compartment handle, as its backend changed it.
    pub fn set_agent_compartment(
        &mut self,
        host: &str,
        agent_ref: &str,
        compartment: &str,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET compartment = ?3 WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref, compartment],
        )?;
        if n == 0 {
            return Err(no_agent(host, agent_ref));
        }
        Ok(())
    }

    /// Records that an agent's start has finished.
    pub fn finish_agent_start(&mut self, host: &str, agent_ref: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET starting = 0 WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref],
        )?;
        if n == 0 {
            return Err(no_agent(host, agent_ref));
        }
        Ok(())
    }

    /// Records the Claude session an agent is now in: a `/clear` starts a
    /// new one, which a resume must continue.
    pub fn set_agent_session(
        &mut self,
        host: &str,
        agent_ref: &str,
        session_id: &str,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET session_id = ?3 WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref, session_id],
        )?;
        if n == 0 {
            return Err(no_agent(host, agent_ref));
        }
        Ok(())
    }

    /// Records where an agent's session now is, after a new shim started.
    pub fn set_agent_socket(&mut self, host: &str, agent_ref: &str, socket: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET shim_socket = ?3 WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref, socket],
        )?;
        if n == 0 {
            return Err(no_agent(host, agent_ref));
        }
        Ok(())
    }

    pub fn remove_agent(&mut self, host: &str, agent_ref: &str) -> Result<()> {
        let n = self.conn.execute(
            "DELETE FROM agents WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref],
        )?;
        if n == 0 {
            return Err(no_agent(host, agent_ref));
        }
        Ok(())
    }

    // API keys.

    pub fn create_key(&mut self, name: &str, hash: &str, prefix: &str, now: i64) -> Result<ApiKey> {
        let tx = self.conn.transaction()?;
        let taken = tx
            .query_row("SELECT 1 FROM api_keys WHERE name = ?1", [name], |_| Ok(()))
            .optional()?
            .is_some();
        if taken {
            return Err(StoreError::Conflict(format!(
                "a key named {name} already exists"
            )));
        }
        tx.execute(
            "INSERT INTO api_keys (name, hash, prefix, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, hash, prefix, now],
        )?;
        tx.commit()?;
        self.key(name)
    }

    /// Every key, oldest first: this host's own, then those a Mac pushed.
    pub fn keys(&self) -> Result<Vec<ApiKey>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {KEY_COLUMNS} FROM api_keys ORDER BY id"))?;
        let mut keys: Vec<ApiKey> = stmt
            .query_map([], key_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PUSHED_KEY_COLUMNS} FROM pushed_keys ORDER BY id"
        ))?;
        keys.extend(
            stmt.query_map([], pushed_key_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        Ok(keys)
    }

    /// This host's own keys, by their hashes, as a Mac pushes them to the
    /// hosts it's linked to. Keys pushed here aren't passed on.
    pub fn key_hashes(&self) -> Result<Vec<KeyHash>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, prefix, hash, created_at FROM api_keys ORDER BY id")?;
        let keys = stmt
            .query_map([], |r| {
                Ok(KeyHash {
                    name: r.get(0)?,
                    prefix: r.get(1)?,
                    hash: r.get(2)?,
                    created_at: rfc3339(r.get(3)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(keys)
    }

    /// Replaces the keys `source` pushed with `keys`, all at once, leaving what any
    /// other Mac pushed. A key that stays keeps when it was last used here.
    ///
    /// What came from a Mac too old to say, source `''`, is adopted by the Mac that
    /// pushes the same hash: that's the Mac it was, so the rest of what it pushed then
    /// goes too, as it would have. A Mac that shares none of them isn't that one, and
    /// takes nothing from it.
    pub fn sync_pushed_keys(&mut self, source: &str, keys: &[KeyHash]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let hashes: Vec<&str> = keys.iter().map(|k| k.hash.as_str()).collect();
        // Whether this is the Mac that pushed what has no source.
        let mut scope = vec![source];
        if !source.is_empty() {
            let mut stmt = tx.prepare("SELECT hash FROM pushed_keys WHERE source = ''")?;
            let shares = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .any(|h| hashes.contains(&h.as_str()));
            if shares {
                scope.push("");
            }
        }
        let mut kept = std::collections::HashMap::new();
        for source in &scope {
            let mut stmt =
                tx.prepare("SELECT hash, last_used_at FROM pushed_keys WHERE source = ?1")?;
            for row in stmt.query_map([source], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })? {
                let (hash, used) = row?;
                if hashes.contains(&hash.as_str()) {
                    kept.insert(hash, used);
                }
            }
            tx.execute("DELETE FROM pushed_keys WHERE source = ?1", [source])?;
        }
        // Revoked here, and so it stays. One the Mac has stopped pushing needn't be
        // remembered: no key comes back with the same hash but by chance.
        let mut revoked = std::collections::HashSet::new();
        for from in &scope {
            let mut stmt = tx.prepare("SELECT hash FROM revoked_pushed_keys WHERE source = ?1")?;
            for row in stmt.query_map([from], |r| r.get::<_, String>(0))? {
                let hash = row?;
                if hashes.contains(&hash.as_str()) {
                    revoked.insert(hash);
                } else {
                    tx.execute("DELETE FROM revoked_pushed_keys WHERE hash = ?1", [&hash])?;
                }
            }
        }
        // Anyone's revoked: pushing it again from here doesn't bring it back.
        for k in &hashes {
            let gone: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM revoked_pushed_keys WHERE hash = ?1)",
                [k],
                |r| r.get(0),
            )?;
            if gone {
                revoked.insert((*k).to_owned());
            }
        }
        for from in scope.iter().filter(|f| f.is_empty()) {
            tx.execute(
                "UPDATE revoked_pushed_keys SET source = ?1 WHERE source = ?2",
                params![source, from],
            )?;
        }
        for k in keys.iter().filter(|k| !revoked.contains(&k.hash)) {
            tx.execute(
                "INSERT INTO pushed_keys (name, hash, prefix, created_at, last_used_at, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (hash) DO NOTHING",
                params![
                    k.name,
                    k.hash,
                    k.prefix,
                    k.created_at,
                    kept.get(&k.hash).copied().flatten(),
                    source
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// This install's id, made the first time it's asked for: what a Mac tells a host
    /// its keys are from.
    pub fn install_id(&mut self, make: impl FnOnce() -> String) -> Result<String> {
        let tx = self.conn.transaction()?;
        let found: Option<String> = tx
            .query_row("SELECT id FROM install", [], |r| r.get(0))
            .optional()?;
        let id = match found {
            Some(id) => id,
            None => {
                let id = make();
                tx.execute("INSERT INTO install (id) VALUES (?1)", [&id])?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }

    /// The pushed key `name`, gone for good on this host, if there is one.
    fn revoke_pushed_key(&mut self, name: &str) -> Result<Option<ApiKey>> {
        let tx = self.conn.transaction()?;
        // Two Macs can each have a key of one name, and a guess could be the wrong one.
        let matching: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pushed_keys WHERE name = ?1 OR prefix = ?1",
            [name],
            |r| r.get(0),
        )?;
        if matching > 1 {
            return Err(StoreError::Conflict(format!(
                "more than one key is called {name}: revoke one by the prefix `vibe key` shows"
            )));
        }
        let found = tx
            .query_row(
                &format!(
                    "SELECT {PUSHED_KEY_COLUMNS}, hash, source FROM pushed_keys WHERE name = ?1 OR prefix = ?1"
                ),
                [name],
                |r| {
                    Ok((
                        pushed_key_row(r)?,
                        r.get::<_, String>("hash")?,
                        r.get::<_, String>("source")?,
                    ))
                },
            )
            .optional()?;
        let Some((key, hash, source)) = found else {
            return Ok(None);
        };
        tx.execute(
            "INSERT OR IGNORE INTO revoked_pushed_keys (hash, source) VALUES (?1, ?2)",
            params![hash, source],
        )?;
        tx.execute("DELETE FROM pushed_keys WHERE hash = ?1", [&hash])?;
        tx.commit()?;
        Ok(Some(key))
    }

    fn key(&self, name: &str) -> Result<ApiKey> {
        self.conn
            .query_row(
                &format!("SELECT {KEY_COLUMNS} FROM api_keys WHERE name = ?1"),
                [name],
                key_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("no key named {name}")))
    }

    /// Deletes a key, so its hash matches nothing from now on. A key pushed here goes
    /// for this host alone, and stays gone whatever the Mac pushes next: it's how a host
    /// shuts out a key that's leaked while the Mac is away.
    pub fn revoke_key(&mut self, name: &str) -> Result<ApiKey> {
        let key = match self.key(name) {
            Err(StoreError::NotFound(m)) => {
                if let Some(pushed) = self.revoke_pushed_key(name)? {
                    return Ok(pushed);
                }
                let own: Option<String> = self
                    .conn
                    .query_row("SELECT name FROM api_keys WHERE prefix = ?1", [name], |r| {
                        r.get(0)
                    })
                    .optional()?;
                return match own {
                    Some(own) => self.revoke_key(&own),
                    None => Err(StoreError::NotFound(m)),
                };
            }
            other => other?,
        };
        self.conn
            .execute("DELETE FROM api_keys WHERE name = ?1", [name])?;
        Ok(key)
    }

    /// Records that this host's own key `name` (with `prefix`, so a key
    /// made again under the same name isn't confused with it) was used at
    /// `at` somewhere else: on a Linux host it was pushed to. Only a later
    /// use than the one known moves it. Says whether it did.
    pub fn note_key_used(&mut self, name: &str, prefix: &str, at: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE api_keys SET last_used_at = ?3 \
             WHERE name = ?1 AND prefix = ?2 AND (last_used_at IS NULL OR last_used_at < ?3)",
            params![name, prefix, at],
        )?;
        Ok(n > 0)
    }

    /// The key with this hash, now marked as used, or `None` if there's no
    /// such key (it never existed, or it was revoked).
    pub fn use_key(&mut self, hash: &str, now: i64) -> Result<Option<ApiKey>> {
        let own = self
            .conn
            .query_row(
                &format!(
                    "UPDATE api_keys SET last_used_at = ?2 WHERE hash = ?1 RETURNING {KEY_COLUMNS}"
                ),
                params![hash, now],
                key_row,
            )
            .optional()?;
        if own.is_some() {
            return Ok(own);
        }
        Ok(self
            .conn
            .query_row(
                &format!(
                    "UPDATE pushed_keys SET last_used_at = ?2 WHERE hash = ?1 \
                     RETURNING {PUSHED_KEY_COLUMNS}"
                ),
                params![hash, now],
                pushed_key_row,
            )
            .optional()?)
    }
}

const PROFILE_COLUMNS: &str =
    "name, environment, model, reasoning, mode, memory, shared_services, agent, pi_model";
const KEY_COLUMNS: &str = "name, prefix, created_at, last_used_at";
const PUSHED_KEY_COLUMNS: &str = "name, prefix, created_at, last_used_at";

/// A comma-separated list, as `hosts.missing` is kept; empty for none.
/// A host's needs as [`split_list`] reads them back.
fn admin_needs_text(needs: &[AdminNeed]) -> String {
    let names: Vec<&str> = needs.iter().map(|n| n.as_str()).collect();
    names.join(",")
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn no_project(host: &str, name: &str) -> StoreError {
    StoreError::NotFound(format!("no project named {name} on {host}"))
}

fn no_agent(host: &str, agent_ref: &str) -> StoreError {
    StoreError::NotFound(format!("no agent {agent_ref} on {host}"))
}

fn no_profile(project: &str, name: &str) -> StoreError {
    StoreError::NotFound(format!("{project} has no profile named {name}"))
}

fn profile_taken(project: &str, name: &str) -> StoreError {
    StoreError::Conflict(format!("{project} already has a profile named {name}"))
}

fn project_id(conn: &Connection, host: &str, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM projects WHERE host = ?1 AND name = ?2",
            params![host, name],
            |r| r.get(0),
        )
        .optional()?)
}

fn profile_exists(conn: &Connection, project_id: i64, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM profiles WHERE project_id = ?1 AND name = ?2",
            params![project_id, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn insert_profile(conn: &Connection, project_id: i64, p: &Profile) -> Result<()> {
    conn.execute(
        "INSERT INTO profiles (project_id, name, environment, model, reasoning, mode, memory,
             shared_services, agent, pi_model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            project_id,
            p.name,
            p.environment.as_str(),
            p.model.as_str(),
            p.reasoning.as_str(),
            p.mode.as_str(),
            memory_column(p.memory)?,
            p.shared_services,
            p.agent.as_str(),
            p.pi_model,
        ],
    )?;
    Ok(())
}

fn memory_column(bytes: u64) -> Result<i64> {
    i64::try_from(bytes)
        .map_err(|e| StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn profiles_of(conn: &Connection, project_id: i64) -> Result<Vec<Profile>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROFILE_COLUMNS} FROM profiles WHERE project_id = ?1 ORDER BY id"
    ))?;
    let profiles = stmt
        .query_map([project_id], profile_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(profiles)
}

fn profile_row(r: &Row<'_>) -> rusqlite::Result<Profile> {
    let memory: i64 = r.get(5)?;
    Ok(Profile {
        name: r.get(0)?,
        environment: parse(r, 1)?,
        model: parse(r, 2)?,
        reasoning: parse(r, 3)?,
        mode: parse(r, 4)?,
        memory: u64::try_from(memory).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, Box::new(e))
        })?,
        shared_services: r.get(6)?,
        agent: parse(r, 7)?,
        pi_model: r.get(8)?,
    })
}

fn key_row(r: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        name: r.get(0)?,
        prefix: r.get(1)?,
        created_at: rfc3339(r.get(2)?),
        last_used_at: r.get::<_, Option<i64>>(3)?.map(rfc3339),
        pushed: false,
        stale: false,
    })
}

fn pushed_key_row(r: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        name: r.get(0)?,
        prefix: r.get(1)?,
        created_at: r.get(2)?,
        last_used_at: r.get::<_, Option<i64>>(3)?.map(rfc3339),
        pushed: true,
        stale: false,
    })
}

/// Reads a text column as one of the domain's names.
fn parse<T: FromStr<Err = String>>(r: &Row<'_>, i: usize) -> rusqlite::Result<T> {
    let s: String = r.get(i)?;
    s.parse()
        .map_err(|e: String| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LOCAL_HOST, starter_profiles};
    use crate::units::GIB;

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_host(LOCAL_HOST, "macos", "mac-mini", 1).unwrap();
        s
    }

    fn new_project(name: &str, path: &str) -> NewProject {
        NewProject {
            host: LOCAL_HOST.into(),
            name: name.into(),
            source: ProjectSource::Path,
            repo: None,
            path: path.into(),
        }
    }

    /// What was saved before Pi reads as Claude's, and Pi's settings persist.
    #[test]
    fn profiles_and_agents_know_their_agent() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_host(LOCAL_HOST, "macos", "mac", 1).unwrap();
        let project = NewProject {
            host: LOCAL_HOST.into(),
            name: "web".into(),
            source: ProjectSource::Path,
            repo: None,
            path: "/w/web".into(),
        };
        s.add_project(&project, &[], 1).unwrap();
        let id = project_id(&s.conn, LOCAL_HOST, "web").unwrap().unwrap();
        // A row from before the migration: nothing says which agent.
        s.conn
            .execute(
                "INSERT INTO profiles (project_id, name, environment, model, reasoning, mode, memory)
                 VALUES (?1, 'old', 'linux', 'opus', 'high', 'auto', 4294967296)",
                [id],
            )
            .unwrap();
        let old = s.profile(LOCAL_HOST, "web", "old").unwrap();
        assert_eq!((old.agent, old.pi_model), (AgentKind::Claude, None));
        assert!(!s.has_linux_pi_profile().unwrap());

        let pi = Profile {
            agent: AgentKind::Pi,
            pi_model: Some("openai/gpt-5".into()),
            mode: Mode::Bypass,
            ..Profile::with_defaults("pi-linux", Environment::Linux)
        };
        s.create_profile(LOCAL_HOST, "web", &pi).unwrap();
        assert_eq!(s.profile(LOCAL_HOST, "web", "pi-linux").unwrap(), pi);
        assert!(s.has_linux_pi_profile().unwrap());
        let changed = Profile {
            pi_model: None,
            ..pi.clone()
        };
        s.replace_profile(LOCAL_HOST, "web", "pi-linux", &changed)
            .unwrap();
        assert_eq!(s.profile(LOCAL_HOST, "web", "pi-linux").unwrap(), changed);

        let mut a = agent("web", "quiet-amber-otter");
        a.agent = AgentKind::Pi;
        a.pi_model = Some("openai/gpt-5".into());
        s.add_agent(&a).unwrap();
        assert_eq!(s.agent(LOCAL_HOST, "quiet-amber-otter").unwrap(), a);
    }

    fn agent(project: &str, r: &str) -> AgentRecord {
        AgentRecord {
            host: LOCAL_HOST.into(),
            agent_ref: r.into(),
            project: project.into(),
            profile: "software-eng-linux".into(),
            environment: Environment::Linux,
            model: Model::Sonnet,
            reasoning: Reasoning::Max,
            mode: Mode::Bypass,
            memory: 8 * GIB,
            worktree: format!("/w/{r}"),
            branch: r.into(),
            session_id: "0b5c3f6e-0000-4000-8000-000000000000".into(),
            shim_socket: None,
            state: AgentState::Busy,
            created_at: 5,
            compartment: r#"{"x":1}"#.into(),
            starting: true,
            shared_services: true,
            agent: AgentKind::Claude,
            pi_model: None,
        }
    }

    #[test]
    fn hosts() {
        let mut s = store();
        s.ensure_host(LOCAL_HOST, "macos", "renamed", 2).unwrap();
        assert_eq!(
            s.hosts().unwrap(),
            [HostInfo {
                name: "local".into(),
                os: "macos".into(),
                label: "renamed".into(),
                link: HostLink::Local,
                external: false,
                mcp: false,
                address: None,
                version: None,
                missing: vec![],
                detail: None,
                url: None,
                tunnel: None,
                external_changed_at: None,
                external_note: None,
                external_serves: None,
                external_last: None,
                external_detail: None,
                memory: None,
                needs_admin: vec![],
                upgrade_error: None,
            }]
        );
        assert!(s.has_host("local").unwrap());
        assert!(!s.has_host("vps").unwrap());
    }

    fn linux_host(name: &str) -> HostInfo {
        HostInfo {
            name: name.into(),
            os: "linux".into(),
            label: "vps-1".into(),
            link: HostLink::Down,
            external: false,
            mcp: false,
            address: Some(format!("me@{name}.example.com")),
            version: Some("0.1.0".into()),
            missing: vec!["gh".into(), "claude".into()],
            detail: None,
            url: None,
            tunnel: None,
            external_changed_at: None,
            external_note: None,
            external_serves: None,
            external_last: None,
            external_detail: None,
            memory: None,
            needs_admin: vec![],
            upgrade_error: None,
        }
    }

    #[test]
    fn one_external_address() {
        let mut s = store();
        s.add_host(&linux_host("vps"), 3).unwrap();
        s.add_host(&linux_host("edge"), 3).unwrap();
        assert_eq!(s.host_external("vps").unwrap(), None);
        s.set_host_external("vps", Some("vps.example.com")).unwrap();
        let vps = s.host("vps").unwrap();
        assert!(vps.external);
        assert_eq!(vps.url.as_deref(), Some("https://vps.example.com/mcp"));
        // Choosing another turns the first off.
        s.set_host_external("edge", Some("edge.example.com"))
            .unwrap();
        assert_eq!(s.host_external("vps").unwrap(), None);
        assert!(!s.host("vps").unwrap().external);
        assert_eq!(
            s.host_external("edge").unwrap().as_deref(),
            Some("edge.example.com")
        );
        s.set_host_external("edge", None).unwrap();
        assert!(
            s.hosts()
                .unwrap()
                .iter()
                .all(|h| !h.external && h.url.is_none())
        );
        assert!(matches!(
            s.set_host_external("nope", None),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.host_external("nope"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn linux_hosts() {
        let mut s = store();
        let added = s.add_host(&linux_host("vps"), 3).unwrap();
        assert_eq!(added, linux_host("vps"));
        assert!(matches!(
            s.add_host(&linux_host("vps"), 4),
            Err(StoreError::Conflict(_))
        ));
        let mut no_address = linux_host("x");
        no_address.address = None;
        assert!(s.add_host(&no_address, 4).is_err());
        s.add_host(&linux_host("build"), 5).unwrap();
        let names: Vec<_> = s.hosts().unwrap().into_iter().map(|h| h.name).collect();
        assert_eq!(names, ["local", "build", "vps"], "this host first");

        let mut report = linux_host("vps");
        report.version = Some("0.2.0".into());
        report.missing = vec![];
        report.label = "renamed".into();
        s.set_host_report(&report).unwrap();
        assert_eq!(s.host("vps").unwrap(), report);
        let needing = HostInfo {
            needs_admin: vec![AdminNeed::Linger],
            ..report.clone()
        };
        s.set_host_report(&needing).unwrap();
        assert_eq!(s.host("vps").unwrap(), needing);
        s.set_host_report(&report).unwrap();
        let mut local = report.clone();
        local.name = "local".into();
        assert!(s.set_host_report(&local).is_err(), "only Linux hosts");

        assert_eq!(s.host_agents("vps").unwrap(), "[]");
        s.set_host_agents("vps", r#"[{"ref":"x"}]"#).unwrap();
        assert_eq!(s.host_agents("vps").unwrap(), r#"[{"ref":"x"}]"#);

        assert!(matches!(
            s.remove_host("local"),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(s.remove_host("vps").unwrap().name, "vps");
        assert!(matches!(s.remove_host("vps"), Err(StoreError::NotFound(_))));
        assert!(matches!(s.host_agents("vps"), Err(StoreError::NotFound(_))));
    }

    fn pushed(name: &str, hash: &str) -> KeyHash {
        KeyHash {
            name: name.into(),
            prefix: format!("vk_{name}"),
            hash: hash.into(),
            created_at: "2026-10-30T12:00:00Z".into(),
        }
    }

    fn pushed_names(s: &Store) -> Vec<String> {
        let mut names: Vec<String> = s.keys().unwrap().into_iter().map(|k| k.name).collect();
        names.sort();
        names
    }

    /// A host can be shared: one Mac's pushes, and the empty one when it's forgotten,
    /// are about its own keys.
    #[test]
    fn a_mac_replaces_only_the_keys_it_pushed() {
        let mut s = store();
        s.sync_pushed_keys("real", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        s.sync_pushed_keys("test", &[pushed("test", "h3")]).unwrap();
        assert_eq!(pushed_names(&s), ["ci", "laptop", "test"]);
        s.sync_pushed_keys("test", &[]).unwrap();
        assert_eq!(pushed_names(&s), ["ci", "laptop"]);
        // Its own are still replaced whole, so a revoke on the Mac reaches here.
        s.sync_pushed_keys("real", &[pushed("laptop", "h1")])
            .unwrap();
        assert_eq!(pushed_names(&s), ["laptop"]);
    }

    /// What's revoked here stays revoked, and one Mac's push doesn't forget what was
    /// revoked of another's.
    #[test]
    fn a_macs_push_doesnt_forget_what_was_revoked_of_anothers() {
        let mut s = store();
        s.sync_pushed_keys("real", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        s.revoke_pushed_key("ci").unwrap().unwrap();
        s.sync_pushed_keys("test", &[pushed("test", "h3")]).unwrap();
        s.sync_pushed_keys("real", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        assert_eq!(pushed_names(&s), ["laptop", "test"]);
    }

    /// What a Mac pushed before they said who they were. A new Mac that shares none of
    /// it, as a test's has, takes nothing from it. The Mac that pushed it, upgraded, is
    /// known by a key they share, and then the rest goes, as a push always did: it's
    /// what it revoked while the host was away.
    #[test]
    fn keys_from_before_macs_said_who_they_were() {
        let mut s = store();
        s.sync_pushed_keys("", &[pushed("laptop", "h1"), pushed("revoked", "h2")])
            .unwrap();
        s.sync_pushed_keys("test", &[pushed("test", "h3")]).unwrap();
        assert_eq!(pushed_names(&s), ["laptop", "revoked", "test"]);
        s.sync_pushed_keys("test", &[]).unwrap();
        assert_eq!(pushed_names(&s), ["laptop", "revoked"]);
        s.sync_pushed_keys("real", &[pushed("laptop", "h1")])
            .unwrap();
        assert_eq!(pushed_names(&s), ["laptop"]);
        s.sync_pushed_keys("test", &[]).unwrap();
        assert_eq!(pushed_names(&s), ["laptop"], "it's the real Mac's now");
    }

    /// Two Macs can each have a key of one name: a name alone won't revoke either, as a
    /// guess could cut off the wrong Mac, and the prefix will.
    #[test]
    fn a_name_two_macs_use_is_refused_and_a_prefix_revokes() {
        let mut s = store();
        let mut one = pushed("laptop", "h1");
        one.prefix = "vk_one".into();
        let mut two = pushed("laptop", "h2");
        two.prefix = "vk_two".into();
        s.sync_pushed_keys("real", &[one]).unwrap();
        s.sync_pushed_keys("other", &[two]).unwrap();
        assert!(matches!(
            s.revoke_key("laptop"),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(s.revoke_key("vk_two").unwrap().prefix, "vk_two");
        assert_eq!(s.revoke_key("laptop").unwrap().prefix, "vk_one");
        assert!(matches!(
            s.revoke_key("vk_one"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn an_install_has_one_id() {
        let mut s = store();
        let id = s.install_id(|| "first".into()).unwrap();
        assert_eq!(id, "first");
        assert_eq!(s.install_id(|| "second".into()).unwrap(), "first");
    }

    #[test]
    fn a_pushed_key_revoked_here_stays_revoked_whatever_is_pushed_next() {
        let mut s = store();
        let pushed = |name: &str, hash: &str| KeyHash {
            name: name.into(),
            prefix: format!("vk_{name}"),
            hash: hash.into(),
            created_at: "2026-10-30T12:00:00Z".into(),
        };
        s.sync_pushed_keys("mac", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        let gone = s.revoke_key("laptop").unwrap();
        assert!(gone.pushed);
        assert_eq!(s.use_key("h1", 10).unwrap(), None);
        assert!(s.use_key("h2", 10).unwrap().is_some(), "only that one");

        // The Mac doesn't know, and pushes it again.
        s.sync_pushed_keys("mac", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        assert_eq!(s.use_key("h1", 20).unwrap(), None);
        let names: Vec<_> = s.keys().unwrap().into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["ci"]);

        // A new key of the same name is another key.
        s.sync_pushed_keys("mac", &[pushed("laptop", "h3"), pushed("ci", "h2")])
            .unwrap();
        assert!(s.use_key("h3", 30).unwrap().is_some());

        // Once the Mac stops pushing it, there's nothing left to remember.
        let left: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM revoked_pushed_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn pushed_keys() {
        let mut s = store();
        s.create_key("own", "h0", "vk_own", 1).unwrap();
        let pushed = |name: &str, hash: &str| KeyHash {
            name: name.into(),
            prefix: format!("vk_{name}"),
            hash: hash.into(),
            created_at: "2026-10-30T12:00:00Z".into(),
        };
        s.sync_pushed_keys("mac", &[pushed("laptop", "h1"), pushed("ci", "h2")])
            .unwrap();
        let keys = s.keys().unwrap();
        let shown: Vec<_> = keys.iter().map(|k| (k.name.as_str(), k.pushed)).collect();
        assert_eq!(shown, [("own", false), ("laptop", true), ("ci", true)]);
        assert_eq!(keys[1].created_at, "2026-10-30T12:00:00Z");

        let used = s.use_key("h1", 20).unwrap().unwrap();
        assert!(used.pushed);
        assert_eq!(used.last_used_at.as_deref(), Some("1970-01-01T00:00:20Z"));
        // Only this host's own keys go on to other hosts.
        let hashes: Vec<_> = s
            .key_hashes()
            .unwrap()
            .into_iter()
            .map(|k| k.hash)
            .collect();
        assert_eq!(hashes, ["h0"]);

        assert!(matches!(s.revoke_key("nope"), Err(StoreError::NotFound(_))));

        // The next push replaces them all; a key kept keeps its last use.
        s.sync_pushed_keys("mac", &[pushed("laptop", "h1")])
            .unwrap();
        assert_eq!(s.use_key("h2", 30).unwrap(), None, "dropped from the push");
        let laptop = s
            .keys()
            .unwrap()
            .into_iter()
            .find(|k| k.name == "laptop")
            .unwrap();
        assert_eq!(laptop.last_used_at.as_deref(), Some("1970-01-01T00:00:20Z"));
        s.sync_pushed_keys("mac", &[]).unwrap();
        assert_eq!(s.keys().unwrap().len(), 1);
        assert!(
            s.use_key("h0", 40).unwrap().is_some(),
            "the host's own stay"
        );
    }

    #[test]
    fn projects_with_their_profiles() {
        let mut s = store();
        let p = s
            .add_project(
                &new_project("web", "/p/web"),
                &starter_profiles("macos"),
                1_793_361_600,
            )
            .unwrap();
        assert_eq!(p.added_at, "2026-10-30T12:00:00Z");
        assert_eq!(p.profiles, starter_profiles("macos"));
        s.add_project(&new_project("app", "/p/app"), &[], 1)
            .unwrap();
        let names: Vec<_> = s
            .projects(LOCAL_HOST)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["app", "web"]);
        assert!(s.projects("vps").unwrap().is_empty());

        let dup = s
            .add_project(&new_project("web", "/p/other"), &[], 1)
            .unwrap_err();
        assert!(matches!(dup, StoreError::Conflict(_)), "{dup}");
        let same_path = s
            .add_project(&new_project("web2", "/p/web"), &[], 1)
            .unwrap_err();
        assert_eq!(same_path.to_string(), "/p/web is already the project web");

        let removed = s.remove_project(LOCAL_HOST, "web").unwrap();
        assert_eq!(removed.profiles.len(), 2);
        assert!(matches!(
            s.project(LOCAL_HOST, "web"),
            Err(StoreError::NotFound(_))
        ));
        // Its profiles went with it.
        let left: i64 = s
            .conn
            .query_row("SELECT count(*) FROM profiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn projects_need_a_known_host() {
        let mut s = store();
        let mut p = new_project("web", "/p/web");
        p.host = "vps".into();
        assert!(matches!(
            s.add_project(&p, &[], 1),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn a_renamed_profile_keeps_its_agents() {
        let mut s = store();
        s.add_project(&new_project("web", "/p/web"), &starter_profiles("macos"), 1)
            .unwrap();
        s.add_project(&new_project("api", "/p/api"), &starter_profiles("macos"), 1)
            .unwrap();
        let (mine, other) = (agent("web", "mine"), agent("api", "other"));
        s.add_agent(&mine).unwrap();
        s.add_agent(&other).unwrap();

        let mut p = s.profile(LOCAL_HOST, "web", &mine.profile).unwrap();
        let was = std::mem::replace(&mut p.name, "renamed".into());
        s.replace_profile(LOCAL_HOST, "web", &was, &p).unwrap();

        assert_eq!(s.agent(LOCAL_HOST, "mine").unwrap().profile, "renamed");
        assert_eq!(s.agent(LOCAL_HOST, "other").unwrap().profile, was);
    }

    #[test]
    fn profiles_crud() {
        let mut s = store();
        s.add_project(&new_project("web", "/p/web"), &starter_profiles("macos"), 1)
            .unwrap();

        let mut p = Profile::with_defaults("reviewer", Environment::Linux);
        p.model = Model::Sonnet;
        s.create_profile(LOCAL_HOST, "web", &p).unwrap();
        assert!(matches!(
            s.create_profile(LOCAL_HOST, "web", &p),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(s.profile(LOCAL_HOST, "web", "reviewer").unwrap(), p);

        let mut changed = p.clone();
        changed.name = "critic".into();
        changed.reasoning = Reasoning::Max;
        changed.mode = Mode::Bypass;
        changed.memory = 8 * GIB;
        s.replace_profile(LOCAL_HOST, "web", "reviewer", &changed)
            .unwrap();
        let names: Vec<_> = s
            .profiles(LOCAL_HOST, "web")
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["software-eng-linux", "software-eng-mac", "critic"]);
        assert_eq!(s.profile(LOCAL_HOST, "web", "critic").unwrap(), changed);

        // Renaming onto another profile is refused; so is changing a missing one.
        let mut clash = changed.clone();
        clash.name = "software-eng-mac".into();
        assert!(matches!(
            s.replace_profile(LOCAL_HOST, "web", "critic", &clash),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            s.replace_profile(LOCAL_HOST, "web", "nope", &changed),
            Err(StoreError::NotFound(_))
        ));

        assert_eq!(
            s.delete_profile(LOCAL_HOST, "web", "critic").unwrap(),
            changed
        );
        let err = s.delete_profile(LOCAL_HOST, "web", "critic").unwrap_err();
        assert_eq!(err.to_string(), "web has no profile named critic");
        let err = s.profiles(LOCAL_HOST, "nope").unwrap_err();
        assert_eq!(err.to_string(), "no project named nope on local");
    }

    #[test]
    fn projects_with_agents_stay() {
        let mut s = store();
        s.add_project(&new_project("web", "/p/web"), &[], 1)
            .unwrap();
        s.add_agent(&agent("web", "quiet-amber-otter")).unwrap();
        s.add_agent(&agent("web", "brave-copper-lynx")).unwrap();
        assert!(matches!(
            s.add_agent(&agent("web", "quiet-amber-otter")),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            s.add_agent(&agent("nope", "x")),
            Err(StoreError::NotFound(_))
        ));
        assert_eq!(
            s.agents(LOCAL_HOST).unwrap()[0],
            agent("web", "quiet-amber-otter")
        );
        assert_eq!(
            s.agent(LOCAL_HOST, "brave-copper-lynx").unwrap(),
            agent("web", "brave-copper-lynx")
        );
        assert!(matches!(
            s.agent(LOCAL_HOST, "nope"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.agent("vps", "quiet-amber-otter"),
            Err(StoreError::NotFound(_))
        ));

        // State and socket changes; the same state twice is no change.
        assert!(
            s.set_agent_state(LOCAL_HOST, "quiet-amber-otter", AgentState::Exited)
                .unwrap()
        );
        assert!(
            !s.set_agent_state(LOCAL_HOST, "quiet-amber-otter", AgentState::Exited)
                .unwrap()
        );
        s.set_agent_socket(LOCAL_HOST, "quiet-amber-otter", "/run/q.sock")
            .unwrap();
        s.set_agent_compartment(LOCAL_HOST, "quiet-amber-otter", r#"{"y":2}"#)
            .unwrap();
        s.finish_agent_start(LOCAL_HOST, "quiet-amber-otter")
            .unwrap();
        s.set_agent_session(
            LOCAL_HOST,
            "quiet-amber-otter",
            "c1ea4ed0-0000-4000-8000-000000000000",
        )
        .unwrap();
        let q = s.agent(LOCAL_HOST, "quiet-amber-otter").unwrap();
        assert_eq!(q.session_id, "c1ea4ed0-0000-4000-8000-000000000000");
        assert_eq!(q.compartment, r#"{"y":2}"#);
        assert!(!q.starting);
        assert!(matches!(
            s.finish_agent_start(LOCAL_HOST, "nope"),
            Err(StoreError::NotFound(_))
        ));
        assert_eq!(q.state, AgentState::Exited);
        assert_eq!(q.shim_socket.as_deref(), Some("/run/q.sock"));
        assert!(matches!(
            s.set_agent_state(LOCAL_HOST, "nope", AgentState::Busy),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.set_agent_socket(LOCAL_HOST, "nope", "/x"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.set_agent_compartment(LOCAL_HOST, "nope", "{}"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.set_agent_session(LOCAL_HOST, "nope", "x"),
            Err(StoreError::NotFound(_))
        ));

        let err = s.remove_project(LOCAL_HOST, "web").unwrap_err();
        assert_eq!(err.to_string(), "web still has 2 agents; delete them first");
        s.remove_agent(LOCAL_HOST, "quiet-amber-otter").unwrap();
        let err = s.remove_project(LOCAL_HOST, "web").unwrap_err();
        assert_eq!(err.to_string(), "web still has 1 agent; delete it first");
        s.remove_agent(LOCAL_HOST, "brave-copper-lynx").unwrap();
        assert!(matches!(
            s.remove_agent(LOCAL_HOST, "brave-copper-lynx"),
            Err(StoreError::NotFound(_))
        ));
        s.remove_project(LOCAL_HOST, "web").unwrap();
    }

    #[test]
    fn keys() {
        let mut s = store();
        let k = s.create_key("laptop", "h1", "vk_aaaaaaaa", 10).unwrap();
        assert_eq!(k.last_used_at, None);
        assert!(matches!(
            s.create_key("laptop", "h2", "vk_b", 11),
            Err(StoreError::Conflict(_))
        ));
        s.create_key("ci", "h2", "vk_bbbbbbbb", 11).unwrap();

        assert_eq!(s.use_key("nope", 20).unwrap(), None);
        let used = s.use_key("h1", 20).unwrap().unwrap();
        assert_eq!(used.last_used_at.as_deref(), Some("1970-01-01T00:00:20Z"));
        let names: Vec<_> = s.keys().unwrap().into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["laptop", "ci"]);

        // A use on a host the key was pushed to: only a later one counts,
        // and only for the key with that prefix.
        assert!(!s.note_key_used("laptop", "vk_aaaaaaaa", 15).unwrap());
        assert!(!s.note_key_used("laptop", "vk_other", 40).unwrap());
        assert!(!s.note_key_used("nope", "vk_aaaaaaaa", 40).unwrap());
        assert!(s.note_key_used("laptop", "vk_aaaaaaaa", 25).unwrap());
        assert!(s.note_key_used("ci", "vk_bbbbbbbb", 12).unwrap());
        let used: Vec<_> = s
            .keys()
            .unwrap()
            .into_iter()
            .map(|k| k.last_used_at)
            .collect();
        assert_eq!(
            used,
            [
                Some("1970-01-01T00:00:25Z".into()),
                Some("1970-01-01T00:00:12Z".into())
            ]
        );

        s.revoke_key("laptop").unwrap();
        assert_eq!(
            s.use_key("h1", 30).unwrap(),
            None,
            "a revoked key stops working"
        );
        assert!(matches!(
            s.revoke_key("laptop"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn everything_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vibe.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.ensure_host(LOCAL_HOST, "linux", "vps", 1).unwrap();
            s.add_project(&new_project("web", "/p/web"), &starter_profiles("linux"), 1)
                .unwrap();
            s.create_key("laptop", "h1", "vk_a", 1).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION as i64);
        assert_eq!(
            s.projects(LOCAL_HOST).unwrap()[0].profiles,
            starter_profiles("linux")
        );
        assert_eq!(s.keys().unwrap().len(), 1);
    }

    #[test]
    fn a_version_1_registry_is_migrated() {
        // An agents table as #2 made it, with a row in it: the new columns
        // get their defaults.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vibe.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute_batch(
                "INSERT INTO hosts VALUES ('local', 'macos', 'm', NULL, 1);
                 INSERT INTO projects VALUES (1, 'local', 'web', 'path', NULL, '/p/web', 1);
                 INSERT INTO agents (host, ref, project_id, profile, environment, worktree,
                     branch, session_id, state, created_at)
                 VALUES ('local', 'q', 1, 'p', 'macos', '/w/q', 'q', 's', 'busy', 1);",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION as i64);
        let a = s.agent(LOCAL_HOST, "q").unwrap();
        assert_eq!(
            (a.model, a.reasoning, a.mode, a.memory),
            (Model::Opus, Reasoning::High, Mode::Auto, 4 * GIB)
        );
        assert_eq!(a.compartment, "{}");
        assert!(!a.starting);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vibe.db");
        Store::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        let err = Store::open(&path).unwrap_err();
        assert!(
            matches!(err, StoreError::NewerSchema { found: 99, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("newer vibed"), "{err}");
    }

    #[test]
    fn bad_rows_are_errors_not_panics() {
        let mut s = store();
        s.add_project(&new_project("web", "/p/web"), &starter_profiles("linux"), 1)
            .unwrap();
        s.conn
            .execute("UPDATE profiles SET model = 'gpt'", [])
            .unwrap();
        assert!(matches!(
            s.profiles(LOCAL_HOST, "web"),
            Err(StoreError::Sqlite(_))
        ));
    }
}
