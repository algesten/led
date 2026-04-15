# Keybindings

Source: `crates/config-file/src/default_keys.toml` (canonical default keymap).
Dispatch + context detection: `crates/core/src/keys.rs` (KeyCombo, Keymap),
`led/src/model/actions_of.rs` (context resolution), plus per-overlay handlers
in `led/src/model/find_file.rs`, `led/src/model/file_search.rs`,
`led/src/model/isearch_of.rs`, `led/src/model/action/browser.rs`,
`led/src/model/action/lsp.rs`.

Notes on display: TOML keys are lowercase (`ctrl+`, `alt+`, `shift+`); SHIFT on
a `Char` is stripped because the character itself already carries case
(`keys.rs:27-30`).

## Main mode (top-level, no modal/overlay active)

Bindings from `[keys]` in `default_keys.toml`. Applies when no
context-specific table matches (see "Context resolution" in Findings).

| Key | Action | Notes |
|---|---|---|
| Ctrl-a | line_start | |
| Ctrl-e | line_end | |
| Ctrl-d | delete_forward | |
| Ctrl-k | kill_line | |
| Up | move_up | |
| Down | move_down | |
| Left | move_left | |
| Right | move_right | |
| Home | line_start | |
| End | line_end | |
| PageUp | page_up | |
| PageDown | page_down | |
| Ctrl-Home | file_start | |
| Ctrl-End | file_end | |
| Enter | insert_newline | |
| Backspace | delete_backward | |
| Delete | delete_forward | |
| Tab | insert_tab | |
| Ctrl-f | open_file_search | opens file-search sidebar; uses selection as initial query if any |
| Ctrl-v | page_down | |
| Alt-v | page_up | |
| Alt-Tab | toggle_focus | switches between Main and Side panels |
| Ctrl-b | toggle_side_panel | |
| Ctrl-Left | prev_tab | |
| Ctrl-Right | next_tab | |
| Ctrl-/ | undo | |
| Ctrl-_ | undo | alias |
| Ctrl-7 | undo | alias (terminal emits Ctrl-7 for Ctrl-/ on some setups) |
| Ctrl-g | abort | |
| Esc | abort | |
| Ctrl-z | suspend | |
| Alt-< | file_start | |
| Alt-> | file_end | |
| Ctrl-Space | set_mark | |
| Ctrl-w | kill_region | |
| Ctrl-y | yank | |
| Ctrl-s | in_buffer_search | starts isearch overlay; while active, subsequent Ctrl-s → search_next |
| Alt-. | next_issue | next LSP diagnostic |
| Alt-, | prev_issue | |
| Alt-Enter | lsp_goto_definition | |
| Ctrl-r | lsp_rename | opens rename overlay (PanelSlot::Overlay) |
| Alt-i | lsp_code_action | opens code-action picker overlay |
| Ctrl-t | lsp_toggle_inlay_hints | |
| Alt-b | jump_back | |
| Alt-Left | jump_back | |
| Alt-f | jump_forward | |
| Alt-Right | jump_forward | |
| Alt-o | outline | action dispatched but not wired (see Findings) |
| Alt-] | match_bracket | |
| Ctrl-q | reflow_paragraph | |
| Ctrl-x | (chord prefix) | see below |
| Ctrl-h | (chord prefix) | see below |

Any unbound printable key with no Ctrl/Alt emits `InsertChar(c)` when
`allow_char_insert` holds (Main focus or any input-dialog active —
`actions_of.rs:180-185`).

## Chord-prefix: Ctrl-x

Bindings under `[keys."ctrl+x"]`. After Ctrl-x is pressed, the next chord is
looked up here. Digits `0-9` are accumulated as a count prefix
(`actions_of.rs:83-90`); any bound chord consumes the count (see
`kbd_macro_execute` special case).

| Key | Action | Notes |
|---|---|---|
| Ctrl-c | quit | |
| Ctrl-s | save | |
| Ctrl-a | save_all | |
| Ctrl-d | save_no_format | |
| Ctrl-w | save_as | opens find-file overlay in SaveAs mode |
| k | kill_buffer | |
| Ctrl-f | find_file | opens find-file overlay |
| i | sort_imports | |
| ( | kbd_macro_start | |
| ) | kbd_macro_end | |
| e | kbd_macro_execute | toggles "macro repeat" mode — bare `e` replays macro until another key resets it (`actions_of.rs:67-74, 96-106`) |
| Ctrl-p | open_pr_url | |

## Chord-prefix: Ctrl-h

| Key | Action | Notes |
|---|---|---|
| e | open_messages | `Action::OpenMessages` is parsed but never handled anywhere (see Findings) |

## Context: file browser sidebar

