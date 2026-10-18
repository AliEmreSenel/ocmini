use super::{Attachment, PendingPrompt, Ui, BOLD, RESET};
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    pub attachments: Vec<Attachment>,
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

impl Draft {
    pub fn backspace(&mut self) {
        if !pop_grapheme(&mut self.text) {
            self.attachments.pop();
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.attachments.clear();
    }
}

// Keep the editable draft on one physical row. Multiline pastes stay multiline
// in the request; the terminal preview uses ↵ and scrolls horizontally. This
// makes deletion independent of wrapping, wide characters and terminal resize.
fn input_row(text: &str, width: usize) -> String {
    let text = clean_input(text).replace('\n', "↵").replace('\t', "    ");
    let available = width.saturating_sub(2);
    if text.width() <= available {
        return format!("{BOLD}›{RESET} {text}");
    }
    let mut used = 1; // ellipsis
    let suffix: Vec<_> = text
        .graphemes(true)
        .rev()
        .take_while(|g| {
            used += g.width();
            used <= available
        })
        .collect();
    format!(
        "{BOLD}›{RESET} …{}",
        suffix.into_iter().rev().collect::<String>()
    )
}

// Only the unfinished transcript row and the draft are redrawn. Complete
// transcript rows are committed to scrollback and never erased by editing.
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
        self.drawn_rows = 1;
        if self.columns > 0 {
            crate::println!("{}{RESET}", self.tail);
            self.drawn_rows += 1;
        }
        let text = match &ui.prompt {
            Some(PendingPrompt::Question { line, .. }) => line.clone(),
            Some(PendingPrompt::Permission(_)) => "[o]nce [a]lways [r]eject".into(),
            None => {
                let mut text = String::new();
                for (index, attachment) in draft.attachments.iter().enumerate() {
                    text.push_str(&format!("[{}: {}] ", index + 1, attachment.filename));
                }
                text.push_str(&draft.text);
                text
            }
        };
        print!("{RESET}{}", input_row(&text, width));
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
        let mut draft = Draft {
            text: "a\ne\u{301}👩‍💻中".into(),
            attachments: vec![],
        };
        for expected in ["a\ne\u{301}👩‍💻", "a\ne\u{301}", "a\n", "a", "", ""] {
            draft.backspace();
            assert_eq!(draft.text, expected);
        }
    }

    #[test]
    fn attachments_are_removed_without_deleting_source_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shot.png");
        std::fs::write(&path, super::super::tests::PNG).unwrap();
        let mut draft = Draft {
            text: "x".into(),
            attachments: vec![super::super::file_attachment(&path).unwrap()],
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

    #[test]
    fn draft_preview_is_one_bounded_row_without_terminal_controls() {
        assert_eq!(clean_input("a\x1b[2J\x08\r\nb"), "a[2J\nb");
        let row = input_row("long\n中e\u{301}👩‍💻", 10);
        let plain = row.replace(BOLD, "").replace(RESET, "");
        assert!(plain.width() <= 10);
        assert!(!plain.contains('\n'));
        assert!(plain.ends_with("中e\u{301}👩‍💻"));
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
