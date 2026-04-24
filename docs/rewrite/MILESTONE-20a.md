# Milestone 20a — Tiered issue navigation (Alt-./Alt-,)

After M16 (LSP diagnostics) and M19 (git line statuses), the two
`IssueCategory` sources we care about today are populated. M20a
wires the `NextIssue` / `PrevIssue` commands that walk them as
a tiered cycle, exactly matching legacy. Alt-. = forward,
Alt-, = backward. The cycle stays inside the first non-empty
category level — errors trump warnings trump git trump (later)
PR — so a file full of errors doesn't send the user to a stray
warning three lines away.

Prerequisite reading:

1. `docs/spec/navigation.md` § "Issue navigation" — the tiered
   walk + dedup rules the port follows.
2. Legacy `led/src/model/nav_of.rs` — the reference
   implementation for `collect_positions`, `pick_target_index`,
   `scan_level`, and the per-case Mut fan-out.
3. `crates/core/src/issue.rs` §`NAV_LEVELS` — already present,
   already matches legacy. M20a is the first consumer.
4. `ROADMAP.md` § M20a — scope + goldens list.

---

## Goal

```
$ cargo run -p led -- src/foo.rs
# Buffer has three LSP errors (lines 4, 20, 41) and
# two warnings (lines 2, 55).

# Alt-.   → cursor lands on the first *error* past cursor
#           (line 4); status bar: " Jumped to Error 1/3".
# Alt-.   → line 20, " Jumped to Error 2/3".
# Alt-.   → line 41, " Jumped to Error 3/3".
# Alt-.   → wraps to line 4, " Jumped to Error 1/3".

# Fix all three errors: now only warnings remain.
# Alt-.   → " Jumped to Warning 1/2" (line 2).

# No diagnostics, some git unstaged changes:
# Alt-.   → " Jumped to Unstaged 1/N" (first change line).
```

Key invariant: the level is picked **once per Alt-. press**
based on what currently exists. Clearing the highest-priority
category automatically moves the cycle down to the next.

## Scope

### In

- **`Command::NextIssue` / `Command::PrevIssue`** — two new
  keymap variants. Parse cases `"next_issue"` / `"prev_issue"`.
  Default bindings: `alt+.` and `alt+,`.

- **`next_issue_active` / `prev_issue_active`** in
  `dispatch/nav.rs`. Both take the same shape:

  ```rust
  pub(super) fn next_issue_active(
      tabs: &mut Tabs,
      edits: &BufferEdits,
      diagnostics: &DiagnosticsStates,
      git: &GitState,
      jumps: &mut JumpListState,
      alerts: &mut AlertState,
      terminal: &Terminal,
      browser: &BrowserUi,
  ) {
      nav_issue(/* … */, forward = true);
  }
  ```

  Shared helper `nav_issue` does the work:

  1. `compute_navigation(forward)` walks `NAV_LEVELS`, builds
     `Vec<Pos>` per level, returns `Some(NavOutcome)` for the
     first non-empty level (with wrap-around).
  2. If the outcome's target path is the active tab: update
     cursor + recenter scroll via the existing
     `dispatch::center_on_cursor`.
  3. If the target path is a *different* open tab: activate it +
     same cursor/scroll update.
  4. If the target path is **not** currently open: skip for
     M20a (the pending-cursor plumbing is M21).
  5. Record the pre-jump position onto `JumpListState`.
  6. Set info alert: `" Jumped to {label} {idx}/{total}"`.

- **`collect_positions(diagnostics, git, edits, cats)`** —
  pure helper that projects the two atoms into `Vec<Pos>` for
  the requested category set. Mirrors legacy:

  - Diagnostics source filters `Error` → `LspError`, `Warning`
    → `LspWarning`, drops `Info` / `Hint` (never navigable).
  - Git source: for every path in `git.file_statuses` whose
    categories intersect `cats`, prefer per-line ranges from
    `git.line_statuses` (each emits one `Pos` at `rows.start`),
    falling back to `(row=0, col=0)` for file-level-only
    categories (`Untracked` — no line data).
  - Positions are sorted by `(path, row, col)` and deduped on
    that triple so a line that carries both an `LspError` and
    an `Unstaged` bar collapses to one nav target (at whichever
    category the first-hit level assigned it).

