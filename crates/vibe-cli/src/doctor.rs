//! `vibe doctor`: what this host needs for agents to work, as `vibed` sees
//! it (PRD: Key user flows, Install and setup), one line a check, with
//! what's left to do about each that isn't right. There's no step to fix things:
//! asking has `vibed` try at once whatever's its to do, and list what isn't.
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
        CheckId::Linger => "stays up",
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
            out.push_str(&format!("    {:width$}  {hint}\n", ""));
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

/// Whether asking the doctor tries `check` again now. Not the Linux images: that
/// would read as under way rather than say why it failed, and start another large
/// download each time. `vibed` tries those again by itself, and the app has a button.
fn tried_on_asking(check: CheckId) -> bool {
    check != CheckId::Linux
}

/// Runs the checks, with `vibed` first trying now whatever it does by itself.
/// Ok(true) when nothing needs the owner.
pub(crate) fn run(socket: &Path) -> Result<bool, ClientError> {
    let mut report = client::call::<DoctorCheckGet>(socket, &NoParams {}, TIMEOUT)?;
    let fixable: Vec<CheckId> = failures(&report)
        .filter(|c| c.fixable && tried_on_asking(c.id))
        .map(|c| c.id)
        .collect();
    for check in fixable {
        // What's still wrong is in the report, in its own words.
        if let Ok(r) = client::call::<DoctorFix>(socket, &DoctorFixParams { check }, TIMEOUT) {
            report = r;
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
    fn a_failed_download_is_said_and_not_started_again_on_asking() {
        assert!(!tried_on_asking(CheckId::Linux));
        assert!(tried_on_asking(CheckId::GhGit));
        assert!(tried_on_asking(CheckId::Linger));
        assert!(tried_on_asking(CheckId::Pi));
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
                    Some("VibeOS sets this up itself."),
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
                    VibeOS sets this up itself.
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
