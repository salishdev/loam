//! Word wrapping for terminal output, including text that arrives in chunks
//! (streamed narration).
//!
//! Lines break at spaces so no line runs past the width, unless one word is
//! longer than the width. A wrapped line's continuation keeps the leading
//! indentation of the line it came from, so indented lists stay aligned.
//! Existing newlines are kept; trailing spaces before a break are dropped.

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

/// Wraps a whole text at `width` columns.
pub fn wrap(text: &str, width: usize) -> String {
    let mut w = Wrapper::new(width);
    let mut out = w.push(text);
    out.push_str(&w.finish());
    out
}

/// A streaming word wrapper: feed it chunks with [`push`](Self::push), print
/// what each call returns, and print [`finish`](Self::finish) at the end.
/// A word split across chunks is held back until it is complete.
#[derive(Debug)]
pub struct Wrapper {
    width: usize,
    /// Column the next character lands in.
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
        Wrapper {
            width: width.max(1),
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
                    self.col = 0;
                    self.spaces = 0;
                    self.indent.clear();
                    self.at_line_start = true;
                }
                ' ' | '\t' if self.at_line_start => {
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
        let len = self.word.chars().count();
        let indent = self.indent_width();
        if self.col > indent && self.col + self.spaces + len > self.width {
            out.push('\n');
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
