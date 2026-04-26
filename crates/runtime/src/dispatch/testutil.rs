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
use led_state_completions::{CompletionsPending, CompletionsState};
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_diagnostics::DiagnosticsStates;
use led_state_git::GitState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_lsp::{LspExtrasState, LspPending};
use led_state_tabs::{Tab, TabId, Tabs};
use ropey::Rope;

use super::Dispatcher;
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
/// themselves and build a `Dispatcher` directly.
pub(super) fn dispatch_default(
    k: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    let mut chord = ChordState::default();
    let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
    let mut kill_ring = KillRing::default();
    let mut clip = ClipboardState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    let mut isearch: Option<IsearchState> = None;
    let mut file_search: Option<FileSearchState> = None;
    let mut path_chains = std::collections::HashMap::new();
    let mut completions = CompletionsState::default();
    let mut completions_pending = CompletionsPending::default();
    let mut lsp_extras = LspExtrasState::default();
    let mut lsp_pending = LspPending::default();
    let diagnostics = DiagnosticsStates::default();
    let lsp_status = led_state_diagnostics::LspStatuses::default();
    let git = GitState::default();
    let keymap = default_keymap();
    let mut dispatcher = Dispatcher {
        tabs,
        edits,
        kill_ring: &mut kill_ring,
        clip: &mut clip,
        alerts: &mut alerts,
        jumps: &mut jumps,
        browser: &mut browser,
        fs: &fs,
        store,
        terminal,
        find_file: &mut find_file,
        isearch: &mut isearch,
        file_search: &mut file_search,
        completions: &mut completions,
        completions_pending: &mut completions_pending,
        lsp_extras: &mut lsp_extras,
        lsp_pending: &mut lsp_pending,
        diagnostics: &diagnostics,
        lsp_status: &lsp_status,
        git: &git,
        path_chains: &mut path_chains,
        keymap: &keymap,
        chord: &mut chord,
        kbd_macro: &mut kbd_macro,
    };
    dispatcher.dispatch_key(k)
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
    let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
    let mut kill_ring = KillRing::default();
    let mut clip = ClipboardState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    let mut isearch: Option<IsearchState> = None;
    let mut file_search: Option<FileSearchState> = None;
    let mut path_chains = std::collections::HashMap::new();
    let mut completions = CompletionsState::default();
    let mut completions_pending = CompletionsPending::default();
    let mut lsp_extras = LspExtrasState::default();
    let mut lsp_pending = LspPending::default();
    let diagnostics = DiagnosticsStates::default();
    let lsp_status = led_state_diagnostics::LspStatuses::default();
    let git = GitState::default();
    let mut dispatcher = Dispatcher {
        tabs,
        edits,
        kill_ring: &mut kill_ring,
        clip: &mut clip,
        alerts: &mut alerts,
        jumps: &mut jumps,
        browser: &mut browser,
        fs: &fs,
        store,
        terminal,
        find_file: &mut find_file,
        isearch: &mut isearch,
        file_search: &mut file_search,
        completions: &mut completions,
        completions_pending: &mut completions_pending,
        lsp_extras: &mut lsp_extras,
        lsp_pending: &mut lsp_pending,
        diagnostics: &diagnostics,
        lsp_status: &lsp_status,
        git: &git,
        path_chains: &mut path_chains,
        keymap: &keymap,
        chord: &mut chord,
        kbd_macro: &mut kbd_macro,
    };
    dispatcher.dispatch_key(prefix);
    dispatcher.dispatch_key(second)
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
    let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    let mut browser = BrowserUi::default();
    let fs = FsTree::default();
    let mut find_file: Option<FindFileState> = None;
    let mut isearch: Option<IsearchState> = None;
    let mut file_search: Option<FileSearchState> = None;
    let mut path_chains = std::collections::HashMap::new();
    let mut completions = CompletionsState::default();
    let mut completions_pending = CompletionsPending::default();
    let mut lsp_extras = LspExtrasState::default();
    let mut lsp_pending = LspPending::default();
    let diagnostics = DiagnosticsStates::default();
    let lsp_status = led_state_diagnostics::LspStatuses::default();
    let git = GitState::default();
    let keymap = default_keymap();
    let mut dispatcher = Dispatcher {
        tabs,
        edits,
        kill_ring,
        clip,
        alerts: &mut alerts,
        jumps: &mut jumps,
        browser: &mut browser,
        fs: &fs,
        store,
        terminal,
        find_file: &mut find_file,
        isearch: &mut isearch,
        file_search: &mut file_search,
        completions: &mut completions,
        completions_pending: &mut completions_pending,
        lsp_extras: &mut lsp_extras,
        lsp_pending: &mut lsp_pending,
        diagnostics: &diagnostics,
        lsp_status: &lsp_status,
        git: &git,
        path_chains: &mut path_chains,
        keymap: &keymap,
        chord: &mut chord,
        kbd_macro: &mut kbd_macro,
    };
    dispatcher.dispatch_key(k)
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
    let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
    let mut find_file: Option<FindFileState> = None;
    let mut isearch: Option<IsearchState> = None;
    let mut file_search: Option<FileSearchState> = None;
    let mut path_chains = std::collections::HashMap::new();
    let mut completions = CompletionsState::default();
    let mut completions_pending = CompletionsPending::default();
    let mut lsp_extras = LspExtrasState::default();
    let mut lsp_pending = LspPending::default();
    let diagnostics = DiagnosticsStates::default();
    let lsp_status = led_state_diagnostics::LspStatuses::default();
    let git = GitState::default();
    let mut dispatcher = Dispatcher {
        tabs,
        edits: &mut edits,
        kill_ring: &mut kill_ring,
        clip: &mut clip,
        alerts: &mut alerts,
        jumps: &mut jumps,
        browser: &mut browser,
        fs: &fs,
        store: &store,
        terminal: &terminal,
        find_file: &mut find_file,
        isearch: &mut isearch,
        file_search: &mut file_search,
        completions: &mut completions,
        completions_pending: &mut completions_pending,
        lsp_extras: &mut lsp_extras,
        lsp_pending: &mut lsp_pending,
        diagnostics: &diagnostics,
        lsp_status: &lsp_status,
        git: &git,
        path_chains: &mut path_chains,
        keymap: &keymap,
        chord: &mut chord,
        kbd_macro: &mut kbd_macro,
    };
    dispatcher.dispatch_key(k)
}