Active when `state.focus == PanelSlot::Side` (keymap context `"browser"`,
`actions_of.rs:151-153`). These overrides take precedence over `[keys]` for
the listed chords; all other main bindings still fall through because
`Keymap::lookup` checks context then global (`keys.rs:99-116`).

Bindings from `[browser]`:

| Key | Action | Notes |
|---|---|---|
| Left | collapse_dir | |
| Right | expand_dir | |
| Enter | open_selected | opens file or expands/collapses dir |
| Alt-Enter | open_selected_bg | `Action::OpenSelectedBg` is parsed but never handled (see Findings) |
| Ctrl-q | collapse_all | overrides `reflow_paragraph` |

Editor-focus actions (`InsertChar`, `InsertNewline`, `InsertTab`,
`DeleteBackward/Forward`, `KillLine/Region`, `Yank`, `Undo/Redo`,
`SortImports`) are filtered out while focus is Side
(`actions_of.rs:159-174`), so most typing is dropped. Movement actions
(`MoveUp/Down/Left/Right`, `PageUp/Down`, `FileStart/End`) are re-routed to
browser navigation by `action/mod.rs` guards (see `action/browser.rs`).

## Context: file-search sidebar (file_search overlay)

Active when `state.file_search.is_some()`. Context name `"file_search"`
(`actions_of.rs:148-150`). Context table from `[file_search]` overrides
global bindings, then all further actions flow to
`file_search::handle_file_search_action` which absorbs the full action set.

Context-table bindings (`[file_search]`):

| Key | Action | Notes |
|---|---|---|
| Alt-1 | toggle_search_case | |
| Alt-2 | toggle_search_regex | |
| Alt-3 | toggle_search_replace | toggles replace-input row |
| Enter | open_selected | input row: advance; result row: open / bulk-replace |
| Alt-Enter | replace_all | only effective when replace_mode is on |

Actions consumed by `handle_file_search_action` (behaviour depends on
whether selection is input-row or result-row — `file_search.rs:177-460`):

| Action (came from key) | Behaviour |
|---|---|
| InsertChar(c) on input | append to query/replacement |
| DeleteBackward on input | backspace |
| DeleteForward on input | forward delete |
| KillLine on input | truncate input at cursor |
| MoveLeft / MoveRight on input | cursor in input |
| LineStart / LineEnd on input | input cursor |
| MoveLeft / MoveRight on result | unreplace / replace selected hit (replace_mode only) |
| MoveUp / MoveDown | unified vertical nav across inputs and hits |
| PageUp / PageDown on result | page through hits |
| FileStart / FileEnd on result | first/last hit |
| InsertTab on input | cycle between Search/Replace inputs (replace_mode) |
| InsertTab on result | absorbed (no-op) |
| OpenSelected / InsertNewline on input | advance to next input or first hit |
| OpenSelected / InsertNewline on result | open hit (or close after bulk replace) |
| Abort / CloseFileSearch | close overlay |
| Resize / Quit / Suspend | pass through |
| any other on result | absorbed |
| any other on input | deactivate + pass through |

## Context: find-file / save-as overlay

Active when `state.find_file.is_some()` (opened by `find_file` action from
Ctrl-x Ctrl-f, or `save_as` from Ctrl-x Ctrl-w). No TOML context table; all
handling is in `find_file::handle_find_file_action` (`find_file.rs:330-456`).
Actions reach it because `mod.rs:226-232` routes every non-pass-through
action to `Mut::FindFileAction` when `find_file` is active.

| Action | Behaviour |
|---|---|
| InsertChar(c) | insert in input, request completions |
| DeleteBackward | backspace |
| DeleteForward | forward delete |
| InsertTab | tab-complete: expand single match, extend to LCP, or show side |
| InsertNewline | commit: descend dir / open / save-as / create file |
| MoveUp | wrap-up through completion list |
| MoveDown | wrap-down through completion list |
| MoveLeft | cursor left (char boundary) |
| MoveRight | cursor right (char boundary) |
| LineStart | cursor to 0 |
| LineEnd | cursor to end |
| KillLine | truncate input at cursor |
| Abort | close overlay |
| Resize / Quit / Suspend | pass through |
| any other | deactivate + pass through |

Chords from `Ctrl-a / Ctrl-e / Ctrl-k / ...` therefore still work because
they resolve to `LineStart / LineEnd / KillLine` actions via the global
keymap, and find-file consumes those actions.

## Context: isearch (in-buffer search)

Active when `active_buffer.isearch.is_some()` — set by `in_buffer_search`
(Ctrl-s) when not already searching. No TOML context table; dispatch is in
`isearch_of::isearch_of` (`isearch_of.rs:53-167`).

Consumed actions (`isearch_consumes`, `isearch_of.rs:25-31`):

