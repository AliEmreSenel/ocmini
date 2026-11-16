//! Streaming markdown for narrative and thinking transcript rows.
//!
//! Model text arrives in chunks that split anywhere, so the renderer keeps the
//! current line's raw text and re-renders the whole line for every chunk. That
//! is what makes a stream and the same text replayed from history read
//! identically, and what lets a marker that closes late style the text before
//! it. Spans are resolved within one line, which is the most a terminal row
//! shows, and styling uses only the sequences in [`super::STYLES`], so the
//! transcript's escape allowlist accepts the result.
//!
//! ## What is not rendered
//!
//! Tables print as typed. Aligning columns needs every cell's width before the
//! first row prints, which a stream does not have: in native mode a committed
//! row goes to the terminal's scrollback and can never be rewritten, and in
//! full-screen rows re-wrap at paint width, so alignment computed at commit
//! time breaks on resize. Buffering the whole block to lay it out is the only
//! way to align it, and that trades streaming for a table that appears in one
//! piece - and a table at the end of a cut-off answer never prints at all. A
//! model's own `| a | b |` padding reads fine in a monospace transcript, so
//! the pipeline leaves tables alone.

use super::{BOLD, CYAN, DIM, RESET};

/// A rendered chunk: whole rows, plus the row the stream is still building.
pub(super) struct Rendered {
    /// Text to write, ending with the current row's rendering.
    pub text: String,
    /// Bytes of `text` that belong to the unfinished row. The display replaces
    /// that part of its tail when the next chunk arrives, so a marker that
    /// closes late can restyle the text that came before it.
    pub line: usize,
}

/// Model text as it arrives. Fenced code gets a gutter, `#` headings and
/// `**bold**` / `` `code` `` spans get the app's styling, and a delimiter with
/// no closing match prints as typed.
#[derive(Default)]
pub(super) struct Markdown {
    /// The line the stream is in the middle of, as it was sent.
    line: String,
    /// The marker the open code block started with, and how many of it.
    fence: Option<(char, usize)>,
}

impl Markdown {
    /// Render one chunk of model text.
    pub fn push(&mut self, text: &str) -> Rendered {
        let source = std::mem::take(&mut self.line) + text;
        let mut out = String::new();
        let mut rest = source.as_str();
        while let Some(index) = rest.find('\n') {
            // A line that prints no row consumes its newline with it, so a
            // closing fence leaves no blank row in the transcript.
            if Self::row(&mut self.fence, &rest[..index], &mut out) {
                out.push('\n');
            }
            rest = &rest[index + 1..];
        }
        // The unfinished line renders too: it is the row under the caret. Its
        // fences are previewed on a copy, because a partial ``` marker is not
        // a fence: the whole line that completes it opens the block. An empty
        // remainder is a line boundary: no row is under the caret yet.
        let start = out.len();
        if !rest.is_empty() {
            let mut preview = self.fence;
            Self::row(&mut preview, rest, &mut out);
        }
        let line = out.len() - start;
        self.line = rest.to_string();
        Rendered { text: out, line }
    }

    /// Render the line the stream stopped in, for the end of an answer or a
    /// tool chain that interrupts it. Text is never lost to a missing newline.
    pub fn finish(&mut self) -> String {
        let line = std::mem::take(&mut self.line);
        let mut out = String::new();
        if !line.is_empty() {
            Self::row(&mut self.fence, &line, &mut out);
        }
        out
    }

    /// Drop the line and fence state: the text stream restarted.
    pub fn restart(&mut self) {
        self.line.clear();
        self.fence = None;
    }

    /// Stop owning the current line, keeping the open fence. The line's row
    /// wrapped into the scrollback, where rows cannot be rewritten, so
    /// re-rendering it would print part of it a second time.
    pub fn restart_line(&mut self) {
        self.line.clear();
    }

    /// Render one line. False means the line prints no row, so its newline is
    /// consumed with it: that is true of a fence marker standing alone. A blank
    /// prose line prints a blank row, which is the transcript's rhythm.
    fn row(fence: &mut Option<(char, usize)>, line: &str, out: &mut String) -> bool {
        let text = line.trim();
        if let Some((mark, count)) = *fence {
            // Inside a block every row carries the gutter, so a blank line of
            // code is a row with a gutter in it and survives the transcript's
            // blank-line collapse. The closing marker prints nothing.
            if fence_marker(text)
                .is_some_and(|(m, c, info)| m == mark && c >= count && info.is_empty())
            {
                *fence = None;
                return false;
            }
            if text.is_empty() {
                out.push_str(&format!("{DIM}│{RESET}"));
            } else {
                out.push_str(&format!("{DIM}│{RESET} {line}"));
            }
            return true;
        }
        if let Some((mark, count, language)) = fence_marker(text) {
            *fence = Some((mark, count));
            // The language is the only part of the opening marker worth keeping.
            if language.is_empty() {
                return false;
            }
            out.push_str(&format!("{BOLD}{language}{RESET}"));
            return true;
        }
        let hashes = text.chars().take_while(|ch| *ch == '#').count();
        if (1..=6).contains(&hashes) && text[hashes..].starts_with(' ') {
            out.push_str(&format!("{BOLD}{}{RESET}", text[hashes..].trim_start()));
            return true;
        }
        inline(line, out);
        true
    }
}

