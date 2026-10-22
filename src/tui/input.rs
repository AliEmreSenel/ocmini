use super::{Attachment, PendingPrompt, Ui, BOLD, RESET};
use std::collections::VecDeque;
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Edit position: a grapheme index into `text`, always in `0..=count`.
    pub cursor: usize,
    /// Fixed end of the selection, when the caret is moving it.
    anchor: Option<usize>,
    /// Snapshot stack for Ctrl+Z.
    undo: Vec<(String, usize)>,
}

pub(super) fn pop_grapheme(text: &mut String) -> bool {
    let Some((index, _)) = text.grapheme_indices(true).next_back() else {
        return false;
    };
    text.truncate(index);
    true
}

pub(super) fn clean_input(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

fn graphemes(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Byte offset of the grapheme at `index`, or the end of the string.
fn byte_index(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(index)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn word_char(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

fn word_start(chars: &[&str], from: usize) -> usize {
    let mut i = from.min(chars.len());
    while i > 0 && !word_char(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

fn word_end(chars: &[&str], from: usize) -> usize {
    let mut i = from.min(chars.len());
    while i < chars.len() && !word_char(chars[i]) {
        i += 1;
    }
    while i < chars.len() && word_char(chars[i]) {
        i += 1;
    }
    i
}

impl Draft {
    pub fn insert(&mut self, ch: char) {
        // delete_selection() snapshots; typing over a selection is ONE undo step.
        if self.selection().is_none() {
            self.snapshot();
        }
        self.delete_selection();
        self.text.insert(byte_index(&self.text, self.cursor), ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        if self.selection().is_none() {
            self.snapshot();
        }
        self.delete_selection();
        let added = graphemes(text);
        self.text
            .insert_str(byte_index(&self.text, self.cursor), text);
        self.cursor += added;
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (lo, hi) = (anchor.min(self.cursor), anchor.max(self.cursor));
        (lo < hi).then_some((lo, hi))
    }

    fn anchor_here(&mut self) {
        self.anchor = Some(self.anchor.unwrap_or(self.cursor));
    }

    fn settle_anchor(&mut self) {
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    /// Remove the selection, returning its text (for the kill ring).
    fn delete_selection(&mut self) -> Option<String> {
        let (lo, hi) = self.selection()?;
        let text = self.text[byte_index(&self.text, lo)..byte_index(&self.text, hi)].to_string();
        self.snapshot();
        self.text
            .replace_range(byte_index(&self.text, lo)..byte_index(&self.text, hi), "");
        self.cursor = lo;
        self.anchor = None;
        Some(text)
    }

    fn snapshot(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
    }

    pub fn undo(&mut self) {
        if let Some((text, cursor)) = self.undo.pop() {
            self.text = text;
            self.cursor = cursor.min(graphemes(&self.text));
            self.anchor = None;
        }
    }

    fn clamped(&self) -> usize {
        self.cursor.min(graphemes(&self.text))
    }

    fn left_impl(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right_impl(&mut self) {
        self.cursor = (self.cursor + 1).min(graphemes(&self.text));
    }

    /// Start of the current line; 0 for the first line.
    fn home_impl(&mut self) {
        let bytes = byte_index(&self.text, self.clamped());
        let start = self.text[..bytes].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = graphemes(&self.text[..start]);
    }

    /// End of the current line; the text end for the last line.
    fn end_impl(&mut self) {
        let bytes = byte_index(&self.text, self.clamped());
        let end = self.text[bytes..]
            .find('\n')
            .map_or(self.text.len(), |i| bytes + i);
        self.cursor = graphemes(&self.text[..end]);
    }

    fn word_left_impl(&mut self) {
        let chars: Vec<&str> = self.text.graphemes(true).collect();
        self.cursor = word_start(&chars, self.clamped());
    }

    fn word_right_impl(&mut self) {
        let chars: Vec<&str> = self.text.graphemes(true).collect();
        self.cursor = word_end(&chars, self.clamped());
    }

    /// Move the caret to the same column on the row above (negative) or
    /// below, using the same wrapped layout the preview draws.
    fn move_row_impl(&mut self, prefix: &str, width: usize, delta: isize) {
        let l = layout(prefix, &self.text, self.cursor, width, None);
        let total = l.positions.len() - 1;
        let (row, col, _) = l.positions[(l.prefix_len + self.clamped()).min(total)];
        let Some(target) = row.checked_add_signed(delta) else {
            return;
        };
        if target >= l.rows.len() {
            return;
        }
        // The grapheme on the target row closest to the current column; a
        // line-break grapheme's caret stays before it, ending the row.
        let mut caret = None;
        for (index, (r, c, w)) in l.positions.iter().enumerate() {
            if *r == target && *c <= col {
                caret = Some(if index >= total {
                    total
                } else if *w > 0 && c + w <= col {
                    index + 1
                } else {
                    index
                });
            }
        }
        if let Some(caret) = caret {
            self.cursor = caret
                .saturating_sub(l.prefix_len)
                .min(graphemes(&self.text));
        }
    }

    pub fn left(&mut self) {
        self.anchor = None;
        self.left_impl();
    }

    pub fn right(&mut self) {
        self.anchor = None;
        self.right_impl();
    }

    pub fn home(&mut self) {
        self.anchor = None;
        self.home_impl();
    }

    pub fn end(&mut self) {
        self.anchor = None;
        self.end_impl();
    }

    pub fn word_left(&mut self) {
        self.anchor = None;
        self.word_left_impl();
    }

    pub fn word_right(&mut self) {
        self.anchor = None;
        self.word_right_impl();
    }

    pub fn move_row(&mut self, prefix: &str, width: usize, delta: isize) {
        self.anchor = None;
        self.move_row_impl(prefix, width, delta);
    }

    pub fn select_left(&mut self) {
        self.anchor_here();
        self.left_impl();
        self.settle_anchor();
    }

    pub fn select_right(&mut self) {
        self.anchor_here();
        self.right_impl();
        self.settle_anchor();
    }

    pub fn select_home(&mut self) {
        self.anchor_here();
        self.home_impl();
        self.settle_anchor();
    }

    pub fn select_end(&mut self) {
        self.anchor_here();
        self.end_impl();
        self.settle_anchor();
    }

    pub fn select_word_left(&mut self) {
        self.anchor_here();
        self.word_left_impl();
        self.settle_anchor();
    }

    pub fn select_word_right(&mut self) {
        self.anchor_here();
        self.word_right_impl();
        self.settle_anchor();
    }

    /// Kill from the caret to the end of the line (the newline itself when
    /// the rest of the line is empty), for Ctrl+K.
    pub fn kill_line(&mut self) -> Option<String> {
        if let Some(killed) = self.delete_selection() {
            return Some(killed);
        }
        let bytes = byte_index(&self.text, self.clamped());
        let end = match self.text[bytes..].find('\n') {
            Some(0) => bytes + 1,
            Some(rel) => bytes + rel,
            None => self.text.len(),
        };
        if end <= bytes {
            return None;
        }
        let killed = self.text[bytes..end].to_string();
        self.snapshot();
        self.text.replace_range(bytes..end, "");
        Some(killed)
    }

    /// Kill the word before the caret, for Ctrl+W.
    pub fn kill_word(&mut self) -> Option<String> {
        if let Some(killed) = self.delete_selection() {
            return Some(killed);
        }
        let chars: Vec<&str> = self.text.graphemes(true).collect();
        let start = word_start(&chars, self.clamped());
        if start == self.cursor {
            return None;
        }
        let bytes = byte_index(&self.text, start)..byte_index(&self.text, self.cursor);
        let killed = self.text[bytes.clone()].to_string();
        self.snapshot();
        self.text.replace_range(bytes, "");
        self.cursor = start;
        Some(killed)
    }

    /// Delete the grapheme at the caret (forward delete).
    pub fn delete(&mut self) {
        if self.delete_selection().is_some() {
            return;
        }
        let count = graphemes(&self.text);
        self.cursor = self.cursor.min(count);
        if self.cursor == count {
            return;
        }
        self.snapshot();
        self.text.replace_range(
            byte_index(&self.text, self.cursor)..byte_index(&self.text, self.cursor + 1),
            "",
        );
    }

    pub fn backspace(&mut self) {
        if self.delete_selection().is_some() {
            return;
        }
        self.cursor = self.cursor.min(graphemes(&self.text));
        if self.cursor == 0 {
            if self.text.is_empty() {
                self.attachments.pop();
            }
            return;
        }
        self.snapshot();
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn clear(&mut self) {
        self.snapshot();
        self.text.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.anchor = None;
    }
}

/// Recently killed text, newest first; Ctrl+Y yanks the newest.
#[derive(Default)]
pub(super) struct KillRing {
    entries: VecDeque<String>,
}

impl KillRing {
    pub fn push(&mut self, text: String) {
        if text.is_empty() || self.entries.front().is_some_and(|top| *top == text) {
            return;
        }
        self.entries.push_front(text);
    }

    pub fn top(&self) -> Option<&str> {
        self.entries.front().map(String::as_str)
    }
}

/// Previous prompts of this session, recalled with Up/Down on an empty draft.
#[derive(Default)]
pub(super) struct History {
    entries: Vec<String>,
    /// Current navigation slot while walking; None outside history.
    index: Option<usize>,
    /// Draft text saved when entering history, restored below the newest.
    saved: String,
}

impl History {
    pub fn load(&mut self, prompts: impl IntoIterator<Item = String>) {
        self.entries.clear();
        self.index = None;
        self.saved.clear();
        for prompt in prompts {
            self.remember(prompt);
        }
    }

    pub fn remember(&mut self, prompt: String) {
        self.index = None;
        self.saved.clear();
        let text = prompt.trim();
        if text.is_empty() {
            return;
        }
        let text = text.to_string();
        if self.entries.last().is_some_and(|last| *last == text) {
            return;
        }
        self.entries.push(text);
    }

    /// Navigate with Up (-n) / Down (+n). True when history consumed the
    /// key; entering history requires an empty draft.
    pub fn select(&mut self, draft: &mut Draft, delta: isize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let len = self.entries.len();
        let current = match self.index {
            Some(index) => index,
            None => {
                if delta > 0 || !draft.text.is_empty() {
                    return false;
                }
                self.saved = std::mem::take(&mut draft.text);
                len
            }
        };
        let next = (current as isize + delta).clamp(0, len as isize) as usize;
        if next == current {
            return true;
        }
        self.index = (next < len).then_some(next);
        if next == len {
            draft.text = std::mem::take(&mut self.saved);
        } else {
            draft.text.clone_from(&self.entries[next]);
        }
        draft.cursor = graphemes(&draft.text);
        draft.anchor = None;
        true
    }
}

pub(super) fn attachment_prefix(draft: &Draft) -> String {
    let mut prefix = String::new();
    for (index, attachment) in draft.attachments.iter().enumerate() {
        prefix.push_str(&format!("[{}: {}] ", index + 1, attachment.filename));
    }
    prefix.pop();
    prefix
}

pub(super) struct Layout {
    pub rows: Vec<String>,
    /// Cursor position as (row, terminal column) among `rows`.
    pub cursor: (usize, usize),
    /// (row, column, cells) of every prefix+text grapheme, plus a final
    /// sentinel entry for the caret position after the last grapheme.
    positions: Vec<(usize, usize, usize)>,
    prefix_len: usize,
}

fn highlight(sel: Option<(usize, usize)>, index: usize, shown: String) -> String {
    match sel {
        Some((lo, hi)) if index >= lo && index < hi => format!("\x1b[7m{shown}\x1b[27m"),
        _ => shown,
    }
}

/// Lay the draft out across display rows: a hard newline starts a new row,
/// long rows soft-wrap, the first row carries the prompt and continuation rows
/// indent to align under the text. Tabs show as spaces, the selected grapheme
/// range (text indices) is reverse-video. `cursor` counts prefix graphemes
/// too; usize::MAX means the end.
fn layout(
    prefix: &str,
    text: &str,
    cursor: usize,
    width: usize,
    sel: Option<(usize, usize)>,
) -> Layout {
    // terminal_width() keeps the last column free, so a cell below width never
    // triggers the terminal's own autowrap.
    let capacity = width.saturating_sub(2).max(1);
    let prefix = clean_input(prefix).replace('\n', " ");
    let clean = clean_input(text);
    let prefix_len = graphemes(&prefix);
    let text_len = graphemes(&clean);
    let sel = sel.map(|(lo, hi)| (prefix_len + lo.min(text_len), prefix_len + hi.min(text_len)));
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut col = 0usize;
    let mut positions: Vec<(usize, usize, usize)> = Vec::new();
    let place = |styled: String,
                 w: usize,
                 rows: &mut Vec<String>,
                 row: &mut String,
                 col: &mut usize,
                 positions: &mut Vec<(usize, usize, usize)>| {
        if *col + w > capacity && *col > 0 {
            rows.push(std::mem::take(row));
            *col = 0;
        }
        positions.push((rows.len(), 2 + *col, w));
        row.push_str(&styled);
        *col += w;
    };
    for (index, grapheme) in prefix.graphemes(true).enumerate() {
        let w = grapheme.width();
        place(
            highlight(sel, index, grapheme.to_string()),
            w,
            &mut rows,
            &mut row,
            &mut col,
            &mut positions,
        );
    }
    for (offset, grapheme) in clean.graphemes(true).enumerate() {
        let index = prefix_len + offset;
        if grapheme == "\n" {
            positions.push((rows.len(), 2 + col, 0));
            rows.push(std::mem::take(&mut row));
            col = 0;
            continue;
        }
        let shown = if grapheme == "\t" {
            "    ".to_string()
        } else {
            grapheme.to_string()
        };
        let w = shown.width();
        place(
            highlight(sel, index, shown),
            w,
            &mut rows,
            &mut row,
            &mut col,
            &mut positions,
        );
    }
    positions.push((rows.len(), 2 + col, 0));
    rows.push(row);
    let (crow, ccol, _) = positions[(prefix_len + cursor).min(prefix_len + text_len)];
    let rows = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 0 {
                format!("{BOLD}›{RESET} {row}")
            } else {
                format!("  {row}")
            }
        })
        .collect();
    Layout {
        rows,
        cursor: (crow, ccol),
        positions,
        prefix_len,
    }
}

// Only the unfinished transcript row and the draft are redrawn. Complete
// transcript rows are committed to scrollback and never erased by editing.
// The draft may span several wrapped rows; the real terminal cursor is parked
// on the edit position so typing, arrows and the caret stay in sync.
#[derive(Default)]
pub(super) struct Display {
    tail: String,
    columns: usize,
    style: String,
    drawn_rows: usize,
    /// Row of the drawn block the terminal cursor rests on after `draw`: the
    /// caret's row, which is the block's last row only when the caret is at
    /// the end of the draft.
    caret_row: usize,
}

impl Display {
    pub fn erase(&mut self) {
        if self.drawn_rows == 0 {
            return;
        }
        // Climb from the caret to the block's first row. Climbing `drawn_rows
        // - 1` instead assumes the caret sits on the last row, so with the
        // caret higher it lands above the draft and clears committed
        // transcript rows with it.
        print!("\r");
        if self.caret_row > 0 {
            print!("\x1b[{}A", self.caret_row);
        }
        print!("\x1b[J");
        self.drawn_rows = 0;
        self.caret_row = 0;
    }

    pub fn finish_line(&mut self) {
        if self.columns > 0 {
            crate::println!("{}{RESET}", self.tail);
        }
        self.tail.clear();
        self.style.clear();
        self.columns = 0;
    }

    pub fn output(&mut self, text: &str, width: usize) {
        // Track SGR sequences as zero-width tokens. Replay the current style
        // when wrapping a streaming delta across committed transcript rows.
        let mut rest = text;
        while !rest.is_empty() {
            if rest.starts_with("\x1b[") {
                if let Some(end) = rest[2..].find(|ch: char| ('@'..='~').contains(&ch)) {
                    let end = end + 3;
                    let escape = &rest[..end];
                    if escape.ends_with('m') {
                        self.tail.push_str(escape);
                        if escape == RESET {
                            self.style.clear();
                        } else {
                            self.style.push_str(escape);
                        }
                    }
                    rest = &rest[end..];
                    continue;
                }
            }
            let grapheme = rest.graphemes(true).next().unwrap();
            rest = &rest[grapheme.len()..];
            if grapheme.contains('\n') {
                self.commit_row();
            } else if grapheme == "\t" {
                let spaces = 4 - self.columns % 4;
                self.output(&" ".repeat(spaces), width);
            } else if !grapheme.chars().any(char::is_control) {
                let cells = grapheme.width();
                if self.columns + cells > width && self.columns > 0 {
                    self.commit_row();
                }
                self.tail.push_str(grapheme);
                self.columns += cells;
            }
        }
    }

    fn commit_row(&mut self) {
        crate::println!("{}{RESET}", self.tail);
        self.tail.clone_from(&self.style);
        self.columns = 0;
    }

    pub fn draw(&mut self, ui: &Ui, draft: &Draft, width: usize) {
        let (prefix, text, cursor, sel) = match &ui.prompt {
            Some(PendingPrompt::Question {
                questions,
                current,
                line,
                ..
            }) => {
                // The input row shows which question the typed answer belongs to.
                let position = if questions.is_empty() {
                    String::new()
                } else {
                    format!(
                        "[{}/{}] ",
                        (*current).min(questions.len() - 1) + 1,
                        questions.len()
                    )
                };
                (position, line.clone(), usize::MAX, None)
            }
            Some(PendingPrompt::Permission(_)) => (
                String::new(),
                "[o]nce [a]lways [r]eject".into(),
                usize::MAX,
                None,
            ),
            None => (
                attachment_prefix(draft),
                draft.text.clone(),
                draft.cursor,
                draft.selection(),
            ),
        };
        let layout = layout(&prefix, &text, cursor, width, sel);
        self.drawn_rows = 0;
        print!("\r");
        if self.columns > 0 {
            crate::println!("{}{RESET}", self.tail);
            self.drawn_rows += 1;
        }
        // A flushed transcript row sits above the draft, so the block starts
        // one row lower and the caret moves with it.
        let first = self.drawn_rows;
        for (index, row) in layout.rows.iter().enumerate() {
            if index > 0 {
                crate::print!("\r\n");
            }
            crate::print!("{row}");
            self.drawn_rows += 1;
        }
        self.caret_row = first + layout.cursor.0;
        let up = self.drawn_rows - 1 - self.caret_row;
        if up > 0 {
            print!("\x1b[{up}A");
        }
        print!("\r");
        if layout.cursor.1 > 0 {
            print!("\x1b[{}C", layout.cursor.1);
        }
        let _ = std::io::stdout().flush();
    }
}

pub(super) fn terminal_width() -> usize {
    // Leave the last cell unused so the terminal never has a pending autowrap.
    crossterm::terminal::size()
        .map(|(width, _)| usize::from(width).saturating_sub(1).max(3))
        .unwrap_or(79)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_stops_at_draft_boundary_and_removes_whole_graphemes() {
        let mut draft = Draft::default();
        draft.insert_str("a\ne\u{301}👩‍💻中");
        for expected in ["a\ne\u{301}👩‍💻", "a\ne\u{301}", "a\n", "a", "", ""] {
            draft.backspace();
            assert_eq!(draft.text, expected);
        }
    }

    #[test]
    fn cursor_keys_edit_the_middle_of_the_draft() {
        let mut draft = Draft::default();
        draft.insert_str("abd");
        draft.left();
        draft.insert('c');
        assert_eq!(draft.text, "abcd");
        assert_eq!(draft.cursor, 3);
        draft.home();
        draft.right();
        draft.backspace();
        assert_eq!(draft.text, "bcd");
        assert_eq!(draft.cursor, 0);
        draft.end();
        draft.right();
        draft.backspace();
        assert_eq!(draft.text, "bc");
        // Moving and deleting stay on grapheme boundaries with wide characters.
        draft.insert_str("👩‍💻中");
        draft.left();
        draft.backspace();
        assert_eq!(draft.text, "bc中");
        draft.right();
        draft.backspace();
        assert_eq!(draft.text, "bc");
        draft.clear();
        assert_eq!(draft.cursor, 0);
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut draft = Draft::default();
        draft.insert_str("ab");
        draft.home();
        draft.insert_str("1\n2");
        assert_eq!(draft.text, "1\n2ab");
        assert_eq!(draft.cursor, 3);
    }

    #[test]
    fn home_end_and_word_motion_work_line_by_line() {
        // Index: 0..8 = "let x = 1", 9 = newline, 10..22 = "let y = foo()".
        let mut draft = Draft::default();
        draft.insert_str("let x = 1\nlet y = foo()");
        assert_eq!(draft.cursor, 23);
        draft.home();
        assert_eq!(draft.cursor, 10); // start of the second line
        draft.home();
        assert_eq!(draft.cursor, 10); // Home is per line, so it stays
        draft.end();
        assert_eq!(draft.cursor, 23); // end of the second line
        draft.cursor = 0;
        draft.end();
        assert_eq!(draft.cursor, 9); // per-line End: first line's end
        draft.end();
        assert_eq!(draft.cursor, 9);
        // Word motion across the whole draft.
        draft.cursor = 0;
        draft.word_right();
        assert_eq!(draft.cursor, 3); // over "let"
        draft.word_right();
        draft.word_right();
        draft.word_right();
        assert_eq!(draft.cursor, 13); // past the newline, over "let"
        draft.word_right();
        assert_eq!(draft.cursor, 15); // over "y"
        draft.word_right();
        assert_eq!(draft.cursor, 21); // over "foo"
        draft.word_right();
        assert_eq!(draft.cursor, 23); // trailing "( )" is one non-word run
        draft.word_left();
        assert_eq!(draft.cursor, 18); // start of "foo"
                                      // Forward delete removes at the caret, backspace behind it.
        draft.home();
        assert_eq!(draft.cursor, 10); // second line start
        draft.delete();
        assert_eq!(draft.text, "let x = 1\net y = foo()");
        draft.delete();
        draft.delete();
        assert_eq!(draft.text, "let x = 1\n y = foo()");
        draft.backspace();
        assert_eq!(draft.text, "let x = 1 y = foo()"); // caret at 10 deletes the newline
        assert_eq!(draft.cursor, 9);
    }

    #[test]
    fn arrow_rows_move_the_caret_across_wraps_and_newlines() {
        // Wrapped at capacity 4: rows "abcd" / "efgh" / "ij".
        let mut draft = Draft::default();
        draft.insert_str("abcdefghij");
        draft.move_row("", 6, -1);
        assert_eq!(draft.cursor, 6); // column 4 on the middle row
        draft.move_row("", 6, -1);
        assert_eq!(draft.cursor, 2);
        draft.move_row("", 6, 1);
        assert_eq!(draft.cursor, 6);
        // Hard newlines: caret ends the line above, never past its break.
        let mut draft = Draft::default();
        draft.insert_str("ab\ncd");
        draft.move_row("", 40, -1);
        assert_eq!(draft.cursor, 2); // end of "ab", before the newline
        draft.insert('\n');
        assert_eq!(draft.text, "ab\n\ncd");
        assert_eq!(draft.cursor, 3);
        draft.home();
        assert_eq!(draft.cursor, 3); // the middle empty line starts at 3
    }

    #[test]
    fn undo_steps_back_over_edits() {
        let mut draft = Draft::default();
        draft.insert_str("hello");
        draft.backspace();
        draft.insert('p');
        draft.undo();
        assert_eq!(draft.text, "hell");
        draft.undo();
        assert_eq!(draft.text, "hello");
        assert_eq!(draft.cursor, 5);
        draft.undo();
        assert_eq!(draft.text, "");
        draft.undo(); // empty stack: no-op
        assert_eq!(draft.text, "");
    }

    #[test]
    fn selection_is_replaced_and_deleted_as_a_unit() {
        let mut draft = Draft::default();
        draft.insert_str("remove this keep");
        draft.home();
        draft.select_word_right();
        assert_eq!(draft.selection(), Some((0, 6)));
        draft.insert('X'); // typing replaces the selection
        assert_eq!(draft.text, "X this keep");
        assert_eq!(draft.cursor, 1);
        draft.undo(); // replacing a selection is ONE undo step
        assert_eq!(draft.text, "remove this keep");
        assert_eq!(draft.cursor, 6); // caret returns to where typing replaced it
        draft.home();
        draft.select_word_right();
        draft.insert('X');
        draft.select_end();
        assert_eq!(draft.selection(), Some((1, 11)));
        draft.backspace(); // backspace deletes the selection
        assert_eq!(draft.text, "X");
        assert!(draft.selection().is_none());
        // Plain motion drops the selection that Shift starts.
        draft.select_left();
        assert_eq!(draft.selection(), Some((0, 1)));
        draft.left();
        assert!(draft.selection().is_none());
    }

    #[test]
    fn kill_ring_collects_and_yanks() {
        let mut ring = KillRing::default();
        let mut draft = Draft::default();
        draft.insert_str("keep\nchop me");
        draft.cursor = 5; // start of "chop me"
        ring.push(draft.kill_line().unwrap()); // kills to end of line
        assert_eq!(draft.text, "keep\n");
        assert_eq!(ring.top(), Some("chop me"));
        draft.cursor = 0;
        draft.insert_str("yanked "); // Ctrl+Y yank, at the caret
        assert_eq!(draft.text, "yanked keep\n");
        // Ctrl+W kills the word behind the caret (cursor 11 = after "keep").
        draft.cursor = 11;
        ring.push(draft.kill_word().unwrap());
        assert_eq!(ring.top(), Some("keep"));
        assert_eq!(draft.text, "yanked \n");
        // Ctrl+K at a line end kills the newline itself.
        draft.cursor = 7;
        ring.push(draft.kill_line().unwrap());
        assert_eq!(draft.text, "yanked ");
        assert_eq!(ring.top(), Some("\n"));
        // Nothing left to kill: the ring keeps its three entries.
        draft.end();
        assert!(draft.kill_line().is_none());
        assert_eq!(ring.entries.len(), 3);
    }

    #[test]
    fn prompt_history_walks_and_restores_the_draft() {
        let mut history = History::default();
        history.load(["first".into(), "second".into()]);
        let mut draft = Draft::default();
        assert!(history.select(&mut draft, -1));
        assert_eq!(draft.text, "second");
        assert!(history.select(&mut draft, -1));
        assert_eq!(draft.text, "first");
        assert!(history.select(&mut draft, -1)); // top edge: consumed, no change
        assert_eq!(draft.text, "first");
        assert!(history.select(&mut draft, 1));
        assert_eq!(draft.text, "second");
        assert!(history.select(&mut draft, 1)); // below newest: restore saved
        assert_eq!(draft.text, "");
        assert!(!history.select(&mut draft, 1)); // outside: Down moves rows
        draft.insert_str("typing");
        assert!(!history.select(&mut draft, -1)); // non-empty draft: Up moves rows
        history.remember("typing".into());
        history.remember("typing".into()); // duplicate of the newest: skipped
        history.remember(" ".into()); // whitespace-only: skipped
        assert_eq!(history.entries, ["first", "second", "typing"]);
    }

    #[test]
    fn attachments_are_removed_without_deleting_source_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shot.png");
        std::fs::write(&path, super::super::tests::PNG).unwrap();
        let mut draft = Draft {
            text: "x".into(),
            attachments: vec![super::super::file_attachment(&path).unwrap()],
            cursor: 1,
            ..Draft::default()
        };
        draft.backspace();
        assert_eq!(draft.attachments.len(), 1);
        draft.backspace();
        assert!(draft.attachments.is_empty());
        draft
            .attachments
            .push(super::super::file_attachment(&path).unwrap());
        draft.clear();
        assert!(draft.attachments.is_empty());
        assert!(path.exists());
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.replace(BOLD, "")
                    .replace(RESET, "")
                    .replace("\x1b[7m", "")
                    .replace("\x1b[27m", "")
            })
            .collect()
    }

    #[test]
    fn draft_wraps_across_rows_without_terminal_controls() {
        assert_eq!(clean_input("a\x1b[2J\x08\r\nb"), "a[2J\nb");
        let rows = plain(&layout("", "long\n中e\u{301}👩‍💻", usize::MAX, 10, None).rows);
        assert!(rows.iter().all(|row| row.width() <= 10));
        assert!(rows.iter().all(|row| !row.contains('\n')));
        assert!(rows.last().unwrap().ends_with("👩‍💻"));
        // Continuation rows indent under the text.
        let wide = layout("", "abcdefghij", 5, 6, None);
        assert_eq!(plain(&wide.rows), ["› abcd", "  efgh", "  ij"]);
        assert_eq!(wide.cursor, (1, 3));
        // An empty draft keeps one prompt row with the caret behind the prompt.
        let empty = layout("", "", 0, 10, None);
        assert_eq!(plain(&empty.rows), ["› "]);
        assert_eq!(empty.cursor, (0, 2));
        // The cursor at the very end maps to the last cell of the last row.
        let end = layout("", "abcd", usize::MAX, 6, None);
        assert_eq!(end.cursor, (0, 6));
        // Hard newlines start real display rows.
        assert_eq!(plain(&layout("", "a\nb", 0, 40, None).rows), ["› a", "  b"]);
        // Wide characters wrap on cell widths, not grapheme counts.
        let rows = plain(&layout("", "中中中", 0, 5, None).rows);
        assert_eq!(rows, ["› 中", "  中", "  中"]);
    }

    #[test]
    fn selected_graphemes_draw_inverted_and_keep_plain_text_intact() {
        let rows = layout("", "abc", 0, 20, Some((1, 3))).rows;
        assert!(rows[0].contains("\x1b[7mb\x1b[27m"));
        assert!(rows[0].contains("\x1b[7mc\x1b[27m"));
        assert!(!rows[0].contains("\x1b[7ma"));
        assert_eq!(plain(&rows), ["› abc"]);
        // A selection across rows highlights on each row.
        let rows = layout("", "ab\ncd", 0, 20, Some((1, 4))).rows;
        assert!(rows[0].contains("\x1b[7mb\x1b[27m"));
        assert!(rows[1].contains("\x1b[7mc\x1b[27m"));
    }

    #[test]
    fn streaming_wraps_keep_only_one_uncommitted_transcript_row() {
        let mut display = Display::default();
        display.output("\x1b[2m中中ab", 4);
        assert_eq!(display.columns, 2);
        assert_eq!(display.tail, "\x1b[2mab");
        display.output("c\x1b[0m", 5);
        assert_eq!(display.columns, 3);
        assert!(display.style.is_empty());
    }

    #[test]
    fn erasing_the_draft_starts_at_the_caret_row() {
        let ui = Ui {
            part_kind: std::collections::HashMap::new(),
            headered: std::collections::HashSet::new(),
            tool_done: std::collections::HashSet::new(),
            prompt: None,
        };
        let mut draft = Draft::default();
        draft.insert_str("alpha\nbravo\ncharlie");
        let mut display = Display::default();
        // Caret at the end: the terminal cursor rests on the block's last row.
        display.draw(&ui, &draft, 40);
        assert_eq!((display.drawn_rows, display.caret_row), (3, 2));
        // Caret on the first row: the erase climbs to the draft's first row,
        // never above it, so committed transcript rows stay intact.
        draft.cursor = 0;
        display.draw(&ui, &draft, 40);
        assert_eq!((display.drawn_rows, display.caret_row), (3, 0));
        // A transcript row flushed above the draft shifts the block down.
        display.output("streaming", 40);
        display.draw(&ui, &draft, 40);
        assert_eq!((display.drawn_rows, display.caret_row), (4, 1));
        display.erase();
        assert_eq!((display.drawn_rows, display.caret_row), (0, 0));
    }
}