| Action | Behaviour |
|---|---|
| InsertChar(c) | append to query, update search |
| DeleteBackward | pop from query; empty query resets cursor to origin |
| Abort | cancel search (restore cursor to origin) |
| InsertNewline | accept: keep cursor at match, record jump if moved |

Special case: `InBufferSearch` (Ctrl-s again) is NOT consumed by
`isearch_of`; it flows to `handle_action` which calls `search_next`
(`isearch_of.rs:98-100, 148-154`).

All other actions (except `Resize`, `Quit`, `Suspend`, `InBufferSearch`)
while isearch is active emit `Mut::SearchAccept` and then ALSO run their
normal handler — e.g. pressing `Up` accepts the match and moves up.

## Context: LSP rename overlay

Active when `state.lsp.rename.is_some() && state.focus == PanelSlot::Overlay`.
Opened by `lsp_rename` (Ctrl-r). No TOML context table; dispatch in
`action/lsp.rs:169-219`. Absorbs every action (`mod.rs:199-204`).

| Action | Behaviour |
|---|---|
| InsertChar(c) | insert at cursor |
| DeleteBackward | backspace |
| InsertNewline | submit rename to LSP, close overlay |
| Abort | close overlay, no rename |
| any other | absorbed (no-op) |

Note: no `DeleteForward`, `MoveLeft/Right`, `LineStart/End` handling — those
keys are swallowed silently.

## Context: LSP code-action picker overlay

Active when `state.lsp.code_actions.is_some()`. Opened by `lsp_code_action`
(Alt-i). No TOML context table; dispatch in `action/lsp.rs:128-167`.
Absorbs every action (`mod.rs:192-197`).

| Action | Behaviour |
|---|---|
| MoveUp | previous action |
| MoveDown | next action |
| InsertNewline | accept selected, send request, close |
| Abort | close, no request |
| any other | absorbed (no-op) |

## Context: LSP completion popup

Active when `state.lsp.completion.is_some()`. Not opened by a key directly —
triggered after `InsertChar` when the active buffer has completion triggers
(`action/mod.rs:151-174`). Dispatch in `action/lsp.rs:7-126`. Most actions
are absorbed, except `InsertChar` and `DeleteBackward` pass through to the
editor (which re-filters completion).

| Action | Behaviour |
|---|---|
| MoveUp | previous item |
| MoveDown | next item |
| InsertNewline / InsertTab | accept selection, apply edit + additional edits, request resolve |
| Abort | dismiss popup |
| InsertChar(c) | pass through to editing — popup re-evaluates |
| DeleteBackward | pass through to editing — popup re-evaluates |
| any other | dismiss popup + pass through |

## Context: confirm-kill prompt

Active when `state.confirm_kill == true` (triggered when closing a dirty
buffer via Ctrl-x k). No TOML table; handled in `mod.rs:270-282`.

| Action | Behaviour |
|---|---|
| InsertChar('y') / InsertChar('Y') | `Mut::ForceKillBuffer` — kill without saving |
| any other migrated action | `Mut::DismissConfirmKill` |

Not a true "overlay" — normal main-mode bindings all still fire, but the
prompt is dismissed on first keystroke other than y/Y.

## Context: kbd-macro repeat mode

Transient state tracked by a `Cell<bool>` in `actions_of.rs:65-74`. Set
after `Ctrl-x e` (kbd_macro_execute). While active, a bare `e` (no Ctrl,
no Alt) replays the macro. Any other key clears the flag and falls through
to normal processing.

| Key | Action |
|---|---|
| e (no modifiers) | kbd_macro_execute (repeat) |
| any other | clear repeat mode, process normally |

## Chord-count accumulation

While a chord prefix is pending (Ctrl-x consumed, second key not yet
received), bare digits `0-9` accumulate as a repeat count
(`actions_of.rs:83-90`). The count is currently only consumed by
`kbd_macro_execute` (emitted as `Mut::KbdMacroSetCount(n)` before the
action). For any other chord target, the count is discarded
(`actions_of.rs:92-117`).

## Findings

### Actions parsed but never bound to a handler
- `Action::OpenMessages` (Ctrl-h e → `open_messages`). Defined in
  `crates/core/src/lib.rs:298`; no match arm in any `*_of.rs` or action
  handler. Pressing Ctrl-h e produces a `Mut::Action(OpenMessages)` that
  falls off the end of `handle_action`.
- `Action::OpenSelectedBg` (Alt-Enter in browser → `open_selected_bg`).
  Defined in `crates/core/src/lib.rs:297`; no handler. Pressing
  Alt-Enter in the sidebar does nothing.
- `Action::Outline` (Alt-o → `outline`). Defined in
  `crates/core/src/lib.rs:257`; searching for `Action::Outline` in
  `led/src/model` returns zero handler references. No-op.