- **`pick_target_index(positions, cur, forward)`** — given the
  sorted position list and the user's current `(path, row,
  col)`, return the 0-based target index:
  - `forward`: first position strictly `>` cursor; wraps to 0.
  - `backward`: last position strictly `<` cursor; wraps to
    `len - 1`.
  - `cur == None` (no active tab): returns 0.

- **Default bindings**:
  ```rust
  m.bind("alt+.", Command::NextIssue);
  m.bind("alt+,", Command::PrevIssue);
  ```

- **Jump-list record.** Before the cursor moves, push the
  outgoing `(path, cursor)` onto `JumpListState` — matches
  legacy's `record_jump` pattern and lets Alt-b round-trip
  after an issue jump.

- **Trace.** No new dispatched-intent trace line — issue nav is
  a pure dispatch-side state change; the alert is the
  user-visible signal.

### Out

- **PR tier** (`PrComment`, `PrDiff`) → M27. M20a's tier list
  stops after git; M27 extends the nav levels by one.
- **Opening an unopened file at the target** → requires the
  pending-cursor plumbing (`Tab.pending_cursor: Option<Cursor>`
  + a load-completion hook) that also lets Alt-Enter goto-def
  into library code. Both are deferred to M21's session-restore
  work, which introduces the same primitive for real.
- **`SetTabPendingCursor` + `RequestOpen` Muts** — legacy
  fans out to three Muts for the unopened-file case; M20a
  skips that whole branch.
- **Alert with precedence colour** — the alert string is plain
  info; painter picks the chrome style. Future theming can
  tint "Jumped to Error" in red etc.
- **Issue nav respecting the jump-list index** (push a pre-jump
  entry EVERY invocation) — we do push on every call. Legacy
  has a subtler "only push when crossing buffers" rule; our
  one-push-per-call is simpler and roughly equivalent for
  single-buffer workflows.

## Key design decisions

### D1 — Levels are walked in order until one has items

The legacy "escape-hatch from an error-rich file" UX is
load-bearing: if a file has 50 errors and one warning, Alt-.
must never teleport to the warning. `compute_navigation` walks
`NAV_LEVELS` (const `&[u8] = &[1, 2, 3, 4, 5]`) and returns on
the first non-empty level.

### D2 — Dedup on `(path, row, col)` inside a level

A single line can carry LspError + Unstaged (user wrote a
broken statement in an edited region). Listing both would
desync `pick_target_index`'s count from the user's mental
model. Dedup keeps the count honest; the first-seen category
wins the label.

### D3 — Positions are row-indexed, col always 0 for git

Legacy's git nav jumps to `(rows.start, 0)` — column info
isn't meaningful for a line bar. LSP diagnostics carry a real
column, so we preserve it for those. Mixed sort still works
because the `(path, row)` primary sort dominates.

### D4 — Skip unopened files, don't pend-open them (M20a)

The "open this file at a specific cursor" primitive is the
same one needed for:
- Session restore (M21).
- Alt-Enter goto-def into library code (the user just asked
  about this).
- Issue-nav into unopened files.

M21 introduces `Tab.pending_cursor` + the load-completion hook
that applies it. M20a sidesteps by silently skipping unopened
targets — matches the broader "pre-M21 deferrals" pattern
goto-def already uses.

### D5 — Recenter scroll via the existing helper

`dispatch::center_on_cursor` landed two commits ago for
Alt-Enter goto-def. Issue nav reuses it verbatim: if the
target's line is inside the current scroll window, leave
scroll; otherwise pin at body_rows/3 from top. Consistent UX
across the two "big jumps".

### D6 — All nav primitives in `dispatch/nav.rs`

That file already hosts `match_bracket` + `jump_back` +
`jump_forward`. Adding `next_issue` / `prev_issue` + the
shared `compute_navigation` keeps every "big cursor jump"
together. No separate `issue.rs` module.

### D7 — Alert key uses `"nav.issue"`

`AlertState.set_info` accepts a string; we overwrite with the
current "Jumped to …" message. Unlike warns, info alerts have
a 2-second TTL (legacy parity), which is the right feel for a
transient nav hint.

## Types

Already exist. No new state types.

## Crate changes

```
crates/
  runtime/src/
    dispatch/nav.rs          + compute_navigation, collect_positions,
                              pick_target_index, next_issue_active,
                              prev_issue_active. Tests for each.
    dispatch/mod.rs          + run_command arms for NextIssue /
                              PrevIssue; thread diagnostics + git
                              through Dispatcher where needed.
    keymap.rs                + Command::NextIssue / PrevIssue;
                              parse cases; default alt+. / alt+,
                              bindings.
```

No new crate members. `dispatch::Dispatcher` already holds most
of what we need (tabs, edits, jumps, alerts); wire diagnostics
+ git + terminal + browser references.

## Testing

### `dispatch::nav`
- `pick_target_index` forward / backward / wrap-around with
  cross-file positions. (Ports legacy's 6 tests verbatim.)
- `compute_navigation` on an empty state → `None`.
- LSP errors take priority over warnings.
- Falls through to warnings when no errors.
- Falls through to git when no LSP diagnostics.
- Cycles within the first non-empty level (wrap behaviour).
- Skips unopened-file targets silently.
- Records a jump-list entry per call.
- Sets the expected `" Jumped to …"` alert.
- Recenters scroll via `center_on_cursor`.

### `keymap`
- `"next_issue"` parses to `Command::NextIssue`.
- `alt+.` default binds to `Command::NextIssue`.

Expected: +15 tests.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy delta 0 from post-M20.
- Interactive smoke:
  - Open a Rust file with a type error — Alt-. cycles through
    its errors; Alt-, steps backward.
  - Edit a tracked line without saving — Alt-. jumps to the
    git line bar.
  - Mix of diagnostics + git — verify LSP wins.
- Goldens:
  - `actions/next_issue` — expected to move closer to green
    (trace shape matches; frame asserts the specific cursor
    landing which depends on the golden's fixture).
  - `actions/prev_issue` — same.
  - `features/issue_nav/*` — best-effort; some depend on
    M27-tier PR entries and stay red until then.

## Growth-path hooks

- **M21 pending-cursor** — re-enables the "nav into an
  unopened file" branch. No change to `compute_navigation`;
  only `next_issue_active`'s `else` arm grows.
- **M27 PR tier** — `collect_positions` grows a
  `collect_pr_positions` sub-helper (`PrComment` + `PrDiff`)
  that mirrors the git one against a new `PrState` atom.
  `NAV_LEVELS` already includes level 5 for PR, so no
  roadmap churn.
- **Buffer-level issue counts in the status bar** — nothing
  here; but once `compute_navigation` is factored, a separate
  memo can run it in a "just count" mode without moving the
  cursor for a future "N issues" status indicator.
