use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) const TAB_STOP: usize = 4;

pub(crate) fn grapheme_width_at(grapheme: &str, display_column: usize, tab_origin: usize) -> usize {
    if grapheme == "\t" {
        let relative_column = display_column.saturating_sub(tab_origin);
        TAB_STOP - relative_column % TAB_STOP
    } else {
        UnicodeWidthStr::width(grapheme).max(1)
    }
}

pub(crate) fn expand_tabs(value: &str, start_column: usize, tab_origin: usize) -> (String, usize) {
    let mut expanded = String::with_capacity(value.len());
    let mut display_column = start_column;
    for grapheme in value.graphemes(true) {
        let width = grapheme_width_at(grapheme, display_column, tab_origin);
        if grapheme == "\t" {
            expanded.extend(std::iter::repeat_n(' ', width));
        } else {
            expanded.push_str(grapheme);
        }
        display_column = display_column.saturating_add(width);
    }
    (expanded, display_column)
}

#[cfg(test)]
mod tests {
    use super::{TAB_STOP, expand_tabs, grapheme_width_at};

    #[test]
    fn tabs_advance_to_the_next_four_column_stop() {
        assert_eq!(grapheme_width_at("\t", 0, 0), TAB_STOP);
        assert_eq!(grapheme_width_at("\t", 1, 0), 3);
        assert_eq!(grapheme_width_at("\t", 4, 0), TAB_STOP);
        assert_eq!(expand_tabs("\tfield", 0, 0).0, "    field");
        assert_eq!(expand_tabs("a\tb", 0, 0).0, "a   b");
    }

    #[test]
    fn a_diff_marker_does_not_change_source_tab_stops() {
        assert_eq!(expand_tabs("+\tfield", 0, 1).0, "+    field");
        assert_eq!(expand_tabs(" \tfield", 0, 1).0, "     field");
    }
}
