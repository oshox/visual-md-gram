//! Pure decoration planner for visual_md's Markdown live preview.
//!
//! Parses raw buffer text with tree-sitter-md and, given the current cursor /
//! selection byte ranges, decides which markup delimiters should be hidden,
//! which should be revealed-but-dimmed, and which content spans need a
//! persistent style (bold, italic, strikethrough, code, heading level).
//!
//! Deliberately has no GPUI or `Editor` dependency, so the spec's "Core
//! mechanic" (docs/visual-md-spec.md) is unit-testable without a window,
//! and so the same logic can later be scoped to a viewport without touching
//! the rendering side at all.

use std::ops::Range;

use tree_sitter::{Node, Parser};

/// A persistent style to apply to a content span (never to its markers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStyle {
    Heading(u8),
    Bold,
    Italic,
    Strikethrough,
    InlineCode,
    Highlight,
    /// Background tint for a callout, covering its whole `block_quote`
    /// range (title row included, not just the body) -- the `>` bar and
    /// `[!type]` marker both render as non-opaque widgets, so the tint
    /// shows through underneath them.
    Callout(CalloutKind),
    /// A markdown link's visible text (`[text](url)`) or an autolink's URL
    /// text (`<https://...>`) — never the hidden brackets/parens/angle
    /// brackets around it. See `visual_md.rs`'s `link_style` for why this is
    /// the one span style still allowed a distinct color.
    Link,
}

/// The callout types the spec calls out by name; anything else still renders
/// as a callout (title capitalized, generic tint) via `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Tip,
    Warning,
    Danger,
    Other,
}

impl CalloutKind {
    fn from_type_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "note" | "info" => Self::Note,
            "tip" | "success" | "hint" => Self::Tip,
            "warning" | "caution" => Self::Warning,
            "danger" | "error" | "bug" | "failure" => Self::Danger,
            _ => Self::Other,
        }
    }
}

/// A callout's `+`/`-` fold-state suffix, right after `[!type]`. `None` and
/// `Expanded` render identically (both open) -- kept distinct so the
/// fold-toggle button in visual_md.rs never writes a redundant `+` back to a
/// buffer that never had one: collapsing then re-expanding a bare `[!note]`
/// leaves it bare, not `[!note]+`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutFold {
    None,
    Expanded,
    Collapsed,
}

impl CalloutFold {
    pub fn is_collapsed(self) -> bool {
        matches!(self, CalloutFold::Collapsed)
    }
}

/// A parsed `> [!type]` callout, everything visual_md.rs needs to render its
/// title widget and body collapse without re-deriving anything from raw
/// text. See `detect_callout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutInfo {
    /// `[!type]` plus any `+`/`-` suffix, as one range.
    pub marker_range: Range<usize>,
    pub kind: CalloutKind,
    /// The exact typed type name (e.g. `"todo"`), kept even when `kind`
    /// resolves to `Other` -- an unrecognized type still gets a real,
    /// specific title label, just generic styling.
    pub raw_type_name: String,
    pub fold: CalloutFold,
    /// Where a `+`/`-` character does (or, if `fold` is `None`, would) go --
    /// a zero-width range positioned right after `]` when absent. Editing
    /// this one range (an insert, a replace, or a delete to `""`) covers all
    /// three fold-toggle transitions with the same single-edit shape the
    /// checkbox toggle already uses.
    pub suffix_range: Range<usize>,
    /// The whole `block_quote` node's range, used for the background tint so
    /// it covers the title row too, not just the body.
    pub node_range: Range<usize>,
    /// From the end of `marker_range` to the end of `node_range` -- what
    /// gets collapsed into an ellipsis crease when `fold` is `Collapsed`.
    pub body_range: Range<usize>,
    /// Whether a selection touches `marker_range`. When `true`,
    /// `plan_block_quote` reveals it as raw, dimmed text (like every other
    /// hideable construct) instead of visual_md.rs rendering its title
    /// widget for it -- the callout's raw-text editing affordance, standing
    /// in for a right-click "change type" menu this crate doesn't build.
    pub touched: bool,
}

/// What kind of typographic marker a `glyph_markers` entry replaces. Kept as
/// a real enum rather than a raw glyph string so the applying side
/// (`visual_md.rs`) can render a bullet/blockquote-bar as an actual drawn
/// shape and a checkbox-adjacent ordinal as text, without string-sniffing
/// glyph content to figure out which is which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlyphKind {
    /// An unordered list item's `-`/`*`/`+` marker.
    Bullet,
    /// An ordered list item's marker, renumbered and formatted (e.g. `"3. "`
    /// or `"2) "`) — the only glyph kind that's still genuinely text, since
    /// digits have no font-coverage risk the way symbol glyphs do.
    Ordinal(String),
    /// A blockquote or callout's `>` marker, one per nesting level and one
    /// per continuation line.
    BlockquoteBar,
    /// A GFM pipe table's `|` column separator, in a header or data row
    /// (never the delimiter row, which is handled entirely differently --
    /// see `TableInfo`'s own doc comment). Always folded regardless of
    /// selection, the same as `BlockquoteBar`.
    TablePipe,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Marker byte ranges to fold away because no selection touches their
    /// containing construct.
    pub hidden_markers: Vec<Range<usize>>,
    /// Marker byte ranges to render, but dimmed: either a selection touches
    /// their construct, or (inline code) they are always shown this way.
    pub dimmed_markers: Vec<Range<usize>>,
    /// Content byte ranges (markers excluded) that get a persistent style.
    pub styled_spans: Vec<(Range<usize>, SpanStyle)>,
    /// Marker byte ranges replaced with a specific glyph (list bullets and
    /// renumbered ordinals, blockquote/callout left bars) — unlike
    /// `hidden_markers`, these are always folded regardless of selection,
    /// since a typographic marker isn't "raw source syntax" the way
    /// `**`/`#`/`>` are; there's nothing to reveal by touching them. A
    /// callout's own `[!type]` title is deliberately *not* here (see
    /// `callouts` below) — it needs to reveal as raw text on touch, so a
    /// user can retype the type name or its fold suffix directly.
    pub glyph_markers: Vec<(Range<usize>, GlyphKind)>,
    /// Byte ranges of `[ ]`/`[x]` task markers, with their current checked
    /// state (from which grammar node matched, `task_list_marker_checked`
    /// vs. `_unchecked`). These get a real interactive checkbox widget
    /// rather than a plain glyph substitution, since the spec requires them
    /// to stay clickable in both raw and rendered modes, unconditionally.
    /// The checked bool travels with the range (rather than the widget
    /// re-reading the live buffer at render time) deliberately: render runs
    /// *during* the editor's own paint pass, so re-entrantly reading that
    /// same editor entity from inside it is a real borrow conflict, not a
    /// hypothetical one — confirmed the hard way (see `checkbox_placeholder`
    /// in visual_md.rs for the fix this drives on the applying side).
    pub checkboxes: Vec<(Range<usize>, bool)>,
    /// Byte ranges of `thematic_break` nodes (`---`, `***`, `___`) that
    /// aren't touched by a selection. Unlike every other category above,
    /// these don't become a fold at all — a full-width `<hr>` needs the
    /// editor's block-decoration API (`insert_blocks`), not an inline
    /// `FoldPlaceholder`, since a fold can only size itself to its own
    /// content, never to the line's actual available width. See
    /// `apply_horizontal_rules` in visual_md.rs. A touched thematic_break is
    /// simply left out of this list, so its raw `---` shows through exactly
    /// like an untouched-vs-touched heading marker.
    pub horizontal_rules: Vec<Range<usize>>,
    /// One entry per fenced-code-block *line* (the opening ` ``` `+info
    /// string, or the closing ` ``` `) not touched by a selection; the
    /// `Option<String>` is the trimmed language name, `Some` only for the
    /// opening line. Rendered the same way as `horizontal_rules` (a
    /// full-width `insert_blocks` border, plus a language chip on the
    /// opening one) and for the same reason — a fold can't stretch to the
    /// real editor width. A touched line is simply left out, so its raw
    /// ` ``` ` shows through.
    pub code_fence_borders: Vec<(Range<usize>, Option<String>)>,
    /// One entry per fenced code block's `code_fence_content`, with its
    /// trimmed language name if any — *always* present regardless of
    /// selection, unlike every other category here: per spec, a code
    /// block's content highlighting never toggles off, only the fence lines
    /// do. Byte ranges only; the actual `Language`/syntax highlighting is
    /// necessarily computed in visual_md.rs (this module has no GPUI/
    /// `Language` dependency by design), see `apply_code_syntax_highlights`.
    pub code_fence_content: Vec<(Range<usize>, Option<String>)>,
    /// GFM pipe tables. Unlike every other category here, table cells are
    /// never hidden or block-replaced -- every cell stays normal, always-
    /// editable inline text (see `TableInfo`'s own doc comment for why).
    pub tables: Vec<TableInfo>,
    /// Parsed `> [!type]` callouts (see `CalloutInfo`), one per callout
    /// regardless of touch state -- visual_md.rs still needs an untouched-but-
    /// collapsed callout's `body_range` even while its title happens to be
    /// showing raw (touched) text.
    pub callouts: Vec<CalloutInfo>,
}

/// A column's alignment, from its `pipe_table_delimiter_cell`
/// (`pipe_table_align_left`/`_right`, both present = center, neither =
/// default/unspecified).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlignment {
    Default,
    Left,
    Center,
    Right,
}

/// One `pipe_table_cell`'s byte ranges. `content` is the cell's own node
/// range with its captured trailing whitespace trimmed back off; `leading_gap`
/// (between the previous `|` and this cell's first byte) and `trailing_gap`
/// (the whitespace this trimming just removed, or empty if there was none)
/// are the two places visual_md.rs can widen into a computed-width spacer to
/// align this column -- either may be empty, meaning there's no existing
/// whitespace there to widen (that side just doesn't get padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub leading_gap: Range<usize>,
    pub content: Range<usize>,
    pub trailing_gap: Range<usize>,
}