/// Owns every piece of state a macro test needs. Tests construct
/// one with `MacroDispatcherFixture::new(...)`, call `.dispatch(k)`
/// any number of times, and inspect the public fields between calls
/// (chord / kbd_macro / alerts / tabs / edits).
///
/// The fixture exists because building a `Dispatcher` per test
/// requires ~20 `let mut` bindings; concentrating them here keeps
/// every M22 test at five lines of setup. The struct itself owns
/// the auxiliary state; the caller-supplied state (tabs, edits, etc.)
/// passes through `new`.
pub(super) struct MacroDispatcherFixture {
    pub tabs: Tabs,
    pub edits: BufferEdits,
    pub chord: ChordState,
    pub kbd_macro: led_state_kbd_macro::KbdMacroState,
    pub alerts: AlertState,
    pub store: BufferStore,
    pub terminal: Terminal,
    kill_ring: KillRing,
    clip: ClipboardState,
    jumps: JumpListState,
    browser: BrowserUi,
    fs: FsTree,
    find_file: Option<FindFileState>,
    isearch: Option<IsearchState>,
    file_search: Option<FileSearchState>,
    path_chains: std::collections::HashMap<led_core::CanonPath, led_core::PathChain>,
    completions: CompletionsState,
    completions_pending: CompletionsPending,
    lsp_extras: LspExtrasState,
    lsp_pending: LspPending,
    diagnostics: DiagnosticsStates,
    lsp_status: led_state_diagnostics::LspStatuses,
    git: GitState,
    keymap: crate::keymap::Keymap,
}

impl MacroDispatcherFixture {
    pub(super) fn new(
        tabs: Tabs,
        edits: BufferEdits,
        store: BufferStore,
        terminal: Terminal,
        chord: ChordState,
        kbd_macro: led_state_kbd_macro::KbdMacroState,
        alerts: AlertState,
    ) -> Self {
        Self {
            tabs,
            edits,
            chord,
            kbd_macro,
            alerts,
            store,
            terminal,
            kill_ring: KillRing::default(),
            clip: ClipboardState::default(),
            jumps: JumpListState::default(),
            browser: BrowserUi::default(),
            fs: FsTree::default(),
            find_file: None,
            isearch: None,
            file_search: None,
            path_chains: std::collections::HashMap::new(),
            completions: CompletionsState::default(),
            completions_pending: CompletionsPending::default(),
            lsp_extras: LspExtrasState::default(),
            lsp_pending: LspPending::default(),
            diagnostics: DiagnosticsStates::default(),
            lsp_status: led_state_diagnostics::LspStatuses::default(),
            git: GitState::default(),
            keymap: default_keymap(),
        }
    }

    pub(super) fn dispatch(&mut self, k: KeyEvent) -> DispatchOutcome {
        let mut dispatcher = Dispatcher {
            tabs: &mut self.tabs,
            edits: &mut self.edits,
            kill_ring: &mut self.kill_ring,
            clip: &mut self.clip,
            alerts: &mut self.alerts,
            jumps: &mut self.jumps,
            browser: &mut self.browser,
            fs: &self.fs,
            store: &self.store,
            terminal: &self.terminal,
            find_file: &mut self.find_file,
            isearch: &mut self.isearch,
            file_search: &mut self.file_search,
            completions: &mut self.completions,
            completions_pending: &mut self.completions_pending,
            lsp_extras: &mut self.lsp_extras,
            lsp_pending: &mut self.lsp_pending,
            diagnostics: &self.diagnostics,
            lsp_status: &self.lsp_status,
            git: &self.git,
            path_chains: &mut self.path_chains,
            keymap: &self.keymap,
            chord: &mut self.chord,
            kbd_macro: &mut self.kbd_macro,
        };
        dispatcher.dispatch_key(k)
    }
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
