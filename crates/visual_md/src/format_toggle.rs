//! Pure "toggle bold/italic on the selection" logic for visual_md, backing
//! `Ctrl/Cmd+B`/`I` per `docs/live-preview-spec.md`'s "Selection formatting
//! shortcuts" bullet.
//!
//! Like [`crate::plan`] and [`crate::list_continuation`], this has no
//! GPUI/`Editor` dependency: it takes raw buffer text plus a set of cursor or
//! selection byte ranges and returns the edits to apply (or an empty set to
//! mean "do nothing").

use std::ops::Range;

use tree_sitter::{Node, Parser};

/// Which construct a shortcut toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Bold,
    Italic,
}

impl Emphasis {
    fn marker(self) -> &'static str {
        match self {
            Emphasis::Bold => "**",
            Emphasis::Italic => "*",
        }
    }

    fn node_kind(self) -> &'static str {
        match self {
            Emphasis::Bold => "strong_emphasis",
            Emphasis::Italic => "emphasis",
        }
    }
}

/// The result of toggling `kind` for every selection: a single batch of
/// edits plus each selection's resulting position, in the same order as the
/// input selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatEdit {
    /// Edits to apply together in one `Editor::edit` call, in the
    /// *original* text's byte coordinates. `text::Buffer::apply_local_edit`
    /// walks the pre-edit rope once, combining every edit in the batch
    /// itself, so every range here must stay expressed against the original
    /// text -- pre-shifting them by an earlier edit's length delta would
    /// double-count that delta once the buffer applies the batch. Zed's own
    /// `Editor::handle_input` builds its multi-cursor edit lists the same
    /// way (each selection's own unshifted `start..end`).
    pub edits: Vec<(Range<usize>, String)>,
    /// Where each input selection should land afterwards, one per input
    /// selection, in the *new* text's coordinates -- unlike `edits`, these
    /// genuinely do need to account for every earlier selection's own
    /// length delta, since that delta is a real shift in the final,
    /// post-batch document.
    pub selections: Vec<Range<usize>>,
}

/// Computes the edits (and resulting selections) for pressing the `kind`
/// shortcut with `selections` active in `text`. See the module doc for the
/// five rules this follows.
pub fn toggle(text: &str, selections: &[Range<usize>], kind: Emphasis) -> FormatEdit {
    let identity = || FormatEdit {
        edits: Vec::new(),
        selections: selections.to_vec(),
    };

    let mut block_parser = Parser::new();
    if block_parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .is_err()
    {
        return identity();
    }
    let Some(block_tree) = block_parser.parse(text, None) else {
        return identity();
    };
    let mut inline_parser = Parser::new();
    if inline_parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .is_err()
    {
        return identity();
    }

    let mut delta: isize = 0;
    // For a span two cursors share, `plan_selection` computes both cursors'
    // `selection_after` the same way -- already fully accounting for that
    // span's own (single) edit, independently of `delta`. So a duplicate
    // must be shifted by the delta as it stood *before* that span's edit was
    // folded in, not the current running delta, which already includes it
    // once (from the first cursor to claim the span) -- shifting by the
    // current delta would double-count it. This records that "delta before"
    // value per claimed span.
    let mut claimed: Vec<(Range<usize>, isize)> = Vec::new();
    let mut edits = Vec::new();
    let mut new_selections = Vec::with_capacity(selections.len());

    for selection in selections {
        let plan = plan_selection(
            text,
            selection,
            kind,
            block_tree.root_node(),
            &mut inline_parser,
        );

        let (applied, shift_by): (&[(Range<usize>, String)], isize) = match &plan.target {
            Some(target) => match claimed
                .iter()
                .find(|(claimed_range, _)| claimed_range == target)
            {
                Some((_, delta_before)) => (&[], *delta_before),
                None => {
                    claimed.push((target.clone(), delta));
                    (&plan.edits, delta)
                }
            },
            None => (&plan.edits, delta),
        };

        edits.extend(applied.iter().cloned());
        new_selections.push(apply_delta(plan.selection_after.clone(), shift_by));

        let local_delta: isize = applied
            .iter()
            .map(|(range, new_text)| new_text.len() as isize - (range.end - range.start) as isize)
            .sum();
        delta += local_delta;
    }

    FormatEdit {
        edits,
        selections: new_selections,
    }
}