/// A `pipe_table`. Every `|` in a header/data row always folds to a bar
/// glyph regardless of selection -- pushed straight into `Plan::glyph_markers`
/// as `GlyphKind::TablePipe`, the same unconditional treatment
/// `plan_block_quote` already gives `>` (see `GlyphKind::BlockquoteBar`'s own
/// doc comment: "there's nothing to reveal by touching them"), rather than
/// duplicated here. Cell text itself is never in `hidden_markers`/
/// `dimmed_markers` at all: it was never hidden, so there's nothing to
/// toggle when the cursor enters a cell -- that's what gives this design
/// genuine per-cell editing without a rendered/raw mode switch. Column width
/// alignment and the header/body divider are computed by visual_md.rs
/// (`apply_table_alignment`/`apply_table_dividers`), which needs real text
/// measurement and the block-decoration API this module deliberately has no
/// dependency on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// One entry per column, from the delimiter row.
    pub alignments: Vec<TableAlignment>,
    /// The delimiter row's own line range, only if it isn't touched by a
    /// selection (same convention as `horizontal_rules`) -- a touched
    /// delimiter line is left out entirely so its raw `|---|---:|` shows
    /// through for editing (e.g. to add a column or change alignment).
    pub delimiter_line: Option<Range<usize>>,
    /// One entry per row (header first, then each data row in order), each
    /// with one `TableCell` per column.
    pub rows: Vec<Vec<TableCell>>,
}

/// Computes the live-preview decoration plan for `text`, given the current
/// selections as byte ranges (collapsed cursors are zero-width ranges).
///
/// Equivalent to [`plan_viewport`] with a visible range spanning the whole
/// document — see that function's doc comment for why a caller with an
/// actual viewport should prefer it instead. `visual_md.rs` never calls this
/// (it always has a real viewport), so it's only exercised by this module's
/// own tests below -- kept `pub` as a convenience non-viewport-scoped entry
/// point for exactly that, rather than making every one of those dozens of
/// `plan(text, &[])` calls spell out `plan_viewport(text, selections,
/// 0..text.len())` instead.
#[allow(dead_code)]
pub fn plan(text: &str, selections: &[Range<usize>]) -> Plan {
    plan_viewport(text, selections, 0..text.len())
}

/// Computes the live-preview decoration plan for `text`, restricted to
/// constructs that intersect `visible_range` (plus whatever selections add,
/// see below).
///
/// The whole document is still parsed once — tree-sitter is fast enough that
/// re-parsing on every keystroke is not the actual cost problem for large
/// files. The real cost is downstream: every decoration this planner emits
/// becomes a crease or a `highlight_text` range in the real editor, and
/// creating/diffing thousands of those for a multi-MB file on every
/// keystroke is what makes typing feel laggy. So the walk itself prunes any
/// block-level node (heading, paragraph, list, blockquote, and anything
/// nested inside them) whose byte range doesn't overlap `visible_range` at
/// all, skipping its entire subtree — no decorations are emitted for it, and
/// no inline reparse happens for its content either. A node that does
/// overlap, even partially, is processed in full (so e.g. an ordered list
/// that's mostly offscreen still renumbers correctly for the part that
/// isn't).
///
/// `visible_range` is unioned with every selection's own range before
/// pruning, not used as-is: a selection should always reveal its construct's
/// raw markers regardless of scroll position (this matters if the caller's
/// notion of "visible" is ever stale relative to where the cursor actually
/// is; cheap to guard against unconditionally).
pub fn plan_viewport(text: &str, selections: &[Range<usize>], visible_range: Range<usize>) -> Plan {
    let mut block_parser = Parser::new();
    let Ok(()) = block_parser.set_language(&tree_sitter_md::LANGUAGE.into()) else {
        return Plan::default();
    };
    let Some(block_tree) = block_parser.parse(text, None) else {
        return Plan::default();
    };

    let mut inline_parser = Parser::new();
    let Ok(()) = inline_parser.set_language(&tree_sitter_md::INLINE_LANGUAGE.into()) else {
        return Plan::default();
    };

    let mut visible_range = visible_range;
    for selection in selections {
        visible_range.start = visible_range.start.min(selection.start);
        visible_range.end = visible_range.end.max(selection.end);
    }

    let mut plan = Plan::default();
    walk_block(
        block_tree.root_node(),
        text,
        selections,
        &mut inline_parser,
        &visible_range,
        &mut plan,
    );

    // Nested constructs (`***bold italic***`, and the grammar's own
    // self-nested `~~strikethrough~~` representation) each contribute their
    // own marker ranges independently, so an outer and inner construct can
    // produce genuinely overlapping ranges (e.g. an outer emphasis's merged
    // "***" prefix fully contains an inner strong_emphasis's "**" prefix). A
    // fold can't be created over an already-folded sub-range — Zed's own
    // display-map code assumes disjoint fold regions and panics otherwise —
    // so this must be resolved to a disjoint partition before it's usable.
    // A dimmed (cursor-touched) determination always wins over hidden for
    // the same bytes, since it's the more conservative/correct choice when
    // constructs disagree about whether the cursor is "in" them.
    plan.hidden_markers = merge_ranges(plan.hidden_markers);
    plan.dimmed_markers = merge_ranges(plan.dimmed_markers);
    plan.hidden_markers = subtract_ranges(&plan.hidden_markers, &plan.dimmed_markers);

    plan
}

/// Sorts and merges overlapping/touching ranges into a minimal disjoint set.
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Removes from `ranges` any portion overlapping `subtract`. Both inputs must
/// already be disjoint and sorted by start (as [`merge_ranges`] produces).
fn subtract_ranges(ranges: &[Range<usize>], subtract: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    for range in ranges {
        let mut cursor = range.start;
        for sub in subtract {
            if sub.end <= cursor || sub.start >= range.end {
                continue;
            }
            if sub.start > cursor {
                result.push(cursor..sub.start.min(range.end));
            }
            cursor = cursor.max(sub.end);
            if cursor >= range.end {
                break;
            }
        }
        if cursor < range.end {
            result.push(cursor..range.end);
        }
    }
    result
}

fn touches_selection(range: &Range<usize>, selections: &[Range<usize>]) -> bool {
    selections
        .iter()
        .any(|selection| selection.start <= range.end && selection.end >= range.start)
}

/// Inclusive-ish overlap test: touching/zero-width ranges count as
/// overlapping, so pruning only ever risks decorating a few extra bytes at a
/// viewport boundary, never dropping a decoration that should be there.
fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start <= b.end && a.end >= b.start
}

fn walk_block(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    plan: &mut Plan,
) {
    if !overlaps(&node.byte_range(), visible_range) {
        return;
    }
    match node.kind() {
        "atx_heading" => {
            plan_heading(node, text, selections, inline_parser, plan);
            return;
        }
        "inline" | "pipe_table_cell" => {
            plan_inline(node, text, selections, inline_parser, plan);
            return;
        }
        "list" => {
            plan_list(node, text, selections, inline_parser, visible_range, plan);
            return;
        }
        "block_quote" => {
            plan_block_quote(node, text, selections, inline_parser, visible_range, plan);
            return;
        }
        "thematic_break" => {
            if !touches_selection(&node.byte_range(), selections) {
                plan.horizontal_rules.push(node.byte_range());
            }
            return;
        }
        "fenced_code_block" => {
            plan_fenced_code_block(node, text, selections, plan);
            return;
        }
        "pipe_table" => {
            plan_pipe_table(node, text, selections, inline_parser, visible_range, plan);
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_block(child, text, selections, inline_parser, visible_range, plan);
    }
}

fn heading_level(node: Node) -> Option<u8> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "atx_h1_marker" => return Some(1),
            "atx_h2_marker" => return Some(2),
            "atx_h3_marker" => return Some(3),
            "atx_h4_marker" => return Some(4),
            "atx_h5_marker" => return Some(5),
            "atx_h6_marker" => return Some(6),
            _ => {}
        }
    }
    None
}

fn plan_heading(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    plan: &mut Plan,
) {
    let Some(level) = heading_level(node) else {
        return;
    };
    let node_range = node.byte_range();
    let content = node.child_by_field_name("heading_content");
    let content_start = content
        .map(|n| n.byte_range().start)
        .unwrap_or(node_range.end);
    let marker_range = node_range.start..content_start;

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(marker_range);
    } else {
        plan.hidden_markers.push(marker_range);
    }

    if let Some(content) = content {
        let content_range = content.byte_range();
        if !content_range.is_empty() {
            plan.styled_spans
                .push((content_range, SpanStyle::Heading(level)));
        }
        plan_inline(content, text, selections, inline_parser, plan);
    }
}

pub(crate) const UNORDERED_MARKERS: [&str; 3] =
    ["list_marker_minus", "list_marker_plus", "list_marker_star"];
pub(crate) const ORDERED_MARKERS: [&str; 2] = ["list_marker_dot", "list_marker_parenthesis"];

