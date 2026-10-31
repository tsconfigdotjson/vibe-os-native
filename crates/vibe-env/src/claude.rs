//! Claude Code's state for an agent (PRD: Key user flows, Start an agent;
//! Matching an agent to its transcript).
//!
//! Claude keeps its settings and transcripts in a config directory,
//! `~/.claude`, and its other state in a file, `~/.claude.json`. With
//! `CLAUDE_CONFIG_DIR` set, both live in that directory instead. A `macos`
//! agent uses the Mac's own; a `linux` agent on the Mac, and the test
//! backend's, have one of their own.
//!
//! Before Claude starts, its state is pre-filled so it opens straight to its
//! prompt: first-run onboarding done (or it stops at its theme picker, as
//! spike 4 found) and the worktree trusted. Deleting the agent forgets the
//! worktree again: its transcripts, the files Claude keeps per session, and
//! its entry in the state file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The environment variable that moves Claude's config directory.
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Claude names a project's directory after its path, cut to this many
/// characters (and a hash added) when it's longer.
const MAX_PROJECT_DIR: usize = 200;

/// Where Claude keeps its state for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHome {
    /// The config directory, which holds `projects/` and the transcripts.
    pub dir: PathBuf,
    /// The state file, `.claude.json`.
    pub state: PathBuf,
    /// Whether the agent has it to itself. Deleting the agent deletes an
    /// own home whole; from a shared one, only what's the agent's.
    pub own: bool,
}

impl ClaudeHome {
    /// The user's own, `~/.claude` and `~/.claude.json`, shared with
    /// everything else that runs Claude as them.
    pub fn user(home: &Path) -> ClaudeHome {
        ClaudeHome {
            dir: home.join(".claude"),
            state: home.join(".claude.json"),
            own: false,
        }
    }

    /// One the agent has to itself, which Claude finds through
    /// [`CONFIG_DIR_ENV`]. Its state file is inside it.
    pub fn own(dir: PathBuf) -> ClaudeHome {
        ClaudeHome {
            state: dir.join(".claude.json"),
            dir,
            own: true,
        }
    }

