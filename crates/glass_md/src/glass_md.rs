//! glass_md: an Obsidian-style live-preview Markdown editor for Zed.
//!
//! This crate hooks every full-mode editor via [`editor::Addon`] and decorates
//! Markdown buffers without ever mutating the underlying buffer text —
//! decorations are view-only, per the spec's core mechanic. See
//! `docs/live-preview-spec.md` for the full behavior spec this crate targets.
//!
//! ## Status
//!
//! **M4** (current): M1's inline engine (headings, bold, italic, bold-italic,
//! strikethrough, `==highlight==`, inline code) plus M2's lists (bullets and
//! renumbered ordinals, nesting inherited for free from source indentation),
//! interactive task checkboxes (real click-to-toggle, unconditionally
//! rendered per spec — never hidden, never gated on the cursor), blockquotes
//! (marker replaced with a left-bar glyph, nesting stacks naturally), and
//! callouts (`> [!type]`: bracket syntax hidden, body tinted per
//! recognized type). A full callout box (its own accent-bar element, icon,
//! foldable title row) needs block-level rendering this pass doesn't add;
//! see `callout_style`'s doc comment for the precise scope line. Links,
//! tables, and frontmatter remain out of scope until later milestones.
//!
//! M3 hardened the above rather than adding new constructs: decoration
//! generation is scoped to the visible viewport (see [`plan::plan_viewport`]
//! and `visible_byte_range`) so a large document doesn't replan/rediff its
//! entire contents on every keystroke, the fold-diffing algorithm is O(n)
//! instead of O(n²), and the planner has property-based tests (random
//! documents/selections never produce overlapping or out-of-bounds
//! decorations) alongside the hand-written fixtures.
//!
//! M4 gives headings genuine variable row height instead of style-only
//! (bold + color at uniform size): each level now carries a
//! `HighlightStyle::font_size_scale` (a new field added to `gpui`, see its
//! doc comment) that `crates/editor/src/element.rs` reads per row to shape
//! that row's text at a taller size and lay out every row below it
//! accordingly — see `LineWithInvisibles::row_y_offset`/`row_for_y`, the
//! canonical replacement for the uniform `line_height * (row -
//! scroll_position.y)` formula used throughout that file. The core-editor
//! surface this touches is deliberately bounded: text painting, cursor
//! positioning, hit-testing, the current-line/search-highlight backgrounds,
//! and the gutter (line numbers, fold/crease icons) all stay pixel-aligned
//! with a resized heading row. Diff-hunk markers, the full git-blame gutter,
//! and the excerpt-expand icon are known, disclosed gaps — those still
//! assume uniform row height, a rare/secondary-feature cosmetic
//! misalignment rather than a functional one.
//!
//! **M5** adds the one editing behavior this crate has beyond pure
//! decoration: "smart list continuation" (see [`list_continuation`]).
//! Pressing Enter on a bullet/ordinal/checkbox item's own marker line
//! continues that marker onto the new line instead of inserting a plain
//! newline; Enter on an empty item outdents it (or exits the list at the top
//! level) instead. Wired in ahead of `Editor::newline` via
//! `editor.register_action`, so it only ever changes behavior for the exact
//! cases the spec calls out and falls through to the normal handler (via
//! `cx.propagate()`) for everything else.
//!
//! **M6** adds markdown links (`[text](url)`), autolinks (`<https://...>` /
//! `<user@example.com>`), and horizontal rules (`---`/`***`/`___`).
//! Reference-style links (`[text][1]`, `[shortcut]`) are deliberately not
//! handled — see `plan::plan_link`'s doc comment for why the per-paragraph
//! inline grammar can't tell those apart from ordinary bracketed prose
//! without a document-wide reference-definition lookup this crate doesn't
//! do. Two more scope lines, both disclosed the same way the callout-box gap
//! above already is:
//! - Link text gets a real color (`theme::colors().link_text_hover`) even
//!   though every other construct in this crate deliberately renders in the
//!   same color as prose — color is a link's only non-structural cue, so
//!   this is a narrow, intentional exception (see `apply_style_highlights`).
//! - Only autolinks are actually clickable to navigate: Zed's generic
//!   cmd+click URL detection (`find_url` in
//!   `crates/editor/src/hover_links.rs`) scans the *raw buffer text* around
//!   the click for a URL-shaped substring, which still works once only the
//!   `<`/`>` chars are hidden (the visible text remains the literal URL at
//!   its real offset). A `[text](url)` link's visible glyph is the link
//!   *text*, not the URL, so that same generic mechanism can't find the URL
//!   from a click there — making it clickable would need a custom widget
//!   (like the checkbox's) or extending `hover_links`, deferred here.
//!
//! The horizontal rule uses a genuinely different mechanism from every other
//! decoration in this file: `insert_blocks`/`BlockProperties` (see
//! `apply_horizontal_rules`) instead of a `FoldPlaceholder`, since a fold can
//! only size itself to its own content and there's no way to make one
//! stretch to the editor's actual visible width for a full-width `<hr>`.
//!
//! The parsing and decision logic lives in [`plan`], a pure function with no
//! GPUI or `Editor` dependency. This module's job is only to drive it off
//! editor events and translate its plain byte ranges into creases and
//! `highlight_text` calls, diffing against what was previously applied so a
//! single keystroke or cursor move touches only what changed.

mod list_continuation;
mod plan;

use std::any::Any;
use std::collections::HashSet;
use std::ops::Range;

use editor::actions::Newline;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, Crease, CreaseId, CustomBlockId, DisplayPoint,
    DisplayRow, DisplaySnapshot, ToDisplayPoint,
};
use editor::{Addon, Anchor, Bias, Editor, EditorEvent, MultiBufferOffset, MultiBufferSnapshot, SelectionEffects};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontStyle, FontWeight, HighlightStyle, Hsla,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, StrikethroughStyle,
    Styled, Subscription, WeakEntity, Window, div, px, rgb, svg,
};
use gpui::prelude::FluentBuilder;
use plan::{GlyphKind, Plan, SpanStyle};
use settings::{RegisterSetting, Settings, SettingsContent};
use util::ResultExt;

/// The `glass_md` user setting: `{ "glass_md": { "enabled": true } }`. Defaults to
/// on, per the project goal of always using live preview for Markdown in Zed.
#[derive(RegisterSetting)]
pub struct GlassMdSettings {
    pub enabled: bool,
}

impl Settings for GlassMdSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .glass_md
                .as_ref()
                .and_then(|glass_md| glass_md.enabled)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(register_editor).detach();
}

