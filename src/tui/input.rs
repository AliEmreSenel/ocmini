use super::{Attachment, PendingPrompt, RowKind, Ui, Visibility, BOLD, RESET};
use std::cell::{Ref, RefCell};
use std::collections::VecDeque;
use std::io::Write;
use std::ops::Range;
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
    /// Reverse edits retain deleted text, never a copy of the entire draft.
    undo: VecDeque<UndoEdit>,
    undo_bytes: usize,
    typing: bool,
    /// Invalidated by externally assigned cursor positions or history recall.
    position: Option<(usize, usize)>,
}

const MAX_UNDO_BYTES: usize = 1024 * 1024;
const MAX_UNDO_EDITS: usize = 1000;

struct UndoEdit {
    start: usize,
    inserted: usize,
    removed: String,
    cursor: usize,
}

impl UndoEdit {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.removed.capacity()
    }
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
        self.insert_text(ch.encode_utf8(&mut [0; 4]), true);
    }

    pub fn insert_str(&mut self, text: &str) {
        self.insert_text(text, false);
    }

    fn offset(&self) -> usize {
        self.position
            .filter(|(cursor, _)| *cursor == self.cursor)
            .map_or_else(|| byte_index(&self.text, self.cursor), |(_, offset)| offset)
    }

    fn insert_text(&mut self, text: &str, typing: bool) {
        let (lo, hi) = self.selection().unwrap_or((self.cursor, self.cursor));
        let start = if lo == self.cursor {
            self.offset()
        } else {
            byte_index(&self.text, lo)
        };
        let end = if hi == self.cursor {
            self.offset()
        } else {
            byte_index(&self.text, hi)
        };
        let append_ascii =
            start == self.text.len() && text.is_ascii() && !self.text.ends_with('\r');
        self.replace(start..end, text, typing && lo == hi);
        self.cursor = if append_ascii {
            lo + text.len()
        } else {
            self.text
                .grapheme_indices(true)
                .take_while(|(index, _)| *index < start + text.len())
                .count()
        };
        let offset = if append_ascii {
            start + text.len()
        } else {
            byte_index(&self.text, self.cursor)
        };
        self.position = Some((self.cursor, offset));
    }

    fn replace(&mut self, range: Range<usize>, text: &str, typing: bool) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        let edit = UndoEdit {
            start: range.start,
            inserted: text.len(),
            removed: self.text[range.clone()].to_owned(),
            cursor: self.cursor,
        };
        let coalesce = typing
            && self.typing
            && edit.removed.is_empty()
            && self.undo.back().is_some_and(|last| {
                last.removed.is_empty() && last.start + last.inserted == edit.start
            });
        if coalesce {
            self.undo.back_mut().unwrap().inserted += edit.inserted;
        } else if edit.bytes() > MAX_UNDO_BYTES {
            // An oversized deletion cannot fit. Do not leave older records
            // whose byte ranges refer to text this edit has replaced.
            self.reset_undo();
        } else {
            while self.undo_bytes + edit.bytes() > MAX_UNDO_BYTES
                || self.undo.len() >= MAX_UNDO_EDITS
            {
                self.undo_bytes -= self.undo.pop_front().unwrap().bytes();
            }
            self.undo_bytes += edit.bytes();
            self.undo.push_back(edit);
        }
        self.text.replace_range(range, text);
        self.anchor = None;
        self.position = None;
        self.typing = typing;
    }

    fn reset_undo(&mut self) {
        self.undo.clear();
        self.undo_bytes = 0;
        self.typing = false;
        self.position = None;
    }

    /// Submitting starts a fresh editing lifetime, including undo/selection.
    pub fn take(&mut self) -> (String, Vec<Attachment>) {
        let Self {
            text, attachments, ..
        } = std::mem::take(self);
        (text, attachments)
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (lo, hi) = (anchor.min(self.cursor), anchor.max(self.cursor));
        (lo < hi).then_some((lo, hi))
    }

    fn anchor_here(&mut self) {
        self.typing = false;
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
        let range = byte_index(&self.text, lo)..byte_index(&self.text, hi);
        let text = self.text[range.clone()].to_owned();
        self.replace(range, "", false);
        self.cursor = lo;
        Some(text)
    }

    pub fn undo(&mut self) {
        if let Some(edit) = self.undo.pop_back() {
            self.undo_bytes -= edit.bytes();
            self.text
                .replace_range(edit.start..edit.start + edit.inserted, &edit.removed);
            self.cursor = edit.cursor.min(graphemes(&self.text));
            self.anchor = None;
            self.position = None;
        }
        self.typing = false;
    }

    fn clamped(&self) -> usize {
        self.cursor.min(graphemes(&self.text))
    }

    fn left_impl(&mut self) {
        self.typing = false;
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right_impl(&mut self) {
        self.typing = false;
        self.cursor = (self.cursor + 1).min(graphemes(&self.text));
    }

    /// Start of the current line; 0 for the first line.
    fn home_impl(&mut self) {
        self.typing = false;
        let bytes = byte_index(&self.text, self.clamped());
        let start = self.text[..bytes].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = graphemes(&self.text[..start]);
    }

    /// End of the current line; the text end for the last line.
    fn end_impl(&mut self) {
        self.typing = false;
        let bytes = byte_index(&self.text, self.clamped());
        let end = self.text[bytes..]
            .find('\n')
            .map_or(self.text.len(), |i| bytes + i);
        self.cursor = graphemes(&self.text[..end]);
    }

    fn word_left_impl(&mut self) {
        self.typing = false;
        let chars: Vec<&str> = self.text.graphemes(true).collect();
        self.cursor = word_start(&chars, self.clamped());
    }

    fn word_right_impl(&mut self) {
        self.typing = false;
        let chars: Vec<&str> = self.text.graphemes(true).collect();
        self.cursor = word_end(&chars, self.clamped());
    }

    /// Move the caret to the same column on the row above (negative) or
    /// below, using the same wrapped layout the preview draws.
    fn move_row_impl(&mut self, prefix: &str, width: usize, delta: isize) {
        self.typing = false;
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
        self.replace(bytes..end, "", false);
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
        self.replace(bytes, "", false);
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
        self.replace(
            byte_index(&self.text, self.cursor)..byte_index(&self.text, self.cursor + 1),
            "",
            false,
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
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.replace(start..end, "", false);
        self.cursor -= 1;
    }

    pub fn clear_text(&mut self) {
        self.replace(0..self.text.len(), "", false);
        self.cursor = 0;
        self.anchor = None;
    }

    pub fn clear(&mut self) {
        self.clear_text();
        self.attachments.clear();
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
        draft.reset_undo();
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

/// The text drawn in front of the active draft: a pending question shows its
/// position, the normal prompt shows its attachment tags. Drawing and row
/// math (`Draft::move_row`) must agree on it, so both call this.
pub(super) fn input_prefix(ui: &Ui, draft: &Draft) -> String {
    match &ui.prompt {
        Some(PendingPrompt::Question {
            questions, current, ..
        }) => {
            if questions.is_empty() {
                String::new()
            } else {
                format!(
                    "[{}/{}] ",
                    (*current).min(questions.len() - 1) + 1,
                    questions.len()
                )
            }
        }
        Some(PendingPrompt::Permission(_)) => String::new(),
        None => attachment_prefix(draft),
    }
}

/// What the input row shows: its prefix plus the active draft's text, caret
/// and selection. A pending question draws the draft of the question being
/// answered; the prompt draft stays hidden underneath until it settles.
fn active_input(ui: &Ui, draft: &Draft) -> (String, String, usize, Option<(usize, usize)>) {
    let prefix = input_prefix(ui, draft);
    match &ui.prompt {
        Some(PendingPrompt::Question {
            drafts, current, ..
        }) => {
            let draft = drafts.get(*current).unwrap_or(draft);
            (prefix, draft.text.clone(), draft.cursor, draft.selection())
        }
        Some(PendingPrompt::Permission(_)) => {
            (prefix, "[o]nce [a]lways [r]eject".into(), usize::MAX, None)
        }
        _ => (prefix, draft.text.clone(), draft.cursor, draft.selection()),
    }
}

/// Page keys move the draft: prompt history when the draft is empty, caret
/// rows otherwise. Shared by Up/Down, PageUp/PageDown and Shift+Arrow. The
/// caller resolves `input_prefix` first, so `ui` is borrowed once.
pub(super) fn page_move(
    prompt: &mut Option<PendingPrompt>,
    draft: &mut Draft,
    history: &mut History,
    prefix: &str,
    delta: isize,
) {
    let active = super::active_draft(prompt, draft);
    if !history.select(active, delta) {
        active.move_row(prefix, terminal_width(), delta);
    }
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
    // usize::MAX means the caret sits after the last grapheme. Adding the
    // prefix length to it would overflow, so resolve the sentinel first.
    let caret = if cursor == usize::MAX {
        prefix_len + text_len
    } else {
        (prefix_len + cursor).min(prefix_len + text_len)
    };
    let (crow, ccol, _) = positions[caret];
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

/// Length of the leading SGR/CSI escape in `text`, matching the terminator
/// range crossterm reports. Escapes are zero-width for layout.
fn escape_len(text: &str) -> Option<usize> {
    if !text.starts_with("\x1b[") {
        return None;
    }
    text[2..]
        .find(|ch: char| ('@'..='~').contains(&ch))
        .map(|end| end + 3)
}

/// Split one styled transcript row into display rows of at most `width`
/// cells, replaying its active SGR style after every break.
fn wrap_row(row: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut columns = 0;
    let mut style = String::new();
    let mut rest = row;
    while !rest.is_empty() {
        if let Some(end) = escape_len(rest) {
            let escape = &rest[..end];
            if escape.ends_with('m') {
                out.push_str(escape);
                if escape == RESET {
                    style.clear();
                } else {
                    style.push_str(escape);
                }
            }
            rest = &rest[end..];
            continue;
        }
        let grapheme = rest.graphemes(true).next().unwrap();
        rest = &rest[grapheme.len()..];
        if grapheme.contains('\n') {
            rows.push(std::mem::take(&mut out));
            columns = 0;
            continue;
        }
        if grapheme.chars().any(char::is_control) {
            continue;
        }
        let cells = grapheme.width();
        if columns + cells > width && columns > 0 {
            rows.push(std::mem::take(&mut out));
            out.push_str(&style);
            columns = 0;
        }
        out.push_str(grapheme);
        columns += cells;
    }
    rows.push(out);
    rows
}

// Two transcript renderers share one event pipeline. In native mode only the
// unfinished row and the draft are redrawn: complete rows are committed to the
// terminal's scrollback and never erased by editing, and the terminal owns
// wheel scrolling. In full-screen mode (`ui.scroll_mode: "fullscreen"`) rows
// are kept in memory and the whole screen is repainted, so the app scrolls the
// transcript with PageUp/PageDown, Ctrl+Home/Ctrl+End and the wheel.
//
// Every row carries the Ctrl+T / Ctrl+O toggle that can hide it. Full-screen
// mode filters at paint time, so hidden text returns when the toggle flips;
// native mode cannot rewrite the terminal's scrollback, so a hidden row is
// never printed and the toggle only affects output from then on.
// The draft may span several wrapped rows; the real terminal cursor is parked
// on the edit position so typing, arrows and the caret stay in sync.
pub(super) struct Display {
    tail: String,
    columns: usize,
    style: String,
    drawn_rows: usize,
    /// Row of the drawn block the terminal cursor rests on after `draw`: the
    /// caret's row, which is the block's last row only when the caret is at
    /// the end of the draft.
    caret_row: usize,
    /// Full-screen mode: committed transcript rows with the kind that can hide
    /// them, oldest first. Hidden rows stay stored so a toggle brings them back.
    lines: Vec<(RowKind, String)>,
    fullscreen: bool,
    /// First transcript display row painted at the viewport top. None follows
    /// the newest output.
    scroll: Option<usize>,
    /// Transcript rows the last repaint had room for: the scroll page size.
    page: usize,
    /// Width the last repaint wrapped transcript rows at: scrolling counts
    /// display rows, so it has to wrap the same way the paint did.
    paint_width: usize,
    /// Kind of the row being written, so the unfinished tail is hidden with it.
    tail_kind: RowKind,
    /// Ctrl+T / Ctrl+O state: which kinds are painted.
    visibility: Visibility,
    wrapped: RefCell<WrappedRows>,
}

/// Only new logical rows need wrapping between paints. Resizing or changing
/// visibility rebuilds this index once; ordinary paints copy the viewport only.
#[derive(Default)]
struct WrappedRows {
    width: usize,
    visibility: Visibility,
    through: usize,
    rows: Vec<String>,
    #[cfg(test)]
    work: usize,
}

impl Default for Display {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Display {
    pub fn new(fullscreen: bool) -> Self {
        Self {
            tail: String::new(),
            columns: 0,
            style: String::new(),
            drawn_rows: 0,
            caret_row: 0,
            lines: Vec::new(),
            fullscreen,
            scroll: None,
            page: 24,
            paint_width: 80,
            tail_kind: RowKind::Plain,
            visibility: Visibility::default(),
            wrapped: RefCell::new(WrappedRows::default()),
        }
    }

    /// Ctrl+T / Ctrl+O: change which transcript rows are painted. Full-screen
    /// mode repaints from the rows it kept, so hidden text comes back; native
    /// mode drops hidden rows as they arrive.
    pub fn set_visibility(&mut self, visibility: Visibility) {
        self.visibility = visibility;
    }

    pub fn erase(&mut self) {
        // A full-screen repaint clears the screen, so nothing to erase here.
        if self.fullscreen {
            return;
        }
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
            let tail = std::mem::take(&mut self.tail);
            self.push_row(tail);
        }
        self.tail.clear();
        self.style.clear();
        self.columns = 0;
    }

    pub fn output(&mut self, text: &str, width: usize) {
        self.output_as(text, width, RowKind::Plain);
    }

    /// Output one run of transcript text tagged with the toggle that can hide
    /// it. A hidden run is stored in full-screen mode, so flipping the toggle
    /// brings it back, and dropped in native mode, where the terminal's
    /// scrollback cannot be rewritten.
    pub fn output_as(&mut self, text: &str, width: usize, kind: RowKind) {
        let mut text = text;
        if kind != self.tail_kind {
            let committed = self.columns > 0;
            // The existing tail belongs to the preceding event, even when the
            // next event begins with its terminating newline under another kind.
            self.finish_line();
            if committed {
                text = text
                    .strip_prefix("\r\n")
                    .or_else(|| text.strip_prefix('\n'))
                    .unwrap_or(text);
            }
        }
        self.tail_kind = kind;
        // Track SGR sequences as zero-width tokens. Replay the current style
        // when wrapping a streaming delta across committed transcript rows.
        let mut rest = text;
        while !rest.is_empty() {
            if let Some(end) = escape_len(rest) {
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
            let grapheme = rest.graphemes(true).next().unwrap();
            rest = &rest[grapheme.len()..];
            if grapheme.contains('\n') {
                self.commit_row();
            } else if grapheme == "\t" {
                let spaces = 4 - self.columns % 4;
                self.output_as(&" ".repeat(spaces), width, kind);
            } else if !grapheme.chars().any(char::is_control) {
                // Full-screen rows wrap at paint time, so a transcript row
                // breaks only on a real newline and resizing re-wraps it.
                let cells = grapheme.width();
                if !self.fullscreen && self.columns + cells > width && self.columns > 0 {
                    self.commit_row();
                }
                self.tail.push_str(grapheme);
                self.columns += cells;
            }
        }
    }

    fn commit_row(&mut self) {
        let tail = std::mem::take(&mut self.tail);
        self.push_row(tail);
        self.tail.clone_from(&self.style);
        self.columns = 0;
    }

    /// Native mode prints a finished row into the terminal's scrollback;
    /// full-screen mode keeps it in memory for the next repaint. A row whose
    /// toggle is off is never printed, and never stored in native mode, where
    /// it could not be taken back.
    fn push_row(&mut self, row: String) {
        if self.fullscreen {
            self.lines.push((self.tail_kind, format!("{row}{RESET}")));
        } else if self.visibility.visible(self.tail_kind) {
            crate::println!("{row}{RESET}");
        }
    }

    /// Ctrl+L: drop the transcript; the next draw restores the status and draft.
    pub fn clear_screen(&mut self) {
        if self.fullscreen {
            self.lines.clear();
            self.wrapped.get_mut().rows.clear();
            self.wrapped.get_mut().through = 0;
            self.scroll = None;
        } else {
            self.finish_line();
            print!("\x1b[2J\x1b[H");
        }
        self.tail.clear();
        self.style.clear();
        self.columns = 0;
    }

    /// Leaving full-screen mode: the transcript is printed on the primary
    /// screen so the conversation stays in the terminal's own scrollback.
    pub fn leave_screen(&mut self) {
        if !self.fullscreen {
            return;
        }
        self.finish_line();
        // Leave the alternate screen first: a transcript painted on it would
        // disappear with it, so the conversation lands on the primary screen.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        print!("\x1b[2J\x1b[H");
        // Export the view the user selected, with one form of each tool call.
        for line in self.export_rows() {
            crate::println!("{line}");
        }
        self.lines.clear();
        self.wrapped.get_mut().rows.clear();
        self.wrapped.get_mut().through = 0;
        self.scroll = None;
    }

    /// Scroll the transcript window by `delta` display rows, staying attached
    /// to the newest output once the bottom is reached again.
    pub fn scroll_by(&mut self, delta: isize) {
        let last = self.last_scroll();
        let top = (self.scroll.unwrap_or(last) as isize)
            .saturating_add(delta)
            .clamp(0, last as isize) as usize;
        self.scroll = (top < last).then_some(top);
    }

    pub fn scroll_top(&mut self) {
        self.scroll = (self.last_scroll() > 0).then_some(0);
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll = None;
    }

    /// Scroll position of the bottom view: the first display row that still
    /// fits a full page.
    fn last_scroll(&self) -> usize {
        self.wrapped(self.paint_width)
            .len()
            .saturating_sub(self.page)
    }

    /// Rows PageUp/PageDown move: the transcript viewport height.
    pub fn page(&self) -> usize {
        self.page.max(1)
    }

    /// Export one visible representation, respecting both transcript toggles.
    fn export_rows(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .filter(|(kind, _)| self.visibility.visible(*kind))
            .map(|(_, line)| line.as_str())
    }

    /// Cache visible display rows; scrolling and painting share this index.
    fn wrapped(&self, width: usize) -> Ref<'_, Vec<String>> {
        let mut cache = self.wrapped.borrow_mut();
        if cache.width != width || cache.visibility != self.visibility {
            cache.width = width;
            cache.visibility = self.visibility;
            cache.through = 0;
            cache.rows.clear();
        }
        for (kind, line) in &self.lines[cache.through..] {
            if self.visibility.visible(*kind) {
                cache.rows.extend(wrap_row(line, width));
                #[cfg(test)]
                {
                    cache.work += 1;
                }
            }
        }
        cache.through = self.lines.len();
        drop(cache);
        Ref::map(self.wrapped.borrow(), |cache| &cache.rows)
    }

    /// Transcript display rows for the current scroll position, oldest first,
    /// at most `self.page` rows.
    fn viewport(&self, width: usize) -> Vec<String> {
        let rows = self.wrapped(width);
        let last = rows.len().saturating_sub(self.page);
        let top = self.scroll.unwrap_or(last).min(last);
        rows[top..(top + self.page).min(rows.len())].to_vec()
    }

    pub fn draw(&mut self, ui: &Ui, draft: &Draft, width: usize, model: &str) {
        let (prefix, text, cursor, sel) = active_input(ui, draft);
        let mut layout = layout(&prefix, &text, cursor, width, sel);
        // Status belongs to the editable block, never the transcript. Clip it
        // to one row even when the provider/model name is very long.
        let header = super::prompt_header(model, ui.visibility);
        let row = wrap_row(&header, width)
            .into_iter()
            .next()
            .unwrap_or_default();
        layout.rows.insert(0, format!("{row}{RESET}"));
        layout.cursor.0 += 1;
        if self.fullscreen {
            self.draw_screen(&layout);
            return;
        }
        self.drawn_rows = 0;
        print!("\r");
        // The streaming row follows the toggle of the text it carries, so
        // hidden thinking never peeks out above the draft.
        if self.columns > 0 && self.visibility.visible(self.tail_kind) {
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

    /// Repaint the whole screen: the transcript window, then the draft. The
    /// clear replaces every cell, so nothing needs row-by-row erase
    /// bookkeeping, and rows are joined without a trailing newline so the
    /// bottom row never scrolls the screen.
    fn draw_screen(&mut self, layout: &Layout) {
        let (width, height) = terminal_size();
        self.page = height.saturating_sub(layout.rows.len());
        self.paint_width = width;
        // The tail is the unfinished transcript row: paint it, never commit
        // it. Committing on every repaint would stack a snapshot of the text
        // so far each time a streaming delta arrives.
        let mut rows = self.viewport(width);
        if self.columns > 0 && self.scroll.is_none() && self.visibility.visible(self.tail_kind) {
            rows.extend(wrap_row(&format!("{}{RESET}", self.tail), width));
            let cut = rows.len().saturating_sub(self.page);
            rows.drain(..cut);
        }
        let mut out = String::from("\x1b[2J\x1b[H");
        let mut drawn = 0usize;
        for row in rows.iter().chain(layout.rows.iter()) {
            if drawn > 0 {
                out.push_str("\r\n");
            }
            out.push_str(row);
            drawn += 1;
        }
        self.caret_row = rows.len() + layout.cursor.0;
        self.drawn_rows = drawn;
        out.push_str(&format!(
            "\x1b[{};{}H",
            self.caret_row + 1,
            layout.cursor.1 + 1
        ));
        crate::print!("{out}");
        let _ = std::io::stdout().flush();
    }
}

pub(super) fn terminal_size() -> (usize, usize) {
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    (
        usize::from(width).saturating_sub(1).max(3),
        usize::from(height).max(1),
    )
}

pub(super) fn terminal_width() -> usize {
    // Leave the last cell unused so the terminal never has a pending autowrap.
    terminal_size().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::DIM;

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
    fn the_end_caret_sits_after_the_prefix() {
        // The question prompt's "usize::MAX means the end" sentinel must land
        // after the position prefix, not wrap into it.
        let end = layout("[2/3] ", "1", usize::MAX, 40, None);
        assert_eq!(end.cursor, (0, 9)); // "› [2/3] 1" is 8 cells, caret after
        let empty = layout("[2/3] ", "", usize::MAX, 40, None);
        assert_eq!(empty.cursor, (0, 8));
    }

    #[test]
    fn a_pending_question_owns_the_input_row() {
        // The question's own draft is what the input row draws; the prompt
        // draft stays hidden underneath it.
        let ui = Ui {
            part_kind: std::collections::HashMap::new(),
            headered: std::collections::HashSet::new(),
            tool_done: std::collections::HashSet::new(),
            prompt: Some(PendingPrompt::Question {
                id: "q1".into(),
                questions: vec![serde_json::json!({"question": "Pick", "options": []})],
                answers: vec![Vec::new()],
                current: 0,
                confirm: false,
                drafts: vec![Draft {
                    text: "2".into(),
                    cursor: 1,
                    ..Draft::default()
                }],
            }),
            visibility: Visibility::default(),
        };
        let mut prompt = Draft::default();
        prompt.insert_str("hidden prompt");
        let (prefix, text, cursor, _) = active_input(&ui, &prompt);
        assert_eq!((prefix.as_str(), text.as_str(), cursor), ("[1/1] ", "2", 1));
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
            visibility: Visibility::default(),
        };
        let mut draft = Draft::default();
        draft.insert_str("alpha\nbravo\ncharlie");
        let mut display = Display::default();
        // Caret at the end: the terminal cursor rests on the block's last row.
        display.draw(&ui, &draft, 40, "mock/m");
        assert_eq!((display.drawn_rows, display.caret_row), (4, 3));
        // Caret on the first row: the erase climbs to the draft's first row,
        // never above it, so committed transcript rows stay intact.
        draft.cursor = 0;
        display.draw(&ui, &draft, 40, "mock/m");
        assert_eq!((display.drawn_rows, display.caret_row), (4, 1));
        // A transcript row flushed above the draft shifts the block down.
        display.output("streaming", 40);
        display.draw(&ui, &draft, 40, "mock/m");
        assert_eq!((display.drawn_rows, display.caret_row), (5, 2));
        display.erase();
        assert_eq!((display.drawn_rows, display.caret_row), (0, 0));
    }

    #[test]
    fn wrap_row_breaks_on_cells_and_replays_the_style() {
        // Width 3: "中" is two cells, so "中中a" fills rows "中" and "中a",
        // and the DIM style restarts after each break.
        assert_eq!(
            wrap_row(&format!("{DIM}中中a{RESET}b"), 3),
            [
                format!("{DIM}中"),
                format!("{DIM}中a{RESET}"),
                "b".to_string()
            ]
        );
        assert_eq!(wrap_row("", 10), [""]);
        assert_eq!(wrap_row("abc", 10), ["abc"]);
    }

    #[test]
    fn the_toggles_filter_rows_at_paint_time_and_bring_hidden_ones_back() {
        let mut display = Display::new(true);
        display.output("the answer\n", 80);
        display.output_as("pondering\n", 80, RowKind::Thinking);
        display.output_as("  collapsed result\n", 80, RowKind::ToolBrief);
        display.output_as("  full command\n  full result\n", 80, RowKind::ToolFull);
        let all = display.wrapped(80).join("|");
        assert!(all.contains("the answer"));
        assert!(all.contains("pondering"));
        // Tool calls start collapsed: one form is painted, never both.
        assert!(all.contains("collapsed result"));
        assert!(!all.contains("full result"));
        display.set_visibility(Visibility {
            thinking: false,
            tools_expanded: false,
        });
        let hidden = display.wrapped(80).join("|");
        assert!(hidden.contains("the answer"));
        // Hidden rows are filtered at paint time, never dropped, so flipping
        // the toggle restores the transcript exactly.
        assert!(!hidden.contains("pondering"));
        // Collapsed rows are what a collapsed view shows.
        assert!(hidden.contains("collapsed result"));
        assert!(!hidden.contains("full result"));
        // Expanding swaps in the full command and result, and keeps thinking off.
        display.set_visibility(Visibility {
            thinking: false,
            tools_expanded: true,
        });
        let expanded = display.wrapped(80).join("|");
        assert!(expanded.contains("full command") && expanded.contains("full result"));
        assert!(!expanded.contains("collapsed result"));
        assert!(!expanded.contains("pondering"));
        display.set_visibility(Visibility {
            thinking: true,
            tools_expanded: true,
        });
        assert!(display.wrapped(80).join("|").contains("pondering"));
        display.set_visibility(Visibility::default());
        assert_eq!(display.wrapped(80).join("|"), all);
    }

    #[test]
    fn a_hidden_streaming_row_never_peeks_above_the_draft() {
        let ui = Ui {
            part_kind: std::collections::HashMap::new(),
            headered: std::collections::HashSet::new(),
            tool_done: std::collections::HashSet::new(),
            prompt: None,
            visibility: Visibility::default(),
        };
        let draft = Draft::default();
        let mut display = Display::default();
        display.set_visibility(Visibility {
            thinking: false,
            tools_expanded: false,
        });
        display.output_as("pondering", 40, RowKind::Thinking);
        // The hidden tail is not painted, so no thinking text shows above the
        // draft and nothing reaches the scrollback when the row commits.
        display.draw(&ui, &draft, 40, "mock/m");
        assert_eq!(display.drawn_rows, 2);
        display.output("\n", 40);
        assert_eq!(display.tail, "");
    }

    #[test]
    fn fullscreen_repaint_keeps_the_streaming_tail_uncommitted() {
        let ui = crate::tui::Ui {
            part_kind: std::collections::HashMap::new(),
            headered: std::collections::HashSet::new(),
            tool_done: std::collections::HashSet::new(),
            prompt: None,
            visibility: Visibility::default(),
        };
        let draft = Draft::default();
        let mut display = Display::new(true);
        display.output("assistant\n", 40);
        display.draw(&ui, &draft, 40, "mock/m");
        // Streaming deltas grow one unfinished row. Repainting paints it; it
        // must not be committed, which used to stack a snapshot per repaint.
        for delta in ["The ", "user ", "says ", "hi"] {
            display.output(delta, 40);
            display.draw(&ui, &draft, 40, "mock/m");
        }
        assert_eq!(display.lines.len(), 1);
        // Only a real newline commits the row.
        display.output("\n", 40);
        assert_eq!(display.lines.len(), 2);
    }

    #[test]
    fn clearing_the_help_command_keeps_attachments_and_resets_selection() {
        let mut draft = Draft::default();
        draft.insert_str("/help");
        draft.select_home();
        draft.attachments.push(crate::tui::Attachment {
            filename: "image.png".into(),
            url: "data:image/png;base64,test".into(),
            mime: "image/png",
        });
        draft.clear_text();
        assert!(draft.text.is_empty());
        assert_eq!(draft.cursor, 0);
        assert_eq!(draft.selection(), None);
        assert_eq!(draft.attachments.len(), 1);
        draft.insert_str("next prompt");
        assert_eq!(draft.text, "next prompt");
        draft.undo();
        assert!(draft.text.is_empty());
        draft.undo();
        assert_eq!(draft.text, "/help");
        assert_eq!(draft.cursor, 0);
        assert_eq!(draft.attachments.len(), 1);
        draft.undo();
        assert!(draft.text.is_empty());
    }

    #[test]
    fn status_redraws_stay_one_row_and_never_enter_the_transcript() {
        let mut ui = Ui {
            part_kind: std::collections::HashMap::new(),
            headered: std::collections::HashSet::new(),
            tool_done: std::collections::HashSet::new(),
            prompt: None,
            visibility: Visibility::default(),
        };
        let mut draft = Draft::default();
        draft.insert_str("unfinished");
        let model = "provider/中👩‍💻".repeat(30);
        for fullscreen in [false, true] {
            let mut display = Display::new(fullscreen);
            display.output("existing transcript\n", 40);
            let saved = display.lines.clone();
            for _ in 0..6 {
                ui.visibility.thinking = !ui.visibility.thinking;
                ui.visibility.tools_expanded = !ui.visibility.tools_expanded;
                display.set_visibility(ui.visibility);
                display.erase();
                display.draw(&ui, &draft, 40, &model);
                let transcript_rows = usize::from(fullscreen);
                assert_eq!(display.drawn_rows, 2 + transcript_rows);
                assert_eq!(display.caret_row, 1 + transcript_rows);
                assert_eq!(display.lines, saved);
                assert!(display.tail.is_empty());
                assert_eq!(draft.text, "unfinished");
            }
            display.erase();
            display.clear_screen();
            assert!(display.lines.is_empty());
            assert!(display.tail.is_empty());
            if fullscreen {
                assert!(display.viewport(40).is_empty());
                display.output("after clear\n", 40);
                assert_eq!(display.viewport(40), [format!("after clear{RESET}")]);
            }
        }
    }

    #[test]
    fn fullscreen_scrolling_counts_wrapped_rows() {
        // 40-column lines at width 20 take two display rows each: scrolling is
        // counted in display rows, so a transcript of two long lines scrolls.
        let mut display = Display::new(true);
        display.page = 2;
        display.paint_width = 20;
        for line in ["x", "y"] {
            display.output(&format!("{}\n", line.repeat(40)), 20);
        }
        let rows = |display: &Display| display.viewport(20);
        assert_eq!(rows(&display).len(), 2);
        assert!(rows(&display).iter().all(|row| row.starts_with('y')));
        display.scroll_top();
        assert!(rows(&display).iter().all(|row| row.starts_with('x')));
        display.scroll_by(1);
        assert!(rows(&display)[0].starts_with('x'));
        assert!(rows(&display)[1].starts_with('y'));
    }

    #[test]
    fn fullscreen_scrolling_pages_the_transcript() {
        let mut display = Display::new(true);
        display.page = 2;
        for line in ["a", "b", "c", "d"] {
            display.output(&format!("{line}\n"), 40);
        }
        let plain = |display: &Display| {
            display
                .viewport(40)
                .iter()
                .map(|row| row.replace(RESET, ""))
                .collect::<Vec<_>>()
        };
        // Following the newest output keeps the last rows in view.
        assert_eq!(plain(&display), ["c", "d"]);
        display.scroll_by(-1);
        assert_eq!(display.scroll, Some(1));
        assert_eq!(plain(&display), ["b", "c"]);
        display.scroll_by(-10);
        assert_eq!(display.scroll, Some(0));
        assert_eq!(plain(&display), ["a", "b"]);
        // Scrolling back to the bottom resumes following the newest output.
        display.scroll_by(10);
        assert_eq!(display.scroll, None);
        assert_eq!(plain(&display), ["c", "d"]);
        display.scroll_top();
        assert_eq!(plain(&display), ["a", "b"]);
        display.scroll_bottom();
        assert_eq!(plain(&display), ["c", "d"]);
        // A new row below a scrolled view keeps the page where it was.
        display.scroll_top();
        display.output("e\n", 40);
        assert_eq!(display.scroll, Some(0));
        assert_eq!(plain(&display), ["a", "b"]);
        display.scroll_bottom();
        assert_eq!(plain(&display), ["d", "e"]);
    }

    #[test]
    fn large_pastes_and_typing_store_edits_and_submission_drops_undo() {
        let mut draft = Draft::default();
        let original = "x".repeat(1024 * 1024);
        draft.insert_str(&original);
        for _ in 0..1000 {
            draft.insert('a');
        }
        assert_eq!(draft.text.len(), original.len() + 1000);
        assert!(
            draft.undo_bytes < 1024,
            "typing must not retain whole draft copies"
        );
        draft.undo();
        assert_eq!(draft.text, original);
        draft.undo();
        assert!(draft.text.is_empty());
        draft.insert_str("sent");
        let (text, _) = draft.take();
        assert_eq!(text, "sent");
        draft.undo();
        assert!(draft.text.is_empty());
        draft.insert_str(&"x".repeat(2 * MAX_UNDO_BYTES));
        draft.clear();
        assert!(draft.undo_bytes <= MAX_UNDO_BYTES);
        draft.undo();
        assert!(
            draft.text.is_empty(),
            "an omitted large deletion invalidates older records"
        );
    }

    #[test]
    fn undo_records_are_bounded_and_unicode_edits_round_trip() {
        let mut draft = Draft::default();
        for _ in 0..2 * MAX_UNDO_EDITS {
            draft.insert_str("x");
        }
        assert_eq!(draft.undo.len(), MAX_UNDO_EDITS);
        assert!(draft.undo_bytes <= MAX_UNDO_BYTES);
        draft.take();
        draft.insert_str("e");
        draft.insert('\u{301}');
        assert_eq!(draft.cursor, 1);
        draft.insert('!');
        assert_eq!(draft.text, "e\u{301}!");
        draft.undo();
        assert_eq!(draft.text, "e");
        draft.undo();
        assert!(draft.text.is_empty());
        draft.insert_str("a👩‍💻z");
        draft.left();
        draft.select_left();
        draft.insert_str("中");
        assert_eq!(draft.text, "a中z");
        draft.undo();
        assert_eq!(draft.text, "a👩‍💻z");
        draft.backspace();
        draft.undo();
        assert_eq!(draft.text, "a👩‍💻z");
    }

    #[test]
    fn kind_transitions_preserve_the_previous_tail_and_export_one_tool_form() {
        let mut display = Display::new(true);
        display.output_as("secret thinking", 80, RowKind::Thinking);
        display.output("\nassistant\nanswer without newline", 80);
        display.output_as("\nbrief command\n", 80, RowKind::ToolBrief);
        display.output_as("expanded command\nexpanded output\n", 80, RowKind::ToolFull);
        display.set_visibility(Visibility {
            thinking: false,
            tools_expanded: false,
        });
        let rows = display.wrapped(80).join("\n");
        assert!(!rows.contains("secret thinking"));
        assert!(rows.contains("answer without newline"));
        let exported = display.export_rows().collect::<Vec<_>>().join("\n");
        assert!(exported.contains("brief command"));
        assert!(!exported.contains("expanded command"));
        assert!(!exported.contains("secret thinking"));
        display.set_visibility(Visibility {
            thinking: true,
            tools_expanded: true,
        });
        let exported = display.export_rows().collect::<Vec<_>>().join("\n");
        assert!(exported.contains("secret thinking"));
        assert!(exported.contains("expanded command"));
        assert!(!exported.contains("brief command"));
    }

    #[test]
    fn paints_reuse_wrapped_history_and_invalidate_on_resize_toggle_and_clear() {
        let mut display = Display::new(true);
        for _ in 0..8000 {
            display.output("a long transcript row\n", 80);
        }
        display.viewport(80);
        let work = display.wrapped.borrow().work;
        for _ in 0..10 {
            display.viewport(80);
        }
        assert_eq!(display.wrapped.borrow().work, work);
        display.output("new row\n", 80);
        display.viewport(80);
        assert_eq!(display.wrapped.borrow().work, work + 1);
        display.viewport(10);
        let resized = display.wrapped.borrow().work;
        assert_eq!(resized, work + 1 + 8001);
        display.set_visibility(Visibility {
            thinking: false,
            tools_expanded: false,
        });
        display.viewport(10);
        assert_eq!(display.wrapped.borrow().work, resized + 8001);
        display.clear_screen();
        assert!(display.viewport(10).is_empty());
    }
}
