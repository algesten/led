# Milestone 10 — Extended navigation

Tenth vertical slice. After M10 the user has a jump list
(back/forward across "interesting" positions) and a match-bracket
jump. Word-granularity motion, already implemented in M6, gets
its default bindings corrected to match legacy; tab cycling
gains an auto-record side-effect for the jump list.

Prerequisite reading:

1. `docs/spec/navigation.md` — whole file. The authoritative
   reference for movement primitives, jump list, match-bracket,
   and scroll-margin behaviour.
2. `docs/extract/default_keys.md` (or the legacy file
   `crates/config-file/src/default_keys.toml` — the default keys
   for `alt+b/f` + `alt+left/right` + `alt+]`).
3. `MILESTONE-6.md` § "Chord bindings + richer keymap" — M10
   extends the same keymap surface.
4. `MILESTONE-8.md` § "D7 — Rebase primitive" — jump positions
   aren't rebased in M10 (matches legacy), but the same
   consideration applies: zombie entries survive across edits
   and get clamped on read.

---

## Goal

```
$ cargo run -p led -- src/main.rs
# Cursor at line 200 after page-down-page-down.
# Ctrl-x k             → no jump recorded (not a navigation command).
# Alt-]                → cursor on `{`, jumps to matching `}`;
#                        jump list now holds [200] at index 0, cursor
#                        at line 437.
# Alt-b (alt+left)     → jumps back to line 200, cursor restored.
#                        Also pushed the pre-jump (437) onto the list
#                        first since we were at the head.
# Alt-f (alt+right)    → jumps forward to line 437 again.
# Tab                  → switch to another tab. Before the switch,
#                        the current (path, line, col) is pushed onto
#                        the jump list. Alt-b from the new tab jumps
#                        back to the old buffer at the exact cursor.
```

## Scope

### In

- **`state-jumps` crate** — new workspace member.
  ```rust
  pub struct JumpListState {
      pub entries: VecDeque<JumpPosition>,
      pub index:   usize,  // entries.len() == "at head, no jump in progress"
  }

  pub struct JumpPosition {
      pub path: CanonPath,
      pub line: usize,
      pub col:  usize,
  }

  const MAX_ENTRIES: usize = 100;

  impl JumpListState {
      pub fn record(&mut self, pos: JumpPosition);  // truncates forward + caps
      pub fn can_back(&self) -> bool;
      pub fn can_forward(&self) -> bool;
      pub fn step_back(&mut self, current: JumpPosition) -> Option<JumpPosition>;
      pub fn step_forward(&mut self) -> Option<JumpPosition>;
  }
  ```
  Matches legacy behaviour (`led/src/model/jump.rs:6`): on
  `record`, truncate `entries[index..]` first, then push, then
  cap the front to 100. `index` is set to `entries.len()` after
  a record (back at head).

- **`Command::JumpBack`, `Command::JumpForward`, `Command::MatchBracket`**
  — three new keymap commands with `parse_command` cases
  (`"jump_back"`, `"jump_forward"`, `"match_bracket"`).

- **Default keymap changes**:
  - `alt+b` + `alt+left` → `JumpBack`
  - `alt+f` + `alt+right` → `JumpForward`
  - `alt+]` → `MatchBracket`
  - `alt+b` / `alt+f` are **unbound** from `CursorWordLeft` /
    `CursorWordRight` — matches legacy, which has no default
    word-move binding. The commands stay available for users
    who want to bind them in `keys.toml`.

- **`MatchBracket` primitive** — pure rope scan. Considers the
  char at the cursor first, then the char immediately before.
  Brackets recognised: `()`, `[]`, `{}`. Scans forward for an
  open bracket, backward for a close bracket, balancing depth.
  No-op when no bracket is under/before the cursor or when no
  match is found in-buffer.

  M10 does **not** consult syntax highlighting to skip brackets
  inside strings / comments — M15 will revisit once `SyntaxState`
  exists.

- **`JumpBack` primitive**:
  1. No-op if `index == 0`.
  2. If `index == entries.len()` (at head), first push the
     current `(path, line, col)` — the implicit save-before-back
     that lets the user round-trip.
  3. Decrement `index`.
  4. Read `entries[index]`. If the tab is open, activate it + set
     cursor. If the path isn't open yet (buffer closed, old
     entry), the jump is a no-op for M10. (M11's tab
     materialisation logic + M13/M16's pending-cursor plumbing
     is the proper story; M10 skips.)

- **`JumpForward` primitive** — the mirror.
  1. No-op if `index + 1 >= entries.len()`.
  2. Increment `index`.
  3. Read `entries[index]`, activate + cursor.