fn apply_delta(range: Range<usize>, delta: isize) -> Range<usize> {
    ((range.start as isize + delta) as usize)..((range.end as isize + delta) as usize)
}

fn shift(range: Range<usize>, offset: usize) -> Range<usize> {
    (range.start + offset)..(range.end + offset)
}

/// One selection's own edits (in original-text coordinates, unaffected by
/// any other selection) and where it should end up, plus -- for an unwrap --
/// the enclosing node's range, used to dedupe multiple cursors inside the
/// same span.
struct SelectionPlan {
    edits: Vec<(Range<usize>, String)>,
    selection_after: Range<usize>,
    target: Option<Range<usize>>,
}

fn plan_selection(
    text: &str,
    selection: &Range<usize>,
    kind: Emphasis,
    block_root: Node,
    inline_parser: &mut Parser,
) -> SelectionPlan {
    if let Some(span) = detect_span(text, selection, kind, block_root, inline_parser) {
        let edits = vec![
            (span.open.clone(), String::new()),
            (span.close.clone(), String::new()),
        ];
        let selection_after = map_offset_after_removal(selection.start, &span.open, &span.close)
            ..map_offset_after_removal(selection.end, &span.open, &span.close);
        return SelectionPlan {
            edits,
            selection_after,
            target: Some(span.node_range),
        };
    }

    if selection.is_empty() {
        if let Some(remove) = detect_empty_pair_at_cursor(text, selection.start, kind) {
            return SelectionPlan {
                edits: vec![(remove.clone(), String::new())],
                selection_after: remove.start..remove.start,
                target: None,
            };
        }
        let marker = kind.marker();
        let insert = format!("{marker}{marker}");
        let cursor = selection.start + marker.len();
        return SelectionPlan {
            edits: vec![(selection.start..selection.start, insert)],
            selection_after: cursor..cursor,
            target: None,
        };
    }

    let Some((trim_start, trim_end)) = trim_whitespace_range(text, selection) else {
        return SelectionPlan {
            edits: Vec::new(),
            selection_after: selection.clone(),
            target: None,
        };
    };
    if trim_start >= trim_end {
        return SelectionPlan {
            edits: Vec::new(),
            selection_after: selection.clone(),
            target: None,
        };
    }
    let marker = kind.marker();
    let edits = vec![
        (trim_start..trim_start, marker.to_string()),
        (trim_end..trim_end, marker.to_string()),
    ];
    let selection_after = (trim_start + marker.len())..(trim_end + marker.len());
    SelectionPlan {
        edits,
        selection_after,
        target: None,
    }
}

/// The enclosing bold/italic span for `selection`, if any, found the same
/// way `crate::plan` finds a block's inline content: the smallest block-tree
/// descendant covering the selection, walked up to its nearest `inline` (or
/// `pipe_table_cell`) ancestor, then re-parsed as its own inline tree so the
/// selection can be re-anchored against a real `strong_emphasis`/`emphasis`
/// node.
struct Span {
    node_range: Range<usize>,
    open: Range<usize>,
    close: Range<usize>,
}

