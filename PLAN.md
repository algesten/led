# Implementation Plan: FRP Text Editor

**State -> Derived -> Drivers -> Model -> State**

Every side effect lives in a Driver. The Model is a pure reduce: `(State, Mut) -> State`. Derived selects and transforms State fields into Driver inputs. Side effects that react to state live in `_of` functions (e.g. `process_of`).

---

## Phase 17: LSP Integration

Full server lifecycle, JSON-RPC transport, request tracking.

### Server registry
Hardcoded configs: rust-analyzer, typescript-language-server, pyright, clangd, sourcekit-lsp, taplo, vscode-json-language-server, bash-language-server.

### Features
- `textDocument/didOpen`, `didChange` (incremental), `didSave`, `didClose`
- Goto definition
- Rename (workspace edits applied in reverse document order)
- Code actions with resolve
- Format (organize imports then format)
- Completion with fuzzy filtering (nucleo) and resolve for additional edits
- Inlay hints (ghost text at line end, `editor.inlay_hint` style)
- Diagnostics (pull + push, severity mapping)
- Progress tracking (spinner in status bar)
- File watching (notify crate, forwarded to servers)

### Completion popup
- Positioned below cursor (or above if near bottom)
- Width from longest label + detail, max 60 chars
- Max 10 visible items with scroll
- Fuzzy filtered via nucleo (case-insensitive, smart normalization)

### Outline

`Outline` action: uses `textDocument/documentSymbol` to show symbol list. Fuzzy-filtered selection panel.

### Rename dialog

Modal input overlay for `LspRename` — captures new name, renders centered overlay, applies workspace edit.

---

## Phase 18: Message buffer

- Messages panel (`OpenMessages`): read-only buffer syncing from SharedLog, auto-scroll, claims Main panel slot
