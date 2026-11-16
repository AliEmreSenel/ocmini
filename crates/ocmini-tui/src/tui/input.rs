use super::markdown::Markdown;
use super::render::{Row, ThoughtUpdate, ToolCall};
use super::{
    style_ok, Attachment, Key, PendingPrompt, RowKind, ToolView, Ui, Visibility, BOLD, CYAN, RESET,
};
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, VecDeque};
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

/// Ordinary editing applies to either the prompt or the active answer draft.
pub(super) fn edit(key: &Key, draft: &mut Draft, killring: &mut KillRing) -> bool {
    match key {
        Key::Char(ch) => draft.insert(*ch),
        Key::NewLine => draft.insert('\n'),
        Key::Backspace => draft.backspace(),
        Key::Delete => draft.delete(),
        Key::Undo => draft.undo(),
        Key::KillLine | Key::KillWord => {
            let killed = if matches!(key, Key::KillLine) {
                draft.kill_line()
            } else {
                draft.kill_word()
            };
            if let Some(text) = killed {
                killring.push(text);
            }
        }
        Key::Yank => {
            if let Some(text) = killring.top() {
                draft.insert_str(text);
            }
        }
        Key::Move(motion, selecting) => draft.motion(*motion, *selecting),
        _ => return false,
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    Home,
    End,
    WordLeft,
    WordRight,
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

    pub fn motion(&mut self, motion: Motion, selecting: bool) {
        if selecting {
            self.anchor_here();
        } else {
            self.anchor = None;
        }
        self.typing = false;
        let cursor = self.clamped();
        self.cursor = match motion {
            Motion::Left => cursor.saturating_sub(1),
            Motion::Right => (cursor + 1).min(graphemes(&self.text)),
            Motion::Home => {
                let bytes = byte_index(&self.text, cursor);
                let start = self.text[..bytes].rfind('\n').map_or(0, |i| i + 1);
                graphemes(&self.text[..start])
            }
            Motion::End => {
                let bytes = byte_index(&self.text, cursor);
                let end = self.text[bytes..]
                    .find('\n')
                    .map_or(self.text.len(), |i| bytes + i);
                graphemes(&self.text[..end])
            }
            Motion::WordLeft | Motion::WordRight => {
                let chars: Vec<&str> = self.text.graphemes(true).collect();
                if matches!(motion, Motion::WordLeft) {
                    word_start(&chars, cursor)
                } else {
                    word_end(&chars, cursor)
                }
            }
        };
        self.settle_anchor();
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

    pub fn move_row(&mut self, prefix: &str, width: usize, delta: isize) {
        self.anchor = None;
        self.move_row_impl(prefix, width, delta);
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

/// Most recently killed text, retrieved by Ctrl+Y.
#[derive(Default)]
pub(super) struct KillRing {
    latest: Option<String>,
}

impl KillRing {
    pub fn push(&mut self, text: String) {
        if !text.is_empty() {
            self.latest = Some(text);
        }
    }
    pub fn top(&self) -> Option<&str> {
        self.latest.as_deref()
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
fn active_input<'a>(
    ui: &'a Ui,
    draft: &'a Draft,
) -> (String, &'a str, usize, Option<(usize, usize)>) {
    let prefix = input_prefix(ui, draft);
    match &ui.prompt {
        Some(PendingPrompt::Question {
            drafts, current, ..
        }) => {
            let draft = drafts.get(*current).unwrap_or(draft);
            (prefix, &draft.text, draft.cursor, draft.selection())
        }
        Some(PendingPrompt::Permission(_)) => {
            (prefix, "[o]nce [a]lways [r]eject", usize::MAX, None)
        }
        _ => (prefix, &draft.text, draft.cursor, draft.selection()),
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
            if escape.ends_with('m') && style_ok(escape) {
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
            out.push_str(&style);
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

fn unstyled(text: &str) -> String {
    let mut rest = text;
    let mut plain = String::new();
    while !rest.is_empty() {
        if let Some(end) = escape_len(rest) {
            rest = &rest[end..];
        } else {
            let ch = rest.chars().next().unwrap();
            plain.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    plain
}

/// Streamed whitespace outside code blocks is a paragraph separator, not one
/// transcript row per newline token. A fenced code row carries a gutter, so it
/// is never blank, and tool results bypass this presentation rule.
#[derive(Default)]
struct Spacing {
    blank: bool,
}

impl Spacing {
    fn accept(&mut self, row: &str) -> bool {
        let blank = unstyled(row).trim().is_empty();
        if blank && self.blank {
            return false;
        }
        self.blank = blank;
        true
    }
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
    /// Logical transcript rows and anchors for chains, oldest first. Chains
    /// replace their own state instead of appending snapshots or blank rows.
    lines: Vec<(RowKind, String)>,
    chains: Vec<ToolChain>,
    calls: HashMap<String, (usize, usize)>,
    active_chain: Option<usize>,
    thoughts: Vec<Thought>,
    thought_ids: HashMap<String, usize>,
    spacing: Spacing,
    /// Narrative or thinking markdown state: the current line and open fence.
    markdown: Markdown,
    /// Where the markdown renderer's current row starts inside `tail`, valid
    /// only while `md_row` holds. The renderer owns that part of the row: it
    /// re-renders the whole line for every chunk, so a marker that closes late
    /// can restyle the text before it.
    md_tail: usize,
    /// The renderer's row is fully inside the tail. A row that wrapped has
    /// part of it in the scrollback, so there is nothing left to rewrite.
    md_row: bool,
    /// Rows committed, so a write's commits can be counted.
    committed: usize,
    /// The transcript has painted at least one row, so the next block needs a
    /// separator. Ctrl+L and leaving full-screen start the transcript over.
    printed: bool,
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
    input: Option<InputPaint>,
}

struct InputPaint {
    prefix: String,
    text: String,
    cursor: usize,
    selection: Option<(usize, usize)>,
    width: usize,
    height: usize,
    header: String,
    layout: std::rc::Rc<Layout>,
}

/// Only new logical rows need wrapping between paints. Resizing or changing
/// visibility rebuilds this index once; ordinary paints copy the viewport only.
#[derive(Default)]
struct WrappedRows {
    width: usize,
    visibility: Visibility,
    through: usize,
    rows: Vec<String>,
    /// Display-row count after each logical row, for invalidating a changed chain.
    ends: Vec<usize>,
    #[cfg(test)]
    work: usize,
}

struct ToolChain {
    anchor: usize,
    calls: Vec<ToolCall>,
}

struct Thought {
    anchor: usize,
    message_id: String,
    text: String,
    start: i64,
    end: Option<i64>,
    tokens: Option<i64>,
}

impl Thought {
    fn rows(&self, expanded: bool) -> Vec<String> {
        if self.text.trim().is_empty() {
            return Vec::new();
        }
        if expanded {
            // The block's rows open at the top of their own anchor, so their
            // leading blank row belongs to them; `Spacing::default` drops it.
            let mut spacing = Spacing { blank: true };
            let rendered = Markdown::default().push(self.text.trim_end()).text;
            return std::iter::once(format!("{}◦ thinking{RESET}", super::DIM))
                .chain(
                    rendered
                        .lines()
                        .filter(|line| spacing.accept(line))
                        .map(|line| format!("{line}{RESET}")),
                )
                .collect();
        }
        let seconds = self
            .end
            .unwrap_or_else(crate::types::now_ms)
            .saturating_sub(self.start)
            .max(0) as f64
            / 1000.0;
        let tokens = self
            .tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| format!("~{}", self.text.chars().count().div_ceil(4)));
        let label = if self.end.is_some() {
            "Thought"
        } else {
            "Thinking"
        };
        vec![format!(
            "{}◦ {label} for {seconds:.1}s ({tokens} tokens){RESET}",
            super::DIM
        )]
    }
}

impl ToolChain {
    fn rows(&self, view: ToolView) -> Vec<String> {
        if view != ToolView::Grouped {
            return self
                .calls
                .iter()
                .map(|call| {
                    let text = if view == ToolView::Brief {
                        &call.brief
                    } else {
                        &call.full
                    };
                    format!("{text}{RESET}")
                })
                .collect();
        }
        let mut tools: Vec<(&str, usize)> = Vec::new();
        let (mut pending, mut running, mut failed) = (0, 0, 0);
        for call in &self.calls {
            if let Some((_, count)) = tools.iter_mut().find(|(name, _)| *name == call.name) {
                *count += 1;
            } else {
                tools.push((&call.name, 1));
            }
            match call.status.as_str() {
                "pending" => pending += 1,
                "running" => running += 1,
                "error" => failed += 1,
                _ => {}
            }
        }
        let tools = tools
            .iter()
            .map(|(name, count)| format!("{name} × {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut status = String::new();
        for (count, label) in [
            (pending, "pending"),
            (running, "running"),
            (failed, "failed"),
        ] {
            if count > 0 {
                status.push_str(&format!(" · {count} {label}"));
            }
        }
        let mark = if failed > 0 { "✗" } else { "▸" };
        let count = self.calls.len();
        vec![format!(
            "{mark} {count} tool call{} · {tools}{status}{RESET}",
            if count == 1 { "" } else { "s" }
        )]
    }
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
            chains: Vec::new(),
            calls: HashMap::new(),
            active_chain: None,
            thoughts: Vec::new(),
            thought_ids: HashMap::new(),
            spacing: Spacing::default(),
            markdown: Markdown::default(),
            md_tail: 0,
            md_row: false,
            committed: 0,
            printed: false,
            fullscreen,
            scroll: None,
            page: 24,
            paint_width: 80,
            tail_kind: RowKind::Plain,
            visibility: Visibility::default(),
            wrapped: RefCell::new(WrappedRows::default()),
            input: None,
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
        // A narrative line the stream stopped in is not lost when a tool chain,
        // a prompt or a block interrupts it: the renderer writes the whole line
        // out, replacing the partial rendering it owned. A row that wrapped is
        // committed, so its text is not rendered into the tail a second time.
        let line = self.markdown.finish();
        if self.md_row && !line.is_empty() {
            self.tail.truncate(self.md_tail);
            self.tail.push_str(&line);
            self.columns = unstyled(&self.tail).width();
        }
        self.md_row = false;
        if self.columns > 0 {
            let tail = std::mem::take(&mut self.tail);
            self.push_row(tail);
        }
        self.tail.clear();
        self.style.clear();
        self.columns = 0;
    }

    /// One blank row before a block-level element, and never two. Labels,
    /// notices, tool chains, thinking blocks and user prompts all open with
    /// this, so the transcript has one owner for its vertical rhythm instead of
    /// embedding a leading newline in every header string.
    pub fn ensure_blank(&mut self) {
        self.finish_line();
        if self.printed && !self.spacing.blank {
            self.push_row(String::new());
        }
    }

    /// One user turn. It gets a label of its own with the text indented under
    /// it: the draft's `› ` marker stays for editing, and a committed prompt
    /// used to print the same marker, so a prompt was not findable at a glance.
    pub fn user_prompt(&mut self, text: &str, width: usize) {
        if text.trim().is_empty() {
            return;
        }
        self.ensure_blank();
        self.output(
            &format!(
                "{BOLD}{CYAN}❯ user{RESET}\n  {}\n",
                text.replace('\n', "\n  ")
            ),
            width,
        );
    }

    pub fn apply(&mut self, rows: Vec<Row>, width: usize) {
        for row in rows {
            match row {
                Row::Text(kind, text) => self.output_as(&text, width, kind),
                Row::Blank => self.ensure_blank(),
                Row::Rewrite => {
                    // The answer restarted: drop the line and fence the old
                    // text left behind, and stop owning any of the tail.
                    self.markdown.restart();
                    self.md_row = false;
                }
                Row::Header(kind, text) => {
                    self.ensure_blank();
                    self.output_as(&text, width, kind);
                    // The label's own row ends it. A blank row that follows
                    // comes from the model's leading newline, not from the
                    // transcript's rhythm, so it collapses into the label.
                    self.spacing.blank = true;
                }
                Row::Tool(call) => self.update_tool(call),
                Row::Thought(update) => self.update_thought(update, width),
                Row::ThoughtTokens { message_id, tokens } => {
                    let matching: Vec<_> = self
                        .thoughts
                        .iter()
                        .enumerate()
                        .filter(|(_, thought)| thought.message_id == message_id)
                        .map(|(index, _)| index)
                        .collect();
                    // A message's aggregate usage is exact for a single block;
                    // do not assign that total to every block in older history.
                    if let [index] = matching.as_slice() {
                        self.thoughts[*index].tokens = Some(tokens);
                        self.invalidate_from(self.thoughts[*index].anchor);
                    }
                }
                Row::EndTools => self.end_tools(),
            }
        }
    }

    fn update_thought(&mut self, update: ThoughtUpdate, width: usize) {
        if !self.fullscreen {
            if update.new || update.reset {
                self.finish_line();
                self.markdown.restart();
            }
            let mut text = String::new();
            if update.reset {
                text.push_str(&format!("\n{}[response updated]{RESET}\n", super::DIM));
            }
            text.push_str(&update.text);
            if update.new {
                self.ensure_blank();
                self.output_as(
                    &format!("{}◦ thinking{RESET}\n", super::DIM),
                    width,
                    RowKind::Thinking,
                );
            }
            self.output_as(&text, width, RowKind::Thinking);
            return;
        }
        let index = if let Some(&index) = self.thought_ids.get(&update.id) {
            index
        } else {
            self.active_chain = None;
            self.finish_line();
            self.markdown.restart();
            // The block's rows print right under this anchor, so the next
            // block's separator has to survive the blank-row collapse.
            self.spacing.blank = false;
            let index = self.thoughts.len();
            self.thoughts.push(Thought {
                anchor: self.lines.len(),
                message_id: update.message_id.clone(),
                text: String::new(),
                start: update.start.unwrap_or_else(crate::types::now_ms),
                end: None,
                tokens: None,
            });
            self.thought_ids.insert(update.id, index);
            self.lines.push((RowKind::Thought(index), String::new()));
            index
        };
        let thought = &mut self.thoughts[index];
        if update.reset {
            thought.text.clear();
        }
        thought.text.push_str(&update.text);
        if !update.message_id.is_empty() {
            thought.message_id = update.message_id;
        }
        if let Some(start) = update.start {
            thought.start = start;
        }
        if let Some(end) = update.end {
            thought.end = Some(end);
        }
        let anchor = thought.anchor;
        self.invalidate_from(anchor);
    }

    fn invalidate_from(&mut self, anchor: usize) {
        let cache = self.wrapped.get_mut();
        if anchor < cache.through {
            cache.through = anchor;
            cache.rows.truncate(if anchor == 0 {
                0
            } else {
                cache.ends[anchor - 1]
            });
            cache.ends.truncate(anchor);
        }
    }

    fn update_tool(&mut self, call: ToolCall) {
        let completed = matches!(call.status.as_str(), "completed" | "error");
        let native_text = (!self.fullscreen && completed).then(|| {
            if self.visibility.tools == ToolView::Full {
                call.full.clone()
            } else {
                call.brief.clone()
            }
        });
        let chain = if let Some(&(chain, index)) = self.calls.get(&call.id) {
            if self.chains[chain].calls[index] == call {
                return;
            }
            self.chains[chain].calls[index] = call;
            chain
        } else {
            let chain = match self.active_chain {
                Some(chain) => chain,
                None => {
                    self.finish_line();
                    // The chain's anchor row prints right here, so the next
                    // block's separator has to survive the blank-row collapse.
                    self.spacing.blank = false;
                    let chain = self.chains.len();
                    self.chains.push(ToolChain {
                        anchor: self.lines.len(),
                        calls: Vec::new(),
                    });
                    self.lines.push((RowKind::Tools(chain), String::new()));
                    self.active_chain = Some(chain);
                    chain
                }
            };
            self.calls
                .insert(call.id.clone(), (chain, self.chains[chain].calls.len()));
            self.chains[chain].calls.push(call);
            chain
        };
        if let Some(text) = native_text {
            self.finish_line();
            for row in wrap_row(&text, terminal_width()) {
                crate::println!("{row}{RESET}");
            }
            // The chain's rows are on screen: the next block opens with its
            // own separator, so the collapse must not eat it.
            self.spacing.blank = false;
        }
        let anchor = self.chains[chain].anchor;
        self.invalidate_from(anchor);
    }

    pub fn end_tools(&mut self) {
        // The answer is over: print the narrative line it stopped in.
        self.finish_line();
        self.active_chain = None;
        // The next block opens with a separator row, so the blank-row collapse
        // must not swallow it on the run of blanks that just ended.
        self.spacing.blank = false;
        if !self.fullscreen {
            // Completed native calls already live in terminal scrollback.
            self.lines.clear();
            self.chains.clear();
            self.calls.clear();
            *self.wrapped.get_mut() = WrappedRows::default();
        }
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
            // Finish the preceding stream before rendering the next one, so
            // a thought's unfinished line or fence cannot reach the answer.
            self.finish_line();
            self.markdown.restart();
            if committed {
                text = text
                    .strip_prefix("\r\n")
                    .or_else(|| text.strip_prefix('\n'))
                    .unwrap_or(text);
            }
            self.tail_kind = kind;
        }
        // Narrative and thinking rows are markdown. Re-render the whole line
        // for every chunk, so the row it is building is replaced rather than
        // appended: that is what lets `**bold**` style text already on screen.
        // Tool rows keep their own formatting and print their text verbatim.
        if matches!(kind, RowKind::Plain | RowKind::Thinking) {
            let rendered = self.markdown.push(text);
            if self.md_row {
                self.tail.truncate(self.md_tail);
                self.columns = unstyled(&self.tail).width();
            }
            // Every rendered span closes itself, so the style in effect where
            // the row starts is exactly the style the write leaves behind.
            let commits = self.committed;
            // Track the actual tail offset: expanding tabs and dropping
            // untrusted escapes can change the rendered row's byte length.
            self.md_tail = self.tail.len();
            self.write(&rendered.text, width);
            // A write that committed one row per newline in the render ended
            // every row it printed: the renderer owns the row left in the tail.
            // A row that wrapped commits without a newline, so part of it is
            // in the scrollback, past rewriting: the renderer lets the line go
            // there, and ownership returns at the next row.
            let complete = &rendered.text[..rendered.text.len() - rendered.line];
            self.md_row = self.committed - commits == complete.matches('\n').count();
            if !self.md_row {
                self.markdown.restart_line();
            }
            return;
        }
        self.write(text, width);
    }

    fn write(&mut self, text: &str, width: usize) {
        if !unstyled(text).trim().is_empty() {
            // Only consecutive tool calls belong to a group. Even when
            // thinking is collapsed, intervening messages keep their place.
            self.active_chain = None;
        }
        // Track SGR sequences as zero-width tokens. Replay the current style
        // when wrapping a streaming delta across committed transcript rows.
        let mut rest = text;
        while !rest.is_empty() {
            if let Some(end) = escape_len(rest) {
                let escape = &rest[..end];
                // Only the app's own styling reaches the terminal. An escape
                // from a model, a tool, a fetched page, an MCP server, a hook,
                // a filename or a session title is consumed and dropped, so it
                // cannot restyle the transcript, hide the cursor, clear the
                // screen, set the window title or open a hyperlink. A sequence
                // that is not SGR is dropped whole, which is why an injected
                // OSC never reaches a terminal emulator.
                if escape.ends_with('m') && style_ok(escape) {
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
                self.write(&" ".repeat(spaces), width);
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
        self.md_tail = self.tail.len();
        self.columns = 0;
        self.committed += 1;
        // The renderer's row is committed, so it owns nothing in the tail.
        self.md_row = false;
    }

    /// Native mode prints a finished row into the terminal's scrollback;
    /// full-screen mode keeps it in memory for the next repaint. A row whose
    /// toggle is off is never printed, and never stored in native mode, where
    /// it could not be taken back.
    fn push_row(&mut self, row: String) {
        if !self.spacing.accept(&row) {
            return;
        }
        // The transcript has content from here on, so `ensure_blank` may spend
        // a separator row. Before the first row it would open the transcript
        // with blank space.
        self.printed = true;
        if self.fullscreen {
            self.lines.push((self.tail_kind, format!("{row}{RESET}")));
        } else if self.visibility.visible(self.tail_kind) {
            crate::println!("{row}{RESET}");
        }
    }

    /// Ctrl+L: drop the transcript; the next draw restores the status and draft.
    pub fn clear_screen(&mut self) {
        self.lines.clear();
        self.chains.clear();
        self.calls.clear();
        self.active_chain = None;
        self.thoughts.clear();
        self.thought_ids.clear();
        self.spacing = Spacing::default();
        self.markdown = Markdown::default();
        self.md_row = false;
        self.committed = 0;
        self.printed = false;
        *self.wrapped.get_mut() = WrappedRows::default();
        if self.fullscreen {
            self.scroll = None;
        } else {
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
        // Rows go through `wrap_row`, so they carry the same escape allowlist
        // and width as live output: a tool result, a fetched page or a hook
        // cannot restyle the terminal the transcript is printed on.
        let width = terminal_width();
        for line in self.export_rows() {
            for row in wrap_row(&line, width) {
                crate::println!("{row}{RESET}");
            }
        }
        self.lines.clear();
        self.chains.clear();
        self.calls.clear();
        self.active_chain = None;
        self.thoughts.clear();
        self.thought_ids.clear();
        self.spacing = Spacing::default();
        self.markdown = Markdown::default();
        self.md_row = false;
        self.committed = 0;
        self.printed = false;
        *self.wrapped.get_mut() = WrappedRows::default();
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
    fn export_rows(&self) -> impl Iterator<Item = String> + '_ {
        self.lines.iter().flat_map(|(kind, line)| match *kind {
            RowKind::Tools(chain) => self.chains[chain].rows(self.visibility.tools),
            RowKind::Thought(index) => self.thoughts[index].rows(self.visibility.thinking),
            _ if self.visibility.visible(*kind) => vec![line.clone()],
            _ => Vec::new(),
        })
    }

    /// Cache visible display rows; scrolling and painting share this index.
    fn wrapped(&self, width: usize) -> Ref<'_, Vec<String>> {
        let mut cache = self.wrapped.borrow_mut();
        if cache.width != width || cache.visibility != self.visibility {
            cache.width = width;
            cache.visibility = self.visibility;
            cache.through = 0;
            cache.rows.clear();
            cache.ends.clear();
        }
        for (kind, line) in &self.lines[cache.through..] {
            if let RowKind::Tools(chain) = *kind {
                for line in self.chains[chain].rows(self.visibility.tools) {
                    cache.rows.extend(wrap_row(&line, width));
                }
            } else if let RowKind::Thought(index) = *kind {
                for line in self.thoughts[index].rows(self.visibility.thinking) {
                    cache.rows.extend(wrap_row(&line, width));
                }
            } else if self.visibility.visible(*kind) {
                cache.rows.extend(wrap_row(line, width));
                #[cfg(test)]
                {
                    cache.work += 1;
                }
            }
            let end = cache.rows.len();
            cache.ends.push(end);
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
        let header = super::prompt_header(model, ui.visibility);
        let (_, height) = terminal_size();
        let changed = self.input.as_ref().is_none_or(|cache| {
            cache.prefix != prefix
                || cache.text != text
                || cache.cursor != cursor
                || cache.selection != sel
                || cache.width != width
                || cache.height != height
                || cache.header != header
        });
        if changed {
            let mut layout = layout(&prefix, text, cursor, width, sel);
            // Follow the caret within a bounded editor window. Leave a status
            // row and transcript space even for a draft taller than the terminal.
            let available = height.saturating_sub(2).max(1);
            let top = layout.cursor.0.saturating_sub(available - 1);
            layout.rows = layout.rows.into_iter().skip(top).take(available).collect();
            layout.cursor.0 -= top;
            if height > 1 {
                let row = wrap_row(&header, width)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                layout.rows.insert(0, format!("{row}{RESET}"));
                layout.cursor.0 += 1;
            }
            self.input = Some(InputPaint {
                prefix,
                text: text.into(),
                cursor,
                selection: sel,
                width,
                height,
                header,
                layout: std::rc::Rc::new(layout),
            });
        }
        let layout = self.input.as_ref().unwrap().layout.clone();
        if self.fullscreen {
            self.draw_screen(&layout);
            return;
        }
        self.drawn_rows = 0;
        print!("\r");
        // Bound and wrap every live row before drawing. Counting a long or
        // multiline header as one row makes erase leave a row behind on each
        // event, even while the model is only generating pending calls.
        let live = self.native_live_rows(width, height.saturating_sub(layout.rows.len()));
        for row in &live {
            crate::println!("{row}{RESET}");
        }
        self.drawn_rows = live.len();
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

    fn native_live_rows(&self, width: usize, available: usize) -> Vec<String> {
        let mut rows = Vec::new();
        if self.columns > 0 && self.visibility.visible(self.tail_kind) {
            rows.extend(wrap_row(&format!("{}{RESET}", self.tail), width));
        }
        if let Some(call) = self
            .chains
            .iter()
            .rev()
            .flat_map(|chain| chain.calls.iter().rev())
            .find(|call| !matches!(call.status.as_str(), "completed" | "error"))
        {
            let text = if self.visibility.tools == ToolView::Full {
                &call.full
            } else {
                &call.brief
            };
            rows.extend(wrap_row(text, width));
        }
        let cut = rows.len().saturating_sub(available);
        rows.drain(..cut);
        rows
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
        draft.motion(Motion::Left, false);
        draft.insert('c');
        assert_eq!(draft.text, "abcd");
        assert_eq!(draft.cursor, 3);
        draft.motion(Motion::Home, false);
        draft.motion(Motion::Right, false);
        draft.backspace();
        assert_eq!(draft.text, "bcd");
        assert_eq!(draft.cursor, 0);
        draft.motion(Motion::End, false);
        draft.motion(Motion::Right, false);
        draft.backspace();
        assert_eq!(draft.text, "bc");
        // Moving and deleting stay on grapheme boundaries with wide characters.
        draft.insert_str("👩‍💻中");
        draft.motion(Motion::Left, false);
        draft.backspace();
        assert_eq!(draft.text, "bc中");
        draft.motion(Motion::Right, false);
        draft.backspace();
        assert_eq!(draft.text, "bc");
        draft.clear();
        assert_eq!(draft.cursor, 0);
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut draft = Draft::default();
        draft.insert_str("ab");
        draft.motion(Motion::Home, false);
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
        draft.motion(Motion::Home, false);
        assert_eq!(draft.cursor, 10); // start of the second line
        draft.motion(Motion::Home, false);
        assert_eq!(draft.cursor, 10); // Home is per line, so it stays
        draft.motion(Motion::End, false);
        assert_eq!(draft.cursor, 23); // end of the second line
        draft.cursor = 0;
        draft.motion(Motion::End, false);
        assert_eq!(draft.cursor, 9); // per-line End: first line's end
        draft.motion(Motion::End, false);
        assert_eq!(draft.cursor, 9);
        // Word motion across the whole draft.
        draft.cursor = 0;
        draft.motion(Motion::WordRight, false);
        assert_eq!(draft.cursor, 3); // over "let"
        draft.motion(Motion::WordRight, false);
        draft.motion(Motion::WordRight, false);
        draft.motion(Motion::WordRight, false);
        assert_eq!(draft.cursor, 13); // past the newline, over "let"
        draft.motion(Motion::WordRight, false);
        assert_eq!(draft.cursor, 15); // over "y"
        draft.motion(Motion::WordRight, false);
        assert_eq!(draft.cursor, 21); // over "foo"
        draft.motion(Motion::WordRight, false);
        assert_eq!(draft.cursor, 23); // trailing "( )" is one non-word run
        draft.motion(Motion::WordLeft, false);
        assert_eq!(draft.cursor, 18); // start of "foo"
                                      // Forward delete removes at the caret, backspace behind it.
        draft.motion(Motion::Home, false);
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
        draft.motion(Motion::Home, false);
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
        draft.motion(Motion::Home, false);
        draft.motion(Motion::WordRight, true);
        assert_eq!(draft.selection(), Some((0, 6)));
        draft.insert('X'); // typing replaces the selection
        assert_eq!(draft.text, "X this keep");
        assert_eq!(draft.cursor, 1);
        draft.undo(); // replacing a selection is ONE undo step
        assert_eq!(draft.text, "remove this keep");
        assert_eq!(draft.cursor, 6); // caret returns to where typing replaced it
        draft.motion(Motion::Home, false);
        draft.motion(Motion::WordRight, true);
        draft.insert('X');
        draft.motion(Motion::End, true);
        assert_eq!(draft.selection(), Some((1, 11)));
        draft.backspace(); // backspace deletes the selection
        assert_eq!(draft.text, "X");
        assert!(draft.selection().is_none());
        // Plain motion drops the selection that Shift starts.
        draft.motion(Motion::Left, true);
        assert_eq!(draft.selection(), Some((0, 1)));
        draft.motion(Motion::Left, false);
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
        // Nothing left to kill: the last killed value remains available.
        draft.motion(Motion::End, false);
        assert!(draft.kill_line().is_none());
        assert_eq!(ring.top(), Some("\n"));
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
        rows.iter().map(|row| unstyled(row)).collect()
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
            interactions: Default::default(),
            text: crate::client::TextState::default(),
            tool_done: std::collections::HashSet::new(),
            answer_labelled: false,
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
        assert_eq!((prefix.as_str(), text, cursor), ("[1/1] ", "2", 1));
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
            interactions: Default::default(),
            text: crate::client::TextState::default(),
            tool_done: std::collections::HashSet::new(),
            answer_labelled: false,
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

    fn sample_tool(id: usize, status: &str) -> ToolCall {
        ToolCall {
            id: format!("tool_{id}"),
            name: "bash".into(),
            status: status.into(),
            brief: if status == "completed" {
                format!("● bash command_{id}\n  ✓ result_{id}")
            } else {
                format!("● bash command_{id} · {status}")
            },
            full: if status == "completed" {
                format!("● bash full_command_{id}\n  ✓ full_result_{id}\n  second line")
            } else {
                format!("● bash full_command_{id} · {status}")
            },
        }
    }

    #[test]
    fn the_toggles_filter_rows_at_paint_time_and_bring_hidden_ones_back() {
        let mut display = Display::new(true);
        display.output("the answer\n", 80);
        display.output_as("pondering\n", 80, RowKind::Thinking);
        display.update_tool(sample_tool(0, "completed"));
        let default_view = display.wrapped(80).join("|");
        assert!(default_view.contains("command_0") && default_view.contains("result_0"));
        display.set_visibility(Visibility {
            tools: ToolView::Grouped,
            ..Visibility::default()
        });
        let grouped = display.wrapped(80).join("|");
        assert!(grouped.contains("the answer") && grouped.contains("pondering"));
        assert!(grouped.contains("1 tool call · bash × 1"));
        assert!(!grouped.contains("command_0"));
        display.set_visibility(Visibility {
            thinking: false,
            tools: ToolView::Brief,
        });
        let brief = display.wrapped(80).join("|");
        assert!(brief.contains("command_0") && brief.contains("result_0"));
        assert!(!brief.contains("pondering") && !brief.contains("full_result_0"));
        display.set_visibility(Visibility {
            thinking: false,
            tools: ToolView::Full,
        });
        let full = display.wrapped(80).join("|");
        assert!(full.contains("full_command_0") && full.contains("full_result_0"));
        assert!(!full.contains("pondering"));
        display.set_visibility(Visibility::default());
        assert_eq!(display.wrapped(80).join("|"), default_view);
    }

    #[test]
    fn a_hidden_streaming_row_never_peeks_above_the_draft() {
        let ui = Ui {
            interactions: Default::default(),
            text: crate::client::TextState::default(),
            tool_done: std::collections::HashSet::new(),
            answer_labelled: false,
            prompt: None,
            visibility: Visibility::default(),
        };
        let draft = Draft::default();
        let mut display = Display::default();
        display.set_visibility(Visibility {
            thinking: false,
            tools: ToolView::Brief,
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
            interactions: Default::default(),
            text: crate::client::TextState::default(),
            tool_done: std::collections::HashSet::new(),
            answer_labelled: false,
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
        draft.motion(Motion::Home, true);
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
            interactions: Default::default(),
            text: crate::client::TextState::default(),
            tool_done: std::collections::HashSet::new(),
            answer_labelled: false,
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
                ui.visibility.tools = ui.visibility.tools.next();
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
        draft.motion(Motion::Left, false);
        draft.motion(Motion::Left, true);
        draft.insert_str("中");
        assert_eq!(draft.text, "a中z");
        draft.undo();
        assert_eq!(draft.text, "a👩‍💻z");
        draft.backspace();
        draft.undo();
        assert_eq!(draft.text, "a👩‍💻z");
    }

    #[test]
    fn pending_batches_update_one_chain_before_any_call_completes() {
        for fullscreen in [false, true] {
            let mut display = Display::new(fullscreen);
            for id in 0..200 {
                display.update_tool(sample_tool(id, "pending"));
                assert_eq!(
                    display.lines.len(),
                    1,
                    "pending calls must not append blank rows"
                );
                assert!(display.tail.is_empty());
            }
            display.set_visibility(Visibility {
                tools: ToolView::Grouped,
                ..Visibility::default()
            });
            assert_eq!(
                plain(&display.wrapped(80)),
                ["▸ 200 tool calls · bash × 200 · 200 pending"]
            );
            for id in 0..200 {
                display.update_tool(sample_tool(id, "running"));
            }
            assert_eq!(
                plain(&display.wrapped(80)),
                ["▸ 200 tool calls · bash × 200 · 200 running"]
            );
        }
    }

    #[test]
    fn all_three_views_survive_out_of_order_completion_and_repeated_toggles() {
        let mut display = Display::new(true);
        display.set_visibility(Visibility {
            tools: ToolView::Grouped,
            ..Visibility::default()
        });
        for id in 0..100 {
            let mut call = sample_tool(id, "pending");
            if id % 2 == 0 {
                call.name = "read".into();
            }
            display.update_tool(call);
        }
        assert!(display
            .wrapped(100)
            .join("")
            .contains("100 tool calls · read × 50, bash × 50"));
        for id in (0..100).rev() {
            let mut call = sample_tool(id, "completed");
            if id % 2 == 0 {
                call.name = "read".into();
            }
            display.update_tool(call);
        }
        for _ in 0..3 {
            display.set_visibility(Visibility {
                thinking: true,
                tools: ToolView::Brief,
            });
            let rows = plain(&display.wrapped(100));
            assert_eq!(rows.len(), 200);
            for (id, pair) in rows.chunks(2).enumerate() {
                assert_eq!(
                    pair,
                    [format!("● bash command_{id}"), format!("  ✓ result_{id}")]
                );
            }
            display.set_visibility(Visibility {
                thinking: true,
                tools: ToolView::Full,
            });
            assert_eq!(display.wrapped(100).len(), 300);
            display.set_visibility(Visibility {
                tools: ToolView::Grouped,
                ..Visibility::default()
            });
            assert_eq!(display.wrapped(100).len(), 1);
        }
    }

    #[test]
    fn native_live_rows_wrap_and_stay_inside_the_terminal_during_generation() {
        let mut display = Display::default();
        display.set_visibility(Visibility {
            thinking: true,
            tools: ToolView::Full,
        });
        let mut call = sample_tool(0, "pending");
        call.full = "● bash echo first\n  echo second · pending".into();
        display.update_tool(call);
        let ui = Ui {
            visibility: display.visibility,
            ..Ui::default()
        };
        for _ in 0..200 {
            display.draw(&ui, &Draft::default(), 80, "mock/m");
            assert_eq!((display.drawn_rows, display.caret_row), (4, 3));
            display.erase();
        }
        let mut call = sample_tool(0, "running");
        call.full = format!("● bash {} · running", "x".repeat(5000));
        display.update_tool(call);
        assert_eq!(display.native_live_rows(20, 5).len(), 5);
        assert_eq!(display.lines.len(), 1);
        assert!(display.tail.is_empty());
    }

    #[test]
    fn text_and_parallel_completions_preserve_other_running_calls() {
        let mut display = Display::new(true);
        display.output("half a sentence", 80);
        display.update_tool(sample_tool(0, "running"));
        display.update_tool(sample_tool(1, "running"));
        display.output("assistant commentary\n", 80);
        display.update_tool(sample_tool(0, "completed"));
        display.set_visibility(Visibility {
            thinking: true,
            tools: ToolView::Brief,
        });
        let rows = display.wrapped(80).join("\n");
        assert!(rows.contains("half a sentence") && rows.contains("assistant commentary"));
        assert!(rows.contains("command_1 · running"));
        assert!(!rows.contains("command_0 · running"));
        assert!(display
            .native_live_rows(80, 20)
            .join("")
            .contains("command_1 · running"));
        display.end_tools();
        display.update_tool(sample_tool(2, "pending"));
        assert_eq!(display.chains.len(), 2);
    }

    #[test]
    fn grouped_failures_stay_visible_and_closed_chains_still_expand() {
        let mut display = Display::new(true);
        display.set_visibility(Visibility {
            tools: ToolView::Grouped,
            ..Visibility::default()
        });
        display.output_as("secret thinking", 80, RowKind::Thinking);
        let mut failed = sample_tool(0, "error");
        failed.brief = "● bash command_0\n  ✗ permission denied".into();
        failed.full = failed.brief.clone();
        display.update_tool(failed);
        display.end_tools();
        display.output("answer\n", 80);
        let grouped = display.export_rows().collect::<Vec<_>>().join("\n");
        assert!(grouped.contains("✗ 1 tool call · bash × 1 · 1 failed"));
        display.set_visibility(Visibility {
            thinking: false,
            tools: ToolView::Full,
        });
        let exported = display.export_rows().collect::<Vec<_>>().join("\n");
        assert!(exported.contains("permission denied") && exported.contains("answer"));
        assert!(!exported.contains("secret thinking"));
    }

    #[test]
    fn history_retains_separate_turns_and_expands_every_saved_result() {
        use crate::types::{MessageInfo, MessageWithParts};
        let message = |id: &str, role: &str, parts| MessageWithParts {
            info: MessageInfo {
                id: id.into(),
                session_id: "s".into(),
                role: role.into(),
                ..Default::default()
            },
            parts,
        };
        let part = |id: &str, name: &str, status: &str| {
            serde_json::json!({
                "id":id,"sessionID":"s","type":"tool","tool":name,
                "state":{"status":status,"input":{"command":format!("command_{id}")},
                    "output":format!("result_{id}\nsecond line"),"error":"permission denied"}
            })
        };
        let history = [
            message(
                "u1",
                "user",
                vec![serde_json::json!({"type":"text","text":"first request"})],
            ),
            message("a1", "assistant", vec![part("t1", "bash", "completed")]),
            message("a2", "assistant", vec![part("t2", "read", "error")]),
            message(
                "u2",
                "user",
                vec![serde_json::json!({"type":"text","text":"second request"})],
            ),
            message("a3", "assistant", vec![part("t3", "bash", "completed")]),
        ];
        let mut display = Display::new(true);
        let mut ui = Ui::default();
        crate::tui::render::render_history(&mut display, &mut ui, &history);
        display.set_visibility(Visibility {
            tools: ToolView::Grouped,
            ..Visibility::default()
        });
        let grouped = display.wrapped(100).join("\n");
        assert!(grouped.contains("2 tool calls · bash × 1, read × 1 · 1 failed"));
        assert!(grouped.contains("1 tool call · bash × 1"));
        assert_eq!(display.chains.len(), 2);
        display.set_visibility(Visibility {
            thinking: true,
            tools: ToolView::Full,
        });
        let full = display.wrapped(100).join("\n");
        for text in [
            "command_t1",
            "result_t1",
            "command_t2",
            "permission denied",
            "command_t3",
            "result_t3",
        ] {
            assert!(full.contains(text), "{text}");
        }
        let duplicate = crate::tui::render::render_part(&mut ui, &part("t1", "bash", "completed"));
        assert!(duplicate.is_empty(), "history must seed live deduplication");
    }

    #[test]
    fn tool_updates_reuse_the_wrapped_transcript_before_the_chain() {
        let mut display = Display::new(true);
        for _ in 0..1000 {
            display.output("previous transcript row\n", 80);
        }
        display.viewport(80);
        let work = display.wrapped.borrow().work;
        for id in 0..100 {
            display.update_tool(sample_tool(id, "pending"));
            display.viewport(80);
            assert_eq!(display.wrapped.borrow().work, work);
        }
    }

    #[test]
    fn streaming_narrative_newlines_do_not_reserve_rows_for_tool_calls() {
        let mut display = Display::new(true);
        display.set_visibility(Visibility {
            tools: ToolView::Grouped,
            ..Visibility::default()
        });
        display.output("I will run fifty calls.\n", 80);
        for _ in 0..150 {
            display.output("\n", 80);
        }
        for id in 0..10 {
            display.update_tool(sample_tool(id, "pending"));
        }
        let rows = plain(&display.wrapped(80));
        assert_eq!(
            rows.len(),
            3,
            "narrative newline tokens must not create a huge gap: {rows:?}"
        );
        assert_eq!(rows[0], "I will run fifty calls.");
        assert!(rows[2].contains("10 tool calls"));
        display.output("Batch one done.\n", 80);
        for _ in 0..150 {
            display.output("\n", 80);
        }
        for id in 10..20 {
            display.update_tool(sample_tool(id, "pending"));
        }
        let rows = plain(&display.wrapped(80));
        assert_eq!(display.chains.len(), 2);
        assert_eq!(rows.len(), 6);
        assert!(rows[5].contains("10 tool calls"));
    }

    #[test]
    fn code_blocks_and_full_tool_output_preserve_their_blank_lines() {
        let mut display = Display::new(true);
        display.output("```rust\nfirst\n\n\nsecond\n```\n", 80);
        // A fenced block's rows carry the gutter, so the blank lines inside it
        // are rows with content and survive the blank-row collapse. The closing
        // marker prints no row, so the block leaves no blank of its own.
        let rows = plain(&display.wrapped(80));
        assert_eq!(rows, ["rust", "│ first", "│", "│", "│ second"]);
        let mut call = sample_tool(0, "completed");
        call.full = "● bash command\n  ✓ first\n\n\n  second".into();
        display.update_tool(call);
        display.set_visibility(Visibility {
            thinking: true,
            tools: ToolView::Full,
        });
        let rows = plain(&display.wrapped(80));
        assert!(rows
            .windows(4)
            .any(|rows| rows == ["  ✓ first", "", "", "  second"]));
    }

    /// Narrative rows are markdown; tool rows are the app's own formatting.
    #[test]
    fn narrative_markdown_renders_as_rows_and_keeps_tool_text_verbatim() {
        let mut display = Display::new(true);
        display.output("# Title\nUse **bold** and `code`.\n", 80);
        assert_eq!(plain(&display.wrapped(80)), ["Title", "Use bold and code."]);
        // A fence inside a tool result is the tool's text, not markdown: the
        // renderer never sees it, so its markers print as typed.
        let mut call = sample_tool(0, "completed");
        call.full = "```rust\nfn main() {}\n```".into();
        display.update_tool(call);
        display.set_visibility(Visibility {
            thinking: true,
            tools: ToolView::Full,
        });
        let rows = plain(&display.wrapped(80));
        assert!(rows.iter().any(|row| row == "```rust"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "```"), "{rows:?}");
    }

    /// Chunk boundaries must not change the transcript: the renderer rewrites
    /// the row it owns, so text reads the same in one piece as in single bytes.
    #[test]
    fn markdown_renders_the_same_rows_whatever_the_stream_chunk_size() {
        const TEXT: &str = "# Title\n\nUse **bold** and `code`.\n\n\
            ```rust\nfn main() {}\n\n```\nthe end\n";
        let mut whole = Display::new(true);
        whole.output(TEXT, 80);
        let expected = plain(&whole.wrapped(80));
        for chunk in [1, 2, 3, 5, 8, 17, TEXT.len()] {
            let mut display = Display::new(true);
            for part in TEXT.as_bytes().chunks(chunk) {
                // The corpus is ASCII, so a byte split is a character split.
                display.output(std::str::from_utf8(part).unwrap(), 80);
            }
            display.finish_line();
            assert_eq!(plain(&display.wrapped(80)), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn thinking_markdown_renders_across_chunks_and_visibility_changes() {
        const TEXT: &str = "# Plan\nUse **bold** and `code`.\n```rust\nfn main() {}\n\n```\nDone.";
        let expected = [
            "◦ thinking",
            "Plan",
            "Use bold and code.",
            "rust",
            "│ fn main() {}",
            "│",
            "Done.",
        ];
        for chunk in [1, 2, 5, 17, TEXT.len()] {
            let mut display = Display::new(true);
            for (index, part) in TEXT.as_bytes().chunks(chunk).enumerate() {
                display.update_thought(
                    ThoughtUpdate {
                        id: "r".into(),
                        message_id: "m".into(),
                        text: std::str::from_utf8(part).unwrap().into(),
                        new: index == 0,
                        reset: false,
                        start: Some(1000),
                        end: Some(3000),
                    },
                    80,
                );
                // Paint partial updates too, so later chunks must invalidate
                // and restyle the existing row when a delimiter closes.
                display.wrapped(80);
            }
            let rows = display.wrapped(80);
            assert_eq!(plain(&rows), expected, "chunk size {chunk}");
            assert!(rows[1].contains(&format!("{BOLD}Plan{RESET}")));
            assert!(rows[2].contains(&format!("{CYAN}code{RESET}")));
            drop(rows);
            display.set_visibility(Visibility {
                thinking: false,
                ..Visibility::default()
            });
            assert_eq!(
                plain(&display.wrapped(80)),
                ["◦ Thought for 2.0s (~16 tokens)"]
            );
            display.set_visibility(Visibility::default());
            assert_eq!(plain(&display.wrapped(80)), expected);
            assert_eq!(plain(&display.export_rows().collect::<Vec<_>>()), expected);
        }
    }

    #[test]
    fn native_thinking_restyles_the_live_line_and_keeps_fences_inside_the_block() {
        let mut display = Display::default();
        let thought = |text: &str, new, reset| ThoughtUpdate {
            id: "r".into(),
            message_id: "m".into(),
            text: text.into(),
            new,
            reset,
            start: None,
            end: None,
        };
        display.update_thought(thought("Use **bo", true, false), 80);
        assert_eq!(unstyled(&display.tail), "Use **bo");
        display.update_thought(thought("ld** and `code`.", false, false), 80);
        assert_eq!(unstyled(&display.tail), "Use bold and code.");
        assert!(display.tail.contains(&format!("{BOLD}bold{RESET}")));
        assert!(display.tail.contains(&format!("{CYAN}code{RESET}")));
        display.update_thought(thought("\n```rust\n\tlet x = 1;", false, false), 80);
        assert_eq!(unstyled(&display.tail), "│   let x = 1;");
        display.update_thought(thought("\t// note", false, false), 80);
        assert_eq!(unstyled(&display.tail), "│   let x = 1;  // note");
        display.update_thought(thought("# Revised", false, true), 80);
        assert_eq!(unstyled(&display.tail), "Revised");
        assert!(display.tail.contains(&format!("{BOLD}Revised{RESET}")));
        display.update_thought(thought("\n```\nnew block", false, false), 80);
        display.update_thought(thought("# Next", true, false), 80);
        assert_eq!(unstyled(&display.tail), "Next");
        display.update_thought(thought("\n```\nlast thought", false, false), 80);
        display.output("**Answer**", 80);
        assert_eq!(unstyled(&display.tail), "Answer");
        assert!(display.tail.contains(&format!("{BOLD}Answer{RESET}")));
    }

    /// The renderer owns the row under construction, so a line interrupted by a
    /// tool call has to be written out whole, not dropped.
    #[test]
    fn narrative_stops_at_a_tool_call_and_survives_its_row_being_rewritten() {
        let mut display = Display::new(true);
        display.output("Running **the command** now", 80);
        display.update_tool(sample_tool(0, "completed"));
        let rows = plain(&display.wrapped(80));
        assert_eq!(
            rows[0], "Running the command now",
            "an interrupted line keeps its text and its styling: {rows:?}"
        );
        // The narrative resumes after the chain: the line starts over, and the
        // renderer carries nothing of the interrupted half into the next row.
        display.output(" and **done**.\n", 80);
        display.update_tool(sample_tool(1, "completed"));
        let rows = plain(&display.wrapped(80));
        assert!(rows.iter().any(|row| row == " and done."), "{rows:?}");
    }

    #[test]
    fn tool_groups_stop_at_thinking_and_assistant_messages_in_every_view() {
        let mut display = Display::new(true);
        display.output("Running batch one.\n", 80);
        for id in 0..10 {
            display.update_tool(sample_tool(id, "completed"));
        }
        display.apply(
            vec![
                Row::Thought(ThoughtUpdate {
                    id: "r1".into(),
                    message_id: "m1".into(),
                    text: "Need the next batch.".into(),
                    new: true,
                    reset: false,
                    start: Some(1000),
                    end: Some(3500),
                }),
                Row::ThoughtTokens {
                    message_id: "m1".into(),
                    tokens: 42,
                },
            ],
            80,
        );
        for id in 10..20 {
            display.update_tool(sample_tool(id, "completed"));
        }
        display.output("Batch two done; running batch three.\n", 80);
        for id in 20..30 {
            display.update_tool(sample_tool(id, "completed"));
        }
        display.output("Finished.\n", 80);
        assert_eq!(display.chains.len(), 3);
        for thinking in [true, false] {
            display.set_visibility(Visibility {
                thinking,
                tools: ToolView::Grouped,
            });
            let rows = plain(&display.wrapped(120));
            let groups: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.contains("10 tool calls"))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(groups.len(), 3, "{rows:?}");
            let thought = rows
                .iter()
                .position(|row| {
                    row.contains(if thinking {
                        "Need the next batch."
                    } else {
                        "Thought for 2.5s (42 tokens)"
                    })
                })
                .unwrap();
            let message = rows
                .iter()
                .position(|row| row.contains("Batch two done"))
                .unwrap();
            assert!(groups[0] < thought && thought < groups[1]);
            assert!(groups[1] < message && message < groups[2]);
            assert!(!rows.iter().any(|row| row.contains("30 tool calls")));
            if !thinking {
                assert!(!rows.iter().any(|row| row.contains("Need the next batch.")));
            }
        }
    }

    #[test]
    fn reasoning_updates_one_summary_then_uses_reported_tokens() {
        let mut display = Display::new(true);
        display.set_visibility(Visibility {
            thinking: false,
            tools: ToolView::Grouped,
        });
        for (text, new, end) in [
            ("Planning", true, None),
            (" the work", false, None),
            ("", false, Some(2400)),
        ] {
            display.apply(
                vec![Row::Thought(ThoughtUpdate {
                    id: "r".into(),
                    message_id: "m".into(),
                    text: text.into(),
                    new,
                    reset: false,
                    start: Some(1000),
                    end,
                })],
                80,
            );
        }
        let rows = plain(&display.wrapped(80));
        assert_eq!(rows, ["◦ Thought for 1.4s (~5 tokens)"]);
        display.apply(
            vec![Row::ThoughtTokens {
                message_id: "m".into(),
                tokens: 17,
            }],
            80,
        );
        assert_eq!(
            plain(&display.wrapped(80)),
            ["◦ Thought for 1.4s (17 tokens)"]
        );
        display.set_visibility(Visibility::default());
        assert_eq!(
            plain(&display.wrapped(80)),
            ["◦ thinking", "Planning the work"]
        );
        assert_eq!(display.lines.len(), 1);
    }

    #[test]
    fn mixed_batch_events_and_saved_history_have_the_same_compact_chronology() {
        use crate::types::{BusEvent, MessageInfo, MessageWithParts, Tokens};
        let mut live = Display::new(true);
        let mut ui = Ui::default();
        let mut history = Vec::new();
        let event = |kind: &str, properties| BusEvent {
            id: "event".into(),
            kind: serde_json::from_value(serde_json::json!(kind)).unwrap(),
            properties,
        };
        for batch in 0..5 {
            let message_id = format!("message_{batch}");
            let reasoning_id = format!("reasoning_{batch}");
            let text_id = format!("text_{batch}");
            let mut parts = Vec::new();
            let mut reasoning = serde_json::json!({"id":reasoning_id,"messageID":message_id,"sessionID":"s",
                "type":"reasoning","text":"","time":{"start":1000,"end":3000}});
            live.apply(
                crate::tui::render::render_event(
                    &mut ui,
                    &event(
                        "message.part.updated",
                        serde_json::json!({"part":reasoning}),
                    ),
                    "s",
                ),
                120,
            );
            let reasoning_text = format!(
                "# Thinking about batch {batch}.\nUse **bold** and `code`.\n```rust\nlet x = 1;\n```{}",
                "\n".repeat(100)
            );
            live.apply(crate::tui::render::render_event(&mut ui,&event("message.part.delta",serde_json::json!({"sessionID":"s","messageID":message_id,"partID":reasoning_id,"delta":reasoning_text})),"s"),120);
            reasoning["text"] = serde_json::json!(reasoning_text);
            parts.push(reasoning);
            let mut text = serde_json::json!({"id":text_id,"messageID":message_id,"sessionID":"s","type":"text","text":""});
            live.apply(
                crate::tui::render::render_event(
                    &mut ui,
                    &event("message.part.updated", serde_json::json!({"part":text})),
                    "s",
                ),
                120,
            );
            let text_body = format!("Running batch {batch}.{}", "\n".repeat(100));
            for delta in [format!("Running batch {batch}."), "\n".repeat(100)] {
                live.apply(crate::tui::render::render_event(&mut ui,&event("message.part.delta",serde_json::json!({"sessionID":"s","messageID":message_id,"partID":text_id,"delta":delta})),"s"),120);
            }
            text["text"] = serde_json::json!(text_body);
            parts.push(text);
            for call in 0..10 {
                let part = serde_json::json!({"id":format!("tool_{batch}_{call}"),"messageID":message_id,"sessionID":"s","type":"tool","tool":"bash",
                    "state":{"status":"completed","input":{"command":format!("echo {batch}_{call}")},"output":format!("result {batch}_{call}")}});
                live.apply(
                    crate::tui::render::render_event(
                        &mut ui,
                        &event("message.part.updated", serde_json::json!({"part":part})),
                        "s",
                    ),
                    120,
                );
                parts.push(part);
            }
            let finish = serde_json::json!({"id":format!("finish_{batch}"),"messageID":message_id,"sessionID":"s","type":"step-finish","tokens":{"reasoning":42}});
            live.apply(
                crate::tui::render::render_event(
                    &mut ui,
                    &event("message.part.updated", serde_json::json!({"part":finish})),
                    "s",
                ),
                120,
            );
            parts.push(finish);
            history.push(MessageWithParts {
                info: MessageInfo {
                    id: message_id,
                    session_id: "s".into(),
                    role: "assistant".into(),
                    tokens: Some(Tokens {
                        reasoning: 42,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                parts,
            });
        }
        let mut replay = Display::new(true);
        crate::tui::render::render_history(&mut replay, &mut Ui::default(), &history);
        for thinking in [true, false] {
            for tools in [ToolView::Grouped, ToolView::Brief, ToolView::Full] {
                let visibility = Visibility { thinking, tools };
                live.set_visibility(visibility);
                replay.set_visibility(visibility);
                let rows = plain(&live.wrapped(120));
                assert_eq!(
                    rows,
                    plain(&replay.wrapped(120)),
                    "history drift: {visibility:?}"
                );
                assert!(!rows
                    .windows(2)
                    .any(|pair| pair[0].trim().is_empty() && pair[1].trim().is_empty()));
                if tools == ToolView::Grouped {
                    assert_eq!(
                        rows.iter()
                            .filter(|row| row.contains("10 tool calls"))
                            .count(),
                        5
                    );
                }
                if thinking {
                    assert_eq!(
                        rows.iter()
                            .filter(|row| *row == "Use bold and code.")
                            .count(),
                        5
                    );
                    assert_eq!(rows.iter().filter(|row| *row == "│ let x = 1;").count(), 5);
                } else {
                    assert_eq!(
                        rows.iter()
                            .filter(|row| row.contains("Thought for 2.0s (42 tokens)"))
                            .count(),
                        5
                    );
                }
            }
        }
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
            tools: ToolView::Brief,
        });
        display.viewport(10);
        assert_eq!(display.wrapped.borrow().work, resized + 8001);
        display.clear_screen();
        assert!(display.viewport(10).is_empty());
    }
    #[test]
    fn transcript_text_carries_only_the_apps_own_styling() {
        let mut display = Display::new(true);
        // A model, a tool, a fetched page, an MCP server, a hook, a session
        // title or a path cannot restyle the transcript: SGR outside the app's
        // list is dropped, and a sequence that is not SGR dies at its
        // introducer instead of reaching a terminal emulator.
        display.output("\x1b[8mconcealed\x1b[2Jtail\x1b[0m\n", 80);
        assert_eq!(plain(&display.wrapped(80)), ["concealedtail"]);
        // Nothing is left to replay: a dropped sequence must not leave the
        // transcript styled from here to the end of the turn, because each
        // wrapped row re-seeds itself from the style of the row before it.
        assert!(display.style.is_empty());
        display.output("\x1b[1mapp styling survives\x1b[0m\n", 80);
        assert!(display.wrapped(80)[1].contains(BOLD));
    }

    #[test]
    fn blocks_separate_by_one_blank_row_and_the_transcript_opens_without_one() {
        let mut display = Display::new(true);
        display.set_visibility(Visibility {
            tools: ToolView::Brief,
            ..Default::default()
        });
        // A separator at the top of the transcript is wasted space, and two in
        // a row is the spacing the transcript used to drift into.
        display.ensure_blank();
        display.user_prompt("read src/main.rs", 80);
        display.apply(vec![Row::Blank, Row::Blank], 80);
        display.apply(vec![Row::Text(RowKind::Plain, "assistant\n".into())], 80);
        display.apply(vec![Row::Text(RowKind::Plain, "I read it.\n".into())], 80);
        display.apply(vec![Row::Tool(sample_tool(0, "completed"))], 80);
        display.apply(vec![Row::EndTools, Row::Blank], 80);
        display.apply(vec![Row::Text(RowKind::Plain, "next\n".into())], 80);
        assert_eq!(
            plain(&display.wrapped(80)),
            [
                "❯ user",
                "  read src/main.rs",
                "",
                "assistant",
                "I read it.",
                "● bash command_0",
                "  ✓ result_0",
                "",
                "next",
            ]
        );
    }

    #[test]
    fn one_label_marks_one_answer_not_one_per_step() {
        use crate::events::EventKind;
        use crate::types::BusEvent;
        let mut display = Display::new(true);
        let mut ui = Ui::default();
        let event = |kind: &str, properties: serde_json::Value| BusEvent {
            id: "event".into(),
            kind: serde_json::from_value::<EventKind>(serde_json::json!(kind)).unwrap(),
            properties,
        };
        let say = |ui: &mut Ui, display: &mut Display, message: &str, id: &str| {
            let rows = crate::tui::render::render_event(
                ui,
                &event(
                    "message.part.updated",
                    serde_json::json!({"part": {
                        "id": id, "messageID": message, "sessionID": "s",
                        "type": "text", "text": "part text\n",
                    }}),
                ),
                "s",
            );
            display.apply(rows, 80);
        };
        let labels = |display: &Display| {
            plain(&display.wrapped(80))
                .iter()
                .filter(|row| row.as_str() == "assistant")
                .count()
        };
        // A turn runs one model step per tool batch, every step is its own
        // assistant message, and each part is the start of a part, not of an
        // answer. Three steps of one turn get one label.
        say(&mut ui, &mut display, "step_1", "text_1");
        say(&mut ui, &mut display, "step_1", "text_2");
        say(&mut ui, &mut display, "step_2", "text_3");
        say(&mut ui, &mut display, "step_3", "text_4");
        assert_eq!(labels(&display), 1, "one label marks one whole answer");
        // The label's row ends it, so the model's leading newline cannot push
        // its text a row below the label that marks it.
        assert_eq!(
            plain(&display.wrapped(80)),
            [
                "assistant",
                "part text",
                "part text",
                "part text",
                "part text"
            ]
        );
        // The next prompt starts the next answer, and its label with it.
        display.apply(
            crate::tui::render::render_event(
                &mut ui,
                &event(
                    "message.updated",
                    serde_json::json!({"info": {
                        "id": "u1", "role": "user", "sessionID": "s",
                    }}),
                ),
                "s",
            ),
            80,
        );
        say(&mut ui, &mut display, "step_4", "text_5");
        assert_eq!(labels(&display), 2, "a new prompt labels a new answer");
    }
}
