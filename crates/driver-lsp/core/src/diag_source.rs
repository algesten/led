//! Push-vs-pull diagnostic delivery, window lifecycle, freeze
//! discipline.
//!
//! # Why this exists
//!
//! LSP servers deliver diagnostics two ways: unsolicited pushes
//! via `textDocument/publishDiagnostics`, and synchronous pulls
//! via `textDocument/diagnostic`. Each server advertises one
//! mode (or both) in its `initialize` capabilities.
//!
//! Per the project rule "Pull diagnostics only" (user memory) and
//! audit Theme L Target C: **pull is primary, push is a fallback
//! cache**. The runtime sees pull responses as `LspEvent::Diagnostics`
//! and cached pushes as `LspEvent::PushFallback`; the runtime fold
//! treats `PushFallback` as lower priority than `Diagnostics`.
//!
//! # Mode selection
//!
//! - **Default: pull.** Servers that advertise `diagnosticProvider`
//!   stay in pull mode. The very common case (rust-analyzer,
//!   gopls, clangd, recent typescript-language-server) is covered
//!   without any explicit set_mode call.
//! - **Push-only fallback:** A server that does not advertise pull
//!   capability gets `set_mode(DiagMode::Push)` from the lifecycle
//!   layer after `initialize` — its `publishDiagnostics` pushes are
//!   the *sole* source of diagnostics, surfaced through the same
//!   `PushFallback` channel.
//! - **No more pull→push downgrade.** An unsolicited push arriving
//!   in pull mode no longer flips the server permanently to push.
//!   The push goes into the fallback cache and is only surfaced if
//!   no pull result has landed for that path.
//!
//! # Window lifecycle
//!
//! A "propagation window" is the conceptual span during which one
//! `RequestDiagnostics` is being serviced. Push mode: window opens
//! immediately, cached pushes surface as `PushFallback`, window
//! stays open until an edit closes it. Pull mode: window opens
//! frozen, pulls fly out to every opened buffer, close when all
//! return (or the 5s deadline expires).
//!
//! While a pull window is frozen, the driver's command channel is
//! not read — edits queue. This is the mechanism that keeps
//! "diagnostics fire on save not on keystroke" emergent: under
//! typing, `RequestDiagnostics` events repeatedly fire, but their
//! windows either freeze briefly then get invalidated by the next
//! edit (before sending any server-level pull) or coalesce behind
//! the freeze.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use led_core::{CanonPath, PersistedContentHash};
use led_state_diagnostics::Diagnostic;

/// Hard ceiling on how long a pull window stays frozen. Once
/// reached, the freeze lifts unconditionally and any in-flight
/// pulls that return afterwards fall on the floor. Matches legacy
/// `manager.rs:194`.
const PULL_FREEZE_DEADLINE: Duration = Duration::from_secs(5);

/// Delivery mode, decided once per-server from its
/// `initialize` capabilities. Pull is the default; servers that
/// don't advertise pull are downgraded to Push by the lifecycle
/// layer. There is no longer a runtime mode flip in either
/// direction — mode is latched at initialize time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagMode {
    /// Server only supports push via `publishDiagnostics`. Pushes
    /// are the sole diagnostic source for this server, surfaced
    /// through the same `PushFallback` channel that pull-capable
    /// servers use for stale pre-pull cache.
    Push,
    /// Server supports pull via `textDocument/diagnostic`. Pull is
    /// the primary event channel; pushes are fallback cache.
    Pull,
}

