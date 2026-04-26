//! Push-vs-pull diagnostic delivery, window lifecycle, freeze
//! discipline. Direct port of legacy led's `DiagnosticSource`
//! (`crates/lsp/src/manager.rs:42-367` on main).
//!
//! # Why this exists
//!
//! LSP servers deliver diagnostics two ways: unsolicited pushes
//! via `textDocument/publishDiagnostics`, and synchronous pulls
//! via `textDocument/diagnostic`. Each server advertises one
//! mode (or both) in its `initialize` capabilities. The model
//! only ever sees **one** story: a `Diagnostics` event stamped
//! with the buffer version the server was reasoning about. This
//! state machine is what turns the two protocol flavours into
//! that one story.
//!
//! # Mode selection
//!
//! Legacy's rule, ported unchanged:
//!
//! - **Default: push.** A server that only supports push stays in
//!   push mode forever.
//! - **Pull opt-in:** If the server advertises `diagnosticProvider`,
//!   we enter pull mode on startup. Pull mode freezes the command
//!   queue on each window to avoid interleaving edits with pull
//!   results from the wrong buffer version.
//! - **Fallback: pull → push, one-way.** If a server sends an
//!   unsolicited `publishDiagnostics` while we're in pull mode, we
//!   switch to push permanently. A push-first server that later
//!   advertises pull does **not** get demoted back.
//!
//! # Window lifecycle
//!
//! A "propagation window" is the conceptual span during which one
//! `RequestDiagnostics` is being serviced. Push mode: window opens
//! immediately, cached pushes forward, window stays open until an
//! edit closes it. Pull mode: window opens frozen, pulls fly out
//! to every opened buffer, close when all return (or the 5s
//! deadline expires).
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
/// `initialize` capabilities. Can fall back pull → push at
/// runtime; never push → pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagMode {
    /// Server pushes diagnostics via `publishDiagnostics`.
    /// Default until server capabilities prove otherwise.
    Push,
    /// Server supports pull via `textDocument/diagnostic`.
    Pull,
}

/// What the caller should do after `on_push`.
pub enum DiagPushResult {
    /// Forward this diagnostic to the model, stamped with the
    /// window's snapshot version for the path. Legacy's "late
    /// cargo push" path goes through here: the window stays
    /// open after pulls complete (only closed by actual content
    /// divergence), so pushes arriving minutes later still hit
    /// `Forward` and get the correct snapshot stamp.
    Forward(CanonPath, Vec<Diagnostic>, PersistedContentHash),
    /// Clearing push (empty list) arrived outside any window.
    /// Forward with the CURRENT buffer version the caller
    /// supplies — clearing is always safe to propagate (legacy
    /// `on_push` lines 263-268).
    ForwardClearing(CanonPath),
    /// Pull-mode server sent an unsolicited push. We've
    /// switched permanently to push; caller re-issues a
    /// `RequestDiagnostics` so a push-mode window opens and
    /// drains the cache (includes the push that just triggered
    /// this).
    RestartWindow,
    /// Nothing to do: non-clearing push outside a window. The
    /// diagnostic is cached for the next window open. Matches
    /// legacy's conservative behaviour — stamping a late push
    /// with the current version when the user has already
    /// edited past the snapshot would smear the diagnostic onto
    /// the wrong lines.
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

    /// True once the server's capabilities advertised pull
    /// support (whether or not we're currently in pull mode — a
    /// pull → push fallback leaves this set so push-mode windows
    /// can still use pulls as validation for paths without cache).
    has_pull_capability: bool,

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

