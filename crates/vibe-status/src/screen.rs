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
/// proceed?", "Do you want to make this edit to …?"), which a narrow
/// terminal may wrap onto more lines, followed by numbered options, the
/// first a yes and a later one a no.
pub fn approval_on_screen(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(bare).filter(|l| !l.is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(DIALOG_LINES)..];
    let Some(asked) = tail.iter().rposition(|l| l.starts_with("Do you want")) else {
        return false;
    };
    // The question runs on to the first option; wrapped, its `?` is on the
    // last of its lines.
    let rest = &tail[asked..];
    let Some(first) = rest.iter().position(|l| numbered(l).is_some()) else {
        return false;
    };
    if !rest[first - 1].ends_with('?') {
        return false;
    }
    let options = &rest[first..];
    let yes = options.iter().position(|l| option(l, "Yes"));
    let no = options.iter().rposition(|l| option(l, "No"));
    matches!((yes, no), (Some(y), Some(n)) if y < n)
}

/// How much of the bottom of Pi's screen can hold what stopped it: its error,
/// then its footer and an empty prompt.
const PI_ERROR_LINES: usize = 24;

/// What Pi 1.5 shows when a turn can't start for want of a login, always after
/// `Error: ` (`auth-guidance.ts`, `agent-session.ts`, `showError`). It throws
/// before it writes anything, so it's only here.
const PI_ERRORS: &[&str] = &[
    "No API key found for ",
    "No API key for ",
    "Authentication failed for \"",
    "No model selected",
    "No models available",
];

/// The rest of those messages, which a narrow terminal may wrap onto more lines,
/// and all that comes between them and the editor.
const PI_ERROR_HELP: &str = "Use /login to log into a provider via OAuth or API key. See: \
    Then use /model to select a model. \
    Credentials may have expired or network is unavailable. \
    Run to re-authenticate.";

/// What Pi's screen says stopped its turn before it started, if it does. Pi draws
/// what it's told in order, so the error has to be the last thing above its
/// editor, its own help and all: a message since, a tool's output that only reads
/// like one, or the turn it was told to try next would be below. And if Pi is
/// at work it isn't stopped, whatever's above.
pub fn pi_error_on_screen(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().map(bare).filter(|l| !l.is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(PI_ERROR_LINES)..];
    let message = |l: &str| {
        let l = l.strip_prefix("Error: ")?;
        PI_ERRORS
            .iter()
            .any(|e| l.starts_with(e))
            .then(|| l.to_owned())
    };
    let at = tail.iter().rposition(|l| message(l).is_some())?;
    let below = &tail[at + 1..];
    if below.iter().any(|l| pi_at_work(l)) {
        return None;
    }
    let editor = below.iter().position(|l| editor_border(l))?;
    let only_help = below[..editor]
        .iter()
        .flat_map(|l| l.split_whitespace())
        .all(pi_help_word);
    only_help.then(|| message(tail[at])).flatten()
}

/// What Pi said as it gave up, if it did: `screen` is what was left when it exited
/// with a failure. One it won't start at all, such as on a model it doesn't know or
/// can't choose for, prints `Error: ` and why and exits, before it draws anything
/// (`main.ts`), so the error is the last thing there, however a terminal wrapped it.
/// In Pi's words, but for what it says to type: nobody here is at Pi's command line.
/// Part of a screen can come from a guest, so it's said as an alert is.
pub fn pi_error_at_exit(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.trim_end().lines().map(str::trim_end).collect();
    let at = lines.iter().rposition(|l| l.starts_with("Error: "))?;
    let said = &lines[at..];
    if said.iter().any(|l| l.is_empty()) {
        return None;
    }
    // A full row ran on to the next in the middle of whatever it was saying. One
    // shorter broke at a space, which the screen trims, or was the end of a line.
    let row = said.iter().map(|l| l.chars().count()).max()?;
    let mut message = String::new();
    for line in said {
        message.push_str(line);
        if line.chars().count() < row {
            message.push(' ');
        }
    }
    let message = message.strip_prefix("Error: ")?;
    // Where a terminal broke the line, the space on either side of `Use` may have gone.
    let words = message
        .match_indices("Use")
        .find(|(at, _)| message[at + 3..].trim_start().starts_with("--"))
        .map_or(message, |(at, _)| &message[..at]);
    let words = vibe_core::model::clean_alert(words.trim());
    if words.is_empty() {
        return None;
    }
    Some(if words.starts_with("Model \"") {
        format!(
            "{words} Set this profile's model to one Pi has, written as provider/id, then start \
             a new agent: this one keeps the model it started with."
        )
    } else {
        words
    })
}