- **Auto-record sites in M10**:
  - `TabNext` / `TabPrev` — before changing `active`, push the
    outgoing tab's `(path, cursor.line, cursor.col)`.
  - `MatchBracket` — before moving, push the current position.
  - `JumpBack` from head — as above.

  Nothing else auto-records. Issue navigation (M16 diagnostics,
  M19 git, M20 PR) and isearch (M13) add their own record sites
  later, per legacy.

### Out

Per `ROADMAP.md` and the scope decisions above:

- **Issue navigation** (`next_issue` / `prev_issue`): LSP → M16,
  git hunks → M19, PR comments → M20. The dispatch arms + alert
  plumbing land with those features.
- **Scroll-offset capture in `JumpPosition`**: legacy stores
  `scroll_offset` so the viewport returns to the saved position,
  not just the cursor. M10 restores cursor only; `adjust_scroll`
  (M2) re-solves a viewport around it. Matches the rewrite's
  existing invariant — every cursor move owns its scroll.
  Revisit at M13 or M21 if feel-of-use complaints surface.
- **Jump-on-big-line-delta heuristic** (ROADMAP's "≥ 5 lines"):
  legacy doesn't record automatically on every cursor move, and
  the heuristic's threshold is ad-hoc. M10 sticks to legacy's
  explicit-record model (tab switch, match-bracket, jump-back
  from head). If the UX suffers, revisit.
- **Bracket highlight** in the gutter / under the cursor → M15
  (syntax highlighting). M10's `MatchBracket` scans on demand;
  no persistent highlight state.
- **`Outline` (`alt+o`)** → legacy declared it but never wired
  a handler. `POST-REWRITE-REVIEW.md` flags it as dead code.
  Not scheduled.
- **Mouse / scroll-wheel** → legacy didn't surface scroll
  events; no plan to add now.

## Key design decisions

### D1 — Jump list is a state source, not a driver

No async backing. `JumpListState` lives in `crates/state-jumps/`
alongside the other state sources. Dispatch mutates it directly
in the relevant command primitives.

### D2 — `JumpPosition` omits `scroll_offset`

Legacy stored the scroll so a jump restored the EXACT viewport.
The rewrite's invariant is that every cursor move owns its
scroll via `adjust_scroll`, so a post-jump `adjust_scroll` gives
us a sensible viewport automatically. One less field to
persist; matches the rest of the runtime's scroll discipline.

### D3 — Auto-record sites stay minimal

Matching legacy: record only on explicit "interesting jump"
commands (match-bracket, jump-back, tab switch). A generic
"cursor moved ≥ N lines" rule is tempting but:

- Ad-hoc thresholds age badly.
- Double-tap `Alt-V` (page-up × 2) would spam the jump list.
- Legacy doesn't do it, and `navigation.md` is the contract.

Users can add more record sites via future features
(isearch/LSP) rather than via a heuristic.

### D4 — Tab-switch records pre-switch, not post

When the user hits `Tab`, the *source* tab's position is what
they might want to return to — push that. The destination tab
has its own cursor (possibly stale, possibly fresh) that will
restore on activation. Matches what `Alt-b` would return to.

### D5 — Stale jump targets are silently dropped

If the path in `entries[index]` isn't currently open as a tab
(buffer closed since the entry was recorded), M10 skips — no
alert, no error. The entry stays in the deque so a later
`JumpForward` or tab reopening could still hit it.

When M21 adds session persistence, entries across a restart
will often target closed buffers; the same skip-silent path
applies. M12 (find-file) / M11 (browser) introduce the proper
"re-open this path" affordance; M10 is content to be
conservative.

### D6 — `MatchBracket` uses direct rope scan

Tree-sitter (M15) would let us skip brackets inside strings /
comments. Until then, the scan is naïve and may jump to a
string's `}` from `{` outside. Legacy had the same limitation
before its syntax layer; good enough as a starting point.

The scan is O(len_between_brackets) — bounded by line-length
for typical code; unbounded for huge single-line files. No
caching; computed per invocation.

### D7 — Default bindings match legacy

M6 took a shortcut and bound `alt+b` / `alt+f` to word motion;
legacy has them on `jump_back` / `jump_forward`. M10 corrects
this — legacy semantics win. The word-motion commands stay
available (useful) but are no longer bound by default.

## Types

### `state-jumps` (new crate)

```rust
use std::collections::VecDeque;
use led_core::CanonPath;

const MAX_ENTRIES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpPosition {
    pub path: CanonPath,
    pub line: usize,
    pub col:  usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JumpListState {
    pub entries: VecDeque<JumpPosition>,
    pub index:   usize,
}

impl JumpListState {
    pub fn record(&mut self, pos: JumpPosition);
    pub fn can_back(&self) -> bool;       // index > 0
    pub fn can_forward(&self) -> bool;    // index + 1 < entries.len()
    pub fn step_back(&mut self, current: JumpPosition) -> Option<JumpPosition>;
    pub fn step_forward(&mut self) -> Option<JumpPosition>;
}
```

### Dispatcher grows

```rust
pub struct Dispatcher<'a> {
    // ... M9 fields
    pub jumps: &'a mut JumpListState,   // NEW
}
```

Propagates through `dispatch_key` + `run_command` to
`tabs::cycle_active`, the new `nav::match_bracket` /
`nav::jump_back` / `nav::jump_forward` primitives.

### Keymap + parse

```rust
Command::JumpBack,
Command::JumpForward,
Command::MatchBracket,

"jump_back"     => Ok(Command::JumpBack),
"jump_forward"  => Ok(Command::JumpForward),
"match_bracket" => Ok(Command::MatchBracket),
```

Defaults:
```rust
m.bind("alt+b",     Command::JumpBack);
m.bind("alt+left",  Command::JumpBack);
m.bind("alt+f",     Command::JumpForward);
m.bind("alt+right", Command::JumpForward);
m.bind("alt+]",     Command::MatchBracket);
// remove the word-motion alt+b / alt+f bindings from M6
```

## Crate changes

```
crates/
  state-jumps/               NEW — JumpListState, JumpPosition, tests
  runtime/src/
    dispatch/nav.rs          NEW — match_bracket, jump_back, jump_forward
    dispatch/mod.rs          Dispatcher.jumps; run_command arms for
                             JumpBack/JumpForward/MatchBracket; tab
                             cycle records pre-switch position
    dispatch/tabs.rs         cycle_active signature grows a jumps
                             ref (records outgoing cursor)
    keymap.rs                Command + parse_command + default_keymap
                             (alt+b/f/]/left/right → jump/match)
    lib.rs                   run() threads JumpListState through
```

## Testing

### `state-jumps`
- `record` from empty state → entries=[pos], index=1.
- `record` truncates the forward branch (user was mid-history).
- `record` caps at 100 (101st push drops the oldest).
- `step_back` decrements and returns the new entry.
- `step_back` from head pushes the supplied current position,
  then returns the previous head.
- `step_back` at index 0 returns None.
- `step_forward` at head returns None.
- `step_forward` + `step_back` is a round-trip.

### `runtime::dispatch::nav`
- `match_bracket` at `{` → cursor at matching `}`.
- `match_bracket` at `)` → cursor at matching `(`.
- `match_bracket` considers char BEFORE cursor when the char at
  cursor isn't a bracket.
- `match_bracket` no-op when no bracket match (unbalanced /
  different line structure).
- `match_bracket` records pre-jump position.
- `jump_back` no-op on empty list.
- `jump_back` from head auto-records before stepping.
- `jump_back` + `jump_forward` round-trips cursor.
- `jump_back` to a closed tab is silent no-op (doesn't crash,
  doesn't drop the entry).
- `tab_next` / `tab_prev` push the pre-switch position.

Expected: +15 tests.

## Done criteria

- All existing tests pass.
- New nav / jump-list tests pass.
- Clippy unchanged from post-M9 (13).
- Interactive smoke:
  - Open a large file. `alt+]` on a `{` jumps to its `}`.
  - `alt+b` jumps back. `alt+f` returns.
  - Two-tab session: switch, edit a bit, switch back via tab;
    `alt+b` returns to the exact earlier cursor.
- Goldens baseline: still 0 / 257 — side panel remains the
  blocker (M11).

## Growth-path hooks

- **Session persistence** (M21): `JumpListState` entries get
  serialised to SQLite on save; deserialised on restore.
  `JumpPosition` is cheap to round-trip.
- **Isearch record** (M13): when the user accepts an isearch
  match that moved the cursor, record the pre-search position.
- **LSP goto-definition** (M16): record the pre-goto position.
- **Syntax-aware bracket matching** (M15): swap the rope scan
  for a tree-sitter "pair" query so brackets in strings /
  comments are ignored.
- **Issue navigation** (M16 / M19 / M20): `NextIssue` /
  `PrevIssue` commands live in dispatch/nav.rs, share the same
  activate+cursor path.
- **Scroll preservation** on jump: if feedback warrants,
  `JumpPosition` can grow a `scroll.top` field and the
  primitives restore it instead of recomputing.