    /// Latest push-delivered diagnostics per path, paired with the
    /// buffer content hash they were computed against. Drain keeps
    /// the original stamp so the runtime can replay through later
    /// edits; re-stamping with a newer snapshot would pin the
    /// diagnostic to content the server never analysed — exactly
    /// the bug that lets late cargo-check pushes smear the old
    /// error onto the current buffer.
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
            mode: DiagMode::Push,
            has_pull_capability: false,
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
    /// response. Sets the initial delivery mode; also latches
    /// `has_pull_capability` when mode is Pull.
    pub fn set_mode(&mut self, mode: DiagMode) {
        self.mode = mode;
        if mode == DiagMode::Pull {
            self.has_pull_capability = true;
        }
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

    // ── Window open / close ──────────────────────────────────

    /// Open a propagation window. `hash_snapshot` must cover every
    /// currently-opened buffer keyed by canonical path. `opened` is
    /// just the set (same keys) — kept separate because pull mode
    /// iterates it to decide which paths need pulling; push mode
    /// iterates to decide which lack cache.
    ///
    /// Returns the paths to pull. Empty in push mode unless the
    /// server advertised pull capability (then it's just the
    /// paths without a push-cache hit).
    pub fn open_window(
        &mut self,
        hash_snapshot: HashMap<CanonPath, PersistedContentHash>,
        opened: &HashSet<CanonPath>,
    ) -> Vec<CanonPath> {
        match self.mode {
            DiagMode::Push => {
                // Push mode never freezes. Pull only for paths
                // without cached results, and only if the server
                // advertised pull capability.
                let pull_paths: Vec<CanonPath> = if self.has_pull_capability {
                    opened
                        .iter()
                        .filter(|p| !self.push_cache.contains_key(*p))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: false,
                    deadline: None,
                });
                pull_paths
            }
            DiagMode::Pull => {
                let pull_paths: Vec<CanonPath> = opened.iter().cloned().collect();
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: true,
                    deadline: Some(Instant::now() + PULL_FREEZE_DEADLINE),
                });
                pull_paths
            }
        }
    }

    /// Push mode: drain the current push cache through the
    /// newly-opened window, keeping each entry's ORIGINAL
    /// content-hash stamp (the hash the buffer held when
    /// the push landed). The runtime's `offer_diagnostics` runs
    /// the fast-path / save-point-replay pipeline against that
    /// hash; stamping with the window's current snapshot instead
    /// would pin a stale cargo-check push to content the server
    /// never analysed, which is the exact smear this layer
    /// protects against.
    ///
    /// Safe to call with `None` window (returns empty). `_window`
    /// parameter retained for API symmetry but unused — the old
    /// re-stamping trick was the bug, not a feature.
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

    /// Close the window. Push cache is preserved (legacy's "cache
    /// survives" invariant — see the `push_cache_survives_window_close`
    /// test).
    pub fn close_window(&mut self) {
        self.window = None;
    }

    /// Called by the native side when a `BufferChanged` arrives
    /// and the buffer's content hash no longer matches the
    /// window's snapshot for that path. Matches legacy's exact
    /// rule (`lsp/src/manager.rs:326-334`): content-hash-based so
    /// a type-then-delete round trip back to the original bytes
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
    /// subsequent edits. In pull mode, triggers the one-way
    /// fallback to push mode. Returns instructions for the caller.
    pub fn on_push(
        &mut self,
        path: CanonPath,
        diags: Vec<Diagnostic>,
        current_hash: PersistedContentHash,
    ) -> DiagPushResult {
        if self.mode == DiagMode::Pull {
            self.mode = DiagMode::Push;
            let had_window = self.window.is_some();
            self.window = None;
            self.push_cache.insert(path, (current_hash, diags));
            return if had_window {
                DiagPushResult::RestartWindow
            } else {
                // Pull mode but no window: the push is cached
                // for the next window open. Rare in practice —
                // the save-gated `RequestDiagnostics` normally
                // opens a window that stays open between saves.
                DiagPushResult::Ignore
            };
        }
        // Pure Push mode. Legacy's key trick: after pulls
        // complete in a push-mode window, the window STAYS OPEN
        // (only closed by actual content divergence via
        // `should_close_window`). So this branch is where late
        // cargo-check pushes typically land — we stamp them with
        // the hash they were computed against and forward.
        let is_clearing = diags.is_empty();
        self.push_cache
            .insert(path.clone(), (current_hash, diags.clone()));
        if self.window.is_some() {
            DiagPushResult::Forward(path, diags, current_hash)
        } else if is_clearing {
            // Clearing push outside a window — forward with the
            // current hash the caller supplied.
            DiagPushResult::ForwardClearing(path)
        } else {
            // Non-clearing push outside a window: cached, not
            // forwarded. The next RequestDiagnostics drains via
            // `drain_cache_for_window`, which keeps the stamp.
            DiagPushResult::Ignore
        }
    }

    /// A pull response arrived. Removes `path` from
    /// `pending_pulls`; if that was the last pending path, lifts
    /// the freeze.
    ///
    /// Returns `(maybe_forward, all_pulls_done)`. When both the
    /// push cache and pull have data for the same path, the cache
    /// wins — legacy's "push is more detailed" rule
    /// (`manager.rs:274-319`). Pull never modifies the cache.
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

        // Cache wins when present (legacy's "push is more detailed"
        // rule). The cached tuple also carries its own hash, but
        // we stamp with the window's snapshot here because the pull
        // response IS synchronous against the window — both hashes
        // refer to the same buffer content.
        let result = if let Some((_cached_hash, cached_diags)) =
            self.push_cache.get(&path)
        {
            cached_diags.clone()
        } else {
            pull_diags
        };

        (Some((path, result, h)), all_done)
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
        assert!(ds.push_cache.get(&p("/a.rs")).unwrap().1.is_empty());
    }


    #[test]
    fn push_clearing_without_window_forwards_clearing() {
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
    fn push_non_clearing_without_window_is_cached_not_forwarded() {
        // Legacy behaviour: late cargo-check pushes land here
        // when the user has typed past the snapshot (window
        // closed). We do NOT stamp them with the current version
        // because the diagnostic line numbers are from whatever
        // the server analysed. The push stays cached for the
        // next `RequestDiagnostics` → window open → drain.
        let mut ds = push_source();
        let r = ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        assert!(matches!(r, DiagPushResult::Ignore));
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message,
            "err",
            "push is still cached for next window open"
        );
    }

    #[test]
    fn push_forwarded_with_push_hash_not_window_snapshot() {
        // Legacy behaviour + the bug fix for stuck late-cargo
        // diagnostics: Forward stamps with the hash the push was
        // against, NOT the window's snapshot. Re-stamping would
        // pin a stale cargo error to content the server never
        // analysed.
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 3)]), &opened(&["/a.rs"]));
        let r = ds.on_push(p("/a.rs"), vec![diag("err")], PersistedContentHash(7));
        match r {
            DiagPushResult::Forward(path, diags, v) => {
                assert_eq!(path, p("/a.rs"));
                assert_eq!(diags.len(), 1);
                assert_eq!(v, PersistedContentHash(7));
            }
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn push_window_drains_cache_with_original_push_hash() {
        // Cache entries keep the hash from their push-time call;
        // drain reports that (not the new window's snapshot), so
        // offer_diagnostics downstream can replay or reject
        // against the hash the server actually saw.
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")], PersistedContentHash(7));
        ds.open_window(snap(&[("/a.rs", 11)]), &opened(&["/a.rs"]));
        let drained = ds.drain_cache_for_window();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1[0].message, "cached");
        assert_eq!(drained[0].2, PersistedContentHash(7));
    }

    #[test]
    fn push_cache_survives_window_close() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")], PersistedContentHash(7));
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        ds.close_window();
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message,
            "cached"
        );
    }

    #[test]
    fn push_window_not_frozen() {
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        assert!(!ds.is_frozen());
    }

    #[test]
    fn should_close_window_fires_on_version_movement() {
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 4)]), &opened(&["/a.rs"]));
        assert!(!ds.should_close_window(&p("/a.rs"), PersistedContentHash(4)));
        assert!(ds.should_close_window(&p("/a.rs"), PersistedContentHash(5)));
    }

    // ── Pull mode ───────────────────────────────────────────

    #[test]
    fn pull_window_is_frozen_and_has_deadline() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        assert!(ds.is_frozen());
        assert!(ds.deadline().is_some());
    }

    #[test]
    fn pull_window_returns_all_opened_paths() {
        let mut ds = pull_source();
        let pulls = ds.open_window(
            snap(&[("/a.rs", 1), ("/b.rs", 2)]),
            &opened(&["/a.rs", "/b.rs"]),
        );
        assert_eq!(pulls.len(), 2);
    }

    #[test]
    fn pull_response_forwards_with_snapshot_version() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 7)]), &opened(&["/a.rs"]));
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
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        assert!(ds.is_frozen());
        ds.cancel_freeze();
        assert!(!ds.is_frozen());
        assert!(ds.has_window(), "window stays open for late results");
    }

    #[test]
    fn pull_response_for_unknown_path_is_dropped() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        let (out, done) = ds.on_pull_response(p("/ghost.rs"), vec![diag("stray")]);
        assert!(out.is_none());
        assert!(!done);
        assert!(ds.is_frozen(), "unknown path doesn't close the pending set");
    }

    #[test]
    fn pull_response_uses_push_cache_when_both_exist() {
        // "Push is more detailed" — legacy manager.rs:304-318.
        let mut ds = pull_source();
        // Prime the cache as if a push had won the race.
        ds.push_cache.insert(
            p("/a.rs"),
            (PersistedContentHash(0), vec![diag("from_push")]),
        );
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        let (out, _) = ds.on_pull_response(p("/a.rs"), vec![diag("from_pull")]);
        assert_eq!(out.unwrap().1[0].message, "from_push");
    }

    // ── Mode fallback ──────────────────────────────────────

    #[test]
    fn default_mode_is_push() {
        assert_eq!(DiagnosticSource::new().mode, DiagMode::Push);
    }

    #[test]
    fn pull_to_push_fallback_no_window_caches() {
        // Pull-capable server sends a push with no window in
        // flight. Flips to push mode permanently and caches the
        // push; next `RequestDiagnostics` opens a push-mode
        // window and drains.
        let mut ds = pull_source();
        let r = ds.on_push(p("/a.rs"), vec![diag("pushed")], PersistedContentHash(7));
        assert_eq!(ds.mode, DiagMode::Push);
        assert!(matches!(r, DiagPushResult::Ignore));
        // Cache still populated so a subsequent
        // RequestDiagnostics opens a fresh window and drains.
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap().1[0].message,
            "pushed"
        );
    }

    #[test]
    fn pull_to_push_fallback_with_window_restarts() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        assert!(ds.is_frozen());
        let r = ds.on_push(p("/a.rs"), vec![diag("pushed")], PersistedContentHash(7));
        assert_eq!(ds.mode, DiagMode::Push);
        assert!(matches!(r, DiagPushResult::RestartWindow));
        assert!(!ds.has_window(), "window cleared by fallback");
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
        assert!(ds.push_cache.get(&p("/a.rs")).is_none());
    }
}
