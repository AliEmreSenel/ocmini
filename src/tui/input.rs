use super::{Attachment, PendingPrompt, Ui, BOLD, RESET};
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Edit position: a grapheme index into `text`, always in `0..=count`.
    pub cursor: usize,
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

impl Draft {
    pub fn insert(&mut self, ch: char) {
        self.text.insert(byte_index(&self.text, self.cursor), ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        self.text
            .insert_str(byte_index(&self.text, self.cursor), text);
        self.cursor += graphemes(text);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(graphemes(&self.text));
    }

    fn clamped(&self) -> usize {
        self.cursor.min(graphemes(&self.text))
    }

    fn word_chars(&self) -> Vec<&str> {
        self.text.graphemes(true).collect()
    }

    /// Start of the current line; 0 for the first line.
    pub fn home(&mut self) {
        let bytes = byte_index(&self.text, self.clamped());
        let start = self.text[..bytes].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = graphemes(&self.text[..start]);
    }

    /// End of the current line; the text end for the last line.
    pub fn end(&mut self) {
        let bytes = byte_index(&self.text, self.clamped());
        let end = self.text[bytes..]
            .find('\n')
            .map_or(self.text.len(), |i| bytes + i);
        self.cursor = graphemes(&self.text[..end]);
    }

    pub fn word_left(&mut self) {
        let chars = self.word_chars();
        let mut i = self.clamped();
        while i > 0 && !word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && word_char(chars[i - 1]) {
            i -= 1;
        }
        self.cursor = i;
    }

    pub fn word_right(&mut self) {
        let chars = self.word_chars();
        let mut i = self.clamped();
        while i < chars.len() && !word_char(chars[i]) {
            i += 1;
        }
        while i < chars.len() && word_char(chars[i]) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Move the caret to the same column on the row above (negative) or
    /// below, using the same wrapped layout the preview draws.
    pub fn move_row(&mut self, prefix: &str, width: usize, delta: isize) {
        let l = layout(prefix, &self.text, self.cursor, width);
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

    /// Delete the grapheme at the caret (forward delete).
    pub fn delete(&mut self) {
        let count = graphemes(&self.text);
        self.cursor = self.cursor.min(count);
        if self.cursor == count {
            return;
        }
        self.text.replace_range(
            byte_index(&self.text, self.cursor)..byte_index(&self.text, self.cursor + 1),
            "",
        );
    }

    pub fn backspace(&mut self) {
        self.cursor = self.cursor.min(graphemes(&self.text));
        if self.cursor == 0 {
            if self.text.is_empty() {
                self.attachments.pop();
            }
            return;
        }
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.attachments.clear();
        self.cursor = 0;
    }
}

fn word_char(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

pub(super) fn attachment_prefix(draft: &Draft) -> String {
    draft
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| format!("[{}: {}] ", index + 1, attachment.filename))
        .collect()
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

/// Lay the draft out across display rows: a hard newline starts a new row,
/// long rows soft-wrap, the first row carries the prompt and continuation rows
/// indent to align under the text. Tabs show as spaces. `cursor` counts prefix
/// graphemes too; usize::MAX means the end.
fn layout(prefix: &str, text: &str, cursor: usize, width: usize) -> Layout {
    // terminal_width() keeps the last column free, so a cell below width never
    // triggers the terminal's own autowrap.
    let capacity = width.saturating_sub(2).max(1);
    let prefix = clean_input(prefix).replace('\n', " ");
    let clean = clean_input(text);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut col = 0usize;
    let mut positions: Vec<(usize, usize, usize)> = Vec::new();
    let place = |shown: String,
                 rows: &mut Vec<String>,
                 row: &mut String,
                 col: &mut usize,
                 positions: &mut Vec<(usize, usize, usize)>| {
        let w = shown.width();
        if *col + w > capacity && *col > 0 {
            rows.push(std::mem::take(row));
            *col = 0;
        }
        positions.push((rows.len(), 2 + *col, w));
        row.push_str(&shown);
        *col += w;
    };
    for grapheme in prefix.graphemes(true) {
        place(
            grapheme.to_string(),
            &mut rows,
            &mut row,
            &mut col,
            &mut positions,
        );
    }
    for grapheme in clean.graphemes(true) {
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
        place(shown, &mut rows, &mut row, &mut col, &mut positions);
    }
    positions.push((rows.len(), 2 + col, 0));
    rows.push(row);
    let prefix_len = graphemes(&prefix);
    let (crow, ccol, _) = positions[(prefix_len + cursor).min(prefix_len + graphemes(&clean))];
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
}

impl Display {
    pub fn erase(&mut self) {
        if self.drawn_rows == 0 {
            return;
        }
        print!("\r");
        if self.drawn_rows > 1 {
            print!("\x1b[{}A", self.drawn_rows - 1);
        }
        print!("\x1b[J");
        self.drawn_rows = 0;
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
        let (prefix, text, cursor) = match &ui.prompt {
            Some(PendingPrompt::Question { line, .. }) => (String::new(), line.clone(), usize::MAX),
            Some(PendingPrompt::Permission(_)) => {
                (String::new(), "[o]nce [a]lways [r]eject".into(), usize::MAX)
            }
            None => (attachment_prefix(draft), draft.text.clone(), draft.cursor),
        };
        let layout = layout(&prefix, &text, cursor, width);
        self.drawn_rows = 0;
        print!("\r");
        if self.columns > 0 {
            crate::println!("{}{RESET}", self.tail);
            self.drawn_rows += 1;
        }
        for (index, row) in layout.rows.iter().enumerate() {
            if index > 0 {
                crate::print!("\r\n");
            }
            crate::print!("{row}");
            self.drawn_rows += 1;
        }
        let up = self.drawn_rows - 1 - layout.cursor.0;
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
    fn attachments_are_removed_without_deleting_source_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shot.png");
        std::fs::write(&path, super::super::tests::PNG).unwrap();
        let mut draft = Draft {
            text: "x".into(),
            attachments: vec![super::super::file_attachment(&path).unwrap()],
            cursor: 1,
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
            .map(|row| row.replace(BOLD, "").replace(RESET, ""))
            .collect()
    }

    #[test]
    fn draft_wraps_across_rows_without_terminal_controls() {
        assert_eq!(clean_input("a\x1b[2J\x08\r\nb"), "a[2J\nb");
        let rows = plain(&layout("", "long\n中e\u{301}👩‍💻", usize::MAX, 10).rows);
        assert!(rows.iter().all(|row| row.width() <= 10));
        assert!(rows.iter().all(|row| !row.contains('\n')));
        assert!(rows.last().unwrap().ends_with("👩‍💻"));
        // Continuation rows indent under the text.
        let wide = layout("", "abcdefghij", 5, 6);
        assert_eq!(plain(&wide.rows), ["› abcd", "  efgh", "  ij"]);
        assert_eq!(wide.cursor, (1, 3));
        // An empty draft keeps one prompt row with the caret behind the prompt.
        let empty = layout("", "", 0, 10);
        assert_eq!(plain(&empty.rows), ["› "]);
        assert_eq!(empty.cursor, (0, 2));
        // The cursor at the very end maps to the last cell of the last row.
        let end = layout("", "abcd", usize::MAX, 6);
        assert_eq!(end.cursor, (0, 6));
        // Hard newlines start real display rows.
        assert_eq!(plain(&layout("", "a\nb", 0, 40).rows), ["› a", "  b"]);
        // Wide characters wrap on cell widths, not grapheme counts.
        let rows = plain(&layout("", "中中中", 0, 5).rows);
        assert_eq!(rows, ["› 中", "  中", "  中"]);
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
}
