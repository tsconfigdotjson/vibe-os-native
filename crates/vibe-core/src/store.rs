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

use crate::model::{Environment, Profile, ProjectSource};
use crate::rpc::{ApiKey, HostInfo, Project};
use crate::time::rfc3339;

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

/// An agent's row. #4 creates and deletes them; #2 only needs to know they
/// exist, so a project with agents can't be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub host: String,
    pub agent_ref: String,
    pub project: String,
    pub profile: String,
    pub environment: Environment,
    pub worktree: String,
    pub branch: String,
    pub session_id: String,
    pub shim_socket: Option<String>,
    pub state: String,
    pub created_at: i64,
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

    /// Every host, this one first.
    pub fn hosts(&self) -> Result<Vec<HostInfo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, os, label FROM hosts ORDER BY address IS NOT NULL, name")?;
        let hosts = stmt
            .query_map([], |r| {
                Ok(HostInfo {
                    name: r.get(0)?,
                    os: r.get(1)?,
                    label: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(hosts)
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
                 mode = ?7, memory = ?8
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
            ],
        )?;
        tx.commit()?;
        Ok(profile.clone())
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
            "INSERT INTO agents (host, ref, project_id, profile, environment, worktree, branch,
                 session_id, shim_socket, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                a.host,
                a.agent_ref,
                id,
                a.profile,
                a.environment.as_str(),
                a.worktree,
                a.branch,
                a.session_id,
                a.shim_socket,
                a.state,
                a.created_at
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A host's agents, oldest first.
    pub fn agents(&self, host: &str) -> Result<Vec<AgentRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.host, a.ref, p.name, a.profile, a.environment, a.worktree, a.branch,
                 a.session_id, a.shim_socket, a.state, a.created_at
             FROM agents a JOIN projects p ON p.id = a.project_id
             WHERE a.host = ?1 ORDER BY a.id",
        )?;
        let agents = stmt
            .query_map([host], |r| {
                Ok(AgentRecord {
                    host: r.get(0)?,
                    agent_ref: r.get(1)?,
                    project: r.get(2)?,
                    profile: r.get(3)?,
                    environment: parse(r, 4)?,
                    worktree: r.get(5)?,
                    branch: r.get(6)?,
                    session_id: r.get(7)?,
                    shim_socket: r.get(8)?,
                    state: r.get(9)?,
                    created_at: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(agents)
    }

    pub fn remove_agent(&mut self, host: &str, agent_ref: &str) -> Result<()> {
        let n = self.conn.execute(
            "DELETE FROM agents WHERE host = ?1 AND ref = ?2",
            params![host, agent_ref],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!(
                "no agent {agent_ref} on {host}"
            )));
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

    /// Every key, oldest first.
    pub fn keys(&self) -> Result<Vec<ApiKey>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {KEY_COLUMNS} FROM api_keys ORDER BY id"))?;
        let keys = stmt
            .query_map([], key_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(keys)
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

    /// Deletes a key, so its hash matches nothing from now on.
    pub fn revoke_key(&mut self, name: &str) -> Result<ApiKey> {
        let key = self.key(name)?;
        self.conn
            .execute("DELETE FROM api_keys WHERE name = ?1", [name])?;
        Ok(key)
    }

    /// The key with this hash, now marked as used, or `None` if there's no
    /// such key (it never existed, or it was revoked).
    pub fn use_key(&mut self, hash: &str, now: i64) -> Result<Option<ApiKey>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "UPDATE api_keys SET last_used_at = ?2 WHERE hash = ?1 RETURNING {KEY_COLUMNS}"
                ),
                params![hash, now],
                key_row,
            )
            .optional()?)
    }
}

const PROFILE_COLUMNS: &str = "name, environment, model, reasoning, mode, memory";
const KEY_COLUMNS: &str = "name, prefix, created_at, last_used_at";

fn no_project(host: &str, name: &str) -> StoreError {
    StoreError::NotFound(format!("no project named {name} on {host}"))
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
        "INSERT INTO profiles (project_id, name, environment, model, reasoning, mode, memory)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            project_id,
            p.name,
            p.environment.as_str(),
            p.model.as_str(),
            p.reasoning.as_str(),
            p.mode.as_str(),
            memory_column(p.memory)?,
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
    })
}

fn key_row(r: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        name: r.get(0)?,
        prefix: r.get(1)?,
        created_at: rfc3339(r.get(2)?),
        last_used_at: r.get::<_, Option<i64>>(3)?.map(rfc3339),
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
    use crate::model::{LOCAL_HOST, Mode, Model, Reasoning, starter_profiles};
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

    fn agent(project: &str, r: &str) -> AgentRecord {
        AgentRecord {
            host: LOCAL_HOST.into(),
            agent_ref: r.into(),
            project: project.into(),
            profile: "software-eng-linux".into(),
            environment: Environment::Linux,
            worktree: format!("/w/{r}"),
            branch: r.into(),
            session_id: "0b5c3f6e-0000-4000-8000-000000000000".into(),
            shim_socket: None,
            state: "busy".into(),
            created_at: 5,
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
                label: "renamed".into()
            }]
        );
        assert!(s.has_host("local").unwrap());
        assert!(!s.has_host("vps").unwrap());
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
