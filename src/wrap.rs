//! Word wrapping for terminal output, including text that arrives in chunks
//! (streamed narration).
//!
//! Lines break at spaces so no line runs past the width, unless one word is
//! longer than the width. A wrapped line's continuation keeps the leading
//! indentation of the line it came from, so indented lists stay aligned.
//! Existing newlines are kept; trailing spaces before a break are dropped.
//! A wrapper can also put a left margin before every line that has text.

/// The widest text the REPL prints, in columns, however wide the terminal.
pub const MAX_WIDTH: usize = 80;

/// The width to wrap at: `LOAM_WIDTH` if set to a number (0 turns wrapping
/// off), else the terminal's width capped at [`MAX_WIDTH`], else
/// [`MAX_WIDTH`] when stdout isn't a terminal.
pub fn output_width() -> usize {
    if let Some(w) = std::env::var("LOAM_WIDTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return if w == 0 { usize::MAX } else { w };
    }
    terminal_size::terminal_size()
        .map(|(w, _)| usize::from(w.0))
        .filter(|&w| w > 0)
        .map_or(MAX_WIDTH, |w| w.min(MAX_WIDTH))
}

/// The REPL's left margin in columns when `LOAM_MARGIN` isn't set.
pub const DEFAULT_MARGIN: usize = 2;

/// The left margin for REPL output: `LOAM_MARGIN` if set to a number, else
/// [`DEFAULT_MARGIN`]. The prompt sits in this margin, so input and output
/// start in different columns.
pub fn output_margin() -> usize {
    std::env::var("LOAM_MARGIN")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MARGIN)
}

/// Wraps a whole text at `width` columns.
pub fn wrap(text: &str, width: usize) -> String {
    wrap_with_margin(text, width, 0)
}

/// Wraps a whole text at `width` columns, `margin` of which are a left margin.
pub fn wrap_with_margin(text: &str, width: usize, margin: usize) -> String {
    let mut w = Wrapper::with_margin(width, margin);
    let mut out = w.push(text);
    out.push_str(&w.finish());
    out
}

/// A streaming word wrapper: feed it chunks with [`push`](Self::push), print
/// what each call returns, and print [`finish`](Self::finish) at the end.
/// A word split across chunks is held back until it is complete.
#[derive(Debug)]
pub struct Wrapper {
    /// Width of the text, not counting the margin.
    width: usize,
    /// Spaces put before each line that has text; blank lines stay empty.
    margin: String,
    /// The margin has been printed on the current line.
    margined: bool,
    /// Column the next character lands in, not counting the margin.
    col: usize,
    /// The current line's leading whitespace, repeated on continuations.
    indent: String,
    /// Still reading the current line's leading whitespace.
    at_line_start: bool,
    /// Spaces seen since the last word, printed only if another word follows
    /// on the same line.
    spaces: usize,
    /// The word being read, not yet printed.
    word: String,
}

impl Wrapper {
    pub fn new(width: usize) -> Self {
        Self::with_margin(width, 0)
    }

    /// A wrapper whose lines start with `margin` spaces; the text wraps in
    /// the `width - margin` columns that are left.
    pub fn with_margin(width: usize, margin: usize) -> Self {
        Wrapper {
            width: width.saturating_sub(margin).max(1),
            margin: " ".repeat(margin),
            margined: false,
            col: 0,
            indent: String::new(),
            at_line_start: true,
            spaces: 0,
            word: String::new(),
        }
    }

    /// Takes the next chunk; returns the text that is ready to print.
    pub fn push(&mut self, chunk: &str) -> String {
        let mut out = String::new();
        for c in chunk.chars() {
            match c {
                '\n' => {
                    self.place_word(&mut out);
                    out.push('\n');
                    self.margined = false;
                    self.col = 0;
                    self.spaces = 0;
                    self.indent.clear();
                    self.at_line_start = true;
                }
                ' ' | '\t' if self.at_line_start => {
                    self.put_margin(&mut out);
                    out.push(c);
                    self.indent.push(c);
                    self.col += if c == '\t' { 8 - self.col % 8 } else { 1 };
                }
                ' ' | '\t' => {
                    self.place_word(&mut out);
                    self.spaces += 1;
                }
                _ => {
                    self.at_line_start = false;
                    self.word.push(c);
                }
            }
        }
        out
    }

