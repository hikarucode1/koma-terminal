//! Text selection, in coordinates that survive scrolling.
//!
//! A cell is addressed by an *absolute row*: an id that stays with a line as it
//! scrolls off the screen into history, and keeps working after old history is
//! trimmed. See `Grid::abs_row`. Everything here is a pure function of those
//! coordinates plus a way to read a row, so it can be tested without a grid.

/// Row id that outlives scrolling. Not an index into anything directly.
pub type AbsRow = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Cell by cell, as dragged.
    Char,
    /// Snapped out to whole words — a double click.
    Word,
    /// Whole lines — a triple click.
    Line,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Point {
    pub row: AbsRow,
    /// Column. As the *end* of a range this is exclusive.
    pub col: usize,
}

impl Point {
    pub fn new(row: AbsRow, col: usize) -> Self {
        Point { row, col }
    }
}

/// Characters that count as part of a word for double-click selection.
/// Deliberately generous: paths and URLs should come out in one piece.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '~' | '@' | '+' | '=' | '%')
}

/// Grows `col` out to the word around it. Returns a half-open column range.
/// On a non-word character the "word" is just that one cell, which is what
/// double-clicking a space or a bracket should give you.
pub fn word_at(row: &[char], col: usize) -> (usize, usize) {
    if row.is_empty() {
        return (0, 0);
    }
    let col = col.min(row.len() - 1);
    if !is_word_char(row[col]) {
        return (col, col + 1);
    }
    let mut start = col;
    while start > 0 && is_word_char(row[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < row.len() && is_word_char(row[end]) {
        end += 1;
    }
    (start, end)
}

/// A drag in progress, or a finished selection.
///
/// Both ends are stored already expanded for the mode, so a word-mode drag
/// keeps whole words at both ends rather than snapping only where it started.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    anchor: (Point, Point),
    head: (Point, Point),
    pub mode: Mode,
}

impl Selection {
    pub fn new(start: (Point, Point), mode: Mode) -> Self {
        Selection { anchor: start, head: start, mode }
    }

    pub fn extend_to(&mut self, end: (Point, Point)) {
        self.head = end;
    }

    /// Ordered `(start, end)` covering both ends. `end.col` is exclusive.
    pub fn range(&self) -> (Point, Point) {
        let start = self.anchor.0.min(self.head.0);
        let end = self.anchor.1.max(self.head.1);
        (start, end)
    }

    /// True when the selection covers no cells at all — a plain click that
    /// never became a drag.
    pub fn is_empty(&self) -> bool {
        let (start, end) = self.range();
        start.row == end.row && start.col >= end.col
    }

    /// Whether `(row, col)` is inside the selection, for highlighting.
    pub fn contains(&self, row: AbsRow, col: usize, cols: usize) -> bool {
        let (start, end) = self.range();
        if row < start.row || row > end.row {
            return false;
        }
        let from = if row == start.row { start.col } else { 0 };
        // Rows before the last are selected to the end of the line, so that a
        // multi-line selection reads as a block rather than a ragged staircase.
        let to = if row == end.row { end.col } else { cols };
        col >= from && col < to
    }
}

/// Pulls the selected text out, one line per row.
///
/// Trailing blanks are dropped from each line: a terminal row is padded to the
/// full width, and nobody wants that padding on the clipboard.
pub fn extract(
    range: (Point, Point),
    cols: usize,
    row_at: impl Fn(AbsRow) -> Option<Vec<char>>,
) -> String {
    let (start, end) = range;
    let mut out = String::new();
    for row in start.row..=end.row {
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { cols };
        if let Some(chars) = row_at(row) {
            let hi = to.min(chars.len());
            let line: String =
                if from < hi { chars[from..hi].iter().collect() } else { String::new() };
            out.push_str(line.trim_end());
        }
        if row != end.row {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// A two-line screen, padded to width like a real terminal row.
    fn fixture(row: AbsRow) -> Option<Vec<char>> {
        let text = match row {
            10 => "cargo test --locked  ",
            11 => "hello world          ",
            _ => return None,
        };
        Some(chars(text))
    }

    #[test]
    fn a_word_grows_out_from_the_middle() {
        let row = chars("hello world");
        assert_eq!(word_at(&row, 7), (6, 11));
        assert_eq!(word_at(&row, 0), (0, 5));
    }

    #[test]
    fn a_path_counts_as_one_word() {
        // Double-clicking a path or a flag should not stop at every separator.
        let row = chars("run /usr/local/bin/koma --locked now");
        let (s, e) = word_at(&row, 10);
        assert_eq!(row[s..e].iter().collect::<String>(), "/usr/local/bin/koma");
        let (s, e) = word_at(&row, 26);
        assert_eq!(row[s..e].iter().collect::<String>(), "--locked");
    }

    #[test]
    fn a_non_word_character_selects_only_itself() {
        let row = chars("a (b)");
        assert_eq!(word_at(&row, 2), (2, 3));
        assert_eq!(word_at(&row, 1), (1, 2));
    }

    #[test]
    fn word_lookup_survives_the_edges() {
        assert_eq!(word_at(&[], 5), (0, 0));
        let row = chars("ab");
        assert_eq!(word_at(&row, 99), (0, 2), "a column past the end clamps");
    }

    #[test]
    fn a_range_is_ordered_however_it_was_dragged() {
        let a = (Point::new(11, 2), Point::new(11, 3));
        let b = (Point::new(10, 5), Point::new(10, 6));
        let mut down = Selection::new(b, Mode::Char);
        down.extend_to(a);
        let mut up = Selection::new(a, Mode::Char);
        up.extend_to(b);
        assert_eq!(down.range(), up.range(), "dragging up or down must agree");
        assert_eq!(down.range().0, Point::new(10, 5));
        assert_eq!(down.range().1, Point::new(11, 3));
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let p = (Point::new(10, 4), Point::new(10, 4));
        assert!(Selection::new(p, Mode::Char).is_empty());
    }

    #[test]
    fn containment_runs_to_the_line_end_on_middle_rows() {
        let mut sel = Selection::new((Point::new(10, 6), Point::new(10, 6)), Mode::Char);
        sel.extend_to((Point::new(12, 3), Point::new(12, 3)));

        assert!(!sel.contains(10, 5, 20), "before the start column");
        assert!(sel.contains(10, 6, 20));
        assert!(sel.contains(10, 19, 20), "the first row runs to the edge");
        assert!(sel.contains(11, 0, 20), "a middle row is fully covered");
        assert!(sel.contains(11, 19, 20));
        assert!(sel.contains(12, 2, 20));
        assert!(!sel.contains(12, 3, 20), "the end column is exclusive");
        assert!(!sel.contains(13, 0, 20), "past the last row");
        assert!(!sel.contains(9, 0, 20), "before the first row");
    }

    #[test]
    fn extracting_one_row_takes_just_the_columns_asked_for() {
        let range = (Point::new(10, 6), Point::new(10, 10));
        assert_eq!(extract(range, 21, fixture), "test");
    }

    #[test]
    fn extracting_drops_the_padding_at_the_end_of_a_line() {
        // The row is padded to 21 columns; none of that belongs on the clipboard.
        let range = (Point::new(10, 0), Point::new(10, 21));
        assert_eq!(extract(range, 21, fixture), "cargo test --locked");
    }

    #[test]
    fn extracting_across_rows_joins_them_with_newlines() {
        let range = (Point::new(10, 6), Point::new(11, 5));
        assert_eq!(extract(range, 21, fixture), "test --locked\nhello");
    }

    #[test]
    fn a_row_that_scrolled_out_of_history_is_skipped_not_fatal() {
        // Row 9 was trimmed away mid-drag; the rest must still come out.
        let range = (Point::new(9, 0), Point::new(10, 21));
        assert_eq!(extract(range, 21, fixture), "\ncargo test --locked");
    }

    #[test]
    fn a_selection_past_the_end_of_a_short_row_yields_nothing_for_it() {
        let range = (Point::new(10, 40), Point::new(10, 50));
        assert_eq!(extract(range, 60, fixture), "");
    }

    #[test]
    fn word_selection_handles_multibyte_text() {
        // char columns, not bytes: kana must not split mid-character.
        let row = chars("cd ~/日本語のパス here");
        let (s, e) = word_at(&row, 6);
        assert_eq!(row[s..e].iter().collect::<String>(), "~/日本語のパス");
    }

    #[test]
    fn a_word_drag_keeps_whole_words_at_both_ends() {
        // Double-click "test", drag onto "loc|ked": both ends stay snapped.
        let row = chars("cargo test --locked");
        let start = {
            let (s, e) = word_at(&row, 7);
            (Point::new(10, s), Point::new(10, e))
        };
        let end = {
            let (s, e) = word_at(&row, 15);
            (Point::new(10, s), Point::new(10, e))
        };
        let mut sel = Selection::new(start, Mode::Word);
        sel.extend_to(end);
        assert_eq!(sel.range(), (Point::new(10, 6), Point::new(10, 19)));
    }

    #[test]
    fn dragging_back_over_the_anchor_still_yields_a_forward_range() {
        let mut sel = Selection::new((Point::new(10, 8), Point::new(10, 8)), Mode::Char);
        sel.extend_to((Point::new(10, 2), Point::new(10, 2)));
        let (s, e) = sel.range();
        assert!(s <= e, "range came out reversed: {s:?}..{e:?}");
        assert_eq!((s, e), (Point::new(10, 2), Point::new(10, 8)));
    }
}