/// Renders bullets as "• " and renumbers ordered lists visually (1, 2, 3...
/// regardless of the source's own digits, per spec), recursing into each
/// item's other content (paragraph text, nested lists/quotes) normally.
fn plan_list(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    plan: &mut Plan,
) {
    let mut ordinal = first_ordinal(node, text);
    let mut cursor = node.walk();
    for item in node.children(&mut cursor) {
        if item.kind() != "list_item" {
            continue;
        }
        let mut item_cursor = item.walk();
        let children: Vec<Node> = item.children(&mut item_cursor).collect();
        let has_task_marker = children.iter().any(|child| {
            matches!(
                child.kind(),
                "task_list_marker_checked" | "task_list_marker_unchecked"
            )
        });

        for child in &children {
            match child.kind() {
                // A task item's bullet/ordinal is redundant once the
                // checkbox glyph takes over as the visual marker, so it's
                // just hidden rather than replaced with its usual glyph.
                kind if UNORDERED_MARKERS.contains(&kind) => {
                    if has_task_marker {
                        plan.hidden_markers.push(child.byte_range());
                    } else {
                        plan.glyph_markers
                            .push((child.byte_range(), GlyphKind::Bullet));
                    }
                }
                kind if ORDERED_MARKERS.contains(&kind) => {
                    let separator = if kind == "list_marker_parenthesis" {
                        ")"
                    } else {
                        "."
                    };
                    let n = ordinal.unwrap_or(1);
                    ordinal = Some(n + 1);
                    if has_task_marker {
                        plan.hidden_markers.push(child.byte_range());
                    } else {
                        plan.glyph_markers.push((
                            child.byte_range(),
                            GlyphKind::Ordinal(format!("{n}{separator} ")),
                        ));
                    }
                }
                "task_list_marker_checked" => {
                    plan.checkboxes.push((child.byte_range(), true));
                }
                "task_list_marker_unchecked" => {
                    plan.checkboxes.push((child.byte_range(), false));
                }
                _ => walk_block(*child, text, selections, inline_parser, visible_range, plan),
            }
        }
    }
}

/// The display number an ordered list should start counting from, taken from
/// its first item's own literal digits (so `5. foo` starts a list at 5); `None`
/// for an unordered list.
fn first_ordinal(list_node: Node, text: &str) -> Option<u32> {
    let mut cursor = list_node.walk();
    let first_item = list_node
        .children(&mut cursor)
        .find(|n| n.kind() == "list_item")?;
    let mut item_cursor = first_item.walk();
    let marker = first_item
        .children(&mut item_cursor)
        .find(|child| ORDERED_MARKERS.contains(&child.kind()))?;
    let marker_text = text.get(marker.byte_range())?;
    marker_text.trim_end_matches([' ', '.', ')']).parse().ok()
}

/// Replaces each `>` (or, for a multi-line paragraph's continuation lines,
/// the `block_continuation` markers found within its inline tree — see
/// `walk_inline`) with a left-bar glyph. Detects a callout (`> [!type]`) on
/// the block's first line and tints its body accordingly; nested blockquotes
/// recurse naturally since each nesting level is its own `block_quote` node
/// with its own marker, stacking bars visually without extra bookkeeping.
fn plan_block_quote(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    plan: &mut Plan,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block_quote_marker" {
            plan.glyph_markers
                .push((child.byte_range(), GlyphKind::BlockquoteBar));
        }
    }

    if let Some(callout) = detect_callout(node, text, selections) {
        if callout.touched {
            plan.dimmed_markers.push(callout.marker_range.clone());
        }
        if !callout.node_range.is_empty() {
            plan.styled_spans
                .push((callout.node_range.clone(), SpanStyle::Callout(callout.kind)));
        }
        plan.callouts.push(callout);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "block_quote_marker" {
            walk_block(child, text, selections, inline_parser, visible_range, plan);
        }
    }
}

/// Recognizes a ` ``` `/`~~~` fenced code block's structure: which lines are
/// its opening/closing fence (each independently hidden behind a full-width
/// border+chip via `apply_code_fence_borders` in visual_md.rs, unless the
/// cursor is touching that specific line) and its content range (always
/// exposed via `code_fence_content`, regardless of the cursor, since content
/// highlighting never toggles off per spec).
///
/// Bails out (the whole block is left completely unhandled, i.e. fully raw)
/// if the grammar didn't produce a `code_fence_content` child at all -- an
/// unterminated fence at end-of-file is the one realistic way that happens,
/// and it's not worth guessing at where the "content" would have ended.
fn plan_fenced_code_block(node: Node, text: &str, selections: &[Range<usize>], plan: &mut Plan) {
    let mut opening_delimiter: Option<Range<usize>> = None;
    let mut closing_delimiter: Option<Range<usize>> = None;
    let mut info_string: Option<Range<usize>> = None;
    let mut content: Option<Range<usize>> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "fenced_code_block_delimiter" => {
                if opening_delimiter.is_none() {
                    opening_delimiter = Some(child.byte_range());
                } else {
                    closing_delimiter = Some(child.byte_range());
                }
            }
            "info_string" => info_string = Some(child.byte_range()),
            "code_fence_content" => content = Some(child.byte_range()),
            _ => {}
        }
    }

    let Some(opening_delimiter) = opening_delimiter else {
        return;
    };
    let Some(content) = content else {
        return;
    };

    // The language name is the info string's first whitespace-delimited
    // word: CommonMark allows arbitrary trailing content after it (e.g.
    // `` ```rust {.line-numbers} ``), and only the language itself matters
    // here.
    let language = info_string
        .as_ref()
        .and_then(|range| text.get(range.clone()))
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_string);

    // The opening delimiter (plus info string, if any) already spans the
    // entire first line including its trailing newline -- `info_string`
    // (when present) or `fenced_code_block_delimiter` otherwise ends exactly
    // where `code_fence_content` begins, confirmed by inspecting the
    // grammar's own output directly, so no separate line-boundary scan is
    // needed.
    let opening_line = opening_delimiter.start..content.start;
    if !touches_selection(&opening_line, selections) {
        plan.code_fence_borders
            .push((opening_line, language.clone()));
    }

    if let Some(closing_delimiter) = closing_delimiter {
        // Extend by the trailing newline, if any, the same way the opening
        // line's range includes its own -- the closing delimiter node's own
        // range stops right at the last `` ` ``/`~`.
        let closing_line_end = if text.as_bytes().get(closing_delimiter.end) == Some(&b'\n') {
            closing_delimiter.end + 1
        } else {
            closing_delimiter.end
        };
        let closing_line = closing_delimiter.start..closing_line_end;
        if !touches_selection(&closing_line, selections) {
            plan.code_fence_borders.push((closing_line, None));
        }
    }

    plan.code_fence_content.push((content, language));
}

/// Recognizes a `pipe_table`'s structure -- see `TableInfo`'s own doc
/// comment for why this only ever produces structural byte ranges (pipes,
/// per-cell content/gap ranges, alignments), never hides or block-replaces
/// any cell content itself.
///
/// Bails out (the whole table left unhandled, i.e. fully raw) if either the
/// header or delimiter row is missing -- shouldn't happen for a real
/// `pipe_table` node, but not worth guessing at if the grammar ever produces
/// one some other way.
fn plan_pipe_table(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    plan: &mut Plan,
) {
    let mut cursor = node.walk();
    let mut header = None;
    let mut delimiter_row = None;
    let mut data_rows = Vec::new();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "pipe_table_header" => header = Some(child),
            "pipe_table_delimiter_row" => delimiter_row = Some(child),
            "pipe_table_row" => data_rows.push(child),
            _ => {}
        }
    }
    let Some(header) = header else {
        return;
    };
    let Some(delimiter_row) = delimiter_row else {
        return;
    };

    let alignments = table_alignments(delimiter_row);

    let mut rows = vec![table_row_cells(header, text, plan)];
    for row in &data_rows {
        rows.push(table_row_cells(*row, text, plan));
    }

    // Recurse into every cell's own content for inline formatting (bold,
    // links, etc. inside a cell) -- this is the *only* place viewport
    // pruning applies for a table: `walk_block`'s own entry already skips a
    // cell whose range doesn't overlap `visible_range`. The structural data
    // above is collected for every row unconditionally, regardless of
    // visibility, since visual_md.rs needs every cell in a column measured to
    // keep that column's width (and therefore every row's spacer) visually
    // consistent as the table scrolls in and out of view.
    for row_node in std::iter::once(header).chain(data_rows.iter().copied()) {
        let mut cell_cursor = row_node.walk();
        for cell in row_node.children(&mut cell_cursor) {
            if cell.kind() == "pipe_table_cell" {
                walk_block(cell, text, selections, inline_parser, visible_range, plan);
            }
        }
    }

    let delimiter_range = delimiter_row.byte_range();
    let delimiter_line =
        (!touches_selection(&delimiter_range, selections)).then_some(delimiter_range);

    plan.tables.push(TableInfo {
        alignments,
        delimiter_line,
        rows,
    });
}

/// One [`TableAlignment`] per `pipe_table_delimiter_cell` in a
/// `pipe_table_delimiter_row`.
fn table_alignments(delimiter_row: Node) -> Vec<TableAlignment> {
    let mut cursor = delimiter_row.walk();
    delimiter_row
        .children(&mut cursor)
        .filter(|child| child.kind() == "pipe_table_delimiter_cell")
        .map(|cell| {
            let mut has_left = false;
            let mut has_right = false;
            let mut inner_cursor = cell.walk();
            for part in cell.children(&mut inner_cursor) {
                match part.kind() {
                    "pipe_table_align_left" => has_left = true,
                    "pipe_table_align_right" => has_right = true,
                    _ => {}
                }
            }
            match (has_left, has_right) {
                (true, true) => TableAlignment::Center,
                (true, false) => TableAlignment::Left,
                (false, true) => TableAlignment::Right,
                (false, false) => TableAlignment::Default,
            }
        })
        .collect()
}

