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

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = graphemes(&self.text);
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

pub(super) struct Layout {
    pub rows: Vec<String>,
    /// Cursor position as (row, terminal column) among `rows`.
    pub cursor: (usize, usize),
}

/// Lay the draft out across wrapped rows: the first row carries the prompt and
/// continuation rows indent to align under the text. Control characters show as
/// cells (↵, spaces), so the cursor grapheme maps to a cell even when the text
/// is multiline. `cursor` counts prefix graphemes too; usize::MAX means the end.
fn layout(prefix: &str, text: &str, cursor: usize, width: usize) -> Layout {
    // terminal_width() keeps the last column free, so a cell below width never
    // triggers the terminal's own autowrap.
    let capacity = width.saturating_sub(2).max(1);
    let prefix = clean_input(prefix);
    let clean = clean_input(text);
    let prefix_len = graphemes(&prefix);
    let target = (prefix_len + cursor).min(prefix_len + graphemes(&clean));
    let mut cells: Vec<(String, usize)> = Vec::new();
    let mut cursor_cell = 0;
    let mut used = 0;
    for (index, grapheme) in prefix
        .graphemes(true)
        .chain(clean.graphemes(true))
        .enumerate()
    {
        if index == target {
            cursor_cell = used;
        }
        let shown = match grapheme {
            "\n" => "↵".to_string(),
            "\t" => "    ".to_string(),
            _ => grapheme.to_string(),
        };
        let w = shown.width();
        used += w;
        cells.push((shown, w));
    }
    if target >= cells.len() {
        cursor_cell = used;
    }

    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let (mut col, mut cell) = (0usize, 0usize);
    let mut cursor_pos = (0usize, 2usize);
    let mut placed = false;
    for (shown, w) in cells {
        if !placed && cell == cursor_cell {
            cursor_pos = (rows.len(), 2 + col);
            placed = true;
        }
        if col + w > capacity && col > 0 {
            rows.push(std::mem::take(&mut row));
            col = 0;
        }
        row.push_str(&shown);
        col += w;
        cell += w;
    }
    rows.push(row);
    if !placed {
        cursor_pos = (rows.len() - 1, 2 + col);
    }
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
        cursor: cursor_pos,
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
            None => {
                let mut prefix = String::new();
                for (index, attachment) in draft.attachments.iter().enumerate() {
                    prefix.push_str(&format!("[{}: {}] ", index + 1, attachment.filename));
                }
                (prefix, draft.text.clone(), draft.cursor)
            }
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
        // Multiline pastes show ↵ so rows stay one terminal row each.
        assert_eq!(plain(&layout("", "a\nb", 0, 40).rows), ["› a↵b"]);
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
