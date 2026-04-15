# Functional spec

Narrative, human-readable documentation of what current led does. This is the reference the rewrite matches — or consciously diverges from with explicit notes.

Companions:
- `docs/extract/*.md` — mechanical extractions (enum inventories) that the narrative cites by name
- `docs/drivers/*.md` — per-driver inventory docs
- `tests/golden/` — the enforceable spec (see `docs/rewrite/GOLDENS-PLAN.md`)

## Reading order

Start with whatever feature you're working on. Each doc is self-contained with an "Extract index" section linking back to the mechanical sources.

| Area | Doc | Summary |
|---|---|---|
| Startup / shutdown / resume | [lifecycle.md](lifecycle.md) | 5-phase state machine; 14-step startup sequence; SIGTSTP suspend; quit gating on session save |
| File + tab management | [buffers.md](buffers.md) | Open, save (all variants), tabs, preview-tab, external change, dirty tracking |
| Text manipulation | [editing.md](editing.md) | Insert/delete/newline, undo/redo with groups, mark+region, kill ring, auto-indent, reflow, sort-imports |
| Cursor + issue navigation | [navigation.md](navigation.md) | Char/word/line/file movement, scroll_margin, jump list, NextIssue/PrevIssue across LSP+git+PR |
| In-buffer + file search | [search.md](search.md) | isearch (Ctrl-s), file-search overlay with case/regex/replace toggles, replace-all buffer-mediated flow |
| Find-file picker | [find-file.md](find-file.md) | Open + save-as (shared infrastructure), prefix completion, Tab-LCP, `~` expansion — no fuzzy, no recents |
| File browser sidebar | [file-browser.md](file-browser.md) | Tree, expansion, reveal-active-file, preview-on-nav |
| LSP integration | [lsp.md](lsp.md) | Diagnostics (pull-only + freeze), completions, goto-def, rename, code actions, format, inlay hints |
| Git surface | [git.md](git.md) | File/line status, branch, debounced scan, sidebar+gutter+statusbar consumers |
| GitHub PR integration | [gh-pr.md](gh-pr.md) | `gh` CLI subcommands, ETag polling, branch-change clearing, open-comment-URL |
| Syntax (tree-sitter) | [syntax.md](syntax.md) | Language detection, incremental parsing, version-stamped results, modeline override |
| Status bar + gutter + alerts | [ui-chrome.md](ui-chrome.md) | Layout diagram, status-bar mode precedence, 2-col fixed gutter, alert levels (only Info + Warn) |
| Keymap | [keymap.md](keymap.md) | TOML compilation, chord prefixes, context precedence, post-lookup overlay interceptors |
| Config files | [config.md](config.md) | `keys.toml` / `theme.toml` — **hot-reload is a no-op**; no-merge defaults replacement |
| CLI | [cli.md](cli.md) | Every flag, including test-only (`--test-lsp-server`, `--test-gh-binary`, `--golden-trace`) |
| Persistence | [persistence.md](persistence.md) | SQLite schema, primary flock, session + undo DBs, cross-instance sync via notify files |
| Keyboard macros | [macros.md](macros.md) | Record/execute with count, recursion cap, unpersisted single slot |

## Cross-check

Per `docs/rewrite/SPEC-PLAN.md`, every entry in `docs/extract/*.md` should be referenced by at least one narrative doc, and every narrative's "Extract index" should cite valid entries. The initial automated check found:

- **Actions**: all 59 variants cited across the docs (via either explicit name or feature-area rollup).
- **Keybinding contexts**: all 11 contexts covered.
- **Driver events**: all 12 drivers have corresponding narrative coverage (drivers with mostly-internal events documented in the per-driver inventory instead).
- **Config keys**: covered by `config.md`.

Full mechanical check is not yet automated; see `docs/rewrite/SPEC-PLAN.md` § "Cross-check" for the intent.

## Known gaps

Agent-authored docs flagged ~60 `[unclear — ...]` items inline. They're left in-context rather than consolidated — each one makes sense where it appears. To survey:

```
grep -n "unclear" docs/spec/*.md
```

The highest-signal ones (likely bugs, likely-worth-rewriting) are mirrored in `docs/rewrite/POST-REWRITE-REVIEW.md`.

Phase D (interactive exploration, per SPEC-PLAN.md) is deliberately not done yet. Expect more gaps once the rewrite begins and someone uses led for daily work while paying attention.
