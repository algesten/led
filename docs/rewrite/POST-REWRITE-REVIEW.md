# Post-rewrite review list

Findings surfaced during Phase 0/1 work that aren't worth fixing on `main` (it's the FRP code being replaced) but should be reviewed for the rewrite — either fixed in the new arch, or consciously preserved with documentation.

Sourced from `docs/extract/{actions,keybindings,driver-events,config-keys}.md`. See those for full context.

---

## Dead code (current-led)

These exist in the codebase but have no working triggers:

- `Action::Outline` — bound to `alt+o` in `default_keys.toml`, no handler.
- `Action::OpenMessages` — bound to `ctrl+h e` in `default_keys.toml`, no handler.
- `Action::OpenSelectedBg` — bound to `alt+enter` in `[browser]`, no handler.
- `Action::SaveForce` — defined and listed in `is_migrated`, but unbound and no handler.
- `TerminalInput::FocusGained` / `FocusLost` — emitted by terminal-in driver, no consumer anywhere.
- `DocStoreIn::Opening` — silently dropped on receipt; appears to be a vestigial intermediate signal.

**Action for rewrite**: don't port these; verify nothing breaks; remove from any new `Action`/`Event` enum.

---

## Bugs surfaced during Phase 2 narrative authoring

New findings from agents drafting `docs/spec/*.md` and `docs/drivers/*.md`. See those docs for full context; summaries here.

- **`pending_save_after_format` can stall indefinitely** (`spec/lsp.md`, `spec/editing.md`). If LSP `format` request never returns, the save never fires. "Formatting…" alert is stuck; user must `Ctrl-d` (save-no-format) or reopen the file. No visible timeout or fallback in `manager.rs`.
- **`DocStoreIn::ExternalRemove` silently dropped** (already flagged; `spec/buffers.md`, `spec/persistence.md` confirm the consequence — session holds paths to files that no longer exist on disk).
- **LSP server crash has no restart path** (`spec/lsp.md`, `drivers/lsp.md`). Single `Arc<LanguageServer>` per language in `manager.rs`'s `self.servers`; once the transport task exits, LSP is dormant for that language until led restart.
- **In-flight LSP requests leak oneshot futures on server death** (`drivers/lsp.md`). The model can be left waiting forever on a request that never gets a response.
- **`macro_repeat` flag latches even after failed execute** (`spec/macros.md`). User presses `Ctrl-x e` on a macro that errors; subsequent bare `e` presses keep retrying the failing macro.
- **Config-file hot-reload has NO file watcher at all** (`drivers/config-file.md`). Prior belief was "watcher exists but round-trip unfinished." Actually there's no notify setup in the config-file crate whatsoever. The earlier wording in this doc is misleading — updated below.
- **LspOut::BufferClosed is unreachable code** (`drivers/lsp.md`). `derived.rs:650-651` suppresses it; the LSP crate's own file watcher is relied on instead. Worth simplifying in the rewrite.
- **LspOut::Edits is overloaded** (`drivers/lsp.md`). Same variant carries both real edits and the "format done" signal (empty edits vector). Split in rewrite.
- **`lsp.rename` overlay drops `InsertChar`** (confirmed; already flagged but narrative/driver docs concur).
- **server-initiated `workspace/applyEdit` is not handled** (`drivers/lsp.md`). Typical servers use this for rename-by-code-action or organize-imports-as-command. Grep returns no hits in `manager.rs`.
- **client/registerCapability is single-shot** (`drivers/lsp.md`). No unregister handling.
- **Schema migration is destructive** (`spec/persistence.md`). Every `SCHEMA_VERSION` bump drops-and-recreates. Rewrite may want preservation.
- **`content_hash` i64/u64 cast** (`spec/persistence.md`). Values with the high bit set may round-trip incorrectly through SQLite's signed INTEGER column.
- **Undo DB has no vacuum** (`spec/persistence.md`). Long-lived workspaces grow without bound.
- **`save_session` failure is silent on quit** (`spec/persistence.md`). If the write fails, user loses session state without warning.

---

## Likely bugs (worth fixing in rewrite, possibly worth a separate fix on `main` too)