/// Walks one `pipe_table_header`/`pipe_table_row`'s direct children (`|`
/// tokens alternating with `pipe_table_cell`s), pushing every `|`'s range
/// into `plan.glyph_markers` as `GlyphKind::TablePipe` and returning one
/// [`TableCell`] per cell in column order.
fn table_row_cells(row: Node, text: &str, plan: &mut Plan) -> Vec<TableCell> {
    let mut cells = Vec::new();
    let mut prev_pipe_end = None;
    let mut cursor = row.walk();
    for child in row.children(&mut cursor) {
        match child.kind() {
            "|" => {
                let range = child.byte_range();
                prev_pipe_end = Some(range.end);
                plan.glyph_markers.push((range, GlyphKind::TablePipe));
            }
            "pipe_table_cell" => {
                let cell_range = child.byte_range();
                let leading_gap = match prev_pipe_end {
                    Some(end) => end..cell_range.start,
                    None => cell_range.start..cell_range.start,
                };
                // The cell's own node range already includes any trailing
                // whitespace before the next `|` (confirmed by inspection);
                // trimming it back off here is what leaves it available as
                // `trailing_gap` for visual_md.rs to widen into a spacer.
                let cell_text = text.get(cell_range.clone()).unwrap_or("");
                let trimmed_len = cell_text.trim_end().len();
                let content = cell_range.start..cell_range.start + trimmed_len;
                let trailing_gap = content.end..cell_range.end;
                cells.push(TableCell {
                    leading_gap,
                    content,
                    trailing_gap,
                });
            }
            _ => {}
        }
    }
    cells
}

/// Looks for `[!type]` immediately after the marker on a blockquote's first
/// line. Returns the `[!type]` marker's own byte range (to hide), the
/// recognized callout kind, and the byte range of the rest of the
/// blockquote's content (to tint).
fn detect_callout(
    block_quote: Node,
    text: &str,
    selections: &[Range<usize>],
) -> Option<CalloutInfo> {
    let node_range = block_quote.byte_range();
    let first_marker_end = {
        let mut cursor = block_quote.walk();
        block_quote
            .children(&mut cursor)
            .find(|child| child.kind() == "block_quote_marker")?
            .byte_range()
            .end
    };
    let after_marker = text.get(first_marker_end..node_range.end)?;
    if !after_marker.starts_with("[!") {
        return None;
    }
    let close = after_marker.find(']')?;
    let type_name = &after_marker[2..close];
    if type_name.is_empty() || !type_name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let bracket_end = first_marker_end + close + 1;
    let (fold, suffix_range) = match text.as_bytes().get(bracket_end) {
        Some(b'+') => (CalloutFold::Expanded, bracket_end..bracket_end + 1),
        Some(b'-') => (CalloutFold::Collapsed, bracket_end..bracket_end + 1),
        _ => (CalloutFold::None, bracket_end..bracket_end),
    };
    let marker_end = suffix_range.end;
    let marker_range = first_marker_end..marker_end;
    // Matches `plan_heading`'s own "touched" scope, not a bare overlap with
    // `marker_range`: a heading reveals its marker when the cursor is
    // anywhere on that (single-line) construct, so a callout reveals its
    // title when the cursor is anywhere on *its* title line -- including
    // past the marker, in same-line text like "> [!warning] Be careful" --
    // but deliberately not for a cursor anywhere in the body, which stays
    // independently live-previewed (per spec) rather than coupled to the
    // title's own raw/rendered state.
    let title_line_end = text[node_range.start..node_range.end]
        .find('\n')
        .map(|offset| node_range.start + offset)
        .unwrap_or(node_range.end);
    let touched = touches_selection(&(node_range.start..title_line_end), selections);
    Some(CalloutInfo {
        kind: CalloutKind::from_type_name(type_name),
        raw_type_name: type_name.to_string(),
        fold,
        suffix_range,
        body_range: marker_end..node_range.end,
        node_range,
        marker_range,
        touched,
    })
}

/// The real `tree-sitter-md` crate's `MarkdownParser` (see `plan()`'s doc

/// The real `tree-sitter-md` crate's `MarkdownParser` (see `plan()`'s doc
/// comment for why this crate hand-rolls a simpler two-pass parse instead of
/// using it) builds an inline node's parsed range by explicitly excluding
/// any block-level `block_continuation` children first, via tree-sitter's
/// multi-range parsing — a multi-line blockquote paragraph's repeated "> "
/// prefixes on continuation lines are exactly this. This crate's simpler
/// single-contiguous-substring reparse doesn't get that for free: fed
/// straight through, the inline grammar sees a lone ">" as meaningless plain
/// text (confirmed empirically, not assumed) rather than as markup. Fixed
/// here by reading the block tree's own `block_continuation` children (which
/// *do* show up correctly there) directly for their bar-glyph ranges, then
/// blanking their bytes to spaces before handing the text to the inline
/// parser, so it never sees the stray `>` at all. Blanking preserves length
/// and every other node's byte offsets exactly, so the rest of this module's
/// offset math is untouched by it.
fn plan_inline(
    inline_node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    plan: &mut Plan,
) {
    let range = inline_node.byte_range();
    if range.is_empty() {
        return;
    }
    let Some(original) = text.get(range.clone()) else {
        return;
    };

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
        if text
            .get(child_range.clone())
            .is_some_and(|s| s.trim_start().starts_with('>'))
        {
            plan.glyph_markers
                .push((child_range.clone(), GlyphKind::BlockquoteBar));
        }
        let local_start = child_range.start - range.start;
        let local_end = child_range.end - range.start;
        if let Some(slice) = bytes.get_mut(local_start..local_end) {
            slice.fill(b' ');
        }
    }
    let Ok(inline_text) = String::from_utf8(bytes) else {
        return;
    };

    let Some(tree) = inline_parser.parse(&inline_text, None) else {
        return;
    };

    let mut code_ranges = Vec::new();
    walk_inline(
        tree.root_node(),
        range.start,
        selections,
        plan,
        &mut code_ranges,
    );
    plan_highlight_marks(&inline_text, range.start, &code_ranges, selections, plan);
}

fn walk_inline(
    node: Node,
    offset: usize,
    selections: &[Range<usize>],
    plan: &mut Plan,
    code_ranges: &mut Vec<Range<usize>>,
) {
    match node.kind() {
        "inline_link" => {
            plan_link(node, offset, selections, plan);
            // A link's children are structural tokens (brackets/parens) plus
            // `link_text`/`link_destination`, none of which are themselves
            // emphasis/link/code_span nodes in practice — like `code_span`,
            // there's no nested markup worth recursing into here.
            return;
        }
        "uri_autolink" | "email_autolink" => {
            plan_autolink(node, offset, selections, plan);
            return;
        }
        _ => {}
    }

    let style = match node.kind() {
        "strong_emphasis" => Some((SpanStyle::Bold, "emphasis_delimiter")),
        "emphasis" => Some((SpanStyle::Italic, "emphasis_delimiter")),
        "strikethrough" => Some((SpanStyle::Strikethrough, "emphasis_delimiter")),
        "code_span" => Some((SpanStyle::InlineCode, "code_span_delimiter")),
        _ => None,
    };

    if let Some((span_style, delimiter_kind)) = style {
        plan_delimited_span(node, offset, delimiter_kind, span_style, selections, plan);
        if span_style == SpanStyle::InlineCode {
            code_ranges.push(shift(node.byte_range(), offset));
        }
        // Don't recurse into a code span's contents (raw text, no nested
        // markup); do recurse into emphasis/strong/strikethrough since they
        // can nest (e.g. `***bold italic***`, or the grammar's own
        // self-nested `~~strikethrough~~` representation).
        if node.kind() != "code_span" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_inline(child, offset, selections, plan, code_ranges);
            }
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_inline(child, offset, selections, plan, code_ranges);
    }
}

fn shift(range: Range<usize>, offset: usize) -> Range<usize> {
    (range.start + offset)..(range.end + offset)
}

/// The first direct child of `node` with the given grammar kind, if any.
fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// Hides an `inline_link`'s (`[text](url)`) brackets/parens/destination,
/// leaving only `text` visible and styled as a link; reveals them (dimmed)
/// instead if the cursor is anywhere on the link. Bails out (leaves the node
/// entirely unhandled, i.e. fully raw) if any expected child is missing --
/// malformed/unusual grammar output isn't worth guessing at.
///
/// Only the direct `[text](url)` shape is handled. Reference-style links
/// (`[text][1]`, `[text][]`, `[shortcut]`) are deliberately left alone: this
/// function is only ever reached for the `inline_link` node kind, which the
/// grammar produces exclusively for the immediate, self-contained
/// `[..](..)`  shape -- `full_reference_link`/`collapsed_reference_link`/
/// `shortcut_link` are different node kinds `walk_inline` never dispatches
/// here, since resolving those needs a `link_reference_definition` that may
/// live in a completely different part of the document (out of scope for
/// this crate's per-paragraph, no-cross-block-lookup inline planner).
fn plan_link(node: Node, offset: usize, selections: &[Range<usize>], plan: &mut Plan) {
    let Some(open_bracket) = find_child(node, "[") else {
        return;
    };
    let Some(link_text) = find_child(node, "link_text") else {
        return;
    };
    let Some(close_bracket) = find_child(node, "]") else {
        return;
    };
    let Some(close_paren) = find_child(node, ")") else {
        return;
    };

    let node_range = shift(node.byte_range(), offset);
    let prefix = shift(open_bracket.byte_range(), offset);
    // `]`, `(`, `link_destination`, `)` sit back-to-back with no gaps, so
    // this is a single contiguous span, not several -- same "merge the
    // contiguous run" idea as `plan_delimited_span`'s prefix/suffix, just
    // computed directly since a link's trailing cluster isn't a repeated
    // delimiter of one kind.
    let suffix = shift(
        close_bracket.byte_range().start..close_paren.byte_range().end,
        offset,
    );
    let link_text = shift(link_text.byte_range(), offset);

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(prefix);
        plan.dimmed_markers.push(suffix);
    } else {
        plan.hidden_markers.push(prefix);
        plan.hidden_markers.push(suffix);
    }
    plan.styled_spans.push((link_text, SpanStyle::Link));
}

