//! The text: positions, the selection, the line buffer, and mapping
//! between bytes, screen columns (tabs) and UTF-16 (the IME, servers).

use super::*;

/// A place in the text: a line, and a byte offset in it (on a char
/// boundary).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub(super) fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// The selection: `head` is where the caret is, `anchor` where it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) anchor: Pos,
    pub(super) head: Pos,
}

impl Selection {
    pub(super) fn cursor(pos: Pos) -> Self {
        Self { anchor: pos, head: pos }
    }

    pub(super) fn range(&self) -> Range<Pos> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// The file's text, a line at a time (without line endings).
pub(super) struct Buffer {
    pub(super) lines: Vec<String>,
}

impl Buffer {
    pub(super) fn new(text: &str) -> Self {
        Self {
            lines: text.split('\n').map(String::from).collect(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn line(&self, ix: usize) -> &str {
        self.lines.get(ix).map_or("", String::as_str)
    }

    pub(super) fn end(&self) -> Pos {
        let last = self.lines.len() - 1;
        Pos::new(last, self.lines[last].len())
    }

    pub(super) fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        Pos::new(line, snap(&self.lines[line], pos.col))
    }

    pub(super) fn slice(&self, range: Range<Pos>) -> String {
        let (start, end) = (range.start, range.end);
        if start.line == end.line {
            return self.lines[start.line][start.col..end.col].to_string();
        }
        let mut text = self.lines[start.line][start.col..].to_string();
        for line in &self.lines[start.line + 1..end.line] {
            text.push('\n');
            text.push_str(line);
        }
        text.push('\n');
        text.push_str(&self.lines[end.line][..end.col]);
        text
    }

    /// Replaces `range` with `text`; returns where the new text ends.
    pub(super) fn replace(&mut self, range: Range<Pos>, text: &str) -> Pos {
        let (start, end) = (range.start, range.end);
        let head = &self.lines[start.line][..start.col];
        let tail = &self.lines[end.line][end.col..];
        let mut new: Vec<String> = text.split('\n').map(String::from).collect();
        let last = new.len() - 1;
        let end_pos = Pos::new(
            start.line + last,
            if last == 0 { start.col + new[0].len() } else { new[last].len() },
        );
        new[0].insert_str(0, head);
        new[last].push_str(tail);
        self.lines.splice(start.line..=end.line, new);
        end_pos
    }

    pub(super) fn text(&self, line_ending: &str) -> String {
        self.lines.join(line_ending)
    }

    /// The position of byte `offset` in the text joined with `\n`.
    pub(super) fn pos_at(&self, mut offset: usize) -> Pos {
        for (ix, line) in self.lines.iter().enumerate() {
            if offset <= line.len() {
                return Pos::new(ix, snap(line, offset));
            }
            offset -= line.len() + 1;
        }
        self.end()
    }
}

/// Where inserting `text` at `start` ends.
pub(super) fn end_after(start: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos::new(start.line, start.col + text.len()),
        Some(ix) => Pos::new(start.line + text.matches('\n').count(), text.len() - ix - 1),
    }
}

/// `ix`, moved back to a char boundary of `line` (and into it).
pub(super) fn snap(line: &str, ix: usize) -> usize {
    let mut ix = ix.min(line.len());
    while !line.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

/// What a line shows: tabs as spaces.
pub(super) fn display_text(line: &str) -> String {
    if line.contains('\t') {
        line.replace('\t', &" ".repeat(TAB_WIDTH))
    } else {
        line.to_string()
    }
}

/// A byte offset in `line` as an offset in its display text.
pub(super) fn to_display(line: &str, col: usize) -> usize {
    col + (TAB_WIDTH - 1) * line.as_bytes()[..col].iter().filter(|&&b| b == b'\t').count()
}

/// An offset in `line`'s display text back in the line (inside a tab
/// snaps to its nearer side).
pub(super) fn from_display(line: &str, display: usize) -> usize {
    let mut at = 0;
    for (ix, ch) in line.char_indices() {
        let width = if ch == '\t' { TAB_WIDTH } else { ch.len_utf8() };
        if display < at + width {
            return if ch == '\t' && display - at > width / 2 { ix + 1 } else { ix };
        }
        at += width;
    }
    line.len()
}

/// Columns on screen before `col` (tabs count as `TAB_WIDTH`).
pub(super) fn columns(line: &str, col: usize) -> usize {
    line[..col].chars().map(|ch| if ch == '\t' { TAB_WIDTH } else { 1 }).sum()
}

/// The byte offset at screen column `target` (or the line's end).
pub(super) fn col_at_columns(line: &str, target: usize) -> usize {
    let mut seen = 0;
    for (ix, ch) in line.char_indices() {
        if seen >= target {
            return ix;
        }
        seen += if ch == '\t' { TAB_WIDTH } else { 1 };
    }
    line.len()
}

pub(super) fn utf16_to_byte(line: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (ix, ch) in line.char_indices() {
        if utf16 >= offset {
            return ix;
        }
        utf16 += ch.len_utf16();
    }
    line.len()
}

/// A UTF-16 range from the IME as a byte range in `line`, in order.
pub(super) fn utf16_range(line: &str, range: &Range<usize>) -> Range<usize> {
    let (a, b) = (utf16_to_byte(line, range.start), utf16_to_byte(line, range.end));
    a.min(b)..a.max(b)
}

pub(super) fn byte_to_utf16(line: &str, col: usize) -> usize {
    line[..col].encode_utf16().count()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CharKind {
    Space,
    Word,
    Punct,
}

pub(super) fn char_kind(ch: char) -> CharKind {
    if ch.is_whitespace() {
        CharKind::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        CharKind::Word
    } else {
        CharKind::Punct
    }
}

/// How the file indents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndentStyle {
    Tabs,
    Spaces(usize),
}

impl IndentStyle {
    /// Tabs if most indented lines start with one; else 2 spaces if lines
    /// indent by 2 often, else 4.
    pub(super) fn detect(lines: &[String]) -> Self {
        let (mut tabs, mut spaces, mut odd_two) = (0, 0, 0);
        for line in lines.iter().take(2000) {
            if line.starts_with('\t') {
                tabs += 1;
            } else if line.starts_with("  ") {
                spaces += 1;
                let n = line.len() - line.trim_start_matches(' ').len();
                if n % 4 == 2 {
                    odd_two += 1;
                }
            }
        }
        if tabs > spaces {
            IndentStyle::Tabs
        } else if spaces > 0 && odd_two * 5 >= spaces {
            IndentStyle::Spaces(2)
        } else {
            IndentStyle::Spaces(4)
        }
    }

    pub(super) fn unit(self) -> String {
        match self {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces(n) => " ".repeat(n),
        }
    }

    pub(super) fn label(self) -> String {
        match self {
            IndentStyle::Tabs => t("Tabs").to_string(),
            IndentStyle::Spaces(n) => tf("Spaces: {}", &[&n]),
        }
    }
}