fn detect_span(
    text: &str,
    selection: &Range<usize>,
    kind: Emphasis,
    block_root: Node,
    inline_parser: &mut Parser,
) -> Option<Span> {
    let leaf = block_root.descendant_for_byte_range(selection.start, selection.end)?;
    let inline_node = find_ancestor(leaf, |node| {
        node.kind() == "inline" || node.kind() == "pipe_table_cell"
    })?;

    let range = inline_node.byte_range();
    if selection.start < range.start || selection.end > range.end {
        return None;
    }
    let inline_text = blank_block_continuations(inline_node, text)?;
    let tree = inline_parser.parse(&inline_text, None)?;

    let local_selection = (selection.start - range.start)..(selection.end - range.start);
    let local_leaf = tree
        .root_node()
        .descendant_for_byte_range(local_selection.start, local_selection.end)?;
    let matched = find_ancestor(local_leaf, |node| node.kind() == kind.node_kind())?;

    // Only the matched node's *direct* delimiter children: a recursive
    // search (like `plan::collect_delimiters`) would pull in a nested
    // construct's own markers too (e.g. `***x***`'s inner `strong_emphasis`
    // markers when matching the outer `emphasis`), merging runs that this
    // shortcut needs to keep separate so toggling one doesn't disturb the
    // other. The grammar emits one `emphasis_delimiter` node per character
    // (so `**` is two single-byte nodes, not one two-byte node), so the
    // open/close markers are each the contiguous run of direct delimiter
    // children at that end -- same merge `plan::plan_delimited_span` does,
    // just restricted to direct children instead of every descendant.
    let mut delimiters: Vec<Range<usize>> = Vec::new();
    let mut cursor = matched.walk();
    for child in matched.children(&mut cursor) {
        if child.kind() == "emphasis_delimiter" {
            delimiters.push(child.byte_range());
        }
    }
    if delimiters.len() < 2 {
        return None;
    }
    delimiters.sort_by_key(|range| range.start);
    let first = delimiters.first()?.clone();
    let last = delimiters.last()?.clone();

    let mut prefix_end = first.end;
    for delimiter in delimiters.iter().skip(1) {
        if delimiter.start == prefix_end {
            prefix_end = delimiter.end;
        } else {
            break;
        }
    }
    let mut suffix_start = last.start;
    for delimiter in delimiters.iter().rev().skip(1) {
        if delimiter.end == suffix_start {
            suffix_start = delimiter.start;
        } else {
            break;
        }
    }

    let open_local = first.start..prefix_end;
    let close_local = suffix_start..last.end;
    if open_local.end > close_local.start {
        return None;
    }

    Some(Span {
        node_range: shift(matched.byte_range(), range.start),
        open: shift(open_local, range.start),
        close: shift(close_local, range.start),
    })
}

fn find_ancestor<'a>(node: Node<'a>, matches: impl Fn(&Node) -> bool) -> Option<Node<'a>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches(&candidate) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// Mirrors `plan::plan_inline`'s handling of a multi-line construct's
/// continuation lines (a blockquote's `> ` or a list item's indentation on
/// every line after its first): those bytes are blanked to spaces before
/// re-parsing, so they can't be mistaken for inline content, without
/// disturbing any byte offset within the inline node's own range.
fn blank_block_continuations(inline_node: Node, text: &str) -> Option<String> {
    let range = inline_node.byte_range();
    let original = text.get(range.clone())?;
    let mut bytes = original.as_bytes().to_vec();
    let mut cursor = inline_node.walk();
    for child in inline_node.children(&mut cursor) {
        if child.kind() != "block_continuation" {
            continue;
        }
        let child_range = child.byte_range();
        if child_range.is_empty() {
            continue;
        }
        let local_start = child_range.start - range.start;
        let local_end = child_range.end - range.start;
        if let Some(slice) = bytes.get_mut(local_start..local_end) {
            slice.fill(b' ');
        }
    }
    String::from_utf8(bytes).ok()
}

/// Where original offset `o` (with `open.start <= o <= close.end`, i.e.
/// somewhere in the span being unwrapped) lands once both marker ranges are
/// deleted: unaffected if entirely before a marker, pulled back to that
/// marker's start if inside it (the normal "deleted range collapses to its
/// start" convention), and shifted back by the marker's full length if
/// entirely past it.
fn map_offset_after_removal(o: usize, open: &Range<usize>, close: &Range<usize>) -> usize {
    let mut result = o;
    if o > open.start {
        result -= o.min(open.end) - open.start;
    }
    if o > close.start {
        result -= o.min(close.end) - close.start;
    }
    result
}

/// Rule 5's fallback: a cursor sitting exactly between an already-inserted,
/// still-empty delimiter pair (e.g. `**|**`) doesn't parse as a
/// `strong_emphasis`/`emphasis` node at all, so `detect_span` never finds
/// it. Undoing that insertion needs a direct textual check instead. Bold
/// looks for a `**`/`**` pair; italic looks for a lone `*`/`*` pair whose
/// outer neighbors aren't also `*`, so it never mistakes the middle of an
/// empty bold pair (`**|**`) for an empty italic one.
fn detect_empty_pair_at_cursor(text: &str, at: usize, kind: Emphasis) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    match kind {
        Emphasis::Bold => {
            if at >= 2
                && at + 2 <= bytes.len()
                && &bytes[at - 2..at] == b"**"
                && &bytes[at..at + 2] == b"**"
            {
                Some((at - 2)..(at + 2))
            } else {
                None
            }
        }
        Emphasis::Italic => {
            let is_lone_star_pair = at >= 1
                && at < bytes.len()
                && bytes[at - 1] == b'*'
                && bytes[at] == b'*'
                && (at < 2 || bytes[at - 2] != b'*')
                && (at + 1 == bytes.len() || bytes[at + 1] != b'*');
            is_lone_star_pair.then(|| (at - 1)..(at + 1))
        }
    }
}