/// What the caller should do after `on_push`.
///
/// Push is a fallback cache. The driver never forwards a push as
/// `LspEvent::Diagnostics` (the pull-primary channel); pushes
/// either land as low-priority `LspEvent::PushFallback`, clear
/// existing diagnostics for empty payloads, or get dropped.
pub enum DiagPushResult {
    /// Push entered the fallback cache. Caller should emit
    /// `LspEvent::PushFallback { path, hash, diagnostics }`.
    /// The runtime fold accepts this only when no pull has
    /// answered for the path (gating via `LspState.pull_state`).
    CacheFallback(CanonPath, Vec<Diagnostic>, PersistedContentHash),
    /// Clearing push (empty list). Forward as a clearing
    /// `LspEvent::PushFallback` so the runtime can drop any cached
    /// fallback entry for the path — clearing is safe under both
    /// modes (the runtime's hash-match / replay gate still
    /// applies). Caller supplies the buffer's current hash.
    ForwardClearing(CanonPath),
    /// Push arrived while a pull is currently in flight for this
    /// path. The push is dropped — pull will answer authoritatively
    /// in a moment and we don't want a stale push to race ahead.
    Drop,
    /// Push arrived but the cache was already up-to-date with this
    /// payload, or there's nothing actionable. No event needed.
    Ignore,
}

/// One open propagation window's in-flight state. Closed = `None`
/// on the parent `DiagnosticSource`.
struct DiagWindow {
    /// Content-hash snapshot for every opened doc at window open
    /// time. Every forwarded `Diagnostics` event is stamped with
    /// the matching entry so the model can content-hash-gate /
    /// replay.
    hash_snapshot: HashMap<CanonPath, PersistedContentHash>,
    /// Pull mode only: paths still awaiting their pull response.
    /// Populated at open time, drained in `on_pull_response`.
    pending_pulls: HashSet<CanonPath>,
    /// Pull mode only: whether the freeze is still active.
    /// Cleared by `on_pull_response` once every path has been
    /// answered, or by `cancel_freeze` on timeout.
    frozen: bool,
    /// Pull mode only: instant at which the freeze must lift no
    /// matter what. `None` in push mode.
    deadline: Option<Instant>,
}

/// Normaliser for push/pull delivery semantics. Owned by the
/// LSP driver's native event loop; one instance per spawned
/// server.
pub struct DiagnosticSource {
    mode: DiagMode,

    /// Whether the server exposes `experimental/serverStatus`
    /// quiescence events (rust-analyzer does; most don't).
    has_quiescence: bool,

    /// `true` once the server has reported `quiescent=true` at
    /// least once. Servers without quiescence support start in
    /// the `true` state because there's nothing to wait for.
    lsp_ready: bool,

    /// A `RequestDiagnostics` arrived before first quiescence.
    /// Replayed when `on_quiescence` finally fires.
    init_delayed_request: bool,

    /// Fallback cache populated by push events. Pull responses
    /// always take priority; entries here are only surfaced when
    /// the runtime's pull state for a path is absent or failed.
    ///
    /// Each entry carries the buffer content hash the push was
    /// computed against — downstream replay needs the original
    /// stamp so a late push doesn't get pinned to content the
    /// server never analysed.
    push_cache: HashMap<CanonPath, (PersistedContentHash, Vec<Diagnostic>)>,

    /// Open propagation window, or `None` when idle.
    window: Option<DiagWindow>,
}

impl Default for DiagnosticSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticSource {
    pub fn new() -> Self {
        Self {
            // Default Pull per audit Theme L Target C. Push is a
            // fallback for servers that explicitly don't advertise
            // pull — the lifecycle layer downgrades them after
            // `initialize` with `set_mode(DiagMode::Push)`.
            mode: DiagMode::Pull,
            has_quiescence: false,
            // Default-ready: servers without quiescence support
            // are considered ready the moment their `initialize`
            // response comes back.
            lsp_ready: true,
            init_delayed_request: false,
            push_cache: HashMap::new(),
            window: None,
        }
    }

    // ── Capability / readiness ───────────────────────────────

    /// Called once on server startup after the `initialize`
    /// response. Sets the delivery mode for the server's lifetime.
    /// No mode flips after this — push events stay fallback-only
    /// in pull mode, pull-incapable servers stay Push.
    pub fn set_mode(&mut self, mode: DiagMode) {
        self.mode = mode;
    }

