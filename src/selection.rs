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

/// The character governing cell `i`: its own, or — for the spacer half of a
/// double-width character — the one it belongs to.
fn governing(row: &[Option<char>], i: usize) -> Option<char> {
    match row.get(i) {
        Some(Some(c)) => Some(*c),
        Some(None) if i > 0 => row[i - 1],
        _ => None,
    }
}

/// Grows `col` out to the word around it, in **grid columns** — the same space
/// a mouse position lands in. A double-width character occupies two columns,
/// the second of which is `None`, and both belong to the same word.
///
/// Returns a half-open column range. On a non-word character the "word" is that
/// character alone, which is what double-clicking a space or a bracket should
/// give you.
pub fn word_at(row: &[Option<char>], col: usize) -> (usize, usize) {
    if row.is_empty() {
        return (0, 0);
    }
    let col = col.min(row.len() - 1);
    let is_word = |i: usize| governing(row, i).is_some_and(is_word_char);

    if !is_word(col) {
        // Still take the whole character, spacer included.
        let mut start = col;
        while start > 0 && row[start].is_none() {
            start -= 1;
        }
        let mut end = col + 1;
        while end < row.len() && row[end].is_none() {
            end += 1;
        }
        return (start, end);
    }
    let mut start = col;
    while start > 0 && is_word(start - 1) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < row.len() && is_word(end) {
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
/// Rows are indexed in **grid columns**, so the numbers coming from a mouse
/// position can be used directly. The spacer half of a double-width character
/// is `None` and drops out, which keeps a stray space from following every CJK
/// glyph onto the clipboard.
///
/// Trailing blanks go too: a terminal row is padded to the full width, and
/// nobody wants that padding.
pub fn extract(
    range: (Point, Point),
    cols: usize,
    row_at: impl Fn(AbsRow) -> Option<Vec<Option<char>>>,
) -> String {
    let (start, end) = range;
    let mut out = String::new();
    for row in start.row..=end.row {
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { cols };
        if let Some(cells) = row_at(row) {
            let hi = to.min(cells.len());
            let line: String =
                if from < hi { cells[from..hi].iter().flatten().collect() } else { String::new() };
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

    /// A row in grid columns: a double-width character takes two entries, the
    /// second being the spacer.
    fn cells(s: &str) -> Vec<Option<char>> {
        let mut out = Vec::new();
        for c in s.chars() {
            out.push(Some(c));
            if unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) == 2 {
                out.push(None);
            }
        }
        out
    }

    /// A two-line screen, padded to width like a real terminal row.
    fn fixture(row: AbsRow) -> Option<Vec<Option<char>>> {
        let text = match row {
            10 => "cargo test --locked  ",
            11 => "hello world          ",
            _ => return None,
        };
        Some(cells(text))
    }

    #[test]
    fn a_word_grows_out_from_the_middle() {
        let row = cells("hello world");
        assert_eq!(word_at(&row, 7), (6, 11));
        assert_eq!(word_at(&row, 0), (0, 5));
    }

    #[test]
    fn a_path_counts_as_one_word() {
        // Double-clicking a path or a flag should not stop at every separator.
        let row = cells("run /usr/local/bin/koma --locked now");
        let (s, e) = word_at(&row, 10);
        assert_eq!(row[s..e].iter().flatten().collect::<String>(), "/usr/local/bin/koma");
        let (s, e) = word_at(&row, 26);
        assert_eq!(row[s..e].iter().flatten().collect::<String>(), "--locked");
    }

    #[test]
    fn a_non_word_character_selects_only_itself() {
        let row = cells("a (b)");
        assert_eq!(word_at(&row, 2), (2, 3));
        assert_eq!(word_at(&row, 1), (1, 2));
    }

    #[test]
    fn word_lookup_survives_the_edges() {
        assert_eq!(word_at(&[], 5), (0, 0));
        let row = cells("ab");
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
        // Grid columns, not bytes and not char indices. Each kana takes two
        // columns, and a word must not stop at the spacer half.
        let row = cells("cd ~/日本語のパス here");
        let (s, e) = word_at(&row, 5);
        assert_eq!(row[s..e].iter().flatten().collect::<String>(), "~/日本語のパス");
    }

    #[test]
    fn a_column_after_a_wide_character_still_lands_on_the_right_text() {
        // Regression, reported from review: mouse columns are grid columns, but
        // dropping the spacers made the row char-indexed, so everything after a
        // double-width character came out shifted left. Selecting the columns
        // that show "cargo" used to yield "go".
        let row = "あ い う  cargo";
        let cols = cells(row).len();
        let fixture = |_| Some(cells(row));

        // Where "cargo" actually starts on screen.
        let start = cells(row).iter().position(|c| *c == Some('c')).unwrap();
        // Three kana at two columns each, three spaces: "cargo" begins at 10.
        assert_eq!(start, 10, "the fixture is not laid out as expected");

        let range = (Point::new(0, start), Point::new(0, start + 5));
        assert_eq!(extract(range, cols, fixture), "cargo");
    }

    #[test]
    fn a_wide_word_and_its_highlight_agree() {
        // The other half of the same bug: word_at returned char indices while
        // the highlight used grid columns, so the copied text and the
        // highlighted cells disagreed.
        let row = cells("あ い う  cargo");
        let col = row.iter().position(|c| *c == Some('c')).unwrap();
        let (s, e) = word_at(&row, col);
        assert_eq!(row[s..e].iter().flatten().collect::<String>(), "cargo");
        assert_eq!((s, e), (col, col + 5), "word bounds must be grid columns");
    }

    #[test]
    fn a_double_width_character_selects_whole() {
        // Clicking either half of a wide character takes the whole thing.
        let row = cells("あい");
        assert_eq!(word_at(&row, 0), (0, 4), "kana are word characters, so both");
        assert_eq!(word_at(&row, 1), (0, 4), "the spacer belongs to its character");

        let punct = cells("（あ");
        assert_eq!(word_at(&punct, 0), (0, 2), "a wide non-word char, spacer included");
        assert_eq!(word_at(&punct, 1), (0, 2));
    }

    #[test]
    fn extraction_drops_the_spacer_not_the_column() {
        // One space between the kana and "x", not two.
        let row = "あx";
        let cols = cells(row).len();
        assert_eq!(cols, 3);
        assert_eq!(
            extract((Point::new(0, 0), Point::new(0, 3)), cols, |_| Some(cells(row))),
            "あx"
        );
    }

    #[test]
    fn a_word_drag_keeps_whole_words_at_both_ends() {
        // Double-click "test", drag onto "loc|ked": both ends stay snapped.
        let row = cells("cargo test --locked");
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