/// Whether a line of Pi's screen is its spinner: `⠋ Working (esc to interrupt)`
/// where it stands alone, or `── ⠋ Working ───` set in the editor's border
/// (`status-indicator.ts`, `custom-editor.ts`), under whatever message an
/// extension has given it. Any turn that's under way has one.
fn pi_at_work(line: &str) -> bool {
    let line = line.trim_matches(|c: char| c == '─' || c.is_whitespace());
    let spinner = line
        .chars()
        .next()
        .is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c));
    spinner || line == "Working" || line == "Working..." || line.starts_with("Working (")
}

/// Whether a line is the edge of Pi's editor, a rule right across the screen.
fn editor_border(line: &str) -> bool {
    line.chars().filter(|c| *c == '─').count() >= 3
        && line.chars().all(|c| c == '─' || c.is_whitespace())
}

/// Whether a word could be in what Pi says after one of its errors: its help, the
/// provider it was naming, or the path to its docs.
fn pi_help_word(word: &str) -> bool {
    PI_ERROR_HELP.split_whitespace().any(|w| w == word)
        || word.contains('/')
        || word.ends_with(".md")
        || word.ends_with('\'')
}

/// A line without the dialog's frame and the selection marker.
fn bare(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '|' | '❯' | '›' | '>'))
}

/// A numbered option's words: `Yes` of `1. Yes`.
fn numbered(line: &str) -> Option<&str> {
    let (n, rest) = line.split_once(". ")?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then_some(rest)
}

