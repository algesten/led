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

use led_core::CanonPath;
use led_state_diagnostics::{BufferVersion, Diagnostic};

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
    /// window's snapshot version for the path.
    Forward(CanonPath, Vec<Diagnostic>, BufferVersion),
    /// Push arrived outside any open window — forward with the
    /// CURRENT buffer version the caller supplies. Covers both
    /// clearing pushes (empty list) and non-empty pushes in pure
    /// push mode with no window. The latter matters because
    /// rust-analyzer's cargo-check diagnostics land as late
    /// pushes; the rewrite gates `RequestDiagnostics` on save
    /// only, so we can't rely on the next save to open a window
    /// and drain the cache.
    ForwardOutsideWindow(CanonPath, Vec<Diagnostic>),
    /// Pull-mode server sent an unsolicited push. We've switched
    /// permanently to push; the caller should close any open
    /// pull window and issue a fresh `RequestDiagnostics` so the
    /// window reopens in push mode.
    RestartWindow,
    /// Nothing to do: wrong mode edge cases.
    Ignore,
}

/// One open propagation window's in-flight state. Closed = `None`
/// on the parent `DiagnosticSource`.
struct DiagWindow {
    /// Version snapshot for every opened doc at window open time.
    /// Every forwarded `Diagnostics` event is stamped with the
    /// matching entry, so the model can version-gate.
    version_snapshot: HashMap<CanonPath, BufferVersion>,
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

    /// Latest push-delivered diagnostics per path. Updated on
    /// every push, regardless of whether a window is open. A
    /// freshly-opened push-mode window drains this verbatim.
    push_cache: HashMap<CanonPath, Vec<Diagnostic>>,

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

