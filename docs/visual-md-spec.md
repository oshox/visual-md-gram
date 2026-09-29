# Obsidian Live Preview — Behavior Spec

## Source note

Obsidian's app (including the editor) is closed-source. The `obsidianmd` GitHub
org only publishes `obsidian-api` (TypeScript typings for the plugin API),
sample plugins, and help docs — not the editor implementation itself.
Everything below is reconstructed from observable behavior, official docs, and
the plugin API surface (which exposes real internals: Live Preview is built on
CodeMirror 6, uses Lezer for markdown parsing, and extensions are CM6
`Extension`s / `ViewPlugin`s / `StateField`s registered via
`Plugin.registerEditorExtension`). Treat this as a behavioral spec to
reimplement against, not a source-code transcription.

## What Live Preview is

Live Preview is one of Obsidian's two **editable** modes (the other is Source
mode, where raw markdown is always shown; that mode is out of scope here). In
Live Preview, formatting is applied inline as you write — headings are
actually bigger, `**bold**` is actually bold — while the underlying document
remains plain, unmodified markdown text at all times. It's a hybrid of
Source mode and read-only Reading view: WYSIWYG-like rendering, but with
markdown syntax revealed on demand for editing.

## Core mechanic

This is the part worth getting right; everything else is bookkeeping.

**Principle:** every line has two representations — *raw* (markdown source,
shown when the caret or selection touches that line) and *rendered*
(formatting applied, markup characters hidden). The switch is **per-line**,
evaluated continuously as the cursor moves, not per-click-to-enter/exit an
editing session.

Rules:
1. A line is "raw" if the cursor is anywhere on it, or if any part of an
   active selection overlaps it. Otherwise it's "rendered."
2. Some constructs are "atomic" at a finer grain than the line — e.g. within a
   long paragraph containing several `**bold**` spans, only the span the
   cursor is actually inside reveals its `**` markers; sibling spans on the
   same line stay rendered. Table cells and callout titles behave as their own
   raw/rendered units for the same reason.
3. Toggling raw↔rendered must **not** reflow surrounding lines — no
   line-height jump, no horizontal shift of unrelated text. This is achieved
   by decorating (hiding/replacing) tokens inline rather than swapping the DOM
   structure of the whole line.
