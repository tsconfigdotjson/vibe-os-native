//! `vibe doctor`: what this host needs for agents to work, as `vibed` sees
//! it (PRD: Key user flows, Install and setup), one line a check, with
//! what to do about each that isn't right. `--fix` has `vibed` do what it
//! can: git using `gh` for GitHub, and the Linux images' download.
//! VibeOS.app checks its own permission to control Terminal, which only
//! the app can see.

use std::path::Path;
use std::time::Duration;

use vibe_core::model::{CheckId, CheckState};
use vibe_core::rpc::{
    DoctorCheck, DoctorCheckGet, DoctorFix, DoctorFixParams, DoctorReport, NoParams,
};

use crate::client::{self, ClientError};

/// `gh auth status` alone may take 30 s.
const TIMEOUT: Duration = Duration::from_secs(90);

/// A check's name, for people.
pub(crate) fn title(id: CheckId) -> &'static str {
    match id {
        CheckId::Git => "git",
        CheckId::Gh => "GitHub login",
        CheckId::GhGit => "git through gh",
        CheckId::Claude => "Claude Code",
        CheckId::ClaudeLogin => "Claude login",
        CheckId::Power => "system sleep",
        CheckId::Tailscale => "Tailscale",
        CheckId::Linux => "Linux images",
        CheckId::Pi => "Pi",
    }
}

fn mark(state: CheckState) -> &'static str {
    match state {
        CheckState::Pass => "ok",
        CheckState::Fail => "!!",
        CheckState::Optional => "--",
        CheckState::Working => "..",
    }
}

/// The report, a line a check and an indented line for each hint.
pub(crate) fn render(report: &DoctorReport) -> String {
    let width = report
        .checks
        .iter()
        .map(|c| title(c.id).len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for c in &report.checks {
        let detail = match c.progress {
            Some(p) => format!("{} ({p}%).", c.detail.trim_end_matches('.')),
            None => c.detail.clone(),
        };
        out.push_str(&format!(
            "{}  {:width$}  {detail}\n",
            mark(c.state),
            title(c.id),
        ));
        if let Some(hint) = &c.hint {
            let fix = if c.fixable && c.state == CheckState::Fail {
                " `vibe doctor --fix` can do it."
            } else {
                ""
            };
            out.push_str(&format!("    {:width$}  {hint}{fix}\n", ""));
        }
    }
    let failed = failures(report).count();
    let working = report
        .checks
        .iter()
        .filter(|c| c.state == CheckState::Working)
        .count();
    out.push_str(&match (failed, working) {
        (0, 0) => "This host is ready for agents.\n".to_owned(),
        (0, _) => "Nothing needs you; the rest is under way.\n".to_owned(),
        (1, _) => "1 check needs you.\n".to_owned(),
        (n, _) => format!("{n} checks need you.\n"),
    });
    out
}

fn failures(report: &DoctorReport) -> impl Iterator<Item = &DoctorCheck> {
    report.checks.iter().filter(|c| c.state == CheckState::Fail)
}

/// Runs the checks, and with `fix` fixes what `vibed` can first. Ok(true)
/// when nothing needs the owner.
pub(crate) fn run(socket: &Path, fix: bool) -> Result<bool, ClientError> {
    let mut report = client::call::<DoctorCheckGet>(socket, &NoParams {}, TIMEOUT)?;
    if fix {
        let fixable: Vec<CheckId> = failures(&report)
            .filter(|c| c.fixable)
            .map(|c| c.id)
            .collect();
        for check in fixable {
            match client::call::<DoctorFix>(socket, &DoctorFixParams { check }, TIMEOUT) {
                Ok(r) => {
                    println!("fixed: {}", title(check));
                    report = r;
                }
                Err(e) => eprintln!("vibe: can't fix {}: {e}", title(check)),
            }
        }
    }
    print!("{}", render(&report));
    Ok(failures(&report).next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: CheckId, state: CheckState, hint: Option<&str>, fixable: bool) -> DoctorCheck {
        DoctorCheck {
            id,
            state,
            detail: format!("{} said so.", id.as_str()),
            hint: hint.map(str::to_owned),
            fixable,
            progress: None,
        }
    }

    #[test]
    fn every_check_has_a_name() {
        for id in CheckId::ALL {
            assert!(!title(*id).is_empty());
        }
    }

    #[test]
    fn a_report_reads_as_a_checklist() {
        let mut linux = check(CheckId::Linux, CheckState::Working, None, false);
        linux.progress = Some(40);
        let report = DoctorReport {
            checks: vec![
                check(CheckId::Git, CheckState::Pass, None, false),
                check(
                    CheckId::GhGit,
                    CheckState::Fail,
                    Some("Run `gh auth setup-git`."),
                    true,
                ),
                check(
                    CheckId::Tailscale,
                    CheckState::Optional,
                    Some("Install it, if you want it."),
                    false,
                ),
                linux,
            ],
        };
        assert_eq!(
            render(&report),
            "\
ok  git             git said so.
!!  git through gh  gh_git said so.
                    Run `gh auth setup-git`. `vibe doctor --fix` can do it.
--  Tailscale       tailscale said so.
                    Install it, if you want it.
..  Linux images    linux said so (40%).
1 check needs you.
"
        );
        let ready = DoctorReport {
            checks: vec![check(CheckId::Git, CheckState::Pass, None, false)],
        };
        assert!(render(&ready).ends_with("This host is ready for agents.\n"));
        let downloading = DoctorReport {
            checks: vec![
                check(CheckId::Git, CheckState::Pass, None, false),
                check(CheckId::Linux, CheckState::Working, None, false),
            ],
        };
        assert!(render(&downloading).ends_with("Nothing needs you; the rest is under way.\n"));
    }
}
