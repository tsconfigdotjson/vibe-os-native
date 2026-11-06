//! Pi's own model list, for the app (PRD: Profiles). `vibed` runs the Pi this VibeOS
//! pins with [`vibe_env::pi_setup`]'s extension, offline, and reads what it says. Only
//! a provider's name and whether it has a login are ever read here. VibeOS signs nobody
//! in to Pi: its logins are Pi's own (PRD: Who gets which credentials).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use vibe_core::model::{LOCAL_HOST, PiTarget};
use vibe_core::paths::Layout;
use vibe_core::rpc::{ErrorCode, ErrorObject, PiModels, PiModelsParams};
use vibe_env::pi;
use vibe_env::pi_release;
use vibe_env::pi_setup::{self, Said};

use crate::core::{Core, error};

/// How long a list may take: it reads files and starts Pi, and that's a second.
const LIST_LIMIT: Duration = Duration::from_secs(30);

/// Where a run's own directories go, under the data directory.
const SCRATCH: &str = ".pi-setup";

/// What this holds for `pi.models`.
#[derive(Default)]
pub struct PiSetup {
    /// What each target's list was, and what it was made from.
    lists: tokio::sync::Mutex<BTreeMap<&'static str, (String, PiModels)>>,
}

impl std::fmt::Debug for PiSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiSetup").finish_non_exhaustive()
    }
}

/// A directory for one run of Pi, gone with this, whichever way it ends. What a crash
/// leaves goes at the next start ([`clear_scratch`]).
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(layout: &Layout, name: &str) -> std::io::Result<Scratch> {
        let root = layout.data_dir().join(SCRATCH);
        vibe_core::paths::private_dir(&layout.data_dir())?;
        pi::make_private(&root)?;
        let scratch = Scratch {
            dir: root.join(name),
        };
        let _ = std::fs::remove_dir_all(&scratch.dir);
        pi::make_private(&scratch.cwd())?;
        pi::make_private(&scratch.agent_dir())?;
        std::fs::write(scratch.extension(), pi_setup::EXTENSION)?;
        Ok(scratch)
    }

    fn cwd(&self) -> PathBuf {
        self.dir.join("cwd")
    }

    fn agent_dir(&self) -> PathBuf {
        self.dir.join("agent")
    }

    fn extension(&self) -> PathBuf {
        self.dir.join(pi_setup::EXTENSION_NAME)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Takes away what a run left if `vibed` went down during it.
pub fn clear_scratch(layout: &Layout) {
    let _ = std::fs::remove_dir_all(layout.data_dir().join(SCRATCH));
}

/// Pi's own stream of what it says, a line at a time, of bounded size.
type Lines = tokio::io::Lines<BufReader<tokio::io::Take<ChildStdout>>>;

/// Pi started in `scratch`, on `env`, having been given `command`.
async fn start_pi(
    program: &Path,
    scratch: &Scratch,
    env: Vec<(String, String)>,
    command: &str,
) -> std::io::Result<(Child, ChildStdin, Lines)> {
    let mut child = Command::new(program)
        .args(pi_setup::args(&scratch.extension()))
        .env_clear()
        .envs(env)
        .current_dir(scratch.cwd())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // What it prints there isn't for anyone, and an error's said in its way.
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(std::io::Error::other("Pi started with no pipes"));
    };
    stdin.write_all(format!("{command}\n").as_bytes()).await?;
    stdin.flush().await?;
    Ok((
        child,
        stdin,
        BufReader::new(stdout.take(pi_setup::MAX_OUTPUT)).lines(),
    ))
}

/// What a list depends on, to know when it's out of date: Pi's own files, by when
/// they changed, and which Pi.
fn fingerprint(program: &Path, files: &[PathBuf]) -> String {
    let mut print = program.to_string_lossy().into_owned();
    for file in files {
        let seen = std::fs::metadata(file).ok().map(|m| {
            let at = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (at, m.len())
        });
        print.push_str(&format!(";{seen:?}"));
    }
    print
}

impl Core {
    /// Which Pi a target comes to here: a host with no VMs has only its own.
    fn pi_target(&self, asked: PiTarget) -> PiTarget {
        if self.info().os == "macos" {
            asked
        } else {
            PiTarget::Native
        }
    }

