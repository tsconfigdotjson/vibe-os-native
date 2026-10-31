//! The screen model: a `vt100` terminal fed everything the command writes,
//! plus a ring of the lines that scrolled off its top. It redraws the screen
//! for a client that attaches, and answers `read_output`.

use std::collections::VecDeque;
use std::num::NonZeroU16;

use vibe_core::shim::{Output, Size};
use vt100::capture::{BasicFormattedCaptureState, RowContents, basic_formatted_to_plain};

/// Lines of history kept by default.
pub(crate) const DEFAULT_HISTORY: usize = 10_000;

/// The lines that scrolled off the top of the main screen, as plain text.
#[derive(Debug)]
struct History {
    lines: VecDeque<String>,
    /// How many lines have scrolled off in all, kept or not.
    total: u64,
    capacity: usize,
}

impl vt100::Callbacks for History {
    fn on_scroll(&mut self, row: RowContents<'_>, alternate_screen: bool) {
        // A full-screen program's own scrolling isn't history.
        if alternate_screen || self.capacity == 0 {
            return;
        }
        let mut formatted = String::new();
        let _ = row.write_formatted_basic(&mut formatted, &mut BasicFormattedCaptureState::new());
        let plain: String = basic_formatted_to_plain(&formatted).collect();
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(plain.trim_end().to_owned());
        self.total += 1;
    }
}

pub(crate) struct Screen {
    // No scrollback in the parser: rows that leave the screen go to
    // `History` through `on_scroll` instead.
    parser: vt100::Parser<History>,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

fn dims(size: Size) -> (NonZeroU16, NonZeroU16) {
    let one = NonZeroU16::MIN;
    (
        NonZeroU16::new(size.rows).unwrap_or(one),
        NonZeroU16::new(size.cols).unwrap_or(one),
    )
}

impl Screen {
    pub(crate) fn new(size: Size, history: usize) -> Screen {
        let (rows, cols) = dims(size);
        let history = History {
            lines: VecDeque::new(),
            total: 0,
            capacity: history,
        };
        Screen {
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, history),
        }
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub(crate) fn size(&self) -> Size {
        let (rows, cols) = self.parser.screen().size();
        Size {
            rows: rows.get(),
            cols: cols.get(),
        }
    }

    /// Rows a shrink pushes off the top go to the history.
    pub(crate) fn resize(&mut self, size: Size) {
        let (rows, cols) = dims(size);
        self.parser.set_size(rows, cols);
    }

    /// Bytes that turn a terminal into this one: the right screen, main or
    /// alternate, every cell, the cursor and the input modes. A client that
    /// fell behind may be on either screen, so both cases switch.
    pub(crate) fn redraw(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = String::new();
        out.push_str(if screen.alternate_screen() {
            "\x1b[?1049h"
        } else {
            "\x1b[?1049l"
        });
        out.push_str(&screen.state_formatted());
        out.into_bytes()
    }