    /// Returns whatever is still held back: the last word, if any.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        self.place_word(&mut out);
        self.spaces = 0;
        out
    }

    /// Prints the margin if this line doesn't have it yet.
    fn put_margin(&mut self, out: &mut String) {
        if !self.margined {
            out.push_str(&self.margin);
            self.margined = true;
        }
    }

    fn indent_width(&self) -> usize {
        self.indent
            .chars()
            .fold(0, |col, c| col + if c == '\t' { 8 - col % 8 } else { 1 })
    }

    /// Prints the held word, breaking the line first if it wouldn't fit.
    fn place_word(&mut self, out: &mut String) {
        if self.word.is_empty() {
            return;
        }
        self.put_margin(out);
        let len = self.word.chars().count();
        let indent = self.indent_width();
        if self.col > indent && self.col + self.spaces + len > self.width {
            out.push('\n');
            out.push_str(&self.margin);
            out.push_str(&self.indent);
            self.col = indent;
        } else {
            out.extend(std::iter::repeat_n(' ', self.spaces));
            self.col += self.spaces;
        }
        self.spaces = 0;
        out.push_str(&self.word);
        self.col += len;
        self.word.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaks_at_spaces_before_the_width() {
        assert_eq!(
            wrap("the quick brown fox jumps over", 10),
            "the quick\nbrown fox\njumps over"
        );
    }

    #[test]
    fn keeps_newlines_and_short_lines() {
        assert_eq!(wrap("a b\n\nc d\n", 10), "a b\n\nc d\n");
    }

    #[test]
    fn a_word_longer_than_the_width_overflows_on_its_own_line() {
        assert_eq!(wrap("ab abcdefghijkl cd", 5), "ab\nabcdefghijkl\ncd");
    }

    #[test]
    fn continuations_keep_the_line_indent() {
        assert_eq!(
            wrap("  #3 a long rule line\nnext", 12),
            "  #3 a long\n  rule line\nnext"
        );
    }

    #[test]
    fn drops_trailing_spaces_at_breaks() {
        assert_eq!(wrap("aaaa    bbbb", 6), "aaaa\nbbbb");
        assert_eq!(wrap("a  \nb", 6), "a\nb");
    }

    #[test]
    fn streamed_chunks_wrap_like_the_whole_text() {
        let text = "The mill wheel groans. Water pours through the sluice,\n\
                    black and cold, and somewhere above a door bangs twice.";
        for size in 1..12 {
            let mut w = Wrapper::new(20);
            let chars: Vec<char> = text.chars().collect();
            let mut out = String::new();
            for chunk in chars.chunks(size) {
                out.push_str(&w.push(&chunk.iter().collect::<String>()));
            }
            out.push_str(&w.finish());
            assert_eq!(out, wrap(text, 20), "chunk size {size}");
        }
        assert!(wrap(text, 20).lines().all(|l| l.chars().count() <= 20));
    }

    #[test]
    fn a_margin_starts_every_line_with_text_and_narrows_the_width() {
        assert_eq!(
            wrap_with_margin("the quick brown fox\n\n  jumps over it", 12, 2),
            "  the quick\n  brown fox\n\n    jumps\n    over it"
        );
        let text = "The mill wheel groans. Water pours through the sluice.";
        assert!(
            wrap_with_margin(text, 20, 3)
                .lines()
                .all(|l| l.starts_with("   ") && l.chars().count() <= 20)
        );
    }

    #[test]
    fn a_streamed_margin_matches_the_whole_text() {
        let text = "one two three four five\nsix seven eight";
        for size in 1..8 {
            let mut w = Wrapper::with_margin(12, 2);
            let chars: Vec<char> = text.chars().collect();
            let mut out = String::new();
            for chunk in chars.chunks(size) {
                out.push_str(&w.push(&chunk.iter().collect::<String>()));
            }
            out.push_str(&w.finish());
            assert_eq!(out, wrap_with_margin(text, 12, 2), "chunk size {size}");
        }
    }

    #[test]
    fn spaces_across_a_finish_are_kept_between_words() {
        // Narration streams, then the REPL calls finish; a later stream on the
        // same wrapper starts fresh after the newline the REPL prints.
        let mut w = Wrapper::new(80);
        let mut out = w.push("one ");
        out.push_str(&w.push("two"));
        out.push_str(&w.finish());
        assert_eq!(out, "one two");
    }
}
