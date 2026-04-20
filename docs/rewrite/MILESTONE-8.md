# Milestone 8 — undo / redo + edit log

Eighth vertical slice. After M8 the user can undo edits (one
"group" at a time) and redo them back. The op log that makes this
work is the same log that future async consumers (LSP diagnostics,
git hunks, PR comments) will walk to rebase their position-stamped
payloads through subsequent edits.

Prerequisite reading:

1. `docs/spec/editing.md` § "Undo groups".
2. `docs/extract/actions.md` § `Action::Undo`, `Action::Redo`.
3. `MILESTONE-3.md` — the edit primitives this milestone extends.

---

## Goal

```
$ cargo run -p led -- README.md
# type "hello"               → history: one group (coalesced inserts)
# Ctrl-/ (or Ctrl-_ / Ctrl-7) → all five chars undone in one shot
# Ctrl-/ again                 → nothing more to undo (silent no-op)
# type "world" then Ctrl-space, move, Ctrl-w → two more groups
# Ctrl-/                       → the kill is undone (region returns)
# Ctrl-/                       → the "world" insert is undone
# redo (bound via user config) → redo one group forward
```

## Scope

### In
- Per-buffer undo **history**: past, future, and an optionally-open
  "current group" being accumulated. Survives tab switches, saves,
  and loads. Does not persist across process restarts — persistence
  is M21.
- `EditOp` variants:
  - `Insert { at: usize, text: Arc<str> }` — at is the char index;
    text is the exact Unicode text inserted.
  - `Delete { at: usize, text: Arc<str> }` — at is the char index
    where the text was removed from; text is the removed Unicode.
- `EditGroup`: a contiguous run of `EditOp`s plus
  `cursor_before` / `cursor_after` so undo can restore the cursor
  to where the user was standing when they started the group.
- Coalescing: consecutive `InsertChar(c)` where `c` is a word char
  (alphanumeric or `_`), each at the immediately-following char
  index, merge into the open current group. Any other edit op
  (newline, delete, yank, kill) closes the current group. Any
  non-edit command (cursor move, save, tab switch, abort)
  **finalises** the current group — i.e. moves it into `past`
  without closing an active `current`, making the next edit start
  a fresh group.
- `Undo` (`ctrl+/`, `ctrl+_`, `ctrl+7`): finalise any current
  group, pop the most recent group from `past`, apply its ops in
  reverse, restore `cursor_before`, push the group to `future`.
- `Redo` (`ctrl+?` or bound via user `keys.toml` — legacy leaves
  it unbound, so we match): finalise any current group, pop from
  `future`, apply ops in forward order, restore `cursor_after`,
  push to `past`.
- A fresh edit after an undo clears `future` (branching history).
- Every completed group bumps `EditedBuffer.version` by 1 — so LSP
  rebase queries (M16+) can diff two versions and walk the op log
  between them. M8 does not consume this; the invariant is set up
  now.
- A `rebase_char_index` primitive in `state-buffer-edits` that
  takes `(from_version, char_idx, history)` and returns the
  current char index. Included for the rebase groundwork; unused
  by any memo in M8 but unit-tested.

### Out

Per `ROADMAP.md`:

- **Persisting undo across restarts** → M21 (session / undo DB).
  For M8 history lives in memory only.
- **Rebase of diagnostics / hunks / PR comments** → M16 / M19 / M20.
  The rebase primitive is in place; consumers come with their
  features.
