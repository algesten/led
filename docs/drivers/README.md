# Driver inventory

Per-driver documentation of current led's drivers. Template from `docs/rewrite/DRIVER-INVENTORY-PLAN.md`.

Each doc describes: purpose, lifecycle, every `*Out` and `*In` variant, owned state, side effects, async characteristics, and the proposed translation to the query-driven architecture (`docs/rewrite/QUERY-ARCH.md`).

## Drivers, roughly by complexity

| Driver | Doc | Role(s) |
|---|---|---|
| LSP | [lsp.md](lsp.md) | Input (server-pushed notifications) + resource (request/response). Most complex. Freeze mechanism, pull-only diagnostics, per-language server lifecycle. |
| Workspace | [workspace.md](workspace.md) | Resource + stateful persistence. SQLite session + undo DBs, primary flock, cross-instance sync. |
| docstore | [docstore.md](docstore.md) | Resource for file open/save/save-as. Content-hash reconciliation for external change. |
| fs | [fs.md](fs.md) | Resource (list, find-file) + watcher. Stateless under current code; watching consolidated today across fs/docstore/workspace. |
| Syntax | [syntax.md](syntax.md) | Resource for tree-sitter parsing. Version-stamped results. Incremental replay + shadow-doc. |
| Git | [git.md](git.md) | Resource with timer-driven polling. libgit2 scan → file status + line status (rebase-eligible) + branch. |
| gh-pr | [gh-pr.md](gh-pr.md) | Resource via `gh` CLI. ETag polling. Branch-change clearing. Comment-URL resolution. |
| File search | [file-search.md](file-search.md) | Resource for workspace-wide search + replace. ripgrep-style. Replace flow is mostly buffer-mediated. |
| UI | [ui.md](ui.md) | Output-only (rendering). Unusual `UiIn::EvictOneBuffer` back-channel for memory pressure. |
| terminal-in | [terminal-in.md](terminal-in.md) | Input (sync-push). Key events, resize, initial-size probe. Focus events unused. |
| config-file | [config-file.md](config-file.md) | Resource (load). No file watcher exists despite hot-reload intent. |
| Clipboard | [clipboard.md](clipboard.md) | Resource (read/write). Platform differences (macOS/X11/Wayland/Win). Headless variant for tests. |
| Timers | [timers.md](timers.md) | Input (timer-fired events). **Out-of-contract for the rewrite** per project scope decisions. |

## Query-arch translation summary

Each doc's "Translation to query arch" section proposes how the driver splits in the new architecture:

- **Input drivers** (push `Event`s into the handler): terminal-in, fs-watch, lsp-notifs, timers (if kept).
- **Resource drivers** (dispatch-then-event): docstore, fs-reads, lsp-requests, git, syntax, file-search, clipboard, gh-pr.
- **Both**: fs (watch + reads), lsp (notifications + requests), workspace (session ops + sync notifications).
- **Absorbed into runtime**: ui (becomes a query + `terminal.draw(&frame)`).

See `docs/rewrite/QUERY-ARCH.md` § "Drivers: two kinds" for the full pattern.

## Known gaps

Agent-authored docs flagged ~30 `[unclear — ...]` items inline. To survey:

```
grep -n "unclear" docs/drivers/*.md
```

The highest-signal ones are mirrored in `docs/rewrite/POST-REWRITE-REVIEW.md`.