/// A fence marker: three of the same character, then anything but that
/// character. On an opening line the rest is the language; on a closing line it
/// has to be empty. Markers are ASCII, so byte and character offsets agree.
fn fence_marker(text: &str) -> Option<(char, usize, &str)> {
    let mark = text.chars().next().filter(|ch| matches!(ch, '`' | '~'))?;
    let count = text.chars().take_while(|ch| *ch == mark).count();
    let rest = &text[count..];
    (count >= 3 && !rest.contains(mark)).then_some((mark, count, rest.trim()))
}

/// Inline spans: `**strong**` and `` `code` ``. A lone `*` is emphasis, shown
/// the same way as strong, because the transcript has no italic style.
fn inline(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(index) = rest.find(['`', '*']) {
        out.push_str(&rest[..index]);
        let span = if rest[index..].starts_with("**") {
            "**"
        } else {
            &rest[index..index + 1]
        };
        let style = if span == "`" { CYAN } else { BOLD };
        let body = &rest[index + span.len()..];
        if let Some(end) = body.find(span) {
            let styled = &body[..end];
            // Prose asterisks (`2 * 3`) and padded spans print as typed, so a
            // stray marker cannot restyle half a paragraph.
            if !styled.is_empty() && !styled.starts_with(' ') && !styled.ends_with(' ') {
                out.push_str(&format!("{style}{styled}{RESET}"));
                rest = &body[end + span.len()..];
                continue;
            }
        }
        out.push_str(span);
        rest = body;
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip the styling so assertions can read the text.
    fn plain(text: &str) -> String {
        let mut rest = text;
        let mut out = String::new();
        while !rest.is_empty() {
            if let Some(index) = rest.find('\x1b') {
                out.push_str(&rest[..index]);
                rest = &rest[index + 1..];
                // Skip a CSI introducer, then the sequence's final byte.
                rest = rest.strip_prefix('[').unwrap_or(rest);
                rest = match rest.find(|ch: char| ('@'..='~').contains(&ch)) {
                    Some(end) => &rest[end + 1..],
                    None => "",
                };
            } else {
                let ch = rest.chars().next().unwrap();
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
        out
    }

    fn render(text: &str) -> String {
        let rendered = Markdown::default().push(text);
        assert_eq!(rendered.line, 0, "a whole line leaves no unfinished row");
        plain(&rendered.text)
    }

    #[test]
    fn fenced_code_becomes_a_gutter_and_keeps_its_blank_lines() {
        assert_eq!(
            render("```rust\nfn main() {}\n\n```\nafter\n"),
            "rust\n│ fn main() {}\n│\nafter\n"
        );
        // A fence with no language prints no label row.
        assert_eq!(render("```\nx\n```\n"), "│ x\n");
        // A fence left open keeps everything after it as code: that is what
        // markdown says, and it is visible as a gutter.
        assert_eq!(render("```\nnever closed\n"), "│ never closed\n");
    }

    #[test]
    fn headings_and_inline_spans_take_the_apps_styling() {
        let out = Markdown::default()
            .push("# Title\nUse **bold** and `code`.\n")
            .text;
        assert!(out.starts_with(&format!("{BOLD}Title{RESET}")));
        assert!(out.contains(&format!("{BOLD}bold{RESET}")));
        assert!(out.contains(&format!("{CYAN}code{RESET}")));
        assert_eq!(plain(&out), "Title\nUse bold and code.\n");
    }

    #[test]
    fn a_marker_with_no_match_prints_as_typed() {
        assert_eq!(
            render("2 * 3 * 4 and a lone ` marker\n"),
            "2 * 3 * 4 and a lone ` marker\n"
        );
        assert_eq!(render("**never closed\n"), "**never closed\n");
        // A padded span is not a span: markdown requires the delimiters to sit
        // against the text they style.
        assert_eq!(render("** padded ** stays\n"), "** padded ** stays\n");
    }

    #[test]
    fn code_spans_do_not_stylize_their_own_contents() {
        let out = Markdown::default().push("use `let x = **a**` here\n").text;
        assert_eq!(plain(&out), "use let x = **a** here\n");
        assert!(out.contains(&format!("{CYAN}let x = **a**{RESET}")));
    }

    #[test]
    fn splitting_the_stream_anywhere_renders_the_same_rows() {
        const TEXT: &str = "# Title\n\nSome **bold** text with `code` and\n\
            ```rust\nfn main() {}\n\n```\nthe end\n";
        let whole = Markdown::default().push(TEXT).text;
        for chunk in [1, 2, 3, 5, 8, 17, TEXT.len()] {
            let mut markdown = Markdown::default();
            let mut out = String::new();
            for part in TEXT.as_bytes().chunks(chunk) {
                // The corpus is ASCII, so a byte split is a character split.
                let rendered = markdown.push(std::str::from_utf8(part).unwrap());
                // Drop the row under construction: only finished rows survive.
                out.push_str(&rendered.text[..rendered.text.len() - rendered.line]);
            }
            out.push_str(&markdown.finish());
            assert_eq!(out, whole, "chunk size {chunk}");
        }
    }

    #[test]
    fn the_current_line_is_rendered_while_it_is_still_arriving() {
        let mut markdown = Markdown::default();
        let first = markdown.push("half a **bold");
        assert_eq!(plain(&first.text), "half a **bold");
        assert_eq!(first.line, first.text.len());
        // The closing marker restyles the word that was already on screen.
        let second = markdown.push("** word\n");
        assert_eq!(
            plain(&second.text),
            "half a bold word\n",
            "the line renders once, not twice"
        );
        assert_eq!(second.line, 0);
    }
}