    /// Server advertised `experimental/serverStatus`. Until the
    /// first `on_quiescence` fires, any `RequestDiagnostics` is
    /// deferred (stashed in `init_delayed_request`).
    pub fn set_has_quiescence(&mut self, has: bool) {
        self.has_quiescence = has;
        if has {
            // A quiescence-gated server is NOT ready by default;
            // it must emit `quiescent=true` first.
            self.lsp_ready = false;
        }
    }

    /// Accessor so callers can tell "have we already latched the
    /// server into quiescence-gated mode?" Used by the manager to
    /// avoid re-flipping `lsp_ready` on every serverStatus
    /// notification — only the first arrival should matter.
    pub fn has_quiescence(&self) -> bool {
        self.has_quiescence
    }

    /// Return `true` if the next `RequestDiagnostics` should be
    /// deferred until the server is ready. Only ever `true` for
    /// quiescence-gated servers before their first quiescent event.
    pub fn should_defer_request(&self) -> bool {
        !self.lsp_ready
    }

    /// Remember that a `RequestDiagnostics` arrived while not
    /// ready; `on_quiescence` will re-fire it.
    pub fn defer_init_request(&mut self) {
        self.init_delayed_request = true;
    }

    /// Called when `experimental/serverStatus quiescent=true`
    /// arrives. Returns `true` if a deferred init request should
    /// now be fulfilled.
    pub fn on_quiescence(&mut self) -> bool {
        self.lsp_ready = true;
        if self.init_delayed_request {
            self.init_delayed_request = false;
            true
        } else {
            false
        }
    }

    // ── Freeze / deadline ────────────────────────────────────

