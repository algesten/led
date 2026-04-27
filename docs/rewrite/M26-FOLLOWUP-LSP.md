# M26 follow-up — LSP `workspace/didChangeWatchedFiles`

> **Status (2026-04-27): SHIPPED.** Originally deferred when M26
> shipped its file-watch + cross-instance sync core; landed in
> the same commit window once the substrate stabilised. This
> doc has been refactored from "what to build" into a record of
> what's wired and where, for future readers tracing the LSP
> fan-out path.

This piece wires LSP servers' dynamic file-watch registrations
through the `driver-file-watch` infrastructure so
rust-analyzer (and other servers that rely on
`workspace/didChangeWatchedFiles`) stay current when project
files are edited outside led — closes the
rust-analyzer-goes-stale gap from `lsp-patterns.md` §7.2.

## Why it shipped late

None of the six M26-gated goldens exercise this path. Shipping
the M26 core without it left zero green-able goldens red, so
it slid out of the initial M26 cut to keep that PR focused.
The gap was real (rust-analyzer didn't see `Cargo.toml` edits
made by `cargo add` from a sibling shell) so the work landed
right after.

Tracked in:

- `ROADMAP.md` § "Orphan items worth a concrete home" —
  the named entry now points at this as-shipped doc.
- `MILESTONE-26.md` § In, "LSP `workspace/didChangeWatchedFiles`"
  block — design lives there, prefixed with a "SHIPPED" marker.
- `lsp-patterns.md` §7.2 — the original gap report (now closed).

## What's already shipped

The substrate this follow-up plugs into:

- `driver-file-watch/{core,native}` — emits
  [`FileWatchEvent::Changed { id, path, kinds }`] for any
  change under the workspace root (id `WATCHER_ID_ROOT`,
  recursive). Globbing happens runtime-side, against the LSP
  server's registered glob set.