### Actions defined in the enum but with no default binding
(Not a bug — these may be intended for test harness / headless use.)
- `SaveForce`, `MatchBracket` (bound as `Alt-]` — OK),
  `LspFormat` (no default binding; only dispatched by ui_actions_of),
  `ToggleSearchCase/Regex/Replace`, `ReplaceAll`, `CloseFileSearch`,
  `OpenSelected`, `ExpandDir`, `CollapseDir`, `CollapseAll`,
  `KbdMacroStart/End/Execute` (bound through chord-prefix),
  `Wait(u64)`, `Resize(u16,u16)` (test-only).

### Undo aliases
`Ctrl-/`, `Ctrl-_` and `Ctrl-7` all bind to `undo`. These are terminal
encoding aliases; keep all three in the generator so goldens cover real
terminals (xterm sends Ctrl-7 for Ctrl-/).

### Context-resolution fragility (`actions_of.rs:147-157`)
Context detection is a linear `if/else`:
1. `state.file_search.is_some()` → `"file_search"`
2. `focus == PanelSlot::Side` → `"browser"`
3. everything else → no context

Implications:
- `find_file`, `lsp_rename`, `lsp_code_actions`, `lsp_completion`,
  `isearch`, and `confirm_kill` are NOT keymap contexts — they're
  post-lookup action interceptors in `mod.rs`. A golden generator that
  only consults TOML will miss all of them.
- If `file_search` is active AND `focus == Side`, `file_search` wins.
- `PanelSlot::Overlay` returns no context, so overlays inherit the main
  keymap; overlay-specific filtering happens later in `mod.rs` by gating
  on `state.lsp.rename.is_some() && state.focus == PanelSlot::Overlay`.

### Chord-key state lives in `Cell`s (`actions_of.rs:30-32`)
`chord`, `chord_count`, `macro_repeat` are per-stream cells, not state
fields. Two consecutive `Ctrl-x` chords produce different observable
behaviour depending on history. PTY goldens must always send the full
chord in one go and reset between scenarios.

### Duplicate global bindings (intentional aliases, not conflicts)
- Ctrl-/ / Ctrl-_ / Ctrl-7 → undo
- Alt-b / Alt-Left → jump_back
- Alt-f / Alt-Right → jump_forward
- Alt-< / Ctrl-Home → file_start; Alt-> / Ctrl-End → file_end
- Home / Ctrl-a → line_start; End / Ctrl-e → line_end
- PageUp / Alt-v → page_up; PageDown / Ctrl-v → page_down
- Delete / Ctrl-d → delete_forward
- Esc / Ctrl-g → abort

No true duplicates within a single context were found (same chord → two
different actions in the same context).

### Context collisions between main and `[browser]`
Expected and listed above:
- Left / Right: main = move_left / move_right; browser = collapse_dir / expand_dir
- Enter: main = insert_newline; browser = open_selected
- Alt-Enter: main = lsp_goto_definition; browser = open_selected_bg
- Ctrl-q: main = reflow_paragraph; browser = collapse_all

### Context collisions between main and `[file_search]`
- Enter: main = insert_newline; file_search = open_selected
- Alt-Enter: main = lsp_goto_definition; file_search = replace_all
- Alt-1 / Alt-2 / Alt-3: unbound in main; bound in file_search

### `requires_editor_focus` vs `has_input_dialog`
`actions_of.rs:159-178` blocks a set of editing actions when focus is not
Main unless `file_search` or `find_file` is open. This means (for
example) `InsertChar` typed into the `lsp_rename` overlay is NOT
consumed from the action stream — the action is rejected at the keymap
layer because focus is Overlay. But `lsp_rename` uses
`Action::InsertChar` in its handler… This is a real hole: keystrokes into
the rename overlay only reach the handler because `find_file` / `file_search`
can share the Overlay focus in practice? Re-reading:

Actually `actions_of.rs:111-113` and `136-138` do allow the action when
`has_input_dialog(state) == true`, and `has_input_dialog` checks
`file_search.is_some() || find_file.is_some()`. It does NOT check
`lsp.rename` or `lsp.completion` or `lsp.code_actions`. So rename
overlay typing goes via `PanelSlot::Overlay`… but wait, rename calls
`SetFocus(PanelSlot::Overlay)` (find_file_of.rs:130-138). That combination
— Overlay focus AND `has_input_dialog == false` AND
`requires_editor_focus(InsertChar)` — means rename overlay should NOT
receive `InsertChar`. This looks like a live bug worth flagging to the
golden generator (scenario: Ctrl-r, type a letter → expect the letter in
the rename input; actual behaviour may be silent drop).

### Key codes not representable in TOML
`keys.rs:275` — `format_key_combo` returns `None` for F-keys, Insert, and
anything else not in its match. The parser only recognises the keys in
`keys.rs:219-235`. F-keys, Insert, Num-pad keys are unbindable today.
