//! The `AlertState` source — session-global UI-chrome state.
//!
//! Drives the status bar's non-default modes: transient "info"
//! notices (e.g. `Saved foo.rs`), persistent "warn" notices
//! (e.g. write errors), and the confirm-kill prompt that appears
//! when the user tries to kill a dirty buffer.
//!
//! No driver. Dispatch writes to the state directly; the runtime's
//! tick loop calls [`AlertState::expire_info`] once per tick with
//! `Instant::now()` to clear expired transients. The runtime
//! additionally sets `info` on save completion and `warns` on
//! save error.

use led_state_tabs::TabId;
use std::time::{Duration, Instant};

/// Session-global alert / prompt state.
///
/// Invariants (maintained by dispatch + runtime):
/// - `info_expires_at` is `Some` iff `info` is `Some`.
/// - `warns` keys are unique: `set_warn` replaces on key match.
/// - `confirm_kill` is cleared before the next command runs; the
///   `KillBuffer` arm of dispatch sets it, the next keystroke
///   clears it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertState {
    /// Transient info notice. Replaced by newer info; cleared on
    /// timer expiry.
    pub info: Option<String>,
    /// Wall-clock deadline after which `info` should clear.
    pub info_expires_at: Option<Instant>,
    /// Persistent warn notices, first-arrived wins display. Each
    /// keyed so producers can `clear_warn(key)` without stomping
    /// other warns.
    pub warns: Vec<(String, String)>,
    /// `Some(tab_id)` while the confirm-kill prompt is live. The
    /// prompt targets a specific tab so later tab-switches don't
    /// accidentally confirm the kill of a different buffer.
    pub confirm_kill: Option<TabId>,
}

impl AlertState {
    /// Store a transient info notice with an expiry relative to
    /// `now`. Replaces any existing info.
    pub fn set_info(&mut self, msg: String, now: Instant, ttl: Duration) {
        self.info = Some(msg);
        self.info_expires_at = Some(now + ttl);
    }

    /// Drop `info` if its deadline has passed. No-op otherwise.
    /// Called once per tick by the runtime.
    pub fn expire_info(&mut self, now: Instant) {
        if let Some(deadline) = self.info_expires_at
            && now >= deadline
        {
            self.info = None;
            self.info_expires_at = None;
        }
    }

    /// Clear info immediately, regardless of deadline.
    pub fn clear_info(&mut self) {
        self.info = None;
        self.info_expires_at = None;
    }

    /// Add or replace a keyed warn. Existing entry with the same
    /// key is updated in place (preserving its position in the
    /// vec); a new key is appended.
    pub fn set_warn(&mut self, key: String, msg: String) {
        for entry in self.warns.iter_mut() {
            if entry.0 == key {
                entry.1 = msg;
                return;
            }
        }
        self.warns.push((key, msg));
    }

    /// Remove the warn with the given key. No-op if missing.
    pub fn clear_warn(&mut self, key: &str) {
        self.warns.retain(|(k, _)| k != key);
    }

    /// First-arrived warn for display. Legacy precedence: older
    /// warns stay visible until the producer clears them.
    pub fn first_warn(&self) -> Option<&(String, String)> {
        self.warns.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let a = AlertState::default();
        assert!(a.info.is_none());
        assert!(a.info_expires_at.is_none());
        assert!(a.warns.is_empty());
        assert!(a.confirm_kill.is_none());
    }

    #[test]
    fn set_info_stores_message_and_deadline() {
        let mut a = AlertState::default();
        let now = Instant::now();
        a.set_info("saved".into(), now, Duration::from_secs(2));
        assert_eq!(a.info.as_deref(), Some("saved"));
        assert_eq!(a.info_expires_at, Some(now + Duration::from_secs(2)));
    }

    #[test]
    fn expire_info_clears_after_deadline() {
        let mut a = AlertState::default();
        let now = Instant::now();
        a.set_info("x".into(), now, Duration::from_secs(2));
        // Before deadline.
        a.expire_info(now + Duration::from_secs(1));
        assert!(a.info.is_some());
        // At deadline.
        a.expire_info(now + Duration::from_secs(2));
        assert!(a.info.is_none());
        assert!(a.info_expires_at.is_none());
    }

    #[test]
    fn expire_info_noop_when_none() {
        let mut a = AlertState::default();
        a.expire_info(Instant::now());
        assert!(a.info.is_none());
    }

    #[test]
    fn set_info_replaces_existing() {
        let mut a = AlertState::default();
        let now = Instant::now();
        a.set_info("first".into(), now, Duration::from_secs(2));
        a.set_info("second".into(), now, Duration::from_secs(2));
        assert_eq!(a.info.as_deref(), Some("second"));
    }

    #[test]
    fn set_warn_appends_new_key() {
        let mut a = AlertState::default();
        a.set_warn("k1".into(), "m1".into());
        a.set_warn("k2".into(), "m2".into());
        assert_eq!(a.warns.len(), 2);
        assert_eq!(a.first_warn().unwrap().0, "k1");
    }

    #[test]
    fn set_warn_replaces_existing_key_in_place() {
        let mut a = AlertState::default();
        a.set_warn("k1".into(), "old".into());
        a.set_warn("k2".into(), "other".into());
        a.set_warn("k1".into(), "new".into());
        // k1 still at head (first-arrived precedence preserved).
        assert_eq!(a.warns.len(), 2);
        assert_eq!(a.warns[0], ("k1".into(), "new".into()));
        assert_eq!(a.warns[1], ("k2".into(), "other".into()));
    }

    #[test]
    fn clear_warn_removes_by_key() {
        let mut a = AlertState::default();
        a.set_warn("a".into(), "x".into());
        a.set_warn("b".into(), "y".into());
        a.clear_warn("a");
        assert_eq!(a.warns.len(), 1);
        assert_eq!(a.warns[0].0, "b");
    }

    #[test]
    fn clear_warn_missing_is_noop() {
        let mut a = AlertState::default();
        a.set_warn("a".into(), "x".into());
        a.clear_warn("z");
        assert_eq!(a.warns.len(), 1);
    }

    #[test]
    fn first_warn_returns_head() {
        let mut a = AlertState::default();
        assert!(a.first_warn().is_none());
        a.set_warn("a".into(), "x".into());
        assert_eq!(a.first_warn().unwrap().0, "a");
    }

    #[test]
    fn confirm_kill_round_trip() {
        let mut a = AlertState::default();
        assert!(a.confirm_kill.is_none());
        a.confirm_kill = Some(TabId(5));
        assert_eq!(a.confirm_kill, Some(TabId(5)));
        a.confirm_kill = None;
        assert!(a.confirm_kill.is_none());
    }
}