/// Registers glass_md on every full-mode editor, unconditionally.
///
/// Registration cannot be gated on "is this a Markdown buffer" here: a buffer's
/// language is assigned asynchronously (by `LanguageRegistry`, sometimes after
/// loading a grammar), so it is frequently still unknown at the moment the
/// `Editor` entity is constructed. Gating here raced against that and silently
/// dropped the decorations on newly opened files. Instead every full-mode editor
/// gets the addon and `refresh` itself checks the language on each call, so a
/// later `LanguageChanged` event (subscribed to below) picks it up correctly.
fn register_editor(editor: &mut Editor, window: Option<&mut Window>, cx: &mut Context<Editor>) {
    let Some(window) = window else {
        return;
    };
    if !editor.mode().is_full() {
        return;
    }

    let buffer = editor.buffer().clone();
    let state = GlassMdState::new(buffer, window, cx);
    let newline_action = editor.register_action(cx.listener(intercept_newline));
    editor.register_addon(GlassMdAddon {
        _state: state,
        _newline_action: newline_action,
        folded_markers: Vec::new(),
        hr_blocks: Vec::new(),
    });
    refresh(editor, window, cx);

    // A newly-registered editor's very first refresh can have its first
    // fold-inducing crease (whichever one happens to come first in the
    // document — a heading marker, a `**` marker, ...) render as Zed's own
    // default "⋯" fold ellipsis instead of glass_md's real placeholder.
    // Confirmed via direct user testing to be purely a first-paint timing
    // issue: it self-heals the instant *any* later refresh fires, e.g. just
    // clicking that line, and the planner/placeholder content itself is
    // correct the whole time (this crate's own tests, which read the
    // planner's output and each placeholder's `collapsed_text` directly,
    // never reproduce it). Scheduling one more refresh right after the
    // window's first real paint settles it without waiting on the user to
    // interact with anything first.
    let editor_entity = cx.entity();
    window.on_next_frame(move |window, cx| {
        editor_entity.update(cx, |editor, cx| refresh(editor, window, cx));
    });
}

/// Intercepts a plain `Enter` keypress on a glass_md-managed Markdown editor
/// and, when every cursor sits on a list item's own marker line, replaces it
/// with "smart list continuation" (see `docs/live-preview-spec.md`) instead
/// of a plain newline: the same bullet/ordinal/checkbox is carried onto the
/// new line, and Enter on an empty item outdents (or exits the list) rather
/// than adding another empty bullet.
///
/// Registered on the editor before `Editor::newline` itself (see
/// `register_editor`), so it runs first on every `Newline` dispatch;
/// `cx.propagate()` falls through to the normal handler for every case this
/// doesn't apply to -- a disabled/non-Markdown buffer, a non-empty selection
/// anywhere, or any cursor not on a list marker line.
fn intercept_newline(editor: &mut Editor, _: &Newline, window: &mut Window, cx: &mut Context<Editor>) {
    if !is_markdown_editor(editor, cx) || !GlassMdSettings::try_get(cx).is_some_and(|settings| settings.enabled) {
        cx.propagate();
        return;
    }

    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor.selections.all::<MultiBufferOffset>(&display_snapshot);
    if selections.is_empty() || selections.iter().any(|selection| !selection.is_empty()) {
        cx.propagate();
        return;
    }

    let text = editor.buffer().read(cx).snapshot(cx).text();
    let Some(edits) = selections
        .iter()
        .map(|selection| list_continuation::newline_edit(&text, selection.head().0))
        .collect::<Option<Vec<_>>>()
    else {
        cx.propagate();
        return;
    };

    // Multiple cursors can each produce their own edit; apply them together
    // in one buffer edit (in ascending order, since selections are already
    // reported in document order) and track the running length delta so
    // each cursor's `cursor_after` -- computed independently against the
    // *original* text -- lands at the right offset once every earlier
    // edit's own length change has shifted things.
    let mut buffer_edits = Vec::with_capacity(edits.len());
    let mut new_cursors = Vec::with_capacity(edits.len());
    let mut delta: isize = 0;
    for edit in &edits {
        let start = (edit.replace.start as isize + delta) as usize;
        let end = (edit.replace.end as isize + delta) as usize;
        buffer_edits.push((MultiBufferOffset(start)..MultiBufferOffset(end), edit.insert.clone()));
        let cursor_after = (edit.cursor_after as isize + delta) as usize;
        new_cursors.push(MultiBufferOffset(cursor_after)..MultiBufferOffset(cursor_after));
        delta += edit.insert.len() as isize - edit.replace.len() as isize;
    }

    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(buffer_edits, cx);
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges(new_cursors);
        });
    });
}

fn is_markdown_editor(editor: &Editor, cx: &App) -> bool {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    snapshot
        .language_at(MultiBufferOffset(0))
        .is_some_and(|language| language.name().as_ref() == "Markdown")
}

/// Addon registered on every glass_md-managed editor. Besides keeping
/// [`GlassMdState`] (and the editor-event subscriptions it owns) alive, this
/// is where the currently-folded marker creases are tracked so `refresh` can
/// diff against them instead of re-folding everything from scratch.
struct GlassMdAddon {
    _state: Entity<GlassMdState>,
    /// Keeps the `Newline` interceptor (see [`intercept_newline`]) alive for
    /// as long as this editor is glass_md-managed; dropping it would let
    /// `Editor::newline` handle every Enter keypress unconditionally again.
    _newline_action: Subscription,
    folded_markers: Vec<(Range<usize>, String, CreaseId)>,
    /// Horizontal-rule block decorations currently inserted (see
    /// `apply_horizontal_rules`). Unlike `folded_markers`, no content-key
    /// string travels alongside the range: a horizontal rule's rendering
    /// never varies, so range equality alone is enough to diff old vs. new.
    hr_blocks: Vec<(Range<usize>, CustomBlockId)>,
}

impl Addon for GlassMdAddon {
    fn to_any(&self) -> &dyn Any {
        self
    }