4. Rendering is **viewport-based**: only visible lines (+ a small overscan
   buffer) are parsed/decorated, so multi-MB / million-line files stay
   responsive. Edits trigger incremental re-parse (Lezer's incremental parser)
   and re-decoration scoped to the changed region, not the whole document.
5. The raw markdown text in the document model is always the single source of
   truth. Decorations are view-only (CodeMirror `Decoration`s over a fixed
   `EditorState`) — copy, save, and diff always operate on plain, unmodified
   markdown text.

## Per-element rendering rules

- **Headings** (`# ` … `###### `): marker hidden when not on the line;
  heading text scales up per level (H1 largest) using the same font family.
  Marker (with its trailing space) reappears, in muted/dim color, the instant
  the cursor lands on the line.
- **Bold / italic / bold-italic / strikethrough / highlight**
  (`**x**`, `*x*`/`_x_`, `***x***`, `~~x~~`, `==x==`): delimiter characters are
  hidden and the span gets the corresponding style; delimiters reappear
  (dimmed) only for the span containing the cursor.
- **Inline code** (`` `x` ``): backticks stay visible-but-dim even when not
  focused (unlike bold/italic, code spans keep a monospace background chip at
  all times); font switches to monospace inside.
- **Fenced code blocks** (```` ``` ````): the fence lines collapse to a thin
  top/bottom border + language label chip when cursor is outside the block;
  content area gets syntax highlighting for the declared language always
  (even unfocused). Cursor entering any line inside the block reveals the
  fence lines as plain text again only if the cursor is on the fence line
  itself.
- **Blockquotes** (`> `): left border bar replaces the `>` markers; nested
  quotes stack multiple bars. Marker text hidden unless the quote line has the
  cursor.
- **Callouts** (`> [!type]` … , a specialized blockquote): renders as a
  colored, rounded box with a left accent bar, an icon + title row derived
  from `type` (note/warning/tip/danger/etc., case-insensitive, extensible via
  CSS/theme), and body text indented below. `> [!type]+` / `> [!type]-`
  control default expanded/collapsed foldable state (a small triangle toggle
  appears). Body content is itself fully live-previewed (can contain lists,
  code, links, nested callouts). Clicking the title row toggles fold;
  right-clicking the type name offers a menu to change callout type.
- **Lists** (`- `, `* `, `1. `): bullet/number rendered as a typographic
  marker (bullet glyph or number) rather than the literal characters;
  nesting indents. Ordered lists renumber visually even if source numbers are
  all `1.`.
- **Task list items** (`- [ ] text`, `- [x] text`): checkbox marker becomes an
  actual interactive `<input type=checkbox>`-like widget. Clicking it toggles
  `[ ]` ⇄ `[x]` in the underlying source text and does **not** require
  entering raw mode first — checkbox widgets are always interactive, in both
  raw and rendered states. Custom marks in brackets (`[/]`, `[-]`, etc.,
  theme/plugin dependent) render with distinct icons.
- **Internal links / wikilinks** (`[[Note Name]]`, `[[Note Name|Alias]]`,
  `[[Note#Heading]]`, `[[Note#^blockid]]`): brackets and pipe hidden; only the
  alias (or note name if no alias) shown, styled as a link (color, no
  underline by default). Unresolved targets (file doesn't exist) get a
  distinct "unresolved" style (often dashed/lighter). `Ctrl/Cmd`+hover opens a
  floating **page preview** popover rendering the target note's content
  without navigating; `Ctrl/Cmd`+click navigates.
- **Embeds** (`![[Note Name]]`, `![[image.png]]`, `![[Note#Heading]]`):
  renders the target inline — images as `<img>`, notes as a transcluded,
  boxed rendering of that note's (or that heading/block's) content,
  audio/video/PDF as native players/frames. Embeds render as an actual box
  only when the cursor is off that line; the source line reveals when
  focused.
- **Markdown links** (`[text](url)`) and **autolinks** (`<https://...>`):
  brackets/parens hidden, `text` shown as a styled link; bare/auto URLs are
  clickable directly.
- **Tags** (`#tag`, `#nested/tag`): rendered as a filled pill/chip, clickable
  to open search for that tag.
- **Footnotes** (`text[^1]` … `[^1]: definition`): inline marker renders as a
  superscript reference; hovering shows the definition in a popover.
- **Tables** (GFM pipe tables): rendered as an actual `<table>` with borders,
  alignment per the `---:`/`:---:` header separator row; the cell under the
  cursor reveals its raw pipe-delimited text for editing while sibling cells
  stay rendered.
- **Horizontal rule** (`---`, `***`, `___`): renders as a full-width `<hr>`
  line.
- **Math** (`$inline$`, `$$block$$`): rendered via a LaTeX engine
  (MathJax/KaTeX equivalent) to typeset math; delimiters hidden until
  focused.
- **Comments** (`%%hidden text%%`): shown dimmed/with a distinct background
  only while the cursor is inside them; otherwise fully hidden.
- **YAML frontmatter** (`---` fenced block at file start): rendered as a
  distinct, visually separated "properties" panel (key/value rows with
  type-aware widgets — text, list, checkbox, date) rather than raw YAML.
- **HTML embedded in markdown**: rendered live, same as any static markdown
  renderer would.

## Editing interactions specific to Live Preview

- **Autocomplete popups**: typing `[[` opens a fuzzy-search dropdown of vault
  notes/headings/blocks (filtered as you type, arrow keys + Enter/Tab to
  accept, replacing back to the `[[`); typing `#` opens a similar dropdown of
  existing tags; typing `![[ ` similarly for embeddable files.
- **Folding**: headings and list items get a hover-revealed gutter chevron to
  fold their subtree; folded regions show an ellipsis placeholder and persist
  fold state per file.
- **Paste**: pasting a URL over a text selection wraps it as
  `[selection](url)`; pasting an image from clipboard saves it into the vault
  and inserts an embed (`![[pasted image.png]]`).
- **Drag & drop**: dragging a file from the file explorer (or OS) into the
  editor inserts a link or embed at the drop point depending on file type.
- **Selection formatting shortcuts**: `Ctrl/Cmd+B`/`I` wrap selection in
  `**`/`*`; toggling again on an already-formatted selection unwraps it, even
  if the cursor is just inside the markers without a selection.
- **Smart list continuation**: Enter on a list/checkbox line continues the
  same marker on the next line; Enter on an empty list item outdents it one
  level, or removes the marker entirely at the top level. (Bare blockquote
  lines with no list marker are not auto-continued -- only list/checkbox
  items are, whether or not they're inside a blockquote.)
- **Line-height / caret stability**: because decorations are inline and
  viewport-scoped (see Core mechanic), typing at the end of a long,
  heavily-formatted document does not cause visible re-layout jank of lines
  above the caret.
- **Vim mode**: optional global setting that swaps default keybindings for
  modal (normal/insert/visual) editing, implemented as a CM6 keymap extension
  layered on top of the same decoration system — the rendering rules above
  are unaffected.

## Implementation notes for a CodeMirror-6-based recreation

- Parse markdown with a Lezer (or equivalent incremental) grammar so you get a
  syntax tree usable both for decoration placement and for scoping re-parses
  to edited ranges only.
- Model each renderable construct (heading, emphasis span, link, checkbox,
  callout, code fence, table cell, math span, frontmatter block, comment) as
  its own `ViewPlugin`/decoration builder that:
  1. walks the syntax tree per visible line,
  2. checks whether the current selection/cursor intersects that node's
     range,
  3. emits either a hiding `Decoration.replace` (markers) + `mark`/`widget`
     decoration (rendered form), or nothing (leave raw) if the cursor
     intersects.
- Widgets that must be interactive in *both* raw and rendered states (task
  checkboxes) should be implemented as `WidgetType`s with real DOM event
  handlers that write back to the document via transactions — never as
  read-only decoration.
- Keep an explicit reserved/atomic-range registration for widget decorations
  so cursor movement (arrow keys) skips over a rendered widget in one step
  instead of stepping through hidden characters underneath it.
- Avoid any decoration strategy that inserts/removes DOM nodes of differing
  height between raw/rendered states for the *same* line — that's the
  documented source of Obsidian's own "lines jump while typing" complaints,
  so it's worth explicitly testing for and avoiding regressions here.
