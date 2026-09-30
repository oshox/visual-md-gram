# Zed MD markdown editor milestones

The live-preview markdown editor lives in the `visual_md` crate and is
configured under the `visual_md` settings key. The names stay `visual_md` on
purpose even though the app is branded Zed MD.

## Status

| Milestone | Feature | State |
| --- | --- | --- |
| M1 | Inline formatting (headings, bold, italic, strike, highlight, code) | Done |
| M2 | Lists, tasks, blockquotes | Done |
| M3 | Viewport-only decoration, copy/paste, on/off setting | Done |
| M4 | Taller heading rows | Done |
| M5 | Smart list continuation | Done |
| M6 | Links, autolinks, horizontal rules | Done |
| M7 | Proportional prose font, monospace code | Done |
| M8 | Fenced code blocks with syntax highlighting | Done |
| M9 | GFM tables | Done |
| M10 | Bold and italic shortcuts | Done |
| M11 | Callout boxes | Done |
| M12 | Inline images | Draft PR #2 |
| M13 | Turn the editor off globally or per project, in settings.json and the settings UI | Planned |
| M14 | Full font and color customization, in settings.json, the settings UI and themes | Planned |
| M15 | Extension hooks on par with Obsidian, Notion and Logseq | Planned |

## M13: On/off switch at global and project level

**Today:** `"visual_md": { "enabled": true }` exists, but only in the user
settings file. `visual_md` is not part of `ProjectSettingsContent`, the crate
reads it with `VisualMdSettings::try_get(cx)` (no file location), and the
settings UI has no entry for it.

**Deliverables**
1. `.zed/settings.json` in a project can set `visual_md.enabled`, and it wins
   over the user setting for files in that worktree. This means moving the
   `visual_md` field into the part of the settings content that project files
   merge, and reading it with the buffer's `SettingsLocation` everywhere the
   crate checks `enabled` (decoration refresh, the Enter handler, the
   bold/italic shortcuts, the `visual_md` key context).
2. Per-language override as a bonus of the same change: `enabled` can also be
   set per language (for example off for `MDX` but on for `Markdown`).
3. A "Markdown Live Preview" section in the settings UI with an "Enabled"
   toggle. The settings UI already switches between User and project scopes,
   so the same toggle covers both levels.
4. A command palette action, `visual_md: toggle live preview`, that flips the
   setting for the current buffer only, without writing any file. Useful for
   a quick look at the raw source.
5. Flipping any of these while a file is open re-renders it immediately with
   no leftover folds or highlights (the current off path already clears them;
   this adds tests for the project and per-buffer paths).

**Done when:** a project setting of `false` turns the editor off only in that
project, the settings UI toggle writes the right file for the chosen scope,
and tests cover user, project, language and per-buffer precedence.

## M14: Full font and color customization

**Today:** every font and color is hard-wired. Prose uses the UI font, code
uses the buffer font, heading sizes are fixed multipliers, links use the
theme's `link_text_hover`, and callouts reuse the theme's status colors. The
M11 and M1 code comments record an earlier choice to add no extra tints to
inline code, highlights or callout bodies. M14 keeps those defaults but lets
the user change them.

**Deliverables**
1. **Fonts in settings.json:** new keys under `visual_md`:
   `prose_font_family`, `prose_font_size`, `prose_font_weight`,
   `prose_line_height`, `code_font_family`, `code_font_size`, and
   `heading_font_family`. Leaving a key unset keeps today's behavior
   (UI font for prose, buffer font for code).
2. **Heading scale:** `heading_sizes` (six multipliers for H1 to H6) and
   per-level weight.
3. **Colors in settings.json:** a `visual_md.colors` object with a key per
   element: `heading` (and `heading.1` to `heading.6`), `bold`, `italic`,
   `strikethrough`, `highlight.background`, `inline_code` and
   `inline_code.background`, `link`, `marker` (the dimmed syntax shown on the
   cursor line), `blockquote.bar`, `rule`, `table.border`, `code_block.border`,
   `code_block.background`, `task.checked`, and `callout.<type>` with
   `accent` and `background` for note, tip, warning, danger and custom types.