    // `Addon::to_any_mut` defaults to returning `None` — easy to miss since
    // nothing enforces overriding it, and the failure mode is silent:
    // `Editor::addon_mut::<T>()` just always returns `None` too, rather than
    // panicking or failing to compile. Without this override, `apply_folds`
    // below (which relies on `addon_mut` both to read back the previous
    // refresh's creases and to persist the new ones) always treated "no
    // previous state" as true, so every single refresh recreated every
    // decoration's crease from scratch and never removed the previous set —
    // an unbounded accumulation of duplicate creases on every keystroke or
    // cursor move, not just a missed optimization. Found via an M3 test that
    // actually inspects `GlassMdAddon::folded_markers` after a real refresh
    // through the `cx.observe_new` registration path, rather than only
    // calling `refresh` directly the way earlier tests did.
    fn to_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

struct GlassMdState {
    editor: WeakEntity<Editor>,
    _subscriptions: [Subscription; 2],
}

impl GlassMdState {
    fn new(
        buffer: Entity<editor::MultiBuffer>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Entity<Self> {
        let editor_entity = cx.entity();
        cx.new(|cx| GlassMdState {
            editor: editor_entity.downgrade(),
            _subscriptions: [
                // `WeakEntity::update_in` only works for a window's own root
                // view; `Editor` is a child view within the workspace/pane
                // tree, not a window root, so it always fails with "entity
                // has no current window" there. `subscribe_in` already hands
                // this callback its own `window`, so capture that directly
                // into a plain `update` instead.
                cx.subscribe_in(&editor_entity, window, |_state, editor, event, window, cx| {
                    // Scrolling is included, not just edits/selection moves:
                    // decoration generation is scoped to the visible
                    // viewport (see `visible_byte_range`), so newly
                    // scrolled-into-view content needs its own refresh to
                    // get decorated rather than staying stale/blank until
                    // the next edit or cursor move.
                    if matches!(
                        event,
                        EditorEvent::BufferEdited
                            | EditorEvent::SelectionsChanged { .. }
                            | EditorEvent::ScrollPositionChanged { .. }
                    ) {
                        editor.update(cx, |editor, cx| refresh(editor, window, cx));
                    }
                }),
                cx.subscribe_in(&buffer, window, |state: &mut GlassMdState, _buffer, event, window, cx| {
                    if matches!(event, multi_buffer::Event::LanguageChanged(..)) {
                        state
                            .editor
                            .update(cx, |editor, cx| refresh(editor, window, cx))
                            .log_err();
                    }
                }),
            ],
        })
    }
}

/// Marker type used with `Editor::highlight_text_key` to namespace glass_md's
/// own highlights (`HighlightKey::TypePlus(TypeId::of::<GlassMdMarkdown>(),
/// key)`) from every other highlight source. The `usize` key picks the
/// decoration family (heading, emphasis, etc.) so unrelated categories can be
/// replaced independently.
struct GlassMdMarkdown;

/// Which `GlassMdMarkdown` sub-key each decoration category uses. Kept
/// disjoint by construction (the planner never emits overlapping ranges
/// across categories) so highlight compositing never has to blend two of
/// glass_md's own colors together.
const KEY_DIMMED_MARKER: usize = 0;
// Headings get one key per level (rather than sharing `KEY_HEADING` the way
// M1-M3 did) because each level now also carries its own
// `HighlightStyle::font_size_scale` (see `heading_style`) — keeping them
// disjoint means changing one heading's level cleanly removes its old
// size/color highlight instead of leaving a stale one from a different level
// composited underneath.
const KEY_HEADING_1: usize = 1;
const KEY_HEADING_2: usize = 2;
const KEY_HEADING_3: usize = 3;
const KEY_HEADING_4: usize = 4;
const KEY_HEADING_5: usize = 5;
const KEY_HEADING_6: usize = 6;
const KEY_BOLD: usize = 7;
const KEY_ITALIC: usize = 8;
const KEY_STRIKETHROUGH: usize = 9;
const KEY_LINK: usize = 10;

fn heading_key(level: u8) -> usize {
    match level {
        1 => KEY_HEADING_1,
        2 => KEY_HEADING_2,
        3 => KEY_HEADING_3,
        4 => KEY_HEADING_4,
        5 => KEY_HEADING_5,
        _ => KEY_HEADING_6,
    }
}

/// How many extra display rows above/below the actual visible viewport
/// `refresh` still decorates. Purely a smoothness margin for scrolling (so a
/// decoration doesn't visibly pop in a frame after it scrolls into view) —
/// not a correctness requirement, since `plan_viewport` also always covers
/// the current selection(s) regardless of this margin.
const VIEWPORT_OVERSCAN_ROWS: u32 = 200;

/// The buffer byte range `refresh` should bother planning decorations for.
///
/// Returns the whole buffer when the editor hasn't been laid out yet
/// (`visible_line_count` is `None` until the first paint, which is exactly
/// the situation at `register_editor`'s own initial `refresh` call) — there
/// is no meaningful viewport to scope to yet, and decorating everything is
/// the safe default rather than risking an under-decorated freshly opened
/// file.
fn visible_byte_range(
    editor: &Editor,
    display_snapshot: &DisplaySnapshot,
    text_len: usize,
    _cx: &App,
) -> Range<usize> {
    let Some(visible_lines) = editor.visible_line_count() else {
        return 0..text_len;
    };

    let scroll_top = editor.scroll_manager.anchor().anchor.to_display_point(display_snapshot);
    let top_row = scroll_top.row().0.saturating_sub(VIEWPORT_OVERSCAN_ROWS);
    let bottom_row = scroll_top
        .row()
        .0
        .saturating_add(visible_lines.ceil() as u32)
        .saturating_add(VIEWPORT_OVERSCAN_ROWS);

    // A column of `u32::MAX` is not a safe "clamp to end of line" sentinel:
    // `clip_point` clips the *row* first and the column arithmetic further
    // downstream can overflow before the column itself is ever clamped.
    // Using column 0 of the row just past `bottom_row` instead gives a point
    // that is always valid pre-clip (row is clamped by `clip_point`, column
    // 0 never needs clamping) and still lands at or beyond the true end of
    // `bottom_row`.
    let max_point = display_snapshot.max_point();
    let top_point = display_snapshot.clip_point(DisplayPoint::new(DisplayRow(top_row), 0), Bias::Left);
    let bottom_point = if bottom_row.saturating_add(1) >= max_point.row().0 {
        max_point
    } else {
        display_snapshot.clip_point(DisplayPoint::new(DisplayRow(bottom_row + 1), 0), Bias::Left)
    };

    let start = top_point.to_offset(display_snapshot, Bias::Left).0;
    let end = bottom_point.to_offset(display_snapshot, Bias::Right).0;
    start..end
}

/// Recomputes and reapplies glass_md's decorations for `editor`'s buffer:
/// parses the current text, checks which constructs the current selection(s)
/// touch, and diffs the result against what is already folded/highlighted.
///
/// Decoration generation itself is scoped to the visible viewport (plus a
/// margin, see [`VIEWPORT_OVERSCAN_ROWS`]) via [`plan::plan_viewport`], not
/// the whole buffer: for a multi-MB file, re-planning (and re-diffing) every
/// decoration in the entire document on every keystroke is what actually
/// makes typing feel laggy, not the tree-sitter parse itself. See
/// `plan_viewport`'s own doc comment for the full reasoning.
fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let enabled = is_markdown_editor(editor, cx)
        && GlassMdSettings::try_get(cx).is_some_and(|settings| settings.enabled);

    // Line numbers don't fit the live-preview reading experience (Obsidian's
    // own live preview doesn't show them either), so hide them for as long
    // as glass_md is actually decorating this buffer, restoring whatever the
    // user's real global preference is otherwise so this doesn't fight a
    // manual toggle once glass_md steps out of the way (e.g. the setting
    // gets disabled, or the buffer's language changes away from Markdown).
    editor.set_show_line_numbers(
        if enabled {
            false
        } else {
            editor::EditorSettings::get_global(cx).gutter.line_numbers
        },
        cx,
    );

    // Zed's gutter shows a fold-toggle chevron on hover for any row it
    // thinks is "foldable" — including via a generic indentation heuristic
    // (`EditorSnapshot::starts_indent`) that fires independently of any
    // crease glass_md itself created, and does so for ordinary indented
    // Markdown content (nested list items, etc.) with nothing real to fold.
    // `hide_gutter_toggle` on glass_md's own creases (see `apply_folds`)
    // only suppresses the toggle for rows *those* creases cover; this
    // suppresses the indentation-heuristic fallback too, for the same
    // reason line numbers are hidden above — "foldable" isn't a meaningful
    // concept in a live-preview reading view.
    editor.set_show_fold_indicators(
        if enabled {
            false
        } else {
            editor::EditorSettings::get_global(cx).gutter.folds
        },
        cx,
    );

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let computed = if enabled {
        let text = snapshot.text();
        let display_snapshot = editor.display_snapshot(cx);
        let selections = editor
            .selections
            .all::<MultiBufferOffset>(&display_snapshot)
            .into_iter()
            .map(|selection| {
                let range = selection.range();
                range.start.0..range.end.0
            })
            .collect::<Vec<_>>();
        let visible_range = visible_byte_range(editor, &display_snapshot, text.len(), cx);
        plan::plan_viewport(&text, &selections, visible_range)
    } else {
        Plan::default()
    };