    /// Open a propagation window. `version_snapshot` must cover
    /// every currently-opened buffer keyed by canonical path.
    /// `opened` is just the set (same keys) — kept separate
    /// because pull mode iterates it to decide which paths need
    /// pulling; push mode iterates to decide which lack cache.
    ///
    /// Returns the paths to pull. Empty in push mode unless the
    /// server advertised pull capability (then it's just the
    /// paths without a push-cache hit).
    pub fn open_window(
        &mut self,
        version_snapshot: HashMap<CanonPath, BufferVersion>,
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
                    version_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: false,
                    deadline: None,
                });
                pull_paths
            }
            DiagMode::Pull => {
                let pull_paths: Vec<CanonPath> = opened.iter().cloned().collect();
                self.window = Some(DiagWindow {
                    version_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: true,
                    deadline: Some(Instant::now() + PULL_FREEZE_DEADLINE),
                });
                pull_paths
            }
        }
    }

    /// Push mode: drain the current push cache through the
    /// newly-opened window, stamping each entry with the
    /// snapshot version. Safe to call with `None` window (returns
    /// empty). Caller forwards each triple to the model.
    pub fn drain_cache_for_window(
        &self,
    ) -> Vec<(CanonPath, Vec<Diagnostic>, BufferVersion)> {
        let Some(window) = &self.window else {
            return Vec::new();
        };
        self.push_cache
            .iter()
            .map(|(path, diags)| {
                let v = window
                    .version_snapshot
                    .get(path)
                    .copied()
                    .unwrap_or(BufferVersion(0));
                (path.clone(), diags.clone(), v)
            })
            .collect()
    }

    /// Close the window. Push cache is preserved (legacy's "cache
    /// survives" invariant — see the `push_cache_survives_window_close`
    /// test).
    pub fn close_window(&mut self) {
        self.window = None;
    }

    /// Called by the native side when a `BufferChanged` arrives
    /// and the buffer has moved past the window's version for
    /// that path. Legacy used a content-hash comparison; we use
    /// monotonic `BufferVersion` (any forward movement counts).
    pub fn should_close_window(
        &self,
        path: &CanonPath,
        current: BufferVersion,
    ) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        let Some(snap) = window.version_snapshot.get(path) else {
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

    /// A `publishDiagnostics` push arrived. Always updates the
    /// cache. In pull mode, triggers the one-way fallback to push
    /// mode. Returns instructions for the caller.
    pub fn on_push(
        &mut self,
        path: CanonPath,
        diags: Vec<Diagnostic>,
    ) -> DiagPushResult {
        if self.mode == DiagMode::Pull {
            self.mode = DiagMode::Push;
            let had_window = self.window.is_some();
            self.window = None;
            self.push_cache.insert(path.clone(), diags.clone());
            return if had_window {
                DiagPushResult::RestartWindow
            } else {
                // Pull → Push flip with no window in flight.
                // Forward the push directly so the runtime
                // doesn't wait for the next save to see these
                // diagnostics. Matches the "push is eager"
                // semantics pure-push-mode servers already get.
                DiagPushResult::ForwardOutsideWindow(path, diags)
            };
        }
        // Pure Push mode.
        self.push_cache.insert(path.clone(), diags.clone());
        if let Some(window) = &self.window {
            let v = window
                .version_snapshot
                .get(&path)
                .copied()
                .unwrap_or(BufferVersion(0));
            DiagPushResult::Forward(path, diags, v)
        } else {
            // No window → forward directly. Covers both clearing
            // pushes and late cargo-check pushes that arrive
            // long after the save-triggered pull finished. The
            // runtime's version-gate drops any push stamped
            // against a now-stale version.
            DiagPushResult::ForwardOutsideWindow(path, diags)
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
        Option<(CanonPath, Vec<Diagnostic>, BufferVersion)>,
        bool,
    ) {
        let Some(window) = &mut self.window else {
            return (None, false);
        };
        if !window.pending_pulls.remove(&path) {
            // Either not expecting this path, or already answered.
            return (None, false);
        }
        let v = window
            .version_snapshot
            .get(&path)
            .copied()
            .unwrap_or(BufferVersion(0));
        let all_done = window.pending_pulls.is_empty();
        if all_done {
            window.frozen = false;
            window.deadline = None;
        }

        let result = if let Some(cached) = self.push_cache.get(&path) {
            cached.clone()
        } else {
            pull_diags
        };

        (Some((path, result, v)), all_done)
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

    fn snap(paths: &[(&str, u64)]) -> HashMap<CanonPath, BufferVersion> {
        paths
            .iter()
            .map(|(s, v)| (p(s), BufferVersion(*v)))
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
        ds.on_push(p("/a.rs"), vec![diag("err")]);
        assert_eq!(ds.push_cache.get(&p("/a.rs")).unwrap()[0].message, "err");
    }

    #[test]
    fn push_cache_updated_by_new_push() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("old")]);
        ds.on_push(p("/a.rs"), vec![diag("new")]);
        assert_eq!(ds.push_cache.get(&p("/a.rs")).unwrap()[0].message, "new");
    }

    #[test]
    fn empty_push_clears_cache_entry() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("err")]);
        ds.on_push(p("/a.rs"), vec![]);
        assert!(ds.push_cache.get(&p("/a.rs")).unwrap().is_empty());
    }


    #[test]
    fn push_clearing_without_window_forwards_for_current_hash() {
        let mut ds = push_source();
        let r = ds.on_push(p("/a.rs"), vec![]);
        match r {
            DiagPushResult::ForwardOutsideWindow(path, diags) => {
                assert_eq!(path, p("/a.rs"));
                assert!(diags.is_empty());
            }
            _ => panic!("expected ForwardOutsideWindow"),
        }
    }

    #[test]
    fn push_non_clearing_without_window_also_forwards() {
        // Regression: cargo-check diagnostics arrive long after
        // the save-triggered pull, in pure push mode with no open
        // window. Legacy "Ignore"'d these, relying on the next
        // keystroke-triggered RequestDiagnostics to drain the
        // cache. The rewrite gates RequestDiagnostics on save
        // only, so we MUST forward here.
        let mut ds = push_source();
        let r = ds.on_push(p("/a.rs"), vec![diag("unused import")]);
        match r {
            DiagPushResult::ForwardOutsideWindow(path, diags) => {
                assert_eq!(path, p("/a.rs"));
                assert_eq!(diags.len(), 1);
            }
            _ => panic!("expected ForwardOutsideWindow"),
        }
    }

    #[test]
    fn push_forwarded_with_window() {
        let mut ds = push_source();
        ds.open_window(snap(&[("/a.rs", 3)]), &opened(&["/a.rs"]));
        let r = ds.on_push(p("/a.rs"), vec![diag("err")]);
        match r {
            DiagPushResult::Forward(path, diags, v) => {
                assert_eq!(path, p("/a.rs"));
                assert_eq!(diags.len(), 1);
                assert_eq!(v, BufferVersion(3));
            }
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn push_window_drains_cache_with_snapshot_version() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")]);
        ds.open_window(snap(&[("/a.rs", 11)]), &opened(&["/a.rs"]));
        let drained = ds.drain_cache_for_window();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1[0].message, "cached");
        assert_eq!(drained[0].2, BufferVersion(11));
    }

    #[test]
    fn push_cache_survives_window_close() {
        let mut ds = push_source();
        ds.on_push(p("/a.rs"), vec![diag("cached")]);
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        ds.close_window();
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap()[0].message,
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
        assert!(!ds.should_close_window(&p("/a.rs"), BufferVersion(4)));
        assert!(ds.should_close_window(&p("/a.rs"), BufferVersion(5)));
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
        assert_eq!(v, BufferVersion(7));
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
        ds.push_cache.insert(p("/a.rs"), vec![diag("from_push")]);
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
    fn pull_to_push_fallback_no_window_forwards_directly() {
        // Pull-capable server sends a push with no window in
        // flight. Previously this returned `Ignore`, letting the
        // cache wait for the next RequestDiagnostics. The rewrite
        // gates RequestDiagnostics on save, so we now forward
        // directly so the push isn't stranded until the user
        // saves again.
        let mut ds = pull_source();
        let r = ds.on_push(p("/a.rs"), vec![diag("pushed")]);
        assert_eq!(ds.mode, DiagMode::Push);
        match r {
            DiagPushResult::ForwardOutsideWindow(path, diags) => {
                assert_eq!(path, p("/a.rs"));
                assert_eq!(diags.len(), 1);
                assert_eq!(diags[0].message, "pushed");
            }
            _ => panic!("expected ForwardOutsideWindow"),
        }
        // Cache still populated so a subsequent
        // RequestDiagnostics opens a fresh window and drains.
        assert_eq!(
            ds.push_cache.get(&p("/a.rs")).unwrap()[0].message,
            "pushed"
        );
    }

    #[test]
    fn pull_to_push_fallback_with_window_restarts() {
        let mut ds = pull_source();
        ds.open_window(snap(&[("/a.rs", 1)]), &opened(&["/a.rs"]));
        assert!(ds.is_frozen());
        let r = ds.on_push(p("/a.rs"), vec![diag("pushed")]);
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
        ds.on_push(p("/a.rs"), vec![diag("err")]);
        ds.invalidate_cache(&p("/a.rs"));
        assert!(ds.push_cache.get(&p("/a.rs")).is_none());
    }
}
