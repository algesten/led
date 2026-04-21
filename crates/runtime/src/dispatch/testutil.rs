//! Shared test scaffolding for the dispatch submodules.
//!
//! Holds the generic fixture builders + dispatch wrappers every
//! submodule's tests use. Items are `pub(super)` so sibling test
//! modules can reach them via `super::testutil::*` without widening
//! visibility beyond the `dispatch` crate path.

#![cfg(test)]

use std::sync::Arc;

use led_core::{CanonPath, UserPath};
use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::{Dims, KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_clipboard::ClipboardState;
use led_state_find_file::FindFileState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_tabs::{Tab, TabId, Tabs};
use ropey::Rope;

use super::dispatch_key;
use super::{ChordState, DispatchOutcome};
use crate::keymap::default_keymap;

// ── Atom + event builders ──────────────────────────────────────────────

pub(super) fn canon(s: &str) -> CanonPath {
    UserPath::new(s).canonicalize()
}

pub(super) fn key(mods: KeyModifiers, code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: mods,
    }
}

pub(super) fn tabs_with(paths: &[(&str, u64)], active: Option<u64>) -> Tabs {
    let mut t = Tabs::default();
    for (p, id) in paths {
        t.open.push_back(Tab {
            id: TabId(*id),
            path: canon(p),
            ..Default::default()
        });
    }
    t.active = active.map(TabId);
    t
}

pub(super) fn terminal_with(dims: Option<Dims>) -> Terminal {
    Terminal {
        dims,
        ..Default::default()
    }
}

/// One tab at `file.rs` seeded with the given rope, plus a matching
/// `BufferStore::Ready` entry and a terminal at `dims`.
pub(super) fn fixture_with_content(
    body: &str,
    dims: Dims,
) -> (Tabs, BufferEdits, BufferStore, Terminal) {
    let rope = Arc::new(Rope::from_str(body));
    let mut edits = BufferEdits::default();
    edits
        .buffers
        .insert(canon("file.rs"), EditedBuffer::fresh(rope.clone()));
    let mut store = BufferStore::default();
    store
        .loaded
        .insert(canon("file.rs"), LoadState::Ready(rope));
    (
        tabs_with(&[("file.rs", 1)], Some(1)),
        edits,
        store,
        terminal_with(Some(dims)),
    )
}

// ── Buffer inspection helpers ──────────────────────────────────────────

pub(super) fn rope_of(edits: &BufferEdits, path: &str) -> Arc<Rope> {
    edits
        .buffers
        .get(&canon(path))
        .expect("seeded")
        .rope
        .clone()
}

pub(super) fn version_of(edits: &BufferEdits, path: &str) -> u64 {
    edits.buffers.get(&canon(path)).expect("seeded").version
}

pub(super) fn dirty_of(edits: &BufferEdits, path: &str) -> bool {
    edits.buffers.get(&canon(path)).expect("seeded").dirty()
}

// ── Dispatch wrappers ──────────────────────────────────────────────────

/// Dispatch a key with the default keymap + fresh auxiliaries. Tests
/// that care about chord / kill-ring / alert state construct them
/// themselves and call `dispatch_key` directly.
pub(super) fn dispatch_default(
    k: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    let mut chord = ChordState::default();
    let mut kill_ring = KillRing::default();
    let mut clip = ClipboardState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    dispatch_key(
        k,
        tabs,
        edits,
        &mut kill_ring,
        &mut clip,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &fs,
        store,
        terminal,
        &mut find_file,
        &default_keymap(),
        &mut chord,)
}

/// Press a chord sequence (prefix then second) with a fresh
/// `ChordState`. Used by tests that exercise legacy-style
/// chord-bound commands without duplicating the state setup.
pub(super) fn dispatch_chord_default(
    prefix: KeyEvent,
    second: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    let keymap = default_keymap();
    let mut chord = ChordState::default();
    let mut kill_ring = KillRing::default();
    let mut clip = ClipboardState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    dispatch_key(
        prefix,
        tabs,
        edits,
        &mut kill_ring,
        &mut clip,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &fs,
        store,
        terminal,
        &mut find_file,
        &keymap,
        &mut chord,);
    let mut find_file: Option<FindFileState> = None;
    dispatch_key(
        second,
        tabs,
        edits,
        &mut kill_ring,
        &mut clip,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &fs,
        store,
        terminal,
        &mut find_file,
        &keymap,
        &mut chord,)
}

/// Dispatch a key with a caller-provided kill ring. Used by M7 tests
/// that need to inspect the kill ring afterwards.
pub(super) fn dispatch_with_ring(
    k: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    clip: &mut ClipboardState,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    let mut chord = ChordState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    dispatch_key(
        k,
        tabs,
        edits,
        kill_ring,
        clip,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &fs,
        store,
        terminal,
        &mut find_file,
        &default_keymap(),
        &mut chord,)
}

/// Dispatch a key with everything ambient. Lightest wrapper — for
/// tests that only care about tab-switch / quit behaviour.
pub(super) fn noop_dispatch(k: KeyEvent, tabs: &mut Tabs) -> DispatchOutcome {
    let mut edits = BufferEdits::default();
    let mut kill_ring = KillRing::default();
    let mut clip = ClipboardState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let store = BufferStore::default();
    let terminal = Terminal::default();
    let keymap = default_keymap();
    let mut chord = ChordState::default();
    let mut find_file: Option<FindFileState> = None;
    dispatch_key(
        k,
        tabs,
        &mut edits,
        &mut kill_ring,
        &mut clip,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &fs,
        &store,
        &terminal,
        &mut find_file,
        &keymap,
        &mut chord,)
}

/// Type a run of chars through the full keymap + implicit-insert
/// path so coalescing fires exactly as at runtime.
pub(super) fn type_chars(
    chars: &str,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    term: &Terminal,
) {
    for c in chars.chars() {
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char(c)),
            tabs,
            edits,
            store,
            term,
        );
    }
}