    let editor_handle = cx.weak_entity();
    // The `String` alongside each range is a content key, not just an
    // identifier: `apply_folds` diffs on `(range, key)` together, not range
    // alone, specifically so a checkbox toggle — which edits `[ ]` to `[x]`
    // in place, leaving its byte range unchanged — still gets its crease
    // recreated with the flipped glyph instead of being treated as
    // "unchanged, nothing to do".
    let mut folds: Vec<(Range<usize>, String, editor::FoldPlaceholder)> = computed
        .hidden_markers
        .iter()
        .map(|range| (range.clone(), " ".to_string(), space_placeholder()))
        .chain(computed.glyph_markers.iter().map(|(range, kind)| match kind {
            GlyphKind::Bullet => (range.clone(), "bullet".to_string(), bullet_placeholder()),
            GlyphKind::Ordinal(text) => (
                range.clone(),
                format!("ordinal:{text}"),
                ordinal_placeholder(text.clone()),
            ),
            GlyphKind::BlockquoteBar => (
                range.clone(),
                "blockquote_bar".to_string(),
                blockquote_bar_placeholder(),
            ),
        }))
        .collect();
    folds.extend(computed.checkboxes.iter().map(|(range, checked)| {
        (
            range.clone(),
            format!("checkbox:{checked}"),
            checkbox_placeholder(editor_handle.clone(), *checked),
        )
    }));

    apply_folds(editor, &snapshot, folds, window, cx);
    apply_style_highlights(editor, &snapshot, &computed, cx);
    apply_horizontal_rules(editor, &snapshot, &computed, cx);
}

fn to_anchor_range(snapshot: &MultiBufferSnapshot, range: &Range<usize>) -> Range<Anchor> {
    snapshot.anchor_before(MultiBufferOffset(range.start))
        ..snapshot.anchor_after(MultiBufferOffset(range.end))
}

/// Base settings shared by every glass_md fold placeholder.
///
/// A *genuinely* zero-width fold (`gpui::Empty`, or an empty `div()`)
/// reliably panics deep in `tab_map.rs` ("attempt to subtract with
/// overflow") the moment it's folded — confirmed by bisection down to a
/// single `**bold**` span, and independent of anything in this crate's own
/// ranges (verified overlap-free by the planner's own tests). Zed's fold
/// engine appears to require folds to have some positive measured width, so
/// every placeholder here renders at least one character.
fn base_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        constrain_width: false,
        merge_adjacent: false,
        ..editor::FoldPlaceholder::default()
    }
}

fn space_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, _| div().child(SharedString::from(" ")).into_any_element()),
        collapsed_text: Some(SharedString::from(" ")),
        ..base_placeholder()
    }
}

/// A list bullet, drawn as a real filled dot rather than a Unicode `•`:
/// glyph coverage for symbol characters varies enough across fonts (see
/// `checkbox_placeholder`'s doc comment for a case where it silently failed
/// entirely) that a drawn shape is the more robust choice, not just a
/// stylistic one.
fn bullet_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, cx| {
            let color = {
                use theme::ActiveTheme;
                cx.theme().colors().icon_muted
            };
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(14.))
                .h_full()
                .child(div().size(px(5.)).rounded_full().bg(color))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("• ")),
        ..base_placeholder()
    }
}

/// An ordered list item's renumbered marker (e.g. `"3. "`). Kept as plain
/// text — digits are plain ASCII with no font-coverage risk, unlike the
/// symbol glyphs `bullet_placeholder`/`blockquote_bar_placeholder`/
/// `checkbox_placeholder` deliberately avoid.
fn ordinal_placeholder(text: String) -> editor::FoldPlaceholder {
    let text = SharedString::from(text);
    editor::FoldPlaceholder {
        render: {
            let text = text.clone();
            std::sync::Arc::new(move |_, _, _| div().child(text.clone()).into_any_element())
        },
        collapsed_text: Some(text),
        ..base_placeholder()
    }
}

/// A blockquote/callout's left bar, drawn as a real filled rectangle rather
/// than the Unicode block-drawing character `▎` — see `bullet_placeholder`'s
/// doc comment for why a drawn shape is preferred over a symbol glyph here.
fn blockquote_bar_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, cx| {
            let color = {
                use theme::ActiveTheme;
                cx.theme().colors().border
            };
            div()
                .flex()
                .items_center()
                .w(px(14.))
                .h_full()
                .child(div().w(px(3.)).h_full().bg(color))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("▎ ")),
        ..base_placeholder()
    }
}

/// An interactive checkbox for a `[ ]`/`[x]` task marker. Per spec these stay
/// clickable in both raw and rendered states, so unlike every other
/// placeholder here it is never conditionally hidden vs. dimmed — it is
/// always folded.
///
/// `checked` is captured by value from the plan rather than read live from
/// the buffer inside `render`: `render` runs *during* the editor's own paint
/// pass, and reading the same editor entity back out from inside that —
/// `editor.read_with(cx, ...)` — is a genuine re-entrant borrow conflict, not
/// a hypothetical one. It failed exactly like CLAUDE.md's guidance on this
/// predicts: silently (`render` producing nothing visible, since the
/// contained `read_with` never got a chance to run its closure), not with a
/// crash, which made it look at first like the whole placeholder mechanism
/// was broken rather than this one call. `refresh` recreates this crease
/// with a freshly-captured `checked` on every edit (see its own comment on
/// why the fold-diffing key includes checked state), so this stays correct
/// across a toggle without ever reading live state from inside render.
///
/// Drawn as a real bordered box (with a real `check.svg` icon when checked)
/// rather than the Unicode `☐`/`☑` characters this used to render: `☑`
/// (U+2611) is a much rarer symbol than `☐` (U+2610) in typical font glyph
/// coverage, and silently rendering nothing when a fold's `render` closure
/// produces content the shaper can't display looks identical to the
/// re-entrancy failure mode described above — confirmed the checked state
/// specifically was the one silently blank in the real app while the
/// unchecked box rendered fine. A drawn shape has no font dependency at all.
fn checkbox_placeholder(editor: WeakEntity<Editor>, checked: bool) -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |fold_id, range, cx| {
            let editor = editor.clone();
            let colors = {
                use theme::ActiveTheme;
                cx.theme().colors()
            };
            let box_size = px(13.);
            div()
                .id(fold_id)
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .w(px(18.))
                .h_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(box_size)
                        .rounded(px(3.))
                        .when(!checked, |el| el.border_1().border_color(colors.icon_muted))
                        .when(checked, |el| el.bg(colors.icon_accent))
                        .when(checked, |el| {
                            el.child(
                                svg()
                                    .path("icons/check.svg")
                                    .size(px(9.))
                                    .text_color(colors.editor_background),
                            )
                        }),
                )
                .on_click(move |_event, _window, cx| {
                    let new_text = if checked { "[ ]" } else { "[x]" };
                    editor
                        .update(cx, |editor, cx| editor.edit([(range.clone(), new_text)], cx))
                        .log_err();
                })
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from(if checked { "[x]" } else { "[ ]" })),
        ..base_placeholder()
    }
}