/// Hides a `uri_autolink`/`email_autolink`'s (`<https://...>`) angle
/// brackets, styling the URL text between them as a link -- reveals them
/// (dimmed) instead if the cursor is anywhere on it. Unlike `plan_link` this
/// is a leaf node (no children at all per the grammar), so the brackets are
/// just its first and last byte.
fn plan_autolink(node: Node, offset: usize, selections: &[Range<usize>], plan: &mut Plan) {
    let node_range = shift(node.byte_range(), offset);
    if node_range.len() < 2 {
        return;
    }
    let prefix = node_range.start..node_range.start + 1;
    let suffix = node_range.end - 1..node_range.end;
    let inner = prefix.end..suffix.start;

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(prefix);
        plan.dimmed_markers.push(suffix);
    } else {
        plan.hidden_markers.push(prefix);
        plan.hidden_markers.push(suffix);
    }
    if !inner.is_empty() {
        plan.styled_spans.push((inner, SpanStyle::Link));
    }
}

/// Finds every descendant delimiter node of `kind`, merges the contiguous run
/// touching the node's start into a "prefix" marker and the contiguous run
/// touching its end into a "suffix" marker, and styles the gap between them
/// as content. Handles the grammar's own nested representation of runs like
/// `~~strikethrough~~` (a `strikethrough` node containing another
/// `strikethrough` node) uniformly, since it walks all descendants rather
/// than only direct children.
fn plan_delimited_span(
    node: Node,
    offset: usize,
    delimiter_kind: &str,
    style: SpanStyle,
    selections: &[Range<usize>],
    plan: &mut Plan,
) {
    let node_range = shift(node.byte_range(), offset);

    let mut delimiters: Vec<Range<usize>> = Vec::new();
    collect_delimiters(node, delimiter_kind, offset, &mut delimiters);
    delimiters.sort_by_key(|range| range.start);

    let Some(first) = delimiters.first().cloned() else {
        return;
    };
    let Some(last) = delimiters.last().cloned() else {
        return;
    };

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

    let prefix = node_range.start..prefix_end;
    let suffix = suffix_start..node_range.end;

    let is_code = style == SpanStyle::InlineCode;
    let is_raw = touches_selection(&node_range, selections);
    if is_code || is_raw {
        plan.dimmed_markers.push(prefix.clone());
        plan.dimmed_markers.push(suffix.clone());
    } else {
        plan.hidden_markers.push(prefix.clone());
        plan.hidden_markers.push(suffix.clone());
    }

    if prefix.end < suffix.start {
        plan.styled_spans.push((prefix.end..suffix.start, style));
    }
}

fn collect_delimiters(node: Node, kind: &str, offset: usize, out: &mut Vec<Range<usize>>) {
    if node.kind() == kind {
        out.push(shift(node.byte_range(), offset));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_delimiters(child, kind, offset, out);
    }
}

/// `==highlight==` has no tree-sitter-md node at all (this grammar doesn't
/// support it), so it is found with a manual scan of the inline node's raw
/// text instead, skipping any byte already claimed by a code span so that
/// `` `==literal==` `` inside code is left alone.
fn plan_highlight_marks(
    inline_text: &str,
    offset: usize,
    code_ranges: &[Range<usize>],
    selections: &[Range<usize>],
    plan: &mut Plan,
) {
    let bytes = inline_text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'=' && bytes[i + 1] == b'=' && !in_code(offset + i, code_ranges) {
            if let Some(close) = find_closing(bytes, i + 2, code_ranges, offset) {
                let open = shift(i..i + 2, offset);
                let close_range = shift(close..close + 2, offset);
                let outer = open.start..close_range.end;
                if touches_selection(&outer, selections) {
                    plan.dimmed_markers.push(open.clone());
                    plan.dimmed_markers.push(close_range.clone());
                } else {
                    plan.hidden_markers.push(open.clone());
                    plan.hidden_markers.push(close_range.clone());
                }
                if open.end < close_range.start {
                    plan.styled_spans
                        .push((open.end..close_range.start, SpanStyle::Highlight));
                }
                i = close + 2;
                continue;
            }
        }
        i += 1;
    }
}

fn in_code(byte_offset: usize, code_ranges: &[Range<usize>]) -> bool {
    code_ranges.iter().any(|range| range.contains(&byte_offset))
}