/// Rule 2's whitespace trim: `** x **` is not bold in CommonMark (a
/// delimiter run can't be immediately followed/preceded by whitespace and
/// still open/close emphasis), so wrapping a selection that starts or ends
/// with whitespace must place the markers inside it instead of around it.
fn trim_whitespace_range(text: &str, selection: &Range<usize>) -> Option<(usize, usize)> {
    let slice = text.get(selection.clone())?;
    let trimmed_start = selection.start + (slice.len() - slice.trim_start().len());
    let trimmed_end = selection.end - (slice.len() - slice.trim_end().len());
    Some((trimmed_start, trimmed_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut result = text.to_string();
        let mut ordered = edits.to_vec();
        ordered.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, insert) in ordered {
            result.replace_range(range, &insert);
        }
        result
    }

    #[test]
    fn wrap_selection_bold() {
        let text = "Hello world\n";
        let result = toggle(text, &[6..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..6, "**".to_string()), (11..11, "**".to_string())]
        );
        assert_eq!(result.selections, vec![8..13]);
        assert_eq!(apply(text, &result.edits), "Hello **world**\n");
    }

    #[test]
    fn wrap_selection_italic() {
        let text = "Hello world\n";
        let result = toggle(text, &[6..11], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(6..6, "*".to_string()), (11..11, "*".to_string())]
        );
        assert_eq!(result.selections, vec![7..12]);
        assert_eq!(apply(text, &result.edits), "Hello *world*\n");
    }

    #[test]
    fn wrap_selection_trims_surrounding_whitespace() {
        let text = "Hello  world  now\n";
        let result = toggle(text, &[5..14], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(7..7, "**".to_string()), (12..12, "**".to_string())]
        );
        assert_eq!(result.selections, vec![9..14]);
        assert_eq!(apply(text, &result.edits), "Hello  **world**  now\n");
    }

    #[test]
    fn all_whitespace_selection_is_a_no_op() {
        let text = "Hello   world\n";
        let result = toggle(text, &[5..8], Emphasis::Bold);
        assert!(result.edits.is_empty());
        assert_eq!(result.selections, vec![5..8]);
    }

    #[test]
    fn unwrap_selection_inside_bold_content() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[8..13], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..11]);
        assert_eq!(apply(text, &result.edits), "Hello world now\n");
    }

    #[test]
    fn unwrap_selection_covering_the_markers_too() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[6..15], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..11]);
    }

    #[test]
    fn unwrap_from_cursor_inside_bold() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[11..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![9..9]);
    }

    #[test]
    fn unwrap_from_cursor_at_content_edge() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[8..8], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..6]);
    }

    #[test]
    fn insert_and_remove_empty_bold_pair() {
        let text = "Hello world\n";
        let inserted = toggle(text, &[5..5], Emphasis::Bold);
        assert_eq!(inserted.edits, vec![(5..5, "****".to_string())]);
        assert_eq!(inserted.selections, vec![7..7]);

        let after_insert = apply(text, &inserted.edits);
        assert_eq!(after_insert, "Hello**** world\n");

        let removed = toggle(&after_insert, &[7..7], Emphasis::Bold);
        assert_eq!(removed.edits, vec![(5..9, String::new())]);
        assert_eq!(removed.selections, vec![5..5]);
        assert_eq!(apply(&after_insert, &removed.edits), text);
    }

    #[test]
    fn insert_and_remove_empty_italic_pair() {
        let text = "Hello world\n";
        let inserted = toggle(text, &[5..5], Emphasis::Italic);
        assert_eq!(inserted.edits, vec![(5..5, "**".to_string())]);
        assert_eq!(inserted.selections, vec![6..6]);

        let after_insert = apply(text, &inserted.edits);
        assert_eq!(after_insert, "Hello** world\n");

        let removed = toggle(&after_insert, &[6..6], Emphasis::Italic);
        assert_eq!(removed.edits, vec![(5..7, String::new())]);
        assert_eq!(apply(&after_insert, &removed.edits), text);
    }

    #[test]
    fn italic_empty_pair_detection_ignores_the_middle_of_a_bold_pair() {
        // A cursor exactly between an empty bold pair's two "**" runs must
        // not be mistaken for a lone italic "*|*" pair -- that would eat
        // half of the bold markers.
        let text = "Hello**** world\n";
        assert_eq!(detect_empty_pair_at_cursor(text, 7, Emphasis::Italic), None);
    }

    #[test]
    fn italic_toggle_on_bold_only_text_does_not_touch_bold_markers() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[10..10], Emphasis::Italic);
        assert_eq!(result.edits, vec![(10..10, "**".to_string())]);
    }

    #[test]
    fn triple_star_bold_toggle_leaves_italic_markers() {
        let text = "A ***both*** word.\n";
        let result = toggle(text, &[6..6], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(3..5, String::new()), (9..11, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A *both* word.\n");
    }

    #[test]
    fn triple_star_italic_toggle_leaves_bold_markers() {
        let text = "A ***both*** word.\n";
        let result = toggle(text, &[6..6], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(2..3, String::new()), (11..12, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A **both** word.\n");
    }

    #[test]
    fn underscore_italic_unwraps() {
        let text = "A _word_ end.\n";
        let result = toggle(text, &[4..4], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(2..3, String::new()), (7..8, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A word end.\n");
    }

    #[test]
    fn bold_toggle_inside_blockquote_line() {
        let text = "> **bold** text\n";
        let result = toggle(text, &[4..4], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(2..4, String::new()), (8..10, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "> bold text\n");
    }

    #[test]
    fn bold_toggle_inside_list_item() {
        let text = "- **bold** text\n";
        let result = toggle(text, &[4..4], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(2..4, String::new()), (8..10, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "- bold text\n");
    }

    #[test]
    fn wrap_and_unwrap_round_trip_multibyte_text() {
        let text = "Café naïve end\n";
        let word_start = text.find("naïve").unwrap();
        let word_end = word_start + "naïve".len();

        let wrapped = toggle(text, &[word_start..word_end], Emphasis::Bold);
        assert_eq!(wrapped.edits.len(), 2);
        let after_wrap = apply(text, &wrapped.edits);
        assert_eq!(after_wrap, "Café **naïve** end\n");

        let cursor = wrapped.selections[0].start;
        let unwrapped = toggle(&after_wrap, &[cursor..cursor], Emphasis::Bold);
        assert_eq!(unwrapped.edits.len(), 2);
        assert_eq!(apply(&after_wrap, &unwrapped.edits), text);
    }

    #[test]
    fn multi_cursor_wraps_independently_with_correct_deltas() {
        let text = "aaa bbb ccc\n";
        let result = toggle(text, &[0..3, 8..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![
                (0..0, "**".to_string()),
                (3..3, "**".to_string()),
                (8..8, "**".to_string()),
                (11..11, "**".to_string()),
            ]
        );
        assert_eq!(result.selections, vec![2..5, 14..17]);
        assert_eq!(apply(text, &result.edits), "**aaa** bbb **ccc**\n");
    }

    #[test]
    fn two_cursors_in_the_same_span_only_unwrap_once() {
        let text = "Hello **world** now\n";
        // Cursors after "wo" and after "orl", both inside the same span.
        let result = toggle(text, &[10..10, 12..12], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![8..8, 10..10]);
        assert_eq!(apply(text, &result.edits), "Hello world now\n");
    }

    #[test]
    fn no_panic_on_empty_document() {
        let result = toggle("", &[0..0], Emphasis::Bold);
        assert_eq!(result.edits, vec![(0..0, "****".to_string())]);
    }

    #[test]
    fn no_panic_on_selection_at_eof_without_trailing_newline() {
        let text = "Hello world";
        let result = toggle(text, &[6..11], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(6..6, "*".to_string()), (11..11, "*".to_string())]
        );
    }
}