/// Diffs `folds` against the creases folded by the previous refresh (tracked
/// on [`GlassMdAddon`]), removing (and unfolding) whichever ranges are no
/// longer wanted and folding whichever are newly wanted, leaving unchanged
/// ranges alone entirely.
fn apply_folds(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    folds: Vec<(Range<usize>, String, editor::FoldPlaceholder)>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let previous = editor
        .addon_mut::<GlassMdAddon>()
        .map(|addon| std::mem::take(&mut addon.folded_markers))
        .unwrap_or_default();

    // Diffed on `(range, key)` together, not range alone: see `refresh`'s
    // comment on why a checkbox's content key changes with its checked
    // state even though its range doesn't, so a toggle recreates its crease
    // (with the flipped glyph) instead of being mistaken for "unchanged".
    //
    // A `HashSet` here, not a `Vec` scanned with `.contains`: with viewport
    // scoping (see `refresh`) this still runs on every scroll/edit/selection
    // change, and a linear-scan diff makes the whole function O(n²) in the
    // number of decorations in view — negligible for a handful of markers,
    // but exactly the kind of thing that turns into visible typing lag once
    // a screenful of a dense document is on screen.
    let wanted: HashSet<(Range<usize>, String)> =
        folds.iter().map(|(range, key, _)| (range.clone(), key.clone())).collect();
    let mut kept = Vec::new();
    let mut stale_ids = Vec::new();
    for (range, key, id) in previous {
        if wanted.contains(&(range.clone(), key.clone())) {
            kept.push((range, key, id));
        } else {
            stale_ids.push(id);
        }
    }

    if !stale_ids.is_empty() {
        let removed_ranges: Vec<Range<Anchor>> = editor
            .remove_creases(stale_ids, cx)
            .into_iter()
            .map(|(_, range)| range)
            .collect();
        // `inclusive: false`, not `true`: `unfold_ranges` unfolds every fold
        // that *intersects* the given ranges, and with `inclusive: true`
        // that includes folds merely touching a range's boundary, not just
        // ones overlapping it. glass_md's own folds routinely sit right next
        // to each other byte-for-byte (e.g. a task item's hidden bullet
        // ending exactly where its checkbox crease begins), so
        // `inclusive: true` here was unfolding a perfectly valid *adjacent*
        // crease every time a neighboring one went stale — e.g. every
        // checkbox toggle spuriously unfolded the task's own bullet marker,
        // even though that crease's id was never in `stale_ids` and
        // `addon.folded_markers` still (incorrectly) believed it was folded.
        // The removed ranges here always come from creases that genuinely,
        // strictly overlap themselves (they're each fold's own exact range),
        // so `inclusive: false` still finds and removes exactly the stale
        // folds without also catching their neighbors.
        editor.unfold_ranges(&removed_ranges, false, false, cx);
    }

    let already_kept: HashSet<(Range<usize>, String)> =
        kept.iter().map(|(range, key, _)| (range.clone(), key.clone())).collect();
    // `insert_creases` requires its input sorted by position — its
    // underlying sum-tree cursor can only seek forward and panics
    // ("cannot seek backward") otherwise. `folds` arrives here as several
    // categories concatenated together (hidden markers, glyphs, checkboxes),
    // not globally sorted.
    let mut to_add: Vec<(Range<usize>, String, editor::FoldPlaceholder)> = folds
        .into_iter()
        .filter(|(range, key, _)| !already_kept.contains(&(range.clone(), key.clone())))
        .collect();
    to_add.sort_by_key(|(range, _, _)| range.start);

    if !to_add.is_empty() {
        let creases: Vec<Crease<Anchor>> = to_add
            .iter()
            .map(|(range, _, placeholder)| {
                // These are permanent decorative replacements (a hidden
                // marker, a bullet glyph, a checkbox), never a
                // user-collapsible region, so the gutter's default
                // "any folded row gets a toggle" fallback is wrong here —
                // see `Crease::hide_gutter_toggle`'s doc comment.
                Crease::simple(to_anchor_range(snapshot, range), placeholder.clone())
                    .without_gutter_toggle()
            })
            .collect();
        let ids = editor.insert_creases(creases.clone(), cx);
        editor.fold_creases(creases, false, window, cx);
        kept.extend(to_add.into_iter().map(|(range, key, _)| (range, key)).zip(ids).map(
            |((range, key), id)| (range, key, id),
        ));
    }

    if let Some(addon) = editor.addon_mut::<GlassMdAddon>() {
        addon.folded_markers = kept;
    }
}