    /// The directory of the transcripts of sessions run in `cwd`.
    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        let name = project_dir_name(cwd);
        if name.len() <= MAX_PROJECT_DIR {
            return self.dir.join("projects").join(name);
        }
        // Claude adds a hash of the path to a cut name; find it by the cut.
        let cut = format!("{}-", &name[..MAX_PROJECT_DIR]);
        let found = fs::read_dir(self.dir.join("projects")).ok().and_then(|d| {
            d.filter_map(Result::ok)
                .find(|e| e.file_name().to_string_lossy().starts_with(&cut))
        });
        match found {
            Some(e) => e.path(),
            None => self.dir.join("projects").join(&name[..MAX_PROJECT_DIR]),
        }
    }

    /// The transcript of session `session_id`, run in `cwd`.
    pub fn transcript(&self, cwd: &Path, session_id: &str) -> PathBuf {
        self.project_dir(cwd).join(format!("{session_id}.jsonl"))
    }

    /// Pre-fills Claude's state so it opens straight to its prompt in
    /// `worktree`: onboarding done, a theme chosen if none is, and the
    /// worktree trusted. With `bypass`, the one-time warning about
    /// `--dangerously-skip-permissions` is taken as read too, as the
    /// owner chose that mode for the profile. Everything else in the file
    /// is kept.
    pub fn prefill(&self, worktree: &Path, bypass: bool) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut state = self.read_state()?;
        state.insert("hasCompletedOnboarding".into(), Value::Bool(true));
        state
            .entry("theme")
            .or_insert_with(|| Value::String("dark".into()));
        if bypass {
            state.insert("bypassPermissionsModeAccepted".into(), Value::Bool(true));
        }
        let projects = state
            .entry("projects")
            .or_insert_with(|| Value::Object(Map::new()));
        if !projects.is_object() {
            *projects = Value::Object(Map::new());
        }
        let key = worktree.to_string_lossy().into_owned();
        let project = projects
            .as_object_mut()
            .expect("an object")
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
        if !project.is_object() {
            *project = Value::Object(Map::new());
        }
        let project = project.as_object_mut().expect("an object");
        project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
        project.insert("hasCompletedProjectOnboarding".into(), Value::Bool(true));
        self.write_state(&state)
    }

    /// Forgets `worktree`: an own home goes whole; from a shared one go the
    /// worktree's transcripts, the files Claude keeps for each of those
    /// sessions, and the worktree's entry in the state file.
    pub fn forget(&self, worktree: &Path) -> io::Result<()> {
        if self.own {
            remove_all(&self.dir)?;
            return remove_file(&self.state);
        }
        let project = self.project_dir(worktree);
        for id in session_ids(&project) {
            remove_all(&self.dir.join("file-history").join(&id))?;
            remove_all(&self.dir.join("session-env").join(&id))?;
            remove_file(&self.dir.join("debug").join(format!("{id}.txt")))?;
            if let Ok(todos) = fs::read_dir(self.dir.join("todos")) {
                for e in todos.filter_map(Result::ok) {
                    if e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{id}-"))
                    {
                        remove_file(&e.path())?;
                    }
                }
            }
        }
        remove_all(&project)?;
        if !self.state.exists() {
            return Ok(());
        }
        let mut state = self.read_state()?;
        let key = worktree.to_string_lossy();
        let removed = state
            .get_mut("projects")
            .and_then(Value::as_object_mut)
            .and_then(|p| p.remove(key.as_ref()))
            .is_some();
        if removed {
            self.write_state(&state)?;
        }
        Ok(())
    }

    fn read_state(&self) -> io::Result<Map<String, Value>> {
        let text = match fs::read_to_string(&self.state) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(e),
        };
        if text.trim().is_empty() {
            return Ok(Map::new());
        }
        // A file that isn't Claude's JSON is left for its owner to look at,
        // never overwritten.
        match serde_json::from_str(&text) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} isn't a JSON object", self.state.display()),
            )),
        }
    }

    /// Writes the state file beside itself and renames it into place, so
    /// Claude never reads half of it. It stays private.
    fn write_state(&self, state: &Map<String, Value>) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // A state file that's a symlink, as a dotfile manager makes, stays
        // one: the file it points to is the one replaced.
        let target = fs::canonicalize(&self.state).unwrap_or_else(|_| self.state.clone());
        let mut tmp = target.as_os_str().to_owned();
        tmp.push(format!(".vibe-{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        fs::rename(&tmp, &target).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }
}

/// The name Claude gives the directory of a project at `cwd`: the path with
/// everything but ASCII letters and digits turned into `-`.
pub fn project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The sessions with a transcript in `project`.
fn session_ids(project: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(project) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?;
            // A session id is a UUID; nothing else names files here.
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
                .then(|| id.to_owned())
        })
        .collect()
}