fn find_closing(
    bytes: &[u8],
    start: usize,
    code_ranges: &[Range<usize>],
    offset: usize,
) -> Option<usize> {
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == b'=' && bytes[i + 1] == b'=' && !in_code(offset + i, code_ranges) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, style: SpanStyle) -> Vec<Range<usize>> {
        plan(text, &[])
            .styled_spans
            .into_iter()
            .filter(|(_, s)| *s == style)
            .map(|(range, _)| range)
            .collect()
    }

    #[test]
    fn heading_marker_hidden_when_not_touched() {
        let result = plan("# Heading\n", &[]);
        assert_eq!(result.hidden_markers, vec![0..2]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(2..9, SpanStyle::Heading(1))]);
    }

    #[test]
    fn heading_marker_dimmed_when_cursor_on_line() {
        let result = plan("# Heading\n", &[4..4]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..2]);
    }

    #[test]
    fn heading_levels() {
        for (marker, level) in [
            ("#", 1),
            ("##", 2),
            ("###", 3),
            ("####", 4),
            ("#####", 5),
            ("######", 6),
        ] {
            let text = format!("{marker} Heading\n");
            let result = plan(&text, &[]);
            assert_eq!(
                result.styled_spans[0].1,
                SpanStyle::Heading(level),
                "for {marker}"
            );
        }
    }

    #[test]
    fn bold_hidden_by_default_and_revealed_by_cursor() {
        let text = "Some **bold** text.\n";
        let hidden = plan(text, &[]);
        assert_eq!(hidden.hidden_markers, vec![5..7, 11..13]);
        assert!(hidden.dimmed_markers.is_empty());
        assert_eq!(spans(text, SpanStyle::Bold), vec![7..11]);

        // cursor inside "bold"
        let revealed = plan(text, &[8..8]);
        assert!(revealed.hidden_markers.is_empty());
        assert_eq!(revealed.dimmed_markers, vec![5..7, 11..13]);
    }

    #[test]
    fn italic_single_star() {
        let text = "An *italic* word.\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![3..4, 10..11]);
        assert_eq!(spans(text, SpanStyle::Italic), vec![4..10]);
    }

    #[test]
    fn bold_italic_nested() {
        let text = "A ***both*** word.\n";
        let result = plan(text, &[]);
        // The outer `emphasis` node's own delimiter-collection is recursive
        // (see `plan_delimited_span`'s doc comment), so its merged "*" run
        // absorbs the immediately-adjacent inner "**" run too: outer's
        // prefix/suffix end up as the full "***" on each side. The inner
        // `strong_emphasis` node separately contributes its own narrower
        // "**" prefix/suffix, which is a strict subset of the outer's and so
        // disappears once overlapping hidden ranges are merged (folds can't
        // overlap) — the merged result is just the outer's "***" on each
        // side.
        assert_eq!(result.hidden_markers, vec![2..5, 9..12]);
        // Outer (italic) and inner (bold) content both resolve to the same
        // "both" byte range, so it gets both styles applied together, which
        // is exactly the desired combined bold+italic rendering.
        assert_eq!(spans(text, SpanStyle::Bold), vec![5..9]);
        assert_eq!(spans(text, SpanStyle::Italic), vec![5..9]);
    }

    #[test]
    fn strikethrough_self_nested_grammar_quirk() {
        let text = "A ~~strike~~ word.\n";
        let result = plan(text, &[]);
        // This grammar represents `~~x~~` as a `strikethrough` node wrapping
        // another `strikethrough` node; the inner node's narrower marker
        // ranges are subsets of the outer's and merge away, same as above.
        assert_eq!(result.hidden_markers, vec![2..4, 10..12]);
        assert!(
            spans(text, SpanStyle::Strikethrough)
                .iter()
                .all(|range| *range == (4..10))
        );
    }

    #[test]
    fn inline_code_always_dimmed_never_hidden() {
        let text = "Some `code` here.\n";
        let touching = plan(text, &[]);
        assert!(touching.hidden_markers.is_empty());
        assert_eq!(touching.dimmed_markers, vec![5..6, 10..11]);
        assert_eq!(spans(text, SpanStyle::InlineCode), vec![6..10]);

        let with_cursor = plan(text, &[8..8]);
        assert!(with_cursor.hidden_markers.is_empty());
        assert_eq!(with_cursor.dimmed_markers, vec![5..6, 10..11]);
    }

    #[test]
    fn highlight_mark() {
        let text = "Some ==highlighted== text.\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![5..7, 18..20]);
        assert_eq!(spans(text, SpanStyle::Highlight), vec![7..18]);
    }

    #[test]
    fn highlight_mark_ignored_inside_code_span() {
        let text = "Some `==literal==` code.\n";
        assert!(spans(text, SpanStyle::Highlight).is_empty());
    }

    #[test]
    fn multiple_selections_each_reveal_their_own_span() {
        let text = "**a** and **b**\n";
        // second selection sits inside the second bold span
        let result = plan(text, &[12..12]);
        assert_eq!(result.hidden_markers, vec![0..2, 3..5]);
        assert!(result.dimmed_markers.contains(&(10..12)));
        assert!(result.dimmed_markers.contains(&(13..15)));
    }

    #[test]
    fn no_hidden_range_ever_overlaps_another() {
        // A fold can't be created over an already-folded sub-range (Zed's
        // display-map panics on overlapping fold input), so this is the
        // actual regression this milestone shipped with: nested constructs
        // producing overlapping ranges crashed the real editor.
        for text in [
            "A ***both*** word.\n",
            "A ~~strike~~ word.\n",
            "# H\n\n***nested at start of line***\n",
            "Some **a *b* c** text.\n",
            "# Heading One\n\nSome regular paragraph text that should render completely unstyled by visual_md.\n\nSome **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n## Heading Two\n\nNot a heading: this line starts with a hash but no space:\n#nope\n",
        ] {
            let result = plan(text, &[]);
            for window in result.hidden_markers.windows(2) {
                assert!(
                    window[0].end <= window[1].start,
                    "overlapping hidden ranges {:?} and {:?} for {text:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    #[test]
    fn merge_ranges_merges_overlapping_and_touching() {
        assert_eq!(merge_ranges(vec![2..5, 3..5]), vec![2..5]);
        assert_eq!(merge_ranges(vec![0..2, 2..4]), vec![0..4]);
        assert_eq!(merge_ranges(vec![5..7, 0..2]), vec![0..2, 5..7]);
        assert_eq!(merge_ranges(vec![0..0, 1..3]), vec![1..3]);
    }

    #[test]
    fn subtract_ranges_removes_overlap() {
        assert_eq!(subtract_ranges(&[0..10], &[3..5]), vec![0..3, 5..10]);
        assert_eq!(
            subtract_ranges(&[0..10], &[0..10]),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(subtract_ranges(&[0..10], &[]), vec![0..10]);
        assert_eq!(subtract_ranges(&[0..5, 8..10], &[4..9]), vec![0..4, 9..10]);
    }

    #[test]
    fn unordered_list_bullets() {
        let text = "- one\n- two\n- three\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..2, GlyphKind::Bullet),
                (6..8, GlyphKind::Bullet),
                (12..14, GlyphKind::Bullet),
            ]
        );
    }

    #[test]
    fn ordered_list_renumbers_regardless_of_source_digits() {
        let text = "1. a\n1. b\n1. c\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..3, GlyphKind::Ordinal("1. ".to_string())),
                (5..8, GlyphKind::Ordinal("2. ".to_string())),
                (10..13, GlyphKind::Ordinal("3. ".to_string())),
            ]
        );
    }

    #[test]
    fn ordered_list_honors_custom_start_number() {
        let text = "5. a\n5. b\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..3, GlyphKind::Ordinal("5. ".to_string())),
                (5..8, GlyphKind::Ordinal("6. ".to_string())),
            ]
        );
    }

    #[test]
    fn task_checkbox_suppresses_bullet_and_registers_widget() {
        let text = "- [ ] a\n- [x] b\n";
        let result = plan(text, &[]);
        assert!(
            result.glyph_markers.is_empty(),
            "bullets should be suppressed for task items"
        );
        assert_eq!(result.hidden_markers, vec![0..2, 8..10]);
        assert_eq!(result.checkboxes, vec![(2..5, false), (10..13, true)]);
    }

    #[test]
    fn blockquote_marker_becomes_bar_glyph() {
        let text = "> quoted text\n";
        let result = plan(text, &[]);
        assert!(
            result
                .glyph_markers
                .contains(&(0..2, GlyphKind::BlockquoteBar))
        );
    }

    #[test]
    fn multiline_blockquote_bars_every_line() {
        let text = "> line one\n> line two\n";
        let result = plan(text, &[]);
        let bar_count = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::BlockquoteBar)
            .count();
        assert_eq!(
            bar_count, 2,
            "both the first and continuation line should get a bar"
        );
    }

    #[test]
    fn nested_blockquote_stacks_bars() {
        let text = "> > nested\n";
        let result = plan(text, &[]);
        let bar_count = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::BlockquoteBar)
            .count();
        assert_eq!(bar_count, 2, "each nesting level contributes its own bar");
    }

    #[test]
    fn callout_hides_bracket_syntax_and_tints_the_whole_box() {
        let text = "> [!warning] Be careful\n";
        let result = plan(text, &[]);
        // The marker never lands in `hidden_markers` (that would trigger the
        // generic space-fold): visual_md.rs renders its own title widget for
        // an untouched callout, driven by `Plan::callouts` instead.
        assert!(!result.hidden_markers.contains(&(2..12)));
        assert!(!result.dimmed_markers.contains(&(2..12)));
        let callout = result.callouts.first().expect("expected one callout");
        assert_eq!(callout.marker_range, 2..12);
        assert_eq!(callout.kind, CalloutKind::Warning);
        assert_eq!(callout.raw_type_name, "warning");
        assert_eq!(callout.fold, CalloutFold::None);
        assert!(!callout.touched);
        // The tint covers the whole block_quote node (title row included),
        // not just the body.
        assert_eq!(callout.node_range, 0..text.len());
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(range, style)| *range == callout.node_range
                    && *style == SpanStyle::Callout(CalloutKind::Warning))
        );
    }

    #[test]
    fn touched_callout_marker_reveals_as_raw_dimmed_text() {
        let text = "> [!warning] Be careful\n";
        // Cursor placed inside the `[!warning]` marker range (2..12).
        let result = plan(text, &[5..5]);
        assert!(result.dimmed_markers.contains(&(2..12)));
        assert!(!result.hidden_markers.contains(&(2..12)));
        let callout = result.callouts.first().expect("expected one callout");
        assert!(callout.touched);
        // Still tinted and still tracked, even while its title shows raw.
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(range, style)| *range == callout.node_range
                    && *style == SpanStyle::Callout(CalloutKind::Warning))
        );
    }

    #[test]
    fn cursor_anywhere_on_the_title_line_touches_it_not_just_the_marker_bytes() {
        let text = "> [!warning] Be careful\n";
        // Cursor inside "careful", well past the `[!warning]` marker itself,
        // but still on the same title line -- matches `plan_heading`'s own
        // "touched" scope (the whole construct's line, not a bare overlap
        // with the marker's own bytes).
        let result = plan(text, &[20..20]);
        let callout = result.callouts.first().expect("expected one callout");
        assert!(callout.touched);
        assert!(result.dimmed_markers.contains(&(2..12)));
    }

    #[test]
    fn cursor_in_a_multiline_callouts_body_does_not_touch_the_title() {
        let text = "> [!note] Title\n> Body line one\n> Body line two\n";
        // Cursor inside "Body line one" (second line) -- the body stays
        // independently live-previewed; it shouldn't couple back to
        // revealing the title's raw `[!note]` text.
        let cursor = text.find("line one").unwrap();
        let result = plan(text, &[cursor..cursor]);
        let callout = result.callouts.first().expect("expected one callout");
        assert!(!callout.touched);
        assert!(
            result.hidden_markers.is_empty()
                || !result.hidden_markers.contains(&callout.marker_range)
        );
        assert!(!result.dimmed_markers.contains(&callout.marker_range));
    }

    #[test]
    fn callout_fold_suffix_states_parse_correctly() {
        let none = plan("> [!note] a\n", &[]);
        assert_eq!(none.callouts.first().unwrap().fold, CalloutFold::None);

        let expanded = plan("> [!note]+ a\n", &[]);
        assert_eq!(
            expanded.callouts.first().unwrap().fold,
            CalloutFold::Expanded
        );

        let collapsed = plan("> [!note]- a\n", &[]);
        let collapsed_callout = collapsed.callouts.first().unwrap();
        assert_eq!(collapsed_callout.fold, CalloutFold::Collapsed);
        assert!(collapsed_callout.fold.is_collapsed());
        assert!(!CalloutFold::None.is_collapsed());
    }

    #[test]
    fn callout_suffix_range_supports_insert_and_replace() {
        // No suffix: a zero-width insertion point right after `]`.
        let none = plan("> [!note] a\n", &[]);
        let callout = none.callouts.first().unwrap();
        assert_eq!(
            callout.suffix_range,
            callout.marker_range.end..callout.marker_range.end
        );

        // An existing suffix: a one-byte range to replace or delete.
        let collapsed = plan("> [!note]- a\n", &[]);
        let callout = collapsed.callouts.first().unwrap();
        assert_eq!(callout.suffix_range.end - callout.suffix_range.start, 1);
        assert_eq!(callout.suffix_range.end, callout.marker_range.end);
    }

    #[test]
    fn unrecognized_callout_type_keeps_its_raw_name() {
        let result = plan("> [!todo] buy milk\n", &[]);
        let callout = result.callouts.first().expect("expected one callout");
        assert_eq!(callout.kind, CalloutKind::Other);
        assert_eq!(callout.raw_type_name, "todo");
    }

    #[test]
    fn nested_callouts_do_not_panic_and_each_gets_its_own_info() {
        let text = "> [!note] outer\n> > [!warning] inner\n";
        let result = plan(text, &[]);
        assert_eq!(result.callouts.len(), 2);
        assert!(result.callouts.iter().any(|c| c.kind == CalloutKind::Note));
        assert!(
            result
                .callouts
                .iter()
                .any(|c| c.kind == CalloutKind::Warning)
        );
    }

    #[test]
    fn plain_blockquote_is_not_a_callout() {
        let text = "> just a quote\n";
        let result = plan(text, &[]);
        assert!(
            !result
                .styled_spans
                .iter()
                .any(|(_, style)| matches!(style, SpanStyle::Callout(_)))
        );
    }

    #[test]
    fn callout_kind_recognizes_common_aliases() {
        assert_eq!(CalloutKind::from_type_name("NOTE"), CalloutKind::Note);
        assert_eq!(CalloutKind::from_type_name("info"), CalloutKind::Note);
        assert_eq!(CalloutKind::from_type_name("tip"), CalloutKind::Tip);
        assert_eq!(CalloutKind::from_type_name("caution"), CalloutKind::Warning);
        assert_eq!(CalloutKind::from_type_name("bug"), CalloutKind::Danger);
        assert_eq!(
            CalloutKind::from_type_name("something-else"),
            CalloutKind::Other
        );
    }

    #[test]
    fn no_fold_inducing_range_ever_overlaps_another_m2() {
        // hidden_markers, glyph_markers, and checkboxes all become creases in
        // the real editor and share the same fold space, so all three
        // together must be globally disjoint or folding panics (same class
        // of bug as `no_hidden_range_ever_overlaps_another`).
        for text in [
            "- [ ] a\n- [x] b\n",
            "1. one\n2. two\n   - nested\n   - list\n",
            "> [!note] title\n> body **bold** text\n",
            "- item with **bold** and a [!note] look-alike\n",
        ] {
            let result = plan(text, &[]);
            let mut all: Vec<Range<usize>> = result
                .hidden_markers
                .iter()
                .cloned()
                .chain(result.checkboxes.iter().map(|(range, _)| range.clone()))
                .chain(result.glyph_markers.iter().map(|(range, _)| range.clone()))
                .collect();
            all.sort_by_key(|range| range.start);
            for window in all.windows(2) {
                assert!(
                    window[0].end <= window[1].start,
                    "overlapping fold ranges {:?} and {:?} for {text:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Collects every range the real editor turns into a fold, for the
    /// overlap check shared by several tests below.
    fn fold_inducing_ranges(result: &Plan) -> Vec<Range<usize>> {
        let mut all: Vec<Range<usize>> = result
            .hidden_markers
            .iter()
            .cloned()
            .chain(result.checkboxes.iter().map(|(range, _)| range.clone()))
            .chain(result.glyph_markers.iter().map(|(range, _)| range.clone()))
            .collect();
        all.sort_by_key(|range| range.start);
        all
    }

    fn assert_no_overlaps(ranges: &[Range<usize>], text: &str) {
        for window in ranges.windows(2) {
            assert!(
                window[0].end <= window[1].start,
                "overlapping fold ranges {:?} and {:?} for {text:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn plan_viewport_matches_full_plan_when_range_covers_everything() {
        let text = "# H\n\n- [ ] a\n- [x] b\n\n> [!note] hi **bold** text\n";
        assert_eq!(plan_viewport(text, &[], 0..text.len()), plan(text, &[]));
    }

    /// The whole point of `plan_viewport`: a construct entirely outside the
    /// requested range contributes no decorations at all, while one that
    /// does overlap is decorated exactly as `plan` would decorate it.
    #[test]
    fn plan_viewport_prunes_constructs_outside_the_visible_range() {
        let text = "# First heading\n\nSecond paragraph with **bold** text.\n";
        let second_paragraph_start = text.find("Second").unwrap();

        let scoped = plan_viewport(text, &[], second_paragraph_start..text.len());
        assert!(
            scoped
                .hidden_markers
                .iter()
                .all(|range| range.start >= second_paragraph_start),
            "heading marker should have been pruned: {:?}",
            scoped.hidden_markers
        );
        assert!(
            scoped
                .styled_spans
                .iter()
                .any(|(range, style)| *style == SpanStyle::Bold
                    && range.start >= second_paragraph_start),
            "the in-range bold span should still be planned: {:?}",
            scoped.styled_spans
        );
        assert!(
            !scoped
                .styled_spans
                .iter()
                .any(|(_, style)| matches!(style, SpanStyle::Heading(_))),
            "the offscreen heading should not have been planned at all: {:?}",
            scoped.styled_spans
        );
    }

    /// A selection must always reveal its own construct's raw markers, even
    /// if the caller's notion of "visible" is stale relative to where the
    /// cursor actually is (see `plan_viewport`'s doc comment).
    #[test]
    fn plan_viewport_still_reveals_a_selection_outside_the_given_range() {
        let text = "Some **bold** text far from the viewport.\n";
        let cursor_inside_bold = 8..8;
        // An empty visible range elsewhere in the document; the selection
        // must still win.
        let scoped = plan_viewport(text, &[cursor_inside_bold], 0..0);
        assert_eq!(scoped.dimmed_markers, vec![5..7, 11..13]);
    }

    /// Deterministic xorshift PRNG so this test suite stays dependency-free.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, bound: usize) -> usize {
            (self.next() as usize) % bound.max(1)
        }
    }

    /// Assembles a pseudo-random document out of every construct this
    /// planner understands, so property tests below exercise combinations no
    /// hand-written fixture would think to try.
    fn random_document(rng: &mut Rng, lines: usize) -> String {
        let fragments = [
            "# Heading text\n",
            "## Nested heading\n",
            "Plain paragraph text with nothing special.\n",
            "Some **bold**, *italic*, ***both***, ~~strike~~, ==mark==, and `code`.\n",
            "- bullet one\n",
            "- [ ] unchecked task\n",
            "- [x] checked task\n",
            "1. ordered one\n",
            "2. ordered two\n",
            "> plain quote\n",
            "> [!warning] callout title\n",
            "> continuation of a quote\n",
            "[a link](https://example.com)\n",
            "<https://example.com>\n",
            "---\n",
            "```rust\nfn main() {}\n```\n",
            "| A | B |\n|--|--:|\n| 1 | 2 |\n",
            "\n",
        ];
        let mut text = String::new();
        for _ in 0..lines {
            text.push_str(fragments[rng.below(fragments.len())]);
        }
        text
    }

    #[test]
    fn property_random_documents_never_produce_overlapping_folds_or_panic() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            let selections = if rng.below(2) == 0 {
                vec![]
            } else {
                let start = rng.below(text.len() + 1);
                let end = start + rng.below(text.len() + 1 - start);
                vec![start..end]
            };
            let result = plan(&text, &selections);
            assert_no_overlaps(&fold_inducing_ranges(&result), &text);
            for range in fold_inducing_ranges(&result) {
                assert!(
                    range.end <= text.len(),
                    "out-of-bounds fold range {range:?} for {text:?}"
                );
            }
            for (range, _) in &result.styled_spans {
                assert!(
                    range.end <= text.len(),
                    "out-of-bounds styled span {range:?} for {text:?}"
                );
            }
        }
    }

    /// Same random corpus, but checking `plan_viewport`'s pruning specifically:
    /// restricting to a sub-range must never produce a fold-inducing range
    /// that plan() (the unrestricted equivalent) didn't already produce, and
    /// must still never overlap.
    #[test]
    fn property_random_viewports_are_a_subset_of_the_full_plan_and_never_overlap() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            if text.is_empty() {
                continue;
            }
            let start = rng.below(text.len());
            let end = start + rng.below(text.len() - start + 1);

            let full = plan(&text, &[]);
            let scoped = plan_viewport(&text, &[], start..end);

            assert_no_overlaps(&fold_inducing_ranges(&scoped), &text);

            let full_hidden: std::collections::HashSet<_> =
                full.hidden_markers.iter().cloned().collect();
            for range in &scoped.hidden_markers {
                assert!(
                    full_hidden.contains(range),
                    "scoped plan invented a hidden range {range:?} the full plan didn't have, for {text:?}"
                );
            }
        }
    }

    /// Regression tripwire, not a strict benchmark: on a document with tens
    /// of thousands of decoration-inducing lines, `plan_viewport` restricted
    /// to a small window must (a) finish in a generous but bounded time, and
    /// (b) actually emit far fewer decorations than an unrestricted `plan`
    /// over the same text — proving the pruning in `walk_block` is doing
    /// real work, not a no-op.
    #[test]
    fn plan_viewport_scopes_work_on_a_large_document() {
        let mut rng = Rng(0xc0ff_ee15_dead_beef);
        let text = random_document(&mut rng, 20_000);

        let window_start = text.len() / 2;
        let window_end = window_start + 200;

        let started = std::time::Instant::now();
        let scoped = plan_viewport(&text, &[], window_start..window_end);
        let scoped_elapsed = started.elapsed();
        assert!(
            scoped_elapsed < std::time::Duration::from_secs(5),
            "scoped plan over a large document took {scoped_elapsed:?}, which is suspiciously slow"
        );

        let full = plan(&text, &[]);
        let scoped_decorations = fold_inducing_ranges(&scoped).len() + scoped.styled_spans.len();
        let full_decorations = fold_inducing_ranges(&full).len() + full.styled_spans.len();
        assert!(
            scoped_decorations * 20 < full_decorations,
            "scoped plan ({scoped_decorations} decorations) should be far smaller than the full \
             plan ({full_decorations} decorations) for a large document"
        );
    }

    /// A fold-inducing range that silently swallowed a `\n` would merge two
    /// buffer rows into what the display layer sees as one row (the
    /// per-row chunk-counting in `element.rs`'s `from_chunks` only advances
    /// on a literal `\n` in the chunk stream) — every row after it would
    /// then be off by one, which is exactly the kind of thing that would
    /// make a mouse-driven selection or copy silently skip a line break.
    /// Checked directly here (byte-level, on the planner's own output)
    /// rather than only indirectly via the editor-integration tests, since
    /// this is the one invariant that must hold for every construct
    /// category, not just the ones with dedicated fixtures.
    #[test]
    fn no_fold_inducing_range_contains_a_newline_byte() {
        for text in [
            "> line one\n> line two\n",
            "> > nested\n> > second\n",
            "> [!note] title\n> body line two\n> body line three\n",
            "- [ ] a\n- [x] b\n- plain\n",
            "1. one\n2. two\n   - nested\n",
            "# Heading One\n\nSome regular paragraph text that should render completely unstyled by visual_md.\n\nSome **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n## Heading Two\n",
        ] {
            let result = plan(text, &[]);
            let all: Vec<(&str, Range<usize>)> = result
                .hidden_markers
                .iter()
                .cloned()
                .map(|r| ("hidden", r))
                .chain(
                    result
                        .glyph_markers
                        .iter()
                        .map(|(r, _)| ("glyph", r.clone())),
                )
                .chain(
                    result
                        .checkboxes
                        .iter()
                        .map(|(r, _)| ("checkbox", r.clone())),
                )
                .chain(result.dimmed_markers.iter().cloned().map(|r| ("dimmed", r)))
                .collect();
            for (label, range) in &all {
                let slice = &text[range.clone()];
                assert!(
                    !slice.contains('\n'),
                    "{label} range {range:?} ({slice:?}) contains a newline byte for {text:?}"
                );
            }
        }
    }

    #[test]
    fn link_hidden_when_not_touched() {
        let text = "[Zed](https://zed.dev)\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 4..22]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(1..4, SpanStyle::Link)]);
    }

    #[test]
    fn link_dimmed_when_cursor_touches() {
        let text = "[Zed](https://zed.dev)\n";
        let result = plan(text, &[2..2]); // cursor inside "Zed"
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..1, 4..22]);
        // The link text stays styled regardless of raw/rendered state, same
        // as bold/italic.
        assert_eq!(result.styled_spans, vec![(1..4, SpanStyle::Link)]);
    }

    #[test]
    fn autolink_hidden_when_not_touched() {
        let text = "<https://zed.dev>\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 16..17]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(1..16, SpanStyle::Link)]);
    }

    #[test]
    fn autolink_dimmed_when_cursor_touches() {
        let text = "<https://zed.dev>\n";
        let result = plan(text, &[5..5]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..1, 16..17]);
    }

    #[test]
    fn reference_style_links_are_left_completely_raw() {
        // No `[1]: url` definition backs either of these, and tree-sitter-md
        // can't tell the difference from the per-paragraph inline text alone
        // (see `plan_link`'s doc comment) -- so `shortcut_link`/
        // `full_reference_link` are never dispatched to `plan_link` at all,
        // and nothing about the line should be touched.
        for text in ["[shortcut]\n", "[ref link][1]\n", "[collapsed][]\n"] {
            let result = plan(text, &[]);
            assert!(
                spans(text, SpanStyle::Link).is_empty(),
                "unexpected link styling for {text:?}"
            );
            assert!(
                result.hidden_markers.is_empty(),
                "unexpected hidden markers for {text:?}"
            );
            assert!(
                result.dimmed_markers.is_empty(),
                "unexpected dimmed markers for {text:?}"
            );
        }
    }

    #[test]
    fn link_nested_inside_bold_still_gets_styled() {
        let text = "**[Zed](https://zed.dev)**\n";
        let link_spans = spans(text, SpanStyle::Link);
        let bold_spans = spans(text, SpanStyle::Bold);
        assert_eq!(link_spans.len(), 1);
        assert_eq!(bold_spans.len(), 1);
        assert!(
            bold_spans[0].start <= link_spans[0].start && link_spans[0].end <= bold_spans[0].end,
            "link span {:?} should sit inside bold span {:?}",
            link_spans[0],
            bold_spans[0]
        );
    }

    #[test]
    fn image_and_email_are_not_mistaken_for_a_markdown_link() {
        // `image` is a distinct node kind from `inline_link` (embeds are a
        // separate, not-yet-implemented milestone) -- confirm it never picks
        // up link styling by accident.
        let text = "![alt text](image.png)\n";
        assert!(spans(text, SpanStyle::Link).is_empty());
    }

    #[test]
    fn email_autolink_is_styled_like_a_uri_autolink() {
        let text = "<user@example.com>\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 17..18]);
        assert_eq!(result.styled_spans, vec![(1..17, SpanStyle::Link)]);
    }

    #[test]
    fn thematic_break_variants_populate_horizontal_rules() {
        for text in ["---\n", "***\n", "___\n", "- - -\n"] {
            let result = plan(text, &[]);
            assert_eq!(result.horizontal_rules, vec![0..text.len()], "for {text:?}");
            assert!(result.hidden_markers.is_empty());
            assert!(
                result.glyph_markers.is_empty(),
                "a `- - -` rule must not be mistaken for a list, for {text:?}"
            );
        }
    }

    #[test]
    fn touched_thematic_break_is_left_alone() {
        let text = "---\n";
        let result = plan(text, &[1..1]);
        assert!(result.horizontal_rules.is_empty());
        assert!(result.hidden_markers.is_empty());
        assert!(result.dimmed_markers.is_empty());
    }

    #[test]
    fn fenced_code_block_captures_language_and_borders() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..8, Some("rust".to_string())), (21..25, None)]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))]
        );
    }

    #[test]
    fn fenced_code_block_with_tilde_fence() {
        let text = "~~~python\nprint(1)\n~~~\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..10, Some("python".to_string())), (19..23, None)]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(10..19, Some("python".to_string()))]
        );
    }

    #[test]
    fn fenced_code_block_indentation_is_kept_in_border_range() {
        let text = "  ```js\n  indented\n  ```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..8, Some("js".to_string())), (19..25, None)]
        );
    }

    #[test]
    fn fenced_code_block_with_no_language_still_produces_content_entry() {
        let text = "```\nno language\n```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..4, None), (16..20, None)]
        );
        assert_eq!(result.code_fence_content, vec![(4..16, None)]);
    }

    #[test]
    fn touched_opening_fence_line_is_excluded_but_content_stays() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[1..1]); // cursor on the opening fence line
        assert_eq!(
            result.code_fence_borders,
            vec![(21..25, None)],
            "closing line still not touched"
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))],
            "content stays present regardless of cursor position, per spec"
        );
    }

    #[test]
    fn touched_closing_fence_line_is_excluded_but_content_stays() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[22..22]); // cursor on the closing fence line
        assert_eq!(
            result.code_fence_borders,
            vec![(0..8, Some("rust".to_string()))]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))]
        );
    }

    #[test]
    fn touched_content_line_leaves_both_fence_borders_alone() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[10..10]); // cursor inside the code content
        assert_eq!(
            result.code_fence_borders,
            vec![(0..8, Some("rust".to_string())), (21..25, None)],
            "cursor being in the content shouldn't reveal either fence line"
        );
    }

    #[test]
    fn table_structure_and_alignment_evenly_spaced() {
        let text = "| Name | Role |\n|------|-----:|\n| Ada  | Dev  |\n";
        let result = plan(text, &[]);
        assert_eq!(result.tables.len(), 1);
        let table = &result.tables[0];
        assert_eq!(
            table.alignments,
            vec![TableAlignment::Default, TableAlignment::Right]
        );
        assert_eq!(table.delimiter_line, Some(16..31));
        let pipes: Vec<Range<usize>> = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::TablePipe)
            .map(|(range, _)| range.clone())
            .collect();
        assert_eq!(pipes, vec![0..1, 7..8, 14..15, 32..33, 39..40, 46..47]);
        assert_eq!(table.rows.len(), 2, "one header row + one data row");
        assert_eq!(
            table.rows[0],
            vec![
                TableCell {
                    leading_gap: 1..2,
                    content: 2..6,
                    trailing_gap: 6..7
                },
                TableCell {
                    leading_gap: 8..9,
                    content: 9..13,
                    trailing_gap: 13..14
                },
            ]
        );
        assert_eq!(
            table.rows[1],
            vec![
                TableCell {
                    leading_gap: 33..34,
                    content: 34..37,
                    trailing_gap: 37..39
                },
                TableCell {
                    leading_gap: 40..41,
                    content: 41..44,
                    trailing_gap: 44..46
                },
            ]
        );
    }

    #[test]
    fn table_all_four_alignments() {
        let text = "| A | B | C | D |\n|--|:--|--:|:-:|\n| 1 | 2 | 3 | 4 |\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.tables[0].alignments,
            vec![
                TableAlignment::Default,
                TableAlignment::Left,
                TableAlignment::Right,
                TableAlignment::Center,
            ]
        );
    }

    #[test]
    fn table_with_no_spacing_has_empty_gaps() {
        let text = "|A|B|\n|-|-|\n|1|2|\n";
        let result = plan(text, &[]);
        let table = &result.tables[0];
        for row in &table.rows {
            for cell in row {
                assert!(
                    cell.leading_gap.is_empty(),
                    "no source whitespace to widen: {cell:?}"
                );
                assert!(
                    cell.trailing_gap.is_empty(),
                    "no source whitespace to widen: {cell:?}"
                );
            }
        }
        // Content itself is still captured correctly despite no padding.
        assert_eq!(table.rows[0][0].content, 1..2);
        assert_eq!(table.rows[1][1].content, 15..16);
    }

    #[test]
    fn table_multi_row_groups_cells_by_column() {
        let text = "| A | B |\n|--|--|\n| 1 | 2 |\n| 3 | 4 |\n";
        let result = plan(text, &[]);
        let table = &result.tables[0];
        assert_eq!(table.rows.len(), 3, "header + two data rows");
        assert_eq!(table.rows[1].len(), 2);
        assert_eq!(table.rows[2].len(), 2);
    }

    #[test]
    fn touched_delimiter_line_is_excluded() {
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let result = plan(text, &[11..11]); // cursor on the delimiter row
        assert_eq!(result.tables[0].delimiter_line, None);
    }

    #[test]
    fn untouched_delimiter_line_is_present() {
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let result = plan(text, &[0..0]); // cursor elsewhere in the table
        assert!(result.tables[0].delimiter_line.is_some());
    }

    #[test]
    fn table_cell_with_bold_gets_both_decorations() {
        let text = "| A |\n|--|\n| **bold** |\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.tables.len(),
            1,
            "table structure is still recognized"
        );
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(_, style)| *style == SpanStyle::Bold),
            "bold inside a cell should still be styled"
        );
    }
}