/// Diffs `computed.horizontal_rules` against the blocks inserted by the
/// previous refresh, same shape as `apply_folds`'s diff (`std::mem::take`
/// the addon's previous list, split into kept vs. stale by whether the range
/// is still wanted, remove the stale ones, insert the newly wanted ones).
///
/// This is a real, separate mechanism from every other decoration in this
/// file, not a stylistic choice: a `FoldPlaceholder` (used everywhere else —
/// bullets, blockquote bars, checkboxes) can only size itself to its own
/// content (`AvailableSpace::MinContent` unless `constrain_width` asks it to
/// match a specific *collapsed text* width — see
/// `crates/editor/src/element.rs`'s `ChunkReplacement::Renderer` handling),
/// so there is no way to make one stretch to the editor's actual visible
/// width for a full-width `<hr>`. The editor's block-decoration API
/// (`insert_blocks`/`BlockProperties`) is built for exactly this — its
/// render callback receives a real `max_width: Pixels` to draw against (see
/// `render_horizontal_rule`), and a `BlockPlacement::Replace` swaps out the
/// entire visual row rather than decorating text within it.
fn apply_horizontal_rules(editor: &mut Editor, snapshot: &MultiBufferSnapshot, computed: &Plan, cx: &mut Context<Editor>) {
    let previous = editor
        .addon_mut::<GlassMdAddon>()
        .map(|addon| std::mem::take(&mut addon.hr_blocks))
        .unwrap_or_default();

    let wanted: HashSet<Range<usize>> = computed.horizontal_rules.iter().cloned().collect();
    let mut kept = Vec::new();
    // `remove_blocks` specifically wants `collections::HashSet` (an
    // `FxHashSet`), not `std::collections::HashSet` -- the type this file
    // otherwise uses everywhere else (e.g. `apply_folds`'s own `wanted` set).
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for (range, id) in previous {
        if wanted.contains(&range) {
            kept.push((range, id));
        } else {
            stale_ids.insert(id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let already_kept: HashSet<Range<usize>> = kept.iter().map(|(range, _)| range.clone()).collect();
    let new_ranges: Vec<Range<usize>> = computed
        .horizontal_rules
        .iter()
        .filter(|range| !already_kept.contains(*range))
        .cloned()
        .collect();

    if !new_ranges.is_empty() {
        let new_blocks: Vec<BlockProperties<Anchor>> = new_ranges
            .iter()
            .map(|range| {
                let anchor_range = to_anchor_range(snapshot, range);
                BlockProperties {
                    placement: BlockPlacement::Replace(anchor_range.start..=anchor_range.end),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: std::sync::Arc::new(render_horizontal_rule),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(new_blocks, None, cx);
        kept.extend(new_ranges.into_iter().zip(ids));
    }

    if let Some(addon) = editor.addon_mut::<GlassMdAddon>() {
        addon.hr_blocks = kept;
    }
}

/// Renders a `---`/`***`/`___` line as a full-width thin rule, using
/// `BlockContext::max_width` (the real remaining editor width, given to this
/// callback directly by the block-decoration layer) rather than a fixed
/// pixel guess or a flex `w_full()` that can't resolve against the
/// indeterminate width a fold placeholder would otherwise be laid out in.
fn render_horizontal_rule(cx: &mut BlockContext) -> AnyElement {
    let color = {
        use theme::ActiveTheme;
        cx.theme().colors().border
    };
    div()
        .w(cx.max_width)
        .h(cx.line_height)
        .flex()
        .items_center()
        .child(div().w_full().h(px(1.)).bg(color))
        .into_any_element()
}

fn apply_style_highlights(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let anchor_ranges = |ranges: &[Range<usize>]| -> Vec<Range<Anchor>> {
        ranges.iter().map(|range| to_anchor_range(snapshot, range)).collect()
    };
    let spans_of = |style: SpanStyle| -> Vec<Range<usize>> {
        computed
            .styled_spans
            .iter()
            .filter(|(_, span_style)| *span_style == style)
            .map(|(range, _)| range.clone())
            .collect()
    };

    editor.highlight_text_key::<GlassMdMarkdown>(
        KEY_DIMMED_MARKER,
        anchor_ranges(&computed.dimmed_markers),
        dim_marker_style(),
        false,
        cx,
    );

    // Zed's own tree-sitter-based Markdown syntax theme already colors
    // headings/bold/italic/strikethrough distinctly (that's a separate
    // layer from these `highlight_text` calls, driven by the buffer's
    // language grammar). Per explicit product direction — "remove color
    // customizations", matching the Obsidian live-preview reference, where
    // these render in the same color as surrounding prose — a `None` color
    // here would leave that underlying syntax color showing through
    // unblended, not actually remove it. Pinning to the editor's normal
    // foreground color is what actually cancels it out.
    let normal_color = {
        use theme::ActiveTheme;
        cx.theme().colors().editor_foreground
    };

    for level in 1..=6u8 {
        let ranges: Vec<Range<usize>> = computed
            .styled_spans
            .iter()
            .filter(|(_, style)| *style == SpanStyle::Heading(level))
            .map(|(range, _)| range.clone())
            .collect();
        editor.highlight_text_key::<GlassMdMarkdown>(
            heading_key(level),
            anchor_ranges(&ranges),
            heading_style(level, normal_color),
            false,
            cx,
        );
    }

    editor.highlight_text_key::<GlassMdMarkdown>(
        KEY_BOLD,
        anchor_ranges(&spans_of(SpanStyle::Bold)),
        bold_style(normal_color),
        false,
        cx,
    );
    editor.highlight_text_key::<GlassMdMarkdown>(
        KEY_ITALIC,
        anchor_ranges(&spans_of(SpanStyle::Italic)),
        italic_style(normal_color),
        false,
        cx,
    );
    editor.highlight_text_key::<GlassMdMarkdown>(
        KEY_STRIKETHROUGH,
        anchor_ranges(&spans_of(SpanStyle::Strikethrough)),
        strikethrough_style(normal_color),
        false,
        cx,
    );
    // Inline code, ==highlight== marks, and callout bodies are deliberately
    // left with no color/background styling — glass_md previously tinted
    // all three, but per the live-preview reference (Obsidian) and explicit
    // product direction, only structural styling (bold weight, italic
    // style, marker hiding/dimming, heading size) survives. The `[!type]`
    // bracket syntax on a callout is still hidden via `hidden_markers` —
    // that's handled entirely by `apply_folds`, untouched by this. Unlike
    // headings/bold/italic, Zed's own syntax theme doesn't tint these
    // distinctly enough to need a counteracting color override here.
    //
    // Links are the one deliberate, narrow exception to "no added color"
    // above: color is a link's only non-structural cue (no weight/slant
    // distinguishes it the way bold/italic have their own), so per explicit
    // product direction a real link gets colored using
    // `link_text_hover` — the same theme token Zed's own generic cmd+hover
    // link highlight already uses (`crates/editor/src/hover_links.rs`), so
    // it reads as "this is a link" the same way everywhere else in the app
    // rather than inventing a new color for the same affordance.
    let link_color = {
        use theme::ActiveTheme;
        cx.theme().colors().link_text_hover
    };
    editor.highlight_text_key::<GlassMdMarkdown>(
        KEY_LINK,
        anchor_ranges(&spans_of(SpanStyle::Link)),
        link_style(link_color),
        false,
        cx,
    );
}

fn dim_marker_style() -> HighlightStyle {
    HighlightStyle {
        color: Some(rgb(0x6b7280).into()),
        ..HighlightStyle::default()
    }
}

/// Roughly matches common heading-scale conventions (Obsidian, browser
/// default `<h1>`-`<h6>`): H1 largest, shrinking toward H6, which stays at
/// the editor's normal text size (a size of exactly `1.0`, not slightly
/// under, so a document with no true H6 styling convention doesn't end up
/// with unexpectedly small "normal" text).
fn heading_font_size_scale(level: u8) -> f32 {
    match level {
        1 => 1.8,
        2 => 1.5,
        3 => 1.3,
        4 => 1.15,
        5 => 1.05,
        _ => 1.0,
    }
}

fn heading_style(level: u8, normal_color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(normal_color),
        font_weight: Some(FontWeight::BOLD),
        font_size_scale: Some(heading_font_size_scale(level)),
        ..HighlightStyle::default()
    }
}

fn bold_style(normal_color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(normal_color),
        font_weight: Some(FontWeight::BOLD),
        ..HighlightStyle::default()
    }
}

fn italic_style(normal_color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(normal_color),
        font_style: Some(FontStyle::Italic),
        ..HighlightStyle::default()
    }
}

fn strikethrough_style(normal_color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(normal_color),
        strikethrough: Some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        ..HighlightStyle::default()
    }
}

/// Color only — no weight/slant/underline — per the spec's "styled as a
/// link (color, no underline by default)".
fn link_style(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(color),
        ..HighlightStyle::default()
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(gpui::SemanticVersion::new(0, 0, 0), cx);
            editor::init(cx);
            // Registers the real `cx.observe_new::<Editor>` wiring so
            // `GlassMdAddon` actually exists on the test editor and
            // `apply_folds`'s diffing has real previous-state to diff
            // against (M3's viewport-scoping tests read
            // `GlassMdAddon::folded_markers` directly). Harmless for tests
            // that call `refresh` directly without ever inspecting the
            // addon: `refresh` is idempotent, and this only makes those
            // tests more representative of the real registration path
            // rather than changing what they assert.
            init(cx);
        });
    }

    fn markdown_language() -> std::sync::Arc<language::Language> {
        std::sync::Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Markdown".into(),
                ..Default::default()
            },
            None,
        ))
    }

    /// Reproduces a real crash: folding the M1 test fixture (headings plus
    /// every inline construct) panicked deep in `tab_map.rs` with "attempt
    /// to subtract with overflow", even though the *planner's* output was
    /// already verified overlap-free (see `plan::tests::no_hidden_range_ever_overlaps_another`).
    /// This test exists to catch that class of bug directly against the
    /// real `Editor`/fold machinery, in seconds, instead of only via a full
    /// GUI relaunch.
    #[gpui::test]
    async fn folding_the_full_test_fixture_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "ˇ# Heading One\n\n",
            "Some regular paragraph text that should render completely unstyled by glass_md.\n\n",
            "Some **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n",
            "## Heading Two\n\n",
            "Not a heading: this line starts with a hash but no space:\n#nope\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
    }

    /// Same class of regression as the M1 test above, covering M2's new
    /// fold-inducing constructs (list bullets/ordinals, task checkboxes,
    /// blockquote/callout bars) against the real `Editor`/fold machinery —
    /// exactly where the M1 crashes actually surfaced, not caught by the
    /// pure planner's own tests.
    #[gpui::test]
    async fn folding_lists_tasks_and_callouts_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        // Headings interspersed with lists is deliberate, not incidental:
        // it's exactly the mix that surfaced a real bug — `insert_creases`
        // requires its input sorted by position (its underlying sum-tree
        // cursor can only seek forward and panics "cannot seek backward"
        // otherwise), and concatenating hidden_markers (from headings) with
        // glyph_markers (from lists) as whole groups, rather than merging
        // them by position, produced exactly this out-of-order input the
        // moment a heading and a list traded places in the document like
        // they do here.
        cx.set_state(concat!(
            "ˇ# M2 test\n\n",
            "## Lists\n\n",
            "1. one\n1. two\n1. three\n\n",
            "## Tasks\n\n",
            "- [ ] unchecked\n- [x] checked\n- plain item\n\n",
            "## Quotes\n\n",
            "> [!warning] Careful\n> multi-line body with **bold** text\n\n",
            "> plain quote\n> > nested quote\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
    }

    /// The checkbox click handler edits the buffer with a same-length
    /// `[ ]`/`[x]` replacement at the marker's anchor range; this exercises
    /// that same edit path directly (real click/mouse-event simulation isn't
    /// available in this harness) to confirm it round-trips correctly and
    /// that a follow-up refresh - reacting to the resulting `BufferEdited`
    /// event - doesn't panic either.
    ///
    /// This is also the regression test for two real M3-era bugs in the
    /// diffing path itself, both only observable once `GlassMdAddon` is
    /// actually registered (see `init_test`'s own comment): (1) `Addon`'s
    /// `to_any_mut` defaults to `None`, and this impl never overrode it, so
    /// `apply_folds` never actually had previous state to diff against —
    /// every refresh recreated every crease from scratch, unboundedly; (2)
    /// `apply_folds` called `unfold_ranges(.., inclusive: true, ..)` when
    /// removing the checkbox's now-stale crease, which also unfolds any
    /// fold merely *touching* that range's boundary — exactly the task
    /// item's own hidden bullet, which sits byte-for-byte adjacent to the
    /// checkbox. Toggling the checkbox was spuriously un-hiding the bullet
    /// (`"- ☑ task\n"` instead of `" ☑ task\n"`) even though its crease id
    /// was never marked stale. Fixed by switching to `inclusive: false`.
    #[gpui::test]
    async fn toggling_a_checkbox_edits_the_buffer_and_survives_refresh(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ- [ ] task\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        // A real bug lived here: the checkbox's `render` closure used to
        // read the editor's own live buffer content via `editor.read_with`
        // to determine checked state — a genuine re-entrant borrow (render
        // runs *during* the editor's own paint pass), which failed silently
        // rather than panicking, producing no visible glyph at all. Checking
        // `display_text` here, not just that nothing panics, is what catches
        // that class of bug.
        //
        // `display_text` reflects each fold's `collapsed_text`, not the
        // actual painted widget tree (the checkbox itself is a drawn
        // box/SVG now, not text — see `checkbox_placeholder`) — this is
        // still a real, valuable check that the fold exists with the right
        // checked state, just not a substitute for the real GUI screenshot
        // verification of the actual graphic.
        assert_eq!(cx.display_text(), " [ ] task\n");

        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = to_anchor_range(&snapshot, &(2..5));
            editor.edit([(range, "[x]")], cx);
            refresh(editor, window, cx);
        });
        cx.assert_editor_state("ˇ- [x] task\n");
        // The checked glyph must also update after the toggle: the
        // fold-diffing key includes checked state precisely so a same-length
        // `[ ]` -> `[x]` edit (whose byte range is unchanged) still gets its
        // crease recreated rather than being treated as "nothing to do".
        assert_eq!(cx.display_text(), " [x] task\n");
    }

    /// Diagnostic for a real report: in the actual app, list/checkbox/
    /// blockquote glyphs showed as Zed's own default "⋯" fold placeholder
    /// instead of this crate's intended glyphs, and copying returned raw
    /// source rather than even the fallback space/glyph text — suggesting
    /// the custom `render`/`collapsed_text` on these placeholders isn't
    /// taking effect. Checks the actual rendered `display_text` (not just
    /// "does it panic") to find out directly.
    #[gpui::test]
    async fn rendered_text_shows_bullet_glyph_not_default_ellipsis(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ- one\n- two\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        let displayed = cx.display_text();
        eprintln!("glass_md diagnostic: display_text={displayed:?}");
        assert!(displayed.contains('•'), "expected a bullet glyph in {displayed:?}");
        assert!(!displayed.contains('⋯'), "found Zed's default fold ellipsis in {displayed:?}");
    }

    /// Exercises links/autolinks (M6) through a real `refresh()`, not just
    /// the pure planner directly -- confirms `apply_style_highlights`
    /// actually applies `KEY_LINK` and that the fold-based hiding of the
    /// bracket/paren/angle-bracket syntax reaches the real display text.
    #[gpui::test]
    async fn link_and_autolink_hide_syntax_but_keep_text_visible(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇSee [Zed](https://zed.dev) or <https://zed.dev> for more.\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });

        let displayed = cx.display_text();
        assert!(displayed.contains("Zed"), "link text should stay visible in {displayed:?}");
        assert!(displayed.contains("https://zed.dev"), "the bare autolink URL should stay visible in {displayed:?}");
        assert!(!displayed.contains('['), "markdown link brackets should be hidden in {displayed:?}");
        assert!(!displayed.contains('<'), "autolink angle brackets should be hidden in {displayed:?}");
        assert!(
            !displayed.contains("(https://zed.dev)"),
            "the markdown link's own URL should be hidden, only its text kept, in {displayed:?}"
        );
    }

    /// Exercises the horizontal-rule block-decoration path (M6) end to end:
    /// confirms `apply_horizontal_rules` actually reaches
    /// `Editor::insert_blocks` through a real `refresh()`, reading
    /// `GlassMdAddon::hr_blocks` directly the same way the M3 fold tests
    /// already read `folded_markers`.
    #[gpui::test]
    async fn horizontal_rule_inserts_exactly_one_block(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇtext above\n\n---\n\ntext below\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let hr_blocks = &editor.addon::<GlassMdAddon>().unwrap().hr_blocks;
            assert_eq!(hr_blocks.len(), 1, "expected exactly one horizontal-rule block, got {hr_blocks:?}");
        });
    }

    /// A thematic break the cursor is touching should not become a block at
    /// all (its raw `---` shows through instead, matching the untouched-vs-
    /// touched convention every other construct follows).
    #[gpui::test]
    async fn touched_horizontal_rule_is_not_blocked(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ---\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let hr_blocks = &editor.addon::<GlassMdAddon>().unwrap().hr_blocks;
            assert!(hr_blocks.is_empty(), "a touched thematic break should not be blocked, got {hr_blocks:?}");
        });
    }

    /// Combines every construct into one document, unlike the other tests
    /// which each exercise one construct in isolation. Added while chasing a
    /// real user report of every fold rendering as Zed's default ellipsis —
    /// note that this test alone did *not* reproduce it even with matching
    /// content, confirming that bug was specific to the real GUI's paint
    /// pass and not reachable from this headless harness at all (see the
    /// commit message / memory notes for the actual root cause and fix).
    /// Kept anyway since combining constructs is still worth covering for
    /// its own sake.
    #[gpui::test]
    async fn folding_a_document_with_every_construct_combined_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "ˇ# Heading level one\n\n",
            "Some plain paragraph text with **bold**, *italic*, and `inline code`.\n\n",
            "## Heading level two\n\n",
            "- [ ] an unchecked task\n",
            "- [x] a checked task\n",
            "- a plain bullet\n\n",
            "> [!warning] A callout\n",
            "> body text with **bold** inside it\n\n",
            "1. first item\n",
            "2. second item\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        let displayed = cx.display_text();
        assert!(!displayed.contains('⋯'), "found Zed's default fold ellipsis in {displayed:?}");
    }

    /// M3: `refresh` scopes decoration generation to the scrolled viewport
    /// (see `visible_byte_range`), so a construct far above the current
    /// scroll position (well beyond the overscan margin) should get no
    /// decorations at all, while one at the current scroll position still
    /// does. Reads `GlassMdAddon::folded_markers` directly (available since
    /// this test lives in the same crate) rather than inferring it from
    /// `display_text`, so it can name exactly which marker is/isn't present.
    #[gpui::test]
    async fn refresh_scopes_decorations_to_the_scrolled_viewport(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        let mut text = String::from("# Top Heading\n");
        for i in 0..600 {
            text.push_str(&format!("filler line number {i}\n"));
        }
        let bottom_heading_row = text.matches('\n').count() as u32;
        // The cursor sits two lines after the bottom heading, not on the
        // heading itself or immediately after it: `plan_heading` treats a
        // heading's `atx_heading` node range (which includes its trailing
        // newline) as "touched" by a selection sitting right at that
        // boundary, which correctly reveals (dims) the marker instead of
        // folding it — this test is about viewport visibility, not
        // selection-revealing, so it deliberately puts the cursor
        // unambiguously outside the heading's own node range.
        text.push_str("# Bottom Heading\n\nˇfiller line after bottom heading\n");

        cx.set_state(&text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let (top_marker_start, bottom_marker_start) = (0usize, text.rfind("# Bottom Heading").unwrap());
        cx.update_editor(|editor, window, cx| {
            editor.set_scroll_position(gpui::Point::new(0.0, bottom_heading_row as f64), window, cx);
            refresh(editor, window, cx);
            let folded = &editor.addon::<GlassMdAddon>().unwrap().folded_markers;
            assert!(
                !folded.iter().any(|(range, _, _)| range.start == top_marker_start),
                "the offscreen top heading's marker should not have been folded: {folded:?}"
            );
            assert!(
                folded.iter().any(|(range, _, _)| range.start == bottom_marker_start),
                "the on-screen bottom heading's marker should have been folded: {folded:?}"
            );
        });
    }

    /// M3 correctness invariant named directly in the milestone plan:
    /// decorations are view-only, so no sequence of refreshes driven purely
    /// by scrolling should ever change the underlying buffer text. This is
    /// the regression guard for that, exercised across several scroll
    /// positions on a real (viewport-scoped) `refresh` path.
    #[gpui::test]
    async fn scrolling_and_refreshing_never_mutates_the_buffer_text(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        let mut text = String::from("ˇ# Heading\n\nSome **bold** and *italic* text.\n\n");
        for i in 0..300 {
            text.push_str(&format!("- [ ] task number {i}\n"));
        }
        cx.set_state(&text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let original_text = cx.update_editor(|editor, _window, cx| editor.buffer().read(cx).snapshot(cx).text());

        for scroll_row in [0.0, 50.0, 150.0, 280.0, 10.0] {
            cx.update_editor(|editor, window, cx| {
                editor.set_scroll_position(gpui::Point::new(0.0, scroll_row), window, cx);
                refresh(editor, window, cx);
            });
            let current_text = cx.update_editor(|editor, _window, cx| editor.buffer().read(cx).snapshot(cx).text());
            assert_eq!(
                current_text, original_text,
                "buffer text changed after scrolling to row {scroll_row} and refreshing"
            );
        }
    }

    /// A real user report: copying a multi-line selection through
    /// glass_md-decorated content (folded markers, bullet glyphs) was
    /// losing line breaks. `Editor::copy` reads straight from the
    /// underlying buffer rope (`text_for_range`), which has no knowledge of
    /// folds at all, so this exercises the one place glass_md's own
    /// decorations *could* plausibly interfere: whether a fold-inducing
    /// range ever swallows a `\n` byte (see
    /// `plan::no_fold_inducing_range_contains_a_newline_byte` for the same
    /// invariant checked directly against the planner's output) and whether
    /// a selection spanning decorated, multi-row content still round-trips
    /// through copy with every line break intact.
    #[gpui::test]
    async fn copying_a_multiline_selection_through_decorated_content_preserves_line_breaks(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "# Heading one\n",
            "Some «bold** text\n",
            "- [ ] taˇ»sk item\n",
            "Trailing paragraph.\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });

        cx.update_editor(|_editor, window, cx| {
            window.dispatch_action(Box::new(editor::actions::Copy), cx);
        });

        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text().as_deref().map(str::to_string))
            .expect("copy should have written text to the clipboard");
        assert_eq!(
            copied.matches('\n').count(),
            1,
            "expected exactly one line break in the copied text (selection spans two source \
             lines), got {copied:?}"
        );
        assert_eq!(copied, "bold** text\n- [ ] ta");
    }

    /// Exercises "smart list continuation" (see `list_continuation`) through
    /// the real `Newline` action dispatch, not just the pure planner
    /// function directly -- confirming `intercept_newline` is actually wired
    /// up ahead of `Editor::newline` via `editor.register_action`.
    #[gpui::test]
    async fn newline_continues_a_bullet_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\n- ˇ\n");
    }

    #[gpui::test]
    async fn newline_continues_an_ordered_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("1. oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("1. one\n2. ˇ\n");
    }

    #[gpui::test]
    async fn newline_continues_a_task_item_unchecked(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- [x] oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- [x] one\n- [ ] ˇ\n");
    }

    #[gpui::test]
    async fn newline_on_empty_item_exits_the_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- one\n- ˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\nˇ\n");
    }

    #[gpui::test]
    async fn newline_on_empty_nested_item_outdents(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- one\n  - nested\n  - ˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\n  - nested\n- ˇ\n");
    }

    /// Outside a list -- and in a non-Markdown buffer -- a plain `Enter`
    /// keeps behaving exactly like `Editor::newline` on its own: confirms
    /// `intercept_newline` genuinely falls through (`cx.propagate()`) rather
    /// than swallowing the action whenever it doesn't apply.
    #[gpui::test]
    async fn newline_is_unaffected_outside_lists(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("just a paragraphˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("just a paragraph\nˇ\n");
    }

    #[gpui::test]
    async fn newline_is_unaffected_in_a_non_markdown_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- oneˇ\n");
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\nˇ\n");
    }
}
