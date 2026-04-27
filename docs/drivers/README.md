# Driver inventory

Per-driver documentation of led's drivers.

Each doc describes: purpose, lifecycle, every `*Out` and `*In` variant, owned state, side effects, and async characteristics.

## Drivers, roughly by complexity

| Driver | Doc | Role(s) |
|---|---|---|
| LSP | [lsp.md](lsp.md) | Input (server-pushed notifications) + resource (request/response). Most complex. Freeze mechanism, pull-only diagnostics, per-language server lifecycle. |
| Workspace | [workspace.md](workspace.md) | Resource + stateful persistence. SQLite session + undo DBs, primary flock, cross-instance sync. |
| docstore | [docstore.md](docstore.md) | Resource for file open/save/save-as. Content-hash reconciliation for external change. |
| fs | [fs.md](fs.md) | Resource (list, find-file) + watcher. Stateless; watching consolidated across fs/docstore/workspace. |
| Syntax | [syntax.md](syntax.md) | Resource for tree-sitter parsing. Version-stamped results. Incremental replay + shadow-doc. |
| Git | [git.md](git.md) | Resource with timer-driven polling. libgit2 scan → file status + line status (rebase-eligible) + branch. |
| File search | [file-search.md](file-search.md) | Resource for workspace-wide search + replace. ripgrep-style. Replace flow is mostly buffer-mediated. |
| UI | [ui.md](ui.md) | Output-only (rendering). |
| terminal-in | [terminal-in.md](terminal-in.md) | Input (sync-push). Key events, resize, initial-size probe. |
| config-file | [config-file.md](config-file.md) | Resource (load). |
| Clipboard | [clipboard.md](clipboard.md) | Resource (read/write). Platform differences (macOS/X11/Wayland/Win). Headless variant for tests. |
| Timers | [timers.md](timers.md) | Input (timer-fired events). |

## Known gaps

Agent-authored docs flagged ~30 `[unclear — ...]` items inline. To survey:

```
grep -n "unclear" docs/drivers/*.md
```