    /// The screen as text, one line per row with trailing blanks trimmed,
    /// and no blank rows at the bottom.
    fn text(&self) -> String {
        let screen = self.parser.screen();
        let cols = self.size().cols;
        let mut rows: Vec<String> = screen
            .rows(0, cols)
            .map(|r| r.trim_end().to_owned())
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// The screen, and the history after `after` (everything kept, if
    /// absent), as much of it as fits in `budget` bytes of text.
    pub(crate) fn output(&self, after: Option<u64>, budget: usize) -> Output {
        let h = self.parser.callbacks();
        let first_kept = h.total - h.lines.len() as u64;
        let after = after.unwrap_or(first_kept).min(h.total);
        let start = after.max(first_kept);
        let screen = self.text();
        let mut left = budget.saturating_sub(screen.len());
        let mut history = Vec::new();
        for line in h.lines.iter().skip((start - first_kept) as usize) {
            // A line costs its bytes, and JSON's quotes and comma.
            let cost = line.len() + 3;
            if cost > left {
                break;
            }
            left -= cost;
            history.push(line.clone());
        }
        let cursor = start + history.len() as u64;
        Output {
            screen,
            history,
            cursor,
            dropped: start - after,
            more: cursor < h.total,
            bracketed_paste: self.parser.screen().bracketed_paste(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, history: usize) -> Screen {
        Screen::new(Size { rows, cols: 20 }, history)
    }

    #[test]
    fn bracketed_paste_is_reported() {
        let mut s = screen(3, 10);
        assert!(!s.output(None, 1 << 20).bracketed_paste);
        s.process(b"\x1b[?2004h");
        assert!(s.output(None, 1 << 20).bracketed_paste);
        s.process(b"\x1b[?2004l");
        assert!(!s.output(None, 1 << 20).bracketed_paste);
    }

    #[test]
    fn text_is_trimmed() {
        let mut s = screen(5, 10);
        s.process(b"hello   \r\n\x1b[1mbold\x1b[0m\r\n");
        let out = s.output(None, usize::MAX);
        assert_eq!(out.screen, "hello\nbold");
        assert!(out.history.is_empty());
        assert_eq!((out.cursor, out.dropped), (0, 0));
    }

    #[test]
    fn lines_that_scroll_off_are_history() {
        let mut s = screen(3, 100);
        for i in 0..7 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        // Three rows: two lines and the cursor's empty row.
        let all = s.output(None, usize::MAX);
        assert_eq!(all.screen, "line 5\nline 6");
        assert_eq!(
            all.history,
            ["line 0", "line 1", "line 2", "line 3", "line 4"]
        );
        assert_eq!(all.cursor, 5);
        // Since a cursor: only what's new.
        s.process(b"line 7\r\n");
        let new = s.output(Some(all.cursor), usize::MAX);
        assert_eq!(new.history, ["line 5"]);
        assert_eq!((new.cursor, new.dropped), (6, 0));
        // Nothing new.
        assert!(s.output(Some(6), usize::MAX).history.is_empty());
        // A cursor from the future is treated as now.
        let future = s.output(Some(99), usize::MAX);
        assert!(future.history.is_empty());
        assert_eq!(future.cursor, 6);
    }

    #[test]
    fn history_is_a_ring() {
        let mut s = screen(2, 3);
        for i in 0..10 {
            s.process(format!("{i}\r\n").as_bytes());
        }
        let out = s.output(Some(2), usize::MAX);
        assert_eq!(out.history, ["6", "7", "8"]);
        assert_eq!(out.cursor, 9);
        // Lines 2 to 5 came after the cursor but are gone.
        assert_eq!(out.dropped, 4);
        let all = s.output(None, usize::MAX);
        assert_eq!((all.history.len(), all.dropped), (3, 0));
    }

    #[test]
    fn the_alternate_screen_is_not_history() {
        let mut s = screen(2, 10);
        s.process(b"\x1b[?1049h");
        for i in 0..5 {
            s.process(format!("full {i}\r\n").as_bytes());
        }
        assert_eq!(s.output(None, usize::MAX).cursor, 0);
        let redraw = String::from_utf8(s.redraw()).unwrap();
        assert!(redraw.starts_with("\x1b[?1049h"), "{redraw:?}");
        s.process(b"\x1b[?1049l");
        let redraw = String::from_utf8(s.redraw()).unwrap();
        assert!(redraw.starts_with("\x1b[?1049l"), "{redraw:?}");
    }

    #[test]
    fn redraw_reproduces_the_screen() {
        let mut s = screen(4, 10);
        s.process(b"one\r\n\x1b[31mred\x1b[0m\r\nthree\x1b[?2004h");
        let mut copy = screen(4, 10);
        copy.process(&s.redraw());
        assert_eq!(copy.output(None, usize::MAX).screen, "one\nred\nthree");
        assert_eq!(
            copy.parser.screen().cursor_position(),
            s.parser.screen().cursor_position()
        );
        assert!(copy.parser.screen().bracketed_paste());
        assert_eq!(
            copy.parser.screen().cell(1, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn shrinking_pushes_rows_into_history() {
        let mut s = screen(4, 10);
        s.process(b"a\r\nb\r\nc\r\nd");
        s.resize(Size { rows: 2, cols: 20 });
        assert_eq!(s.size(), Size { rows: 2, cols: 20 });
        let out = s.output(None, usize::MAX);
        assert_eq!(out.history, ["a", "b"]);
        assert_eq!(out.screen, "c\nd");
        // A zero size is clamped rather than refused.
        s.resize(Size { rows: 0, cols: 0 });
        assert_eq!(s.size(), Size { rows: 1, cols: 1 });
    }

    #[test]
    fn history_is_paged_to_fit() {
        let mut s = screen(2, 100);
        for i in 0..10 {
            s.process(format!("line-{i}\r\n").as_bytes());
        }
        // The screen is "line-9" (6 bytes); each line costs 6 + 3.
        let first = s.output(None, 6 + 3 * 9);
        assert_eq!(first.history, ["line-0", "line-1", "line-2"]);
        assert_eq!((first.cursor, first.more), (3, true));
        let rest = s.output(Some(first.cursor), usize::MAX);
        assert_eq!(rest.history.len(), 6);
        assert_eq!((rest.cursor, rest.more, rest.dropped), (9, false, 0));
        // Too small for even one line still answers, with the screen.
        let none = s.output(Some(0), 0);
        assert!(none.history.is_empty());
        assert_eq!((none.cursor, none.more), (0, true));
    }

    #[test]
    fn no_history_kept() {
        let mut s = screen(2, 0);
        s.process(b"1\r\n2\r\n3\r\n");
        let out = s.output(None, usize::MAX);
        assert!(out.history.is_empty());
        assert_eq!(out.cursor, 0);
    }
}