    /// The Pi this can talk to, if there is one yet: the pinned one, as what
    /// [`pi_setup`] does isn't a documented API. An owner's own `pi` of that version
    /// will do, and it's left as it is otherwise.
    async fn pi_program_now(&self) -> Option<PathBuf> {
        if let Some(program) = &self.pi_driver {
            return Some(program.clone());
        }
        let kept = pi_release::kept_binary(&self.layout.data_dir(), pi_release::PINNED);
        let checks = Arc::clone(&self.checks);
        tokio::task::spawn_blocking(move || {
            if kept.is_file() {
                return Some(kept);
            }
            match checks.pi() {
                Ok((path, version)) if version.trim() == pi_release::PINNED => Some(path),
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// That Pi, getting it first if there isn't one, which takes as long as the
    /// download. It's checked as any install is, and linked nowhere.
    async fn pi_program(&self) -> Result<PathBuf, String> {
        if let Some(program) = self.pi_program_now().await {
            return Ok(program);
        }
        let _one_at_a_time = self.pi_installing.lock().await;
        if let Some(program) = self.pi_program_now().await {
            return Ok(program);
        }
        let platform =
            pi_release::platform_here().ok_or("Pi publishes no build for this machine")?;
        let (releases, data) = (self.pi_releases.clone(), self.layout.data_dir());
        let kept = tokio::task::spawn_blocking(move || {
            pi_release::keep(&releases, pi_release::PINNED, platform, &data)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("can't get Pi: {e}"))?;
        crate::log!("vibed: Pi {} is at {}", pi_release::PINNED, kept.display());
        Ok(kept)
    }

    /// A new name for a run.
    fn pi_run_name(&self) -> Result<String, ErrorObject> {
        let random = self.random_16()?;
        Ok(random.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// `pi.models`.
    pub async fn pi_models(&self, p: PiModelsParams) -> Result<PiModels, ErrorObject> {
        self.host(p.host.as_deref())?;
        let target = self.pi_target(p.target);
        let no_list = |why: String| PiModels {
            host: LOCAL_HOST.to_owned(),
            target,
            providers: Vec::new(),
            error: Some(vibe_core::model::clean_alert(&why)),
        };
        // One at a time: it's the same answer for whoever asked meanwhile.
        let mut lists = self.pi_setup.lists.lock().await;
        let program = match self.pi_program().await {
            Ok(program) => program,
            Err(why) => return Ok(no_list(why)),
        };
        let files = self.pi_list_files(target);
        let print = {
            let program = program.clone();
            tokio::task::spawn_blocking(move || fingerprint(&program, &files))
                .await
                .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
        };
        if let Some((seen, list)) = lists.get(target.as_str())
            && *seen == print
        {
            return Ok(list.clone());
        }
        match self.pi_list(target, &program).await {
            Ok(list) => {
                lists.insert(target.as_str(), (print, list.clone()));
                Ok(list)
            }
            Err(why) => Ok(no_list(why)),
        }
    }

    /// What a target's list comes from.
    fn pi_list_files(&self, target: PiTarget) -> Vec<PathBuf> {
        let mut files = vec![self.layout.pi_env_file()];
        match target {
            PiTarget::Native => {
                let own = pi::own_dir(self.layout.home());
                for name in ["auth.json", "models.json", "models-store.json"] {
                    files.push(own.join(name));
                }
            }
            PiTarget::Vm => {
                files.push(self.layout.linux_pi_logins_dir().join("auth.json"));
                files.push(self.layout.linux_pi_dir().join("models.json"));
            }
        }
        files
    }

    /// Asks Pi for a target's list. It's offline and touches no login. Native agents'
    /// is in the owner's own agent dir, where Pi sees what they have. A VM's Pi
    /// can't be run from here, so it's the Mac's, of the same version, on the
    /// owner's models and with the store's logins looked at beside it.
    async fn pi_list(&self, target: PiTarget, program: &Path) -> Result<PiModels, String> {
        let name = self.pi_run_name().map_err(|e| e.message)?;
        let scratch = Scratch::new(&self.layout, &name).map_err(|e| e.to_string())?;
        let keys = pi::read_env_file(&self.layout.pi_env_file()).unwrap_or_default();
        let (agent_dir, store) = match target {
            PiTarget::Native => (pi::own_dir(self.layout.home()), Vec::new()),
            PiTarget::Vm => {
                let models = self.layout.linux_pi_dir().join("models.json");
                if let Ok(Some(text)) = pi::read_plain_file(&models) {
                    let _ = std::fs::write(scratch.agent_dir().join("models.json"), text);
                }
                (
                    scratch.agent_dir(),
                    pi::logins_in(&self.layout.linux_pi_logins_dir()),
                )
            }
        };
        pi::make_private(&agent_dir).map_err(|e| e.to_string())?;
        let env = pi_setup::env(std::env::vars(), &agent_dir, keys);
        let (_child, _stdin, mut lines) =
            start_pi(program, &scratch, env, &pi_setup::catalog_command())
                .await
                .map_err(|e| format!("can't run Pi: {e}"))?;
        let catalog = tokio::time::timeout(LIST_LIMIT, async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match pi_setup::read_line(&line) {
                        Some(Said::Catalog(catalog)) => return Ok(catalog),
                        Some(Said::Failed(why)) => return Err(why),
                        _ => {}
                    },
                    _ => return Err("Pi stopped before it gave its list".to_owned()),
                }
            }
        })
        .await
        .map_err(|_| "Pi took too long to give its list".to_owned())??;
        Ok(PiModels {
            host: LOCAL_HOST.to_owned(),
            target,
            providers: pi_setup::providers_of(catalog, |id| store.iter().any(|s| s == id)),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_crash_leaves_of_a_run_goes() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path(), "linux");
        let scratch = Scratch::new(&layout, "abc").unwrap();
        assert!(scratch.extension().is_file());
        assert!(scratch.cwd().is_dir());
        let dir = scratch.dir.clone();
        drop(scratch);
        assert!(!dir.exists());
        let scratch = Scratch::new(&layout, "abc").unwrap();
        std::mem::forget(scratch);
        clear_scratch(&layout);
        assert!(!layout.data_dir().join(SCRATCH).exists());
    }
}
