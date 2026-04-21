# theming (M14b)

## Summary

Chrome theming in led rewrite: every visible region (tabs, status bar,
side panel, ruler, file-search toggles) resolves its fg/bg/attrs from a
[`Theme`] value rather than being hard-coded in the painter. The
`Theme` is loaded at startup from `theme.toml` — optional, falls back
to a built-in default that ships a coherent colored look.

Default palette mirrors led's long-standing look: peach accents
(`#ffaf87`) for active chrome, deep blue (`#005faf`) for muted gutter
/ border / status bar, pale yellow (`#ffd7af`) status-bar foreground,
dark grey (`#444444`) for inactive / unfocused highlights.

M15 (syntax highlighting) extends the same `theme.toml` with a
`[syntax]` section and a parallel `syntax_*` style vocabulary; M14b
covers chrome only.

## Resolution

Same discipline as `keys.toml`:

1. `--theme <PATH>` CLI flag wins when present.
2. Otherwise: `<config-dir>/theme.toml` (where `config-dir` comes from
   `--config-dir`).
3. Otherwise: `$XDG_CONFIG_HOME/led/theme.toml`, or
   `~/.config/led/theme.toml` when the env var is unset.
4. Otherwise: the built-in default (`Theme::legacy_default`).

Missing file at an explicit path is a hard error. Missing file at an
auto-discovered location is silently OK.

Per-region schema problems (unknown region, unknown color, wrong
value type) emit warnings on stderr and skip that field. I/O errors
and top-level TOML parse errors are fatal (exit 2).

## File shape

```toml
[chrome.tab_active]
reverse = true

[chrome.status_warn]
fg = "white"
bg = "#cd0000"
bold = true

[chrome.browser_selected_focused]
reverse = true

[chrome.browser_selected_unfocused]
bg = "dark_grey"

[chrome.search_toggle_on]
reverse = true

[chrome.ruler]
bg = "#222222"

[chrome]
ruler_column = 110
```

One table per region; each accepts any subset of
`fg` / `bg` / `bold` / `reverse` / `underline`. Missing region →
default (unstyled) Style → painter emits zero SGR for that region
(the terminal's native fg/bg shows through).

`ruler_column` is a flat integer under `[chrome]`, not a table — it
picks the editor-relative column where the ruler paints.

## Colors

Two accepted forms:

- **Named** — case-insensitive. Supported names:
  `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
  `white`, `grey` (alias `gray`), `dark_grey`
  (aliases `dark_gray`, `darkgrey`, `darkgray`).
  These resolve to fixed RGB values chosen to match crossterm's
  legacy 4-bit palette one-to-one.
- **Hex** — `"#rrggbb"`, exactly 6 hex digits, lowercase or uppercase.
  3-digit shorthand is not accepted (`#abc` is rejected with a
  warning).

Every accepted color produces a 24-bit RGB escape
(`\x1b[38;2;r;g;bm` or `\x1b[48;2;r;g;bm`). There is no fall-back
to 4-bit or 8-bit palettes.

## Attributes

Three boolean flags, all additive with fg/bg:

| Flag        | Semantics                          |
| ----------- | ---------------------------------- |
| `bold`      | `SGR 1`                            |
| `reverse`   | `SGR 7` (swaps fg/bg)              |
| `underline` | `SGR 4`                            |

Omitted flags default to `false`.

## Regions

### Tabs

| Region             | Default                     | Applies to                                       |
| ------------------ | --------------------------- | ------------------------------------------------ |
| `tab_active`       | `fg=#080808 bg=#ffaf87`     | The active tab's ` label ` in the tab bar.       |
| `tab_inactive`     | `fg=#ffaf87 bg=#444444`     | Every other tab's ` label `.                     |
| `tab_preview`      | unstyled                    | *Reserved*; painter not yet per-tab-preview-aware. |
| `tab_dirty_marker` | unstyled                    | *Reserved*; dirty dot currently part of the label. |

### Status bar

| Region          | Default                    | Applies to                                         |
| --------------- | -------------------------- | -------------------------------------------------- |
| `status_normal` | `fg=#ffd7af bg=#005faf`    | Non-warn status bar (saved, L1:C1, etc.).          |
| `status_warn`   | `fg=white bg=red bold`     | `StatusBarModel.is_warn` rows (errors, failures). |

### Side panel

| Region                      | Default                  | Applies to                                                                 |
| --------------------------- | ------------------------ | -------------------------------------------------------------------------- |
| `browser_selected_focused`  | `fg=#080808 bg=#ffaf87`  | Selected row while `SidePanelModel.focused == true`.                       |
| `browser_selected_unfocused`| `bg=#444444`             | Selected row while `focused == false` (focus lives in the editor pane). |
| `browser_chevron`           | unstyled                 | *Reserved*; chevron `▷ ▽` currently inline with row text.                  |
| `browser_border`            | `fg=#005faf`             | Vertical `│` between side panel and editor.                                |

### File-search overlay

| Region             | Default                  | Applies to                                                                                   |
| ------------------ | ------------------------ | -------------------------------------------------------------------------------------------- |
| `search_toggle_on` | `fg=#080808 bg=#ffaf87`  | Each of `Aa` / `.*` / `=>` in the header row when the corresponding flag (case / regex / replace) is on. |

### Editor body

| Region        | Default       | Applies to                                                                                            |
| ------------- | ------------- | ----------------------------------------------------------------------------------------------------- |
| `cursor_line` | unstyled      | *Reserved*; currently the terminal's native cursor-row style takes over.                              |
| `ruler`       | `bg=#303030`  | Single column painted over every body row when `ruler_column` is set. The underlying char is preserved when it overlaps. |

`ruler_column` defaults to `None` — the ruler stays off until the
user opts in. Picking a number automatically would surprise users
whose editor width varies with sidebar state.

## Painter contract

- Styles only emit SGR when non-default. A region whose Style is
  `Default::default()` produces zero bytes of ANSI, matching
  unthemed behavior.
- Colors emit as 24-bit RGB. Attributes emit one SGR each.
- Every applied style is followed by `SGR 0` + `ResetColor` before
  the next unstyled print. The painter never leaks attributes
  across regions.
- Dirty-diff caching (body / tab-bar / status-bar / side-panel
  separately) is unchanged by theming — regions are repainted on
  change, same as before.

## Non-goals for M14b

- **Syntax token classes** (keyword, string, comment, ...). These
  extend `theme.toml` with a `[syntax]` section in M15.
- **Per-language chrome.** One theme for the whole workspace.
- **Hot-reload.** Same limitation as `keys.toml` — changes take
  effect on next launch.
- **Cascading.** A region is either set or it's default; there's
  no inheritance from a parent region or from named presets.
- **Nerd-font glyphs.** The existing `▷ ▽ │` stay; theming governs
  color / attrs only.

## Testing

- Unit tests in `crates/driver-terminal/native/tests::ruler_*` and
  `crates/runtime/src/theme.rs::tests::*` cover the parser + painter.
- Golden scenarios under `goldens/scenarios/features/theming/*`
  exercise the end-to-end path: a scenario passes `--theme` to led
  with a scratch theme file and asserts the `dispatched.snap` +
  `frame.snap` that fall out.
- vt100 strips SGR before the frame snapshot, so color changes
  don't show in `frame.snap`. Theme scenarios therefore focus on
  *structural* differences (e.g. ruler column characters kept vs
  overwritten, toggle header layout, mode transitions) and rely on
  unit tests to guard the actual SGR output.