4. **Colors in themes:** the same keys are read from a theme's `syntax` map
   as `visual_md.*` tokens, so a theme extension or `theme_overrides` can
   style the editor. Order of precedence: `visual_md.colors` setting, then
   the active theme's `visual_md.*` token, then today's default.
5. **Custom callout types:** `visual_md.callouts` maps a type name (for
   example `"quote"` or `"todo"`) to an icon, accent and background, so
   `> [!todo]` gets its own look instead of the neutral fallback.
6. **Project scope:** all of the above live in the same settings struct as
   M13, so a project's `.zed/settings.json` can override them.
7. **Settings UI:** the "Markdown Live Preview" section gains font pickers,
   size fields, heading scale fields and a color field for each element,
   each with a reset-to-default button.

**Done when:** each key changes the running editor without a restart,
unset keys match today's rendering exactly, and a theme can restyle the
editor with no settings change.

## M15: Extension hooks

**Today:** Zed extensions are WebAssembly components built on
`zed_extension_api` (WIT under `crates/extension_api/wit`). They can add
languages, language servers, themes, slash commands, context servers and
debug adapters. They cannot touch how the markdown editor renders or
behaves.

**What the reference apps offer, and the equivalent here**

| Obsidian / Notion / Logseq | Zed MD equivalent |
| --- | --- |
| Markdown post-processors, custom syntax (Obsidian), macros (Logseq) | Inline and block **syntax rules**: an extension declares a pattern (regex or tree-sitter node kind) and returns styled spans, hidden ranges and replacement text |
| Code block processors (mermaid, dataview, charts) | **Fenced block renderers**: an extension claims a language tag and returns rendered output (styled text, markdown, SVG or an image) shown in place of the block when the cursor is away |
| Custom callout types and icons | **Callout registration** that plugs into M14's callout table |
| Commands, slash menus (Notion `/`), hotkeys | **Editor commands**: an extension registers a named command that receives the buffer text and selections and returns edits; commands appear in the palette, can be bound in keymaps, and can appear in a `/` insert menu |
| Events (file open, save, change), metadata cache | **Document events** (`opened`, `saved`, `changed` with debounce) and a parsed **document outline** (headings, links, tags, tasks, frontmatter) passed to the extension |
| Link resolvers, wikilink completions | **Link providers**: resolve a custom link scheme or wikilink target and supply completions |
| Settings tabs | Extension settings under `visual_md.extensions.<id>` shown in the settings UI |
| Themes and CSS snippets | Covered by M14's `visual_md.*` theme tokens, which existing theme extensions can already ship |

**Deliverables**
1. A new `visual-md.wit` interface in a new `since_v0.9.0` extension API
   version, with the hooks above. Extensions opt in through
   `extension.toml` (for example `[visual_md] syntax_rules = [...]`,
   `fence_renderers = ["mermaid"]`).
2. Host side in the `visual_md` crate: rule results merge into the existing
   render plan, results are cached per buffer version, and a slow or failing
   extension is timed out and logged instead of blocking typing.
3. Sandboxing stays as Zed's today: extensions only get the capabilities
   they declare (for example network for a renderer that calls a web
   service).
4. A sample extension in `extensions/visual-md-sample` that adds a custom
   syntax rule, a `mermaid`-style fence renderer, a custom callout and an
   editor command, used by integration tests.
5. A developer guide in `docs/` for writing a Zed MD extension.

**Not in M15:** arbitrary custom panes or database views like Notion
databases or Obsidian's Dataview tables beyond what a fence renderer can
draw. Zed's extension model has no general UI API, and building one is a
separate, much larger project.

**Suggested order inside M15:** fence renderers and editor commands first
(highest value, smallest surface), then syntax rules, then events, outline
and link providers.

**Done when:** the sample extension installs as a dev extension and each of
its hooks works in the running app, and removing it restores the default
rendering.