- `driver-lsp/native/classify.rs:159` — routes
  `client/registerCapability` (and now also
  `client/unregisterCapability`) as a notification with
  `forward_as_notification: true` and auto-replies `null`. The
  payload is parsed by `handle_server_request` (see "How it's
  wired" below).
- `lsp_pending` outbox pattern — every other LSP request /
  notification (init, didOpen, didChange, didSave,
  pull-diagnostic, completion, …) is dispatched the same way:
  runtime drains pending vectors into `Vec<LspCmd>` each
  execute tick, calls `drivers.lsp.execute(cmds.iter())`.
- `runtime::Atoms.file_watch.recent_events` — per-tick
  `imbl::HashMap<WatcherId, Vector<FileWatchEvent>>`. Cleared
  at end of each execute phase.

## How it's wired

In dependency order, the four pieces that landed:

### 1. `state-lsp::LspWatchedGlobs` source

New struct, lives next to existing LSP state structs in
`crates/state-lsp/src/lib.rs`:

```rust
use std::sync::Arc;
use imbl::HashMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspWatchedGlobs {
    /// Per-server registrations. Replaced wholesale on each
    /// `client/registerCapability` notification — globs are
    /// immutable, a single Arc'd Vec per server is the
    /// natural cache-hit shape.
    pub by_server: HashMap<String, Arc<Vec<RegistrationGlob>>>,
}

#[derive(Debug, Clone)]
pub struct RegistrationGlob {
    /// Original glob pattern (kept so PartialEq is cheap).
    pub pattern: String,
    /// Compiled `globset::GlobMatcher`. Matching is alloc-free.
    pub matcher: globset::GlobMatcher,
    /// Bitset of `WatchKind` (Created | Changed | Deleted).
    /// LSP `WatchKind` defaults to all three when absent.
    pub kinds: u8,
}

impl PartialEq for RegistrationGlob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern && self.kinds == other.kinds
    }
}
```

`globset` is already a workspace dep (used by
`driver-file-search`). Add it to `state-lsp/Cargo.toml`.

### 2. Parse + emit in `driver-lsp/native`

Two extensions:

**`classify.rs`** — extend the `client/registerCapability` arm
(currently `crates/driver-lsp/native/src/classify.rs:159`) to
parse `params.registrations[]`. For each registration whose
`method` is `workspace/didChangeWatchedFiles`, parse
`registerOptions.watchers` (per LSP spec
`DidChangeWatchedFilesRegistrationOptions`). Compile each
glob via `globset::Glob::new(pattern)?.compile_matcher()`.

**`core/src/lib.rs`** — add two `LspEvent` variants and one
`LspCmd` variant:

```rust
// crates/driver-lsp/core/src/lib.rs — additions
pub enum LspEvent {
    // existing variants …
    WatchedFilesRegistered {
        server: String,
        registration_id: String,
        globs: Arc<Vec<RegistrationGlob>>,
    },
    WatchedFilesUnregistered {
        server: String,
        registration_id: String,
    },
}

pub enum LspCmd {
    // existing variants …
    DidChangeWatchedFiles {
        server: String,
        changes: Vec<FileEvent>,
    },
}

#[derive(Debug, Clone)]
pub struct FileEvent {
    pub uri: String,           // file:// URI, percent-encoded
    pub kind: FileEventKind,   // Created | Changed | Deleted
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}
```

Native side: `LspCmd::DidChangeWatchedFiles` formats as the
LSP notification with the standard `params: { changes: [{
uri, type }] }` payload and sends via the existing JSON-RPC
plumbing.

`RegistrationGlob` is owned by `state-lsp` per
`MILESTONE-26.md` D8 (parsed-state, not ABI-crossing); the
LSP core re-exports it through the `LspEvent` payload.

### 3. Runtime wiring

In `crates/runtime/src/lib.rs`:

**Atoms** — add the source:

```rust
pub struct Atoms {
    // existing fields …
    pub lsp_watched_globs: led_state_lsp::LspWatchedGlobs,
}
```

**Ingest** — handle the two new `LspEvent` variants. Same
spot where other `LspEvent` variants are processed (search
for `LspEvent::Diagnostics`); add arms:

```rust
LspEvent::WatchedFilesRegistered { server, registration_id, globs } => {
    // Append to the server's glob list. Multiple
    // registrations per server are valid (different globs
    // for different needs).
    let entry = lsp_watched_globs.by_server.entry(server).or_default();
    let mut updated = (**entry).clone();
    updated.push(/* glob with id baked in */);
    *entry = Arc::new(updated);
    // Better: maintain a sub-map by registration_id so
    // Unregister is O(1). Single-Vec is fine for M26-followup
    // since registrations are rare.
}
LspEvent::WatchedFilesUnregistered { server, registration_id } => {
    // Remove the matching entry by id.
}
```

**Memo** — `lsp_watched_file_notifications` in
`runtime/src/lib.rs` (or move to `query.rs` as a real memo
once the shape stabilises). Declare two new inputs:

```rust
#[derive(drv::Input)]
struct LspGlobsInput<'a> {
    pub by_server: &'a imbl::HashMap<String, Arc<Vec<RegistrationGlob>>>,
}
impl<'a> LspGlobsInput<'a> {
    pub fn new(g: &'a LspWatchedGlobs) -> Self {
        Self { by_server: &g.by_server }
    }
}

#[derive(drv::Input)]
struct FileWatchEventsInput<'a> { /* see existing in M26 */ }
```

Then the dispatch helper (imperative for now, drv-memo later):

```rust
fn compute_lsp_watched_file_notifications(
    file_watch: &FileWatchState,
    globs: &LspWatchedGlobs,
) -> Vec<LspCmd> {
    use led_driver_file_watch_core::FileWatchEvent;
    let Some(queue) = file_watch.recent_events.get(&WATCHER_ID_ROOT) else {
        return Vec::new();
    };
    let mut per_server: HashMap<String, Vec<FileEvent>> = HashMap::new();
    for ev in queue {
        let FileWatchEvent::Changed { path, kinds, .. } = ev else { continue };
        let kind = file_event_kind_from_change_kinds(*kinds);
        let uri = path_to_file_uri(path);
        for (server, glob_list) in globs.by_server.iter() {
            for g in glob_list.iter() {
                if g.matcher.is_match(path.as_path())
                    && g.kinds & (kind as u8) != 0
                {
                    per_server.entry(server.clone()).or_default().push(FileEvent {
                        uri: uri.clone(),
                        kind,
                    });
                    break;
                }
            }
        }
    }
    per_server.into_iter()
        .map(|(server, changes)| LspCmd::DidChangeWatchedFiles { server, changes })
        .collect()
}
```

**Execute phase** — call this in the same ingest block where
M26's other event-fan-out helpers run (the `if !no_workspace
&& session.init_done && fs.root.is_some()` block in
`runtime/src/lib.rs` — search for `compute_external_reread_targets`
to find it). Add:

```rust
let lsp_notifs = compute_lsp_watched_file_notifications(
    file_watch,
    lsp_watched_globs,
);
if !lsp_notifs.is_empty() {
    drivers.lsp.execute(lsp_notifs.iter());
}
```

### 4. Trace + golden

**Trace** — new method on the runtime's `Trace` trait
(`crates/runtime/src/trace.rs`):

```rust
fn lsp_did_change_watched_files(&self, server: &str, n_changes: usize);
```

`FileTrace::lsp_did_change_watched_files`:
```rust
self.write_line(&format!(
    "LspDidChangeWatchedFiles\tserver={} changes={}",
    server, n_changes,
));
```

**Golden** — author a new scenario:

```
goldens/scenarios/features/lsp/did_change_watched_files/
  setup.toml         git_init=true; rust-analyzer fake LSP that
                     registers a `**/*.toml` watcher in
                     post-initialize.
  script.txt         wait 1s
                     fs_write Cargo.toml ...new contents...
                     wait 2s
  dispatched.snap    expects
                     LspDidChangeWatchedFiles\tserver=... changes=1
                     after the touch.
  frame.snap         (likely no visible UI change)
```

The fake-lsp harness already supports scripted responses; add
a `client/registerCapability` request to its scripted
sequence after the initialize round-trip.

## Order of operations

1. `state-lsp::LspWatchedGlobs` + `RegistrationGlob`. No
   dependencies — land first.
2. `LspEvent::WatchedFilesRegistered/Unregistered`,
   `LspCmd::DidChangeWatchedFiles`, `FileEvent`,
   `FileEventKind`. Pure ABI extension.
3. `driver-lsp/native/classify.rs` parser + emission.
4. Runtime: ingest arm, atom field, dispatch helper, execute
   wiring, trace.
5. Author the golden against the rewrite (no `main`-first
   step needed: legacy didn't trace this notification, so
   there's no legacy capture to reconcile against — author
   directly on `rewrite` per
   `MILESTONE-26.md` D11-style discipline).
6. Verify rust-analyzer interactively: `cargo run -p led
   -- src/main.rs` in a workspace with rust-analyzer
   configured; from another shell `cargo add anyhow`; within
   a few seconds led's diagnostics should reflect the new
   dependency without the manual reload that's currently
   needed.

## Architecture conformance

This follow-up follows the same `EXAMPLE-ARCH.md` discipline
M26 audited against:

- **G1** — `LspWatchedGlobs` is an external-fact source
  (server told us) co-located with other LSP server state in
  `state-lsp`. Not on the runtime's bag of atoms.
- **G2** — `compute_lsp_watched_file_notifications` is a
  pure desired-state function: "what notifications should
  fire this tick?". Not a transition handler.
- **G8** — `driver-lsp/core` doesn't import
  `driver-file-watch-core`. Cross-driver coupling stays
  one-way: file-watch driver → runtime (event drain) →
  runtime memo → LSP driver (cmd dispatch).
- **G11** — `LspGlobsInput` declared in `runtime/` (the
  consumer crate). `state-lsp` carries no `drv` dep.
- **G13** — Cmd/Event types in `driver-lsp/core` (ABI in
  driver core); `RegistrationGlob` in `state-lsp` (parsed
  state, not ABI-crossing).
- **G14** — `LspWatchedGlobs.by_server` is
  `imbl::HashMap<String, Arc<Vec<...>>>`; idle ticks are
  pointer-equal clones. Match loop runs only when
  `file_watch.recent_events` for `WATCHER_ID_ROOT` is
  non-empty.

## Out (still)

- **LSP file-watch payload batching across ticks** — current
  shape sends one notification per tick of new events.
  Could batch across short windows. Performance fine for
  typical edit rates; revisit if a server complains.
- **`workspace/didChangeWorkspaceFolders`** — separate LSP
  capability, separate registration. Out of scope here.
- **Per-glob `kind` filtering** — the `kinds` mask above
  honours the registered `WatchKind`. The LSP spec allows
  servers to register narrower scopes (e.g. Created-only);
  the matcher above does the right thing. No additional UI
  needed.

## Hand-off pointers

If you're picking this up cold:

1. Read `MILESTONE-26.md` first (whole doc) — that's the
   shipped half. The "DEFERRED" block in § In points back
   to this doc.
2. Read `lsp-patterns.md` §7.2 for the original
   problem statement.
3. Read `crates/driver-lsp/native/src/classify.rs:140-180`
   for the existing `client/registerCapability` plumbing.
4. Read `crates/runtime/src/lib.rs` around the
   `compute_external_reread_targets` call (search for that
   string) — that's where the new dispatch helper plugs in.
5. Workspace deps already include `globset = "0.4"` (used
   by `driver-file-search`); no Cargo.toml workspace edits
   needed.