/// A numbered option whose words begin with `word`: `1. Yes`, `3. No, and
/// tell Claude what to do differently (esc)`.
fn option(line: &str, word: &str) -> bool {
    numbered(line).is_some_and(|rest| {
        rest.strip_prefix(word)
            .is_some_and(|r| r.is_empty() || r.starts_with([',', ' ', '.']))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITOR: &str =
        "\n──────────────────\n>\n──────────────────\n~/w  anthropic/claude-sonnet-4  0%\n";

    fn above_the_editor(text: &str) -> String {
        format!("{text}\n{EDITOR}")
    }

    #[test]
    fn a_turn_that_never_started_shows_on_pis_screen() {
        // As Pi's own messages read, its editor and footer below.
        let screen = above_the_editor(
            " pi v1.5.0

 hello

 Error: No API key found for anthropic.

 Use /login to log into a provider via OAuth or API key. See:
   /opt/vibeos/pi/docs/providers.md
   /opt/vibeos/pi/docs/models.md",
        );
        assert_eq!(
            pi_error_on_screen(&screen).as_deref(),
            Some("No API key found for anthropic.")
        );
        for message in [
            "No API key for openai/gpt-5",
            "Authentication failed for \"anthropic\". Credentials may have expired or network \
             is unavailable. Run '/login anthropic' to re-authenticate.",
            "No model selected.",
            "No models available. Use /login",
        ] {
            let screen = above_the_editor(&format!("hello\n\nError: {message}"));
            assert_eq!(pi_error_on_screen(&screen).as_deref(), Some(message));
        }
    }

    /// Pi 1.5's own words, read off it with a model it won't take.
    const NOT_FOUND: &str =
        "Error: Model \"no-such-model\" not found. Use --list-models to see available models.";
    const AMBIGUOUS: &str = "Error: Model \"deepseek-v4.1-flash\" is ambiguous across providers: \
         opencode-go/deepseek-v4.1-flash, opencode/deepseek-v4.1-flash. More than one matching \
         provider is authenticated. Use --provider or provider/model.";
    const WARNING: &str = "Warning: No project session found with id 'x'; creating a new session \
         with that id.";

    #[test]
    fn what_pi_said_as_it_gave_up_is_said_in_its_words_with_nothing_to_type() {
        assert_eq!(
            pi_error_at_exit(&format!("{WARNING}\n{NOT_FOUND}\n\n\n")).as_deref(),
            Some(
                "Model \"no-such-model\" not found. Set this profile's model to one Pi has, \
                 written as provider/id, then start a new agent: this one keeps the model it \
                 started with."
            )
        );
        assert_eq!(
            pi_error_at_exit(AMBIGUOUS).as_deref(),
            Some(
                "Model \"deepseek-v4.1-flash\" is ambiguous across providers: \
                 opencode-go/deepseek-v4.1-flash, opencode/deepseek-v4.1-flash. More than one \
                 matching provider is authenticated. Set this profile's model to one Pi has, \
                 written as provider/id, then start a new agent: this one keeps the model it \
                 started with."
            )
        );
        // Not about a model: only Pi's words.
        assert_eq!(
            pi_error_at_exit("Error: Invalid settings").as_deref(),
            Some("Invalid settings")
        );
    }

    #[test]
    fn what_pi_said_as_it_gave_up_reads_the_same_however_it_was_wrapped() {
        let whole = pi_error_at_exit(AMBIGUOUS).unwrap();
        for width in [40, 61, 80, 120] {
            let chars: Vec<char> = AMBIGUOUS.chars().collect();
            let rows: Vec<String> = chars
                .chunks(width)
                .map(|row| row.iter().collect::<String>().trim_end().to_owned())
                .collect();
            let screen = format!("{WARNING}\n{}\n", rows.join("\n"));
            assert_eq!(
                pi_error_at_exit(&screen).as_deref(),
                Some(&*whole),
                "{width}"
            );
        }
    }

    /// As read off the real Pi through the real shim, at 80 columns, with the model
    /// long enough to put the wrap either side of `Use`.
    #[test]
    fn what_to_type_goes_wherever_the_terminal_broke_the_line() {
        for screen in [
            "Error: Model \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" not found. Use\n\
             --list-models to see available models.\n",
            "Error: Model \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" not found.\n\
             Use --list-models to see available models.\n",
        ] {
            let said = pi_error_at_exit(screen).unwrap();
            assert!(!said.contains("--list-models"), "{said}");
            assert!(!said.contains("Use"), "{said}");
            assert!(said.contains("not found."), "{said}");
        }
        // A model named with the word is left alone.
        let said = pi_error_at_exit("Error: Model \"Use-it\" not found.").unwrap();
        assert!(said.contains("Use-it"), "{said}");
    }

    #[test]
    fn only_the_last_thing_on_the_screen_is_what_pi_gave_up_with() {
        assert_eq!(pi_error_at_exit(""), None);
        assert_eq!(pi_error_at_exit("all done\n"), None);
        assert_eq!(pi_error_at_exit("Error: "), None);
        // A tool's output, long before.
        assert_eq!(
            pi_error_at_exit("Error: from a tool\n\nand on it went\n\nbye\n"),
            None
        );
        // In prose, not at the start of a line.
        assert_eq!(pi_error_at_exit("It said Error: nothing much\n"), None);
    }

    #[test]
    fn what_pi_said_as_it_gave_up_moves_no_cursor_and_is_kept_short() {
        let said = pi_error_at_exit("Error: a\u{1b}[2Jb\u{202e}c\n").unwrap();
        assert_eq!(said, "a[2Jbc");
        let said = pi_error_at_exit(&format!("Error: {}", "x".repeat(5000))).unwrap();
        assert!(said.chars().count() <= 401, "{}", said.len());
    }

    #[test]
    fn a_message_wrapped_in_a_narrow_terminal_is_still_one() {
        let screen = above_the_editor(
            "Error: Authentication failed for \"anthropic\".
Credentials may have expired or
network is unavailable. Run
'/login anthropic' to
re-authenticate.",
        );
        assert!(pi_error_on_screen(&screen).is_some());
    }

    #[test]
    fn talk_of_a_login_is_not_a_failed_turn() {
        assert_eq!(pi_error_on_screen(""), None);
        // In prose or a tool's output, not at the start of a line.
        let screen = above_the_editor("The tool printed: No API key found for foo.");
        assert_eq!(pi_error_on_screen(&screen), None);
        // Without Pi's own `Error: `.
        let screen = above_the_editor("No API key found for foo.");
        assert_eq!(pi_error_on_screen(&screen), None);
        // Scrolled well up, under what came since.
        let mut screen = "Error: No API key found for foo.\n".to_owned();
        for i in 0..PI_ERROR_LINES {
            screen.push_str(&format!("line {i}\n"));
        }
        assert_eq!(pi_error_on_screen(&above_the_editor(&screen)), None);
        // No editor below it, so it isn't Pi drawing it.
        assert_eq!(
            pi_error_on_screen("Error: No API key found for foo.\n"),
            None
        );
    }

    /// N2: a tool's output reads just like Pi's error, and only what's below it
    /// tells them apart.
    #[test]
    fn a_tools_output_is_not_a_failed_turn() {
        let screen = above_the_editor(
            "$ npm test\n  Error: No API key found for stripe.\n  1 test failed\n\nThe tests fail.",
        );
        assert_eq!(pi_error_on_screen(&screen), None);
    }

    /// An answer may start with the word; only Pi's own spinner says it's at work.
    #[test]
    fn an_answer_about_working_above_an_error_doesnt_hide_it() {
        let screen = above_the_editor(
            "Working on it: done.\n\nnext\n\nError: No API key found for mock.\n\nUse /login",
        );
        assert!(pi_error_on_screen(&screen).is_some());
    }

    /// N2: an old error stays on the screen through the turn after it.
    #[test]
    fn an_error_from_before_isnt_this_turns() {
        let error = "hello\n\nError: No API key found for mock.\n\nUse /login";
        // With an answer since, or the next message.
        let screen = above_the_editor(&format!("{error}\n\nand again\n\nThe answer."));
        assert_eq!(pi_error_on_screen(&screen), None);
        // Under way, however Pi draws its spinner.
        for working in [
            "── ⠋ Working ─────────────",
            "── ⠙ Working (esc to interrupt) ──",
            "⠹ Working",
            "⠼ Compacting context...",
            "Working (escape to interrupt)",
        ] {
            let screen = format!("{error}\n\n{working}\n>\n──────────\n~/w mock/m 0%\n");
            assert_eq!(pi_error_on_screen(&screen), None, "{working}");
        }
        // But the rule of an idle editor is no spinner.
        assert!(pi_error_on_screen(&above_the_editor(error)).is_some());
    }

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
    fn a_dialog_wrapped_in_a_narrow_terminal_is_one() {
        let screen = "\
╭────────────────────────────────────────╮
│ Edit file                              │
│ Do you want to make this edit to       │
│ src/a/rather/long/path/to/the/file.rs? │
│ ❯ 1. Yes                               │
│   2. Yes, allow all edits during this  │
│   session (shift+tab)                  │
│   3. No, and tell Claude what to do    │
│   differently (esc)                    │
╰────────────────────────────────────────╯
";
        assert!(approval_on_screen(screen));
        // A question that never ends in `?` before its options isn't one.
        let screen = "Do you want to see the rest\n1. Yes\n2. No\n";
        assert!(!approval_on_screen(screen));
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