### `lsp.rename` overlay drops `InsertChar`

`action/lsp.rs` sets focus to `PanelSlot::Overlay` when the rename overlay opens. But `actions_of.rs::requires_editor_focus` blocks `InsertChar` unless `has_input_dialog` returns true — and `has_input_dialog` only checks `file_search` and `find_file`, not `lsp.rename`. So typing a new name in the rename overlay may silently drop keystrokes.

**Status**: needs a golden to confirm. If real, the rewrite must handle overlay focus consistently.

### `DocStoreIn::ExternalRemove` is silently dropped

The docstore driver fires this when an open file is deleted from disk. The model handler ignores it, so led keeps showing a stale buffer pointing at a non-existent file.

**Status**: probably a bug. Rewrite should handle external delete (close buffer, alert, or mark stale).

---

## save_all dispatch order is non-deterministic

Current led's `save_all` (Ctrl-x Ctrl-a) iterates dirty buffers in HashMap order. The save dispatches complete in a non-deterministic order; the final visible state reflects whichever save completes last (e.g. status bar shows "Saved <X>" where X varies per run). Two attempted goldens (`edge/save_all_multiple_dirty`, `features/save_flows/save_all_two_dirty`) had to be removed for this reason.

**Action for rewrite**: save-all should iterate in a stable order (e.g. tab order). Then `Saved <X>` becomes deterministic and these scenarios can be re-added.

---

## Hot-reload doesn't actually work

`ConfigFileOut::Persist` is wired but does nothing meaningful. Editing `keys.toml` or `theme.toml` at runtime has zero effect — config is read once at startup. The file watcher infrastructure exists but the round-trip isn't completed.

**Action for rewrite**: decide whether hot-reload is in scope. If yes, implement it. If no, document the startup-only behavior.

---

## Hardcoded settings that look like they should be configurable

From `crates/state/src/lib.rs:234` (`Dimensions::new`):
- `tab_stop = 4`
- `side_panel_width = 25`
- `scroll_margin = 3`
- `gutter_width = 2`
- `ruler_column = Some(110)`

Other hardcoded:
- LSP server binaries per language (only overridable via `--test-lsp-server`)
- Git scan debounce (500ms)

**Action for rewrite**: decide which to expose in `theme.toml` / `keys.toml` / a new `settings.toml`.

---

## Quirks worth preserving or documenting

### `SHIFT` stripped on `KeyCode::Char` (`crates/core/src/keys.rs`)

Bindings like `shift+a` parse successfully but never match — SHIFT is dropped from `Char` events before lookup. So `shift+a` ≡ `a` (capitalized via the char itself).

**Action for rewrite**: explicit decision — either drop SHIFT from `Char` bindings entirely (lint at parse time), or honor it.

### F-keys, Insert, numpad keys unbindable

`parse_key_combo` doesn't handle these. Users can't bind them.

**Action for rewrite**: probably extend the keymap parser. Low priority.

### `Action::Redo` has no default binding

Bound implicitly by the redo path of `Action::Undo` (cycle), but no standalone binding. Users who want a separate redo key must add one to `keys.toml`.

**Action for rewrite**: probably bind `Ctrl-Shift-z` or similar by default.

### `Suspend` raises `SIGTSTP`

PTY-based goldens that exercise `Action::Suspend` need to handle the signal (otherwise the test process is stopped). Not a bug — just means this action needs special handling in the goldens runner if covered.

---

## Architectural notes (FRP-side, will dissolve in rewrite)

These don't need action — they're observations about the current code that the rewrite naturally addresses:

- `Mut::Action` is still a mega-dispatcher (anti-Principle-9) handling ~12 actions imperatively. Rewrite splits this into per-action dispatch.
- `Mut::BufferUpdate` is fanned in from 12+ streams. Source can't be distinguished from snapshots — rewrite's per-domain reducer makes this irrelevant.

---

## Process notes

- Items here are NOT to be fixed pre-rewrite unless they actively break the goldens generation.
- Each item should get a one-line decision after the rewrite begins: "fixed in rewrite", "preserved", "deferred", or "removed".