fn remove_all(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "0b5c3f6e-1111-4000-8000-000000000001";

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn transcripts_are_named_after_the_path() {
        let home = ClaudeHome::user(Path::new("/Users/me"));
        let wt = Path::new("/Users/me/VibeOS/worktrees/web.app/quiet-amber-otter");
        assert_eq!(
            home.transcript(wt, ID),
            Path::new(
                "/Users/me/.claude/projects/-Users-me-VibeOS-worktrees-web-app-quiet-amber-otter"
            )
            .join(format!("{ID}.jsonl"))
        );
        let own = ClaudeHome::own("/h/VibeOS/agents/q/claude".into());
        assert_eq!(
            own.state,
            Path::new("/h/VibeOS/agents/q/claude/.claude.json")
        );
        assert!(
            own.project_dir(wt)
                .starts_with("/h/VibeOS/agents/q/claude/projects")
        );
    }

    #[test]
    fn long_paths_are_found_by_their_cut_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::own(tmp.path().join("c"));
        let wt = PathBuf::from(format!("/w/{}", "p".repeat(250)));
        let cut = &project_dir_name(&wt)[..MAX_PROJECT_DIR];
        assert_eq!(home.project_dir(&wt), home.dir.join("projects").join(cut));
        let hashed = home.dir.join("projects").join(format!("{cut}-1a2b3c"));
        fs::create_dir_all(&hashed).unwrap();
        assert_eq!(home.project_dir(&wt), hashed);
    }

    #[test]
    fn prefill_keeps_what_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::user(tmp.path());
        fs::write(
            &home.state,
            json!({"theme": "light", "numStartups": 7, "projects": {"/other": {"x": 1}}})
                .to_string(),
        )
        .unwrap();
        let wt = Path::new("/w/q");
        home.prefill(wt, false).unwrap();
        let state = read(&home.state);
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["theme"], "light", "the owner's theme stays");
        assert_eq!(state["numStartups"], 7);
        assert_eq!(state["projects"]["/other"], json!({"x": 1}));
        assert_eq!(state["projects"]["/w/q"]["hasTrustDialogAccepted"], true);
        assert_eq!(
            state["projects"]["/w/q"]["hasCompletedProjectOnboarding"],
            true
        );
        assert!(state.get("bypassPermissionsModeAccepted").is_none());
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&home.state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        home.prefill(wt, true).unwrap();
        assert_eq!(read(&home.state)["bypassPermissionsModeAccepted"], true);
        // Nothing is left beside it.
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn a_symlinked_state_file_stays_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::user(tmp.path());
        let real = tmp.path().join("dotfiles-claude.json");
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &home.state).unwrap();
        home.prefill(Path::new("/w/q"), false).unwrap();
        assert!(
            fs::symlink_metadata(&home.state)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read(&real)["hasCompletedOnboarding"], true);
    }

    #[test]
    fn prefill_starts_a_missing_file_and_refuses_a_broken_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::own(tmp.path().join("claude"));
        home.prefill(Path::new("/w/q"), false).unwrap();
        let state = read(&home.state);
        assert_eq!(state["theme"], "dark");
        assert_eq!(state["projects"]["/w/q"]["hasTrustDialogAccepted"], true);

        fs::write(&home.state, "[1, 2]").unwrap();
        let err = home.prefill(Path::new("/w/q"), false).unwrap_err();
        assert!(err.to_string().contains("isn't a JSON object"), "{err}");
        assert_eq!(fs::read_to_string(&home.state).unwrap(), "[1, 2]");
    }

    #[test]
    fn forgetting_a_worktree_in_a_shared_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::user(tmp.path());
        let wt = Path::new("/w/q");
        let other = Path::new("/w/other");
        home.prefill(wt, false).unwrap();
        home.prefill(other, false).unwrap();
        let mine = home.transcript(wt, ID);
        let theirs = home.transcript(other, "0b5c3f6e-2222-4000-8000-000000000002");
        for t in [&mine, &theirs] {
            fs::create_dir_all(t.parent().unwrap()).unwrap();
            fs::write(t, "{}\n").unwrap();
        }
        let dir = &home.dir;
        fs::create_dir_all(dir.join("file-history").join(ID)).unwrap();
        fs::create_dir_all(dir.join("todos")).unwrap();
        fs::write(
            dir.join("todos").join(format!("{ID}-agent-{ID}.json")),
            "[]",
        )
        .unwrap();
        fs::write(dir.join("todos").join("other-agent.json"), "[]").unwrap();
        fs::create_dir_all(dir.join("debug")).unwrap();
        fs::write(dir.join("debug").join(format!("{ID}.txt")), "").unwrap();

        home.forget(wt).unwrap();
        assert!(!home.project_dir(wt).exists());
        assert!(theirs.exists());
        assert!(!dir.join("file-history").join(ID).exists());
        assert!(!dir.join("debug").join(format!("{ID}.txt")).exists());
        assert!(dir.join("todos").join("other-agent.json").exists());
        assert_eq!(fs::read_dir(dir.join("todos")).unwrap().count(), 1);
        let state = read(&home.state);
        assert!(state["projects"].get("/w/q").is_none());
        assert!(state["projects"].get("/w/other").is_some());
        // Again, with nothing left, is fine.
        home.forget(wt).unwrap();
    }

    #[test]
    fn forgetting_an_own_home_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::own(tmp.path().join("claude"));
        home.prefill(Path::new("/w/q"), false).unwrap();
        home.forget(Path::new("/w/q")).unwrap();
        assert!(!home.dir.exists());
        home.forget(Path::new("/w/q")).unwrap();
    }
}
