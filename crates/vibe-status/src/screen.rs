//! What the agent's screen says, where the transcript can't: whether a
//! permission dialog is really up.
//!
//! Claude Code writes nothing to its transcript when it asks for
//! permission, so agent-tail only infers a prompt after a tool call has
//! gone 20 s without a result. A long tool run in auto mode looks the same.
//! The shim's screen model settles it: a real prompt draws a dialog.

/// How many of the screen's last non-blank lines a dialog is looked for in.
/// Claude draws its prompt at the bottom, above at most a status line or
/// two.
const DIALOG_LINES: usize = 24;

/// Whether `screen` shows a permission dialog: a question ("Do you want to
/// proceed?", "Do you want to make this edit to …?") followed by numbered
/// options, the first a yes and a later one a no.
pub fn approval_on_screen(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(bare).filter(|l| !l.is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(DIALOG_LINES)..];
    let Some(asked) = tail
        .iter()
        .rposition(|l| l.starts_with("Do you want") && l.ends_with('?'))
    else {
        return false;
    };
    let options = &tail[asked + 1..];
    let yes = options.iter().position(|l| option(l, "Yes"));
    let no = options.iter().rposition(|l| option(l, "No"));
    matches!((yes, no), (Some(y), Some(n)) if y < n)
}

/// A line without the dialog's frame and the selection marker.
fn bare(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '|' | '❯' | '›' | '>'))
}

/// A numbered option whose words begin with `word`: `1. Yes`, `3. No, and
/// tell Claude what to do differently (esc)`.
fn option(line: &str, word: &str) -> bool {
    let Some((n, rest)) = line.split_once(". ") else {
        return false;
    };
    !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
        && rest
            .strip_prefix(word)
            .is_some_and(|r| r.is_empty() || r.starts_with([',', ' ', '.']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bash_permission_dialog_is_one() {
        let screen = "\
⏺ Bash(cargo publish)
╭──────────────────────────────────────────────────────────────╮
│ Bash command                                                 │
│                                                              │
│   cargo publish                                              │
│   Publish the crate                                          │
│                                                              │
│ Do you want to proceed?                                      │
│ ❯ 1. Yes                                                     │
│   2. Yes, and don't ask again for cargo publish commands     │
│   3. No, and tell Claude what to do differently (esc)        │
╰──────────────────────────────────────────────────────────────╯
";
        assert!(approval_on_screen(screen));
    }

    #[test]
    fn an_edit_dialog_without_a_frame_is_one() {
        let screen = "\
 Edit file
 src/main.rs
 Do you want to make this edit to main.rs?
 ❯ 1. Yes
   2. Yes, allow all edits during this session (shift+tab)
   3. No, and tell Claude what to do differently (esc)

 Esc to cancel
";
        assert!(approval_on_screen(screen));
    }

    #[test]
    fn a_long_tool_run_is_not_one() {
        let screen = "\
> run the slow test
⏺ Bash(sleep 60)
  ⎿  Running…

✻ Waiting… (25s · esc to interrupt)

╭──────────────────────────────────────────╮
│ >                                        │
╰──────────────────────────────────────────╯
  ⏵⏵ auto mode on (shift+tab to cycle)
";
        assert!(!approval_on_screen(screen));
        assert!(!approval_on_screen(""));
    }

    #[test]
    fn a_question_in_the_conversation_is_not_one() {
        // Claude asking in prose, with a list, isn't a dialog: it has no
        // yes before a no.
        let screen = "\
⏺ Do you want to keep the old API?
  1. Keep it behind a flag
  2. No, remove it
";
        assert!(!approval_on_screen(screen));
        // A dialog long since scrolled up, with the work going on below.
        let mut screen = String::from("Do you want to proceed?\n1. Yes\n2. No\n");
        for i in 0..DIALOG_LINES {
            screen.push_str(&format!("⏺ step {i}\n"));
        }
        assert!(!approval_on_screen(&screen));
    }
}