    /// Whether the native side should pause reading its command
    /// channel. Only the pull-mode frozen-window case returns
    /// `true`; push mode never freezes.
    pub fn is_frozen(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.frozen)
    }

    /// Instant the current freeze must lift by, if any. Native
    /// side uses this in `tokio::select!` to wake when the
    /// deadline fires.
    pub fn deadline(&self) -> Option<Instant> {
        self.window.as_ref().and_then(|w| w.deadline)
    }

    pub fn mode(&self) -> DiagMode {
        self.mode
    }

    /// Is a pull currently in flight for `path`? Used by `on_push`
    /// to decide whether to drop the push (pull will answer
    /// authoritatively) or cache it (no pull, push is the only
    /// data we have).
    fn pull_in_flight(&self, path: &CanonPath) -> bool {
        self.window
            .as_ref()
            .is_some_and(|w| w.pending_pulls.contains(path))
    }

    // ── Window open / close ──────────────────────────────────

    /// Open a propagation window. `hash_snapshot` must cover every
    /// currently-opened buffer keyed by canonical path. `opened` is
    /// just the set (same keys) — kept separate because pull mode
    /// iterates it to decide which paths need pulling; push mode
    /// iterates to decide which lack cache.
    ///
    /// Returns the paths to pull. Empty in push mode (push-only
    /// servers have no pull capability — their diagnostics flow
    /// through `on_push` → cache → `PushFallback`).
    pub fn open_window(
        &mut self,
        hash_snapshot: HashMap<CanonPath, PersistedContentHash>,
        opened: &HashSet<CanonPath>,
        now: Instant,
    ) -> Vec<CanonPath> {
        match self.mode {
            DiagMode::Push => {
                // Push-only servers don't pull. The window exists
                // so cached pushes can drain through
                // `drain_cache_for_window`, but no pulls fire and
                // the freeze never engages.
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: HashSet::new(),
                    frozen: false,
                    deadline: None,
                });
                Vec::new()
            }
            DiagMode::Pull => {
                let pull_paths: Vec<CanonPath> = opened.iter().cloned().collect();
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: true,
                    deadline: Some(now + PULL_FREEZE_DEADLINE),
                });
                pull_paths
            }
        }
    }

    /// Drain the push fallback cache through the newly-opened
    /// window. Each entry keeps its ORIGINAL content-hash stamp
    /// (the hash the buffer held when the push landed); the
    /// runtime's `offer_diagnostics` runs the fast-path /
    /// save-point-replay pipeline against that hash, so stamping
    /// with the window's current snapshot would pin a stale
    /// cargo-check push to content the server never analysed.
    ///
    /// Called by both push-mode and pull-mode windows: in push
    /// mode this is the only diagnostic source for the window; in
    /// pull mode it lets the runtime see fallback data for paths
    /// whose pull hasn't answered yet (the runtime fold gates on
    /// `pull_state`).
    pub fn drain_cache_for_window(
        &self,
    ) -> Vec<(CanonPath, Vec<Diagnostic>, PersistedContentHash)> {
        if self.window.is_none() {
            return Vec::new();
        }
        self.push_cache
            .iter()
            .map(|(path, (hash, diags))| (path.clone(), diags.clone(), *hash))
            .collect()
    }

    /// Close the window. Push cache is preserved (cache survives
    /// across windows so a future `RequestDiagnostics` re-drains
    /// it for paths whose pull hasn't answered).
    pub fn close_window(&mut self) {
        self.window = None;
    }

    /// Called by the native side when a `BufferChanged` arrives
    /// and the buffer's content hash no longer matches the
    /// window's snapshot for that path. Content-hash-based so a
    /// type-then-delete round trip back to the original bytes
    /// does NOT close the window.
    pub fn should_close_window(
        &self,
        path: &CanonPath,
        current: PersistedContentHash,
    ) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        let Some(snap) = window.hash_snapshot.get(path) else {
            return false;
        };
        snap.0 != current.0
    }

    /// Lift the freeze immediately (used by the native side when
    /// `deadline()` fires). The window stays open for late pull
    /// results to still slip through; they just fall on the floor
    /// since `pending_pulls` is cleared.
    pub fn cancel_freeze(&mut self) {
        if let Some(window) = &mut self.window {
            window.frozen = false;
            window.deadline = None;
            window.pending_pulls.clear();
        }
    }

    // ── Incoming diagnostic events ───────────────────────────

    /// A `publishDiagnostics` push arrived. `current_hash` is the
    /// buffer's content hash at the moment the push landed — we
    /// stamp the cache entry with it, and forward with it, so
    /// downstream replay can map the diagnostic back through any
    /// subsequent edits.
    ///
    /// Decision tree:
    /// 1. Empty list (clearing) → drop cache entry, forward
    ///    `ForwardClearing` so the runtime can drop its fallback.
    /// 2. Pull is currently in flight for this path → `Drop` (pull
    ///    will answer authoritatively).
    /// 3. Otherwise → write to fallback cache, emit
    ///    `CacheFallback` so the runtime can surface it as a
    ///    low-priority `LspEvent::PushFallback`.
    pub fn on_push(
        &mut self,
        path: CanonPath,
        diags: Vec<Diagnostic>,
        current_hash: PersistedContentHash,
    ) -> DiagPushResult {
        if diags.is_empty() {
            // Clearing push: drop any cached fallback for this
            // path and let the runtime clear too. Safe in both
            // modes — clearing carries no stale-content risk.
            self.push_cache.remove(&path);
            return DiagPushResult::ForwardClearing(path);
        }
        if self.pull_in_flight(&path) {
            // Pull is the primary channel and it will answer for
            // this path. Drop the push to avoid racing it.
            return DiagPushResult::Drop;
        }
        // Cache and surface as a fallback. The runtime fold gates
        // acceptance on `pull_state` — in pull mode this fallback
        // is dropped once a pull lands, while in push mode (no
        // pull responses) the fallback IS the diagnostic source.
        self.push_cache
            .insert(path.clone(), (current_hash, diags.clone()));
        DiagPushResult::CacheFallback(path, diags, current_hash)
    }

    /// A pull response arrived. Removes `path` from
    /// `pending_pulls`; if that was the last pending path, lifts
    /// the freeze. Pull responses are authoritative — the
    /// fallback cache for this path is dropped (the runtime's
    /// `LspEvent::Diagnostics` supersedes any prior `PushFallback`).
    ///
    /// Returns `(maybe_forward, all_pulls_done)`. `maybe_forward`
    /// is `None` when the response targets a path we no longer
    /// expect (already answered, or unknown).
    pub fn on_pull_response(
        &mut self,
        path: CanonPath,
        pull_diags: Vec<Diagnostic>,
    ) -> (
        Option<(CanonPath, Vec<Diagnostic>, PersistedContentHash)>,
        bool,
    ) {
        let Some(window) = &mut self.window else {
            return (None, false);
        };
        if !window.pending_pulls.remove(&path) {
            // Either not expecting this path, or already answered.
            return (None, false);
        }
        let h = window
            .hash_snapshot
            .get(&path)
            .copied()
            .unwrap_or_default();
        let all_done = window.pending_pulls.is_empty();
        if all_done {
            window.frozen = false;
            window.deadline = None;
        }

        // Pull is primary — the cached push for this path is
        // superseded. Drop it so a subsequent window doesn't drain
        // stale fallback that the pull already answered.
        self.push_cache.remove(&path);

        (Some((path, pull_diags, h)), all_done)
    }

    /// Drop the push cache entry for a path. Called when the
    /// buffer closes, or when the runtime detects the cache has
    /// diverged from any reachable buffer state.
    pub fn invalidate_cache(&mut self, path: &CanonPath) {
        self.push_cache.remove(path);
    }

    // ── Introspection (for tests + native event loop) ────────

    #[cfg(test)]
    fn has_window(&self) -> bool {
        self.window.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_state_diagnostics::DiagnosticSeverity;

    fn p(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn snap(paths: &[(&str, u64)]) -> HashMap<CanonPath, PersistedContentHash> {
        paths
            .iter()
            .map(|(s, v)| (p(s), PersistedContentHash(*v)))
            .collect()
    }

    fn opened(paths: &[&str]) -> HashSet<CanonPath> {
        paths.iter().map(|s| p(s)).collect()
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn diag(msg: &str) -> Diagnostic {
        Diagnostic {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 5,
            severity: DiagnosticSeverity::Error,
            message: msg.to_string(),
            source: None,
            code: None,
        }
    }

    fn push_source() -> DiagnosticSource {
        let mut ds = DiagnosticSource::new();
        ds.set_mode(DiagMode::Push);
        ds
    }

    fn pull_source() -> DiagnosticSource {
        let mut ds = DiagnosticSource::new();
        ds.set_mode(DiagMode::Pull);
        ds
    }

    // ── Push mode ───────────────────────────────────────────

    #[test]
    fn push_always_caches() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        assert_eq!(ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message, "err");
    }

    #[test]
    fn push_cache_updated_by_new_push() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("old")], PersistedContentHash(7));
        ds.on_push(p("/a.rs"), vec![diag("new")], PersistedContentHash(7));
        assert_eq!(ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message, "new");
    }

    #[test]
    fn empty_push_clears_cache_entry() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        ds.on_push(p("/a.rs"), vec![], PersistedContentHash(0));
        assert!(
            !ds.push_cache.contains_key(&p("/a.rs")),
            "clearing push drops the cache entry"
        );
    }

    #[test]
    fn push_clearing_forwards_clearing() {
        let mut ds = push_source();
        let r = ds.on_push(p("/a.rs"), vec![], PersistedContentHash(0));
        match r {
            DiagPushResult::ForwardClearing(path) => {
                assert_eq!(path, p("/a.rs"));
            }
            _ => panic!("expected ForwardClearing"),
        }
    }

    #[test]
    fn push_non_clearing_emits_cache_fallback() {
        let mut ds = push_source();
        let r = ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        match r {
            DiagPushResult::CacheFallback(path, diags, hash) => {
                assert_eq!(path, p("/a.rs"));
                assert_eq!(diags.len(), 1);
                assert_eq!(hash, PersistedContentHash(7));
            }
            _ => panic!("expected CacheFallback"),
        }
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message,
            "err",
            "push is also stored in fallback cache",
        );
    }

    #[test]
    fn push_window_drains_cache_with_original_push_hash() {
        // Cache entries keep the hash from their push-time call;
        // drain reports that (not the new window's snapshot), so
        // offer_diagnostics downstream can replay or reject
        // against the hash the server actually saw.
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")], PersistedContentHash(7));
        ds.open_window(snap(&[("/a.rs", 11)]), &opened(&["/a.rs"]), now());
        let drained = ds.drain_cache_for_window();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1[0].message, "cached");
        assert_eq!(drained[0].2, PersistedContentHash(7));
    }

    #[test]
    fn push_cache_survives_window_close() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")], PersistedContentHash(7));
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        ds.close_window();
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message,
            "cached"
        );
    }

    #[test]
    fn push_window_not_frozen() {
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        assert!(!ds.is_frozen());
    }

    #[test]
    fn push_window_issues_no_pulls() {
        // Push-only servers have no pull capability; the window
        // exists only to anchor the snapshot for `should_close_window`.
        let mut ds = push_source();
        let pulls = ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        assert!(pulls.is_empty());
    }

    #[test]
    fn should_close_window_fires_on_version_movement() {
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 4)]), &opened(&["/a.rs"]), now());
        assert!(!ds.should_close_window(&p("/a.rs"), PersistedContentHash(4)));
        assert!(ds.should_close_window(&p("/a.rs"), PersistedContentHash(5)));
    }

    // ── Pull mode ───────────────────────────────────────────

    #[test]
    fn pull_window_is_frozen_and_has_deadline() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        assert!(ds.is_frozen());
        assert!(ds.deadline().is_some());
    }

    #[test]
    fn pull_window_returns_all_opened_paths() {
        let mut ds = pull_source();
        let pulls = ds.open_window(
            snap(&[("/a.rs", 1), ("/b.rs", 2)]),
            &opened(&["/a.rs", "/b.rs"]),
            now(),
        );
        assert_eq!(pulls.len(), 2);
    }

    #[test]
    fn pull_response_forwards_with_snapshot_version() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 7)]), &opened(&["/a.rs"]), now());
        let (out, all_done) = ds.on_pull_response(p("/a.rs"), vec![diag("pulled")]);
        let (path, diags, v) = out.expect("forward");
        assert_eq!(path, p("/a.rs"));
        assert_eq!(diags.len(), 1);
        assert_eq!(v, PersistedContentHash(7));
        assert!(all_done);
        assert!(!ds.is_frozen());
    }

    #[test]
    fn pull_unfreezes_only_when_all_pending_returned() {
        let mut ds = pull_source();
        ds.open_window(
            snap(&[("/a.rs", 1), ("/b.rs", 1)]),
            &opened(&["/a.rs", "/b.rs"]),
            now(),
        );
        assert!(ds.is_frozen());
        let (_, done) = ds.on_pull_response(p("/a.rs"), vec![]);
        assert!(!done);
        assert!(ds.is_frozen());
        let (_, done) = ds.on_pull_response(p("/b.rs"), vec![]);
        assert!(done);
        assert!(!ds.is_frozen());
    }

    #[test]
    fn pull_cancel_freeze_lifts_freeze_keeps_window() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        assert!(ds.is_frozen());
        ds.cancel_freeze();
        assert!(!ds.is_frozen());
        assert!(ds.has_window(), "window stays open for late results");
    }

    #[test]
    fn pull_response_for_unknown_path_is_dropped() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        let (out, done) = ds.on_pull_response(p("/ghost.rs"), vec![diag("stray")]);
        assert!(out.is_none());
        assert!(!done);
        assert!(ds.is_frozen(), "unknown path doesn't close the pending set");
    }

    #[test]
    fn pull_response_wins_over_cached_push() {
        // Pull is primary per audit Theme L Target C. The cached
        // push is dropped on a pull response — pull's authoritative
        // answer supersedes any prior fallback.
        let mut ds = pull_source();
        ds.push_cache.insert(
            p("/a.rs"),
            (PersistedContentHash(0), vec![diag("from_push")]),
        );
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        let (out, _) = ds.on_pull_response(p("/a.rs"), vec![diag("from_pull")]);
        assert_eq!(out.unwrap().1[0].message, "from_pull");
        assert!(
            !ds.push_cache.contains_key(&p("/a.rs")),
            "cached push superseded by pull",
        );
    }

    #[test]
    fn push_dropped_while_pull_in_flight_for_same_path() {
        // Pull is primary; a push that races a pending pull is
        // dropped so it doesn't smear stale diagnostics ahead of
        // the authoritative pull response.
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        let r = ds.on_push(p("/a.rs"), vec![diag("race")], PersistedContentHash(7));
        assert!(matches!(r, DiagPushResult::Drop));
        assert!(
            !ds.push_cache.contains_key(&p("/a.rs")),
            "raced push not cached",
        );
    }

    #[test]
    fn push_during_pull_for_different_path_is_cached() {
        // Pull pending for path A; push arrives for path B (no
        // pull in flight for B). Cache + emit fallback so the
        // runtime can show data for B.
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]), now());
        let r = ds.on_push(p("/b.rs"), vec![diag("b_push")], PersistedContentHash(2));
        assert!(matches!(r, DiagPushResult::CacheFallback(_, _, _)));
        assert!(ds.push_cache.contains_key(&p("/b.rs")));
    }

    #[test]
    fn push_in_pull_mode_does_not_flip_mode() {
        // Per audit Theme L Target C: unsolicited pushes in pull
        // mode no longer cause a permanent downgrade. The push is
        // cached and the server stays in pull mode forever.
        let mut ds = pull_source();
        ds.on_push(p("/a.rs"), vec![diag("pushed")], PersistedContentHash(7));
        assert_eq!(ds.mode(), DiagMode::Pull, "mode unchanged");
    }

    // ── Default mode ───────────────────────────────────────

    #[test]
    fn default_mode_is_pull() {
        assert_eq!(DiagnosticSource::new().mode, DiagMode::Pull);
    }

    // ── Quiescence gate ────────────────────────────────────

    #[test]
    fn non_quiescence_server_is_ready_immediately() {
        let ds = DiagnosticSource::new();
        assert!(!ds.should_defer_request());
    }

    #[test]
    fn quiescence_server_defers_until_first_quiescent() {
        let mut ds = DiagnosticSource::new();
        ds.set_has_quiescence(true);
        assert!(ds.should_defer_request());
        ds.defer_init_request();
        let fire = ds.on_quiescence();
        assert!(fire, "deferred request replays");
        assert!(!ds.should_defer_request());
    }

    #[test]
    fn quiescence_with_no_pending_request_is_noop() {
        let mut ds = DiagnosticSource::new();
        ds.set_has_quiescence(true);
        let fire = ds.on_quiescence();
        assert!(!fire);
        assert!(!ds.should_defer_request());
    }

    // ── Cache invalidation ────────────────────────────────

    #[test]
    fn invalidate_cache_drops_entry() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        ds.invalidate_cache(&p("/a.rs"));
        assert!(!ds.push_cache.contains_key(&p("/a.rs")));
    }
}