- **Tree-sitter-aware coalescing** (legacy's semantic groups) —
  not scheduled. M8 uses the straightforward Emacs convention.
- **Per-group cost cap / history truncation** — not scheduled.
  Grows unbounded for the session. Revisit when it bites.
- **Yank-pop as undo boundary** — handled for free; `Yank` is a
  non-coalescing op, so it opens a new group.
- **Selective undo / per-region undo** — not scheduled.

## Key design decisions

### D1 — History lives on `EditedBuffer`

`EditedBuffer` already owns the rope and version. The history is
strictly about transforming *that* buffer; co-locating them keeps
invariants local. Post-M8:

```rust
pub struct EditedBuffer {
    pub rope:          Arc<Rope>,
    pub version:       u64,
    pub saved_version: u64,
    pub history:       History,   // NEW — M8
}
```

### D2 — Three-stack model: past, future, current

```rust
pub struct History {
    past:    Vec<EditGroup>,
    future:  Vec<EditGroup>,
    current: Option<EditGroup>,
}
```

- `past`: groups already applied (undoable). Most recent at the end.
- `future`: groups that were undone but not yet redone. Oldest at
  the end (so `pop()` gives you the one to redo next).
- `current`: an open group being accumulated. Coalescing appends
  ops here. `finalise()` moves `current` into `past` and leaves
  `current = None`.

`past.push`/`future.clear` happens every time an edit is committed
(finalised or a non-coalescable op). This matches Emacs / Zed /
most modern editors: editing after an undo loses the redo branch.

### D3 — `EditGroup` records cursor bookends

```rust
pub struct EditGroup {
    pub ops:           Vec<EditOp>,
    pub cursor_before: Cursor,
    pub cursor_after:  Cursor,
}
```

Undo / redo restore the cursor to the bookend the user would
expect. `cursor_before` is snapshot when the group is opened;
`cursor_after` is the cursor after each appended op (so it's
always the "latest").

### D4 — Coalescing is word-char-only

Rule:

- Current group's last op is `Insert { at: A, text: T }` where `T`
  is one character and a word char.
- New op is `Insert { at: A + 1, text: "c" }` where `c` is a word
  char.
- Then: append `c` to `T` in place (the op's text field grows), not
  a separate op.

Breaks:
- Non-word char insert (space, punctuation).
- Newline insert.
- Any `Delete` op.
- Any non-edit command (movement, save, abort, etc.) — not an op,
  but finalises the open group.

M8 does **not** coalesce successive deletes (legacy behaviour).
`Ctrl-K Ctrl-K` → one kill-ring entry (M7) but two undo groups.
Cheap enough; revisit if it annoys anyone.

### D5 — Non-edit commands finalise the open group

After `dispatch_key` runs its command, the dispatch wrapper calls
`finalise_open_groups(tabs, edits)` which walks `edits.buffers`
and closes any open `current`. This is the coalescing counterpart
to M7's `last_was_kill_line` reset.

Performance: O(n_buffers) per tick, each a constant-time branch.
No hot-path concerns.

Simpler alternative considered and rejected: "finalise on every
non-edit command explicitly." Too many branches; missing one is a
silent coalescing bug. The "finalise blanket after every command"
rule is cleaner.

### D6 — Version semantics unchanged

`EditedBuffer.version` still bumps on every *committed* op
(insert_char, each delete, each yank-insert). M3 set the pattern;
M8 keeps it. Importantly: undo and redo also bump version (they
mutate the rope). The op log is a *content-addressed* history of
the rope; version walks monotonically both on edit and on undo.

An op that's being redone produces *a new version*, not a reset
back to the old one. This simplifies rebase: we never go
backwards in version-space.

### D7 — Rebase primitive: `rebase_char_index(from_version, idx, history)`

Signature:

```rust
pub fn rebase_char_index(
    idx: usize,
    from_version: u64,
    history: &History,
) -> usize;
```

Walks `history.past` from `from_version` onwards. For each op:
- `Insert { at, text }` at or before `idx` → `idx +=
  text.chars().count()`.
- `Insert { at, .. }` strictly after `idx` → no change.
- `Delete { at, text }` where the deleted range overlaps `idx`:
  clamp `idx` to `at` (or pull it back as needed).
- Disjoint delete before `idx` → `idx -= text.chars().count()`.
- Disjoint delete after → no change.

If `history.current` is open at `idx > from_version`, include its
ops too, in order. Undone groups (`future`) are NOT walked — they
aren't part of the applied timeline.

Implementation assumes each group starts at the version right
after the previous group's end. We compute that by iterating
`past` (+ `current`) in order and tracking a running version.

Inefficient for many edits: linear walk per query. Later
milestones can add a per-group version index if it matters;
LSP response frequency makes that unlikely to be hot.

### D8 — Undo / Redo key bindings

Legacy `default_keys.toml` binds undo to three equivalent chords
(`ctrl+/`, `ctrl+_`, `ctrl+7`) because some terminals emit
different byte sequences for `ctrl+/`. Rewrite matches.

**Redo is deliberately unbound by default** in legacy (Emacs
tradition treats repeated undo as its own redo via the branch
history). The rewrite matches. Users who want explicit redo add
to `keys.toml`:

```toml
[keys]
"ctrl+?" = "redo"
"ctrl+shift+/" = "redo"
```

## Types

### `state-buffer-edits` grows

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    Insert { at: usize, text: Arc<str> },
    Delete { at: usize, text: Arc<str> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditGroup {
    pub ops:           Vec<EditOp>,
    pub cursor_before: Cursor,
    pub cursor_after:  Cursor,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    past:    Vec<EditGroup>,
    future:  Vec<EditGroup>,
    current: Option<EditGroup>,
}

impl History {
    pub fn record_insert(&mut self, at: usize, text: Arc<str>, cursor_before: Cursor, cursor_after: Cursor);
    pub fn record_delete(&mut self, at: usize, text: Arc<str>, cursor_before: Cursor, cursor_after: Cursor);
    pub fn record_noncoalescable(&mut self, op: EditOp, cursor_before: Cursor, cursor_after: Cursor);
    pub fn finalise(&mut self);
    pub fn can_undo(&self) -> bool;
    pub fn can_redo(&self) -> bool;
    pub fn take_undo(&mut self) -> Option<EditGroup>;
    pub fn take_redo(&mut self) -> Option<EditGroup>;
}
```

`Cursor` has to be importable. `state-buffer-edits` gains a dep on
`state-tabs` (already the case: M7 introduced `TabId` there).

### `Command` + parser gain two variants

```rust
Command::Undo,
Command::Redo,
```

```rust
"undo" => Ok(Command::Undo),
"redo" => Ok(Command::Redo),
```

Default bindings: `ctrl+/`, `ctrl+_`, `ctrl+7` → Undo; Redo
unbound.

### Dispatch shape

Each edit primitive grows a cursor-before snapshot + call to
`history.record_*` after mutating the rope. Shape:

```rust
fn insert_char(tabs: &mut Tabs, edits: &mut BufferEdits, ch: char) {
    with_active(tabs, edits, |tab, eb| {
        let before = tab.cursor;
        let at = cursor_to_char(&before, &eb.rope);
        let mut rope = (*eb.rope).clone();
        rope.insert_char(at, ch);
        bump(eb, rope);
        // cursor advances one char
        tab.cursor.col += 1;
        tab.cursor.preferred_col = tab.cursor.col;
        let after = tab.cursor;
        eb.history.record_insert(
            at,
            Arc::from(ch.to_string()),
            before,
            after,
        );
    });
}
```

After `dispatch_key` calls `run_command`, a follow-up finalise
walks every loaded buffer and closes open groups for non-edit
commands:

```rust
let was_edit = matches!(cmd, Command::InsertChar(_) | Command::Undo | Command::Redo);
if !was_edit {
    for eb in edits.buffers.values_mut() {
        eb.history.finalise();
    }
}
```

We treat `Undo`/`Redo` as edits so they don't finalise themselves.

## Crate changes

```
crates/
  state-buffer-edits/          + EditOp, EditGroup, History;
                               + rebase_char_index fn
  runtime/src/
    keymap.rs                  + Command::Undo / Redo
    dispatch.rs                edit primitives grow history
                               record; run_command arms for
                               Undo / Redo; finalise pass after
                               non-edit commands
```

No new workspace members.

## Testing

- `state-buffer-edits::history` unit tests:
  - record_insert / finalise / take_undo round trips.
  - record_insert coalesces consecutive word-char inserts.
  - record_insert breaks coalesce on non-word char.
  - record_delete is always its own group.
  - take_redo after take_undo retrieves the same group.
  - edit after undo clears future.
  - rebase_char_index: insert before idx shifts; delete before
    idx shifts; delete overlapping idx clamps; ops after idx are
    no-ops.
- `runtime::dispatch` integration tests:
  - typing `hello` then Ctrl-/ leaves empty buffer, cursor at
    original position.
  - typing `hello ` then Ctrl-/ leaves `hello` (space broke
    coalesce).
  - delete then Ctrl-/ restores deleted text at cursor.
  - Ctrl-K kill then Ctrl-/ restores killed text.
  - Ctrl-Y yank then Ctrl-/ removes pasted text.
  - Two consecutive Ctrl-/ undo two groups.
  - Edit after Ctrl-/ drops future (no redo path).
  - Cursor restored to `cursor_before` on undo, `cursor_after`
    on redo.

Expected: +20 tests.

## Done criteria

- All existing tests pass.
- New undo/redo tests pass.
- Clippy unchanged from post-M7 (13).
- Interactive smoke:
  - Type a word, Ctrl-/, word gone in one shot.
  - Type sentence, Ctrl-/ multiple times, each word pops off.
  - Ctrl-K, Ctrl-/, line reappears at cursor.
  - Ctrl-W region, Ctrl-/, region reappears.
  - Switch tabs, come back, history preserved.
- Goldens baseline unchanged in number (0 / 257 — still UI chrome
  dominating).

## Growth-path hooks

- **Persistent undo DB** (M21): serialise `History` into the
  session SQLite on save; deserialise on load.
- **LSP diagnostic rebase** (M16): first consumer of
  `rebase_char_index`. A diagnostic carries `(version, range)`;
  when the version is older than current, rebase the range.
- **Git hunk rebase** (M19), **PR comment rebase** (M20): same.
- **Semantic / AST-aware grouping** (maybe M23+ when syntax is
  integrated): a typed group boundary could split on statement
  boundaries or on tree-sitter node changes. Not scheduled.
- **Undo compression / capped history** when memory becomes a
  concern. Unlikely near-term.
