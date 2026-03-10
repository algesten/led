//! Shell — the orchestrator that owns components and routes input.
//!
//! # Rules for modifying this file
//!
//! Do NOT add feature logic here. The shell is a thin router. All behaviour
//! lives inside components, communicated through three channels:
//!
//!   Action  (shell → component)  — via `handle_action`
//!   Event   (broadcast)          — via `Effect::Emit` → `handle_event`
//!   Effect  (component → shell)  — returned from `handle_action` / `handle_event`
//!
//! If you need new behaviour, add an Action, Event, or Effect variant and
//! handle it in the relevant component. The shell may only react to Effects
//! (spawn, focus, message, quit) — never inspect or manipulate component state.
//!
//! The shell MUST NOT downcast components to concrete types.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::Connection;

use std::sync::Arc;

use crate::config::{KeyCombo, Keymap, KeymapLookup};
use crate::theme::Theme;
use led_core::{
    Action, Clipboard, Component, Context, DocStore, Effect, Event, FileStatusStore, PanelSlot,
    Waker,
};

struct ArboardClipboard {
    inner: std::sync::Mutex<Option<arboard::Clipboard>>,
}

impl ArboardClipboard {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(arboard::Clipboard::new().ok()),
        }
    }
}

impl Clipboard for ArboardClipboard {
    fn get_text(&self) -> Option<String> {
        self.inner.lock().ok()?.as_mut()?.get_text().ok()
    }

    fn set_text(&self, text: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(cb) = guard.as_mut() {
                let _ = cb.set_text(text);
            }
        }
    }
}

pub enum InputResult {
    Continue,
    Quit,
    Suspend,
}

#[derive(Default)]
enum ChordState {
    #[default]
    None,
    Pending(KeyCombo),
}

pub enum PendingAction {
    KillBuffer,
    Confirmed(Action),
}

pub struct Modal {
    pub prompt: String,
    pub input: String,
    pub action: PendingAction,
}

pub struct RenameModal {
    pub prompt: String,
    pub input: String,
    pub path: PathBuf,
    pub row: usize,
    pub col: usize,
}

enum RenameKeyResult {
    Cancel,
    Submit,
    Backspace,
    Char(char),
    Ignore,
}

fn classify_rename_key(key: &KeyEvent) -> RenameKeyResult {
    let is_abort = matches!(key.code, KeyCode::Esc)
        || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL));
    if is_abort {
        return RenameKeyResult::Cancel;
    }
    if key.code == KeyCode::Enter {
        return RenameKeyResult::Submit;
    }
    if key.code == KeyCode::Backspace {
        return RenameKeyResult::Backspace;
    }
    if let KeyCode::Char(c) = key.code {
        let has_ctrl_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !has_ctrl_alt {
            return RenameKeyResult::Char(c);
        }
    }
    RenameKeyResult::Ignore
}

enum ModalKeyResult {
    Cancel,
    Submit { confirmed: bool },
    Backspace,
    Char(char),
    Ignore,
}

fn classify_modal_key(key: &KeyEvent, input: &str) -> ModalKeyResult {
    let is_abort = matches!(key.code, KeyCode::Esc)
        || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL));
    if is_abort {
        return ModalKeyResult::Cancel;
    }
    if key.code == KeyCode::Enter {
        return ModalKeyResult::Submit {
            confirmed: input == "yes",
        };
    }
    if key.code == KeyCode::Backspace {
        return ModalKeyResult::Backspace;
    }
    if let KeyCode::Char(c) = key.code {
        let has_ctrl_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !has_ctrl_alt {
            return ModalKeyResult::Char(c);
        }
    }
    ModalKeyResult::Ignore
}

fn worst_diagnostic_severity(
    diagnostics: &[led_core::lsp_types::EditorDiagnostic],
) -> Option<led_core::lsp_types::DiagnosticSeverity> {
    use led_core::lsp_types::DiagnosticSeverity;
    diagnostics
        .iter()
        .filter(|d| d.severity != DiagnosticSeverity::Hint)
        .fold(None, |acc: Option<DiagnosticSeverity>, d| {
            Some(match acc {
                None => d.severity,
                Some(prev) => match (prev, d.severity) {
                    (DiagnosticSeverity::Error, _) | (_, DiagnosticSeverity::Error) => {
                        DiagnosticSeverity::Error
                    }
                    (DiagnosticSeverity::Warning, _) | (_, DiagnosticSeverity::Warning) => {
                        DiagnosticSeverity::Warning
                    }
                    _ => DiagnosticSeverity::Info,
                },
            })
        })
}

fn find_preview_idx(components: &[Box<dyn Component>]) -> Option<usize> {
    components
        .iter()
        .position(|c| c.tab().map_or(false, |t| t.preview))
}

struct TabManager {
    active: usize,
    pre_preview: Option<usize>,
    last_touched: Vec<Instant>,
    bar_width: u16,
}

impl TabManager {
    fn new() -> Self {
        Self {
            active: 0,
            pre_preview: None,
            last_touched: Vec::new(),
            bar_width: 0,
        }
    }

    // --- Index mapping (pure, takes tabbed slice) ---

    fn active_component(&self, tabbed: &[usize]) -> Option<usize> {
        tabbed.get(self.active).copied()
    }

    fn tab_index_of(&self, comp_idx: usize, tabbed: &[usize]) -> Option<usize> {
        tabbed.iter().position(|&i| i == comp_idx)
    }

    // --- Navigation ---

    fn next(&mut self, tabs_count: usize) {
        if tabs_count > 1 {
            self.active = (self.active + 1) % tabs_count;
            self.pre_preview = None;
        }
    }

    fn prev(&mut self, tabs_count: usize) {
        if tabs_count > 1 {
            self.active = if self.active == 0 {
                tabs_count - 1
            } else {
                self.active - 1
            };
            self.pre_preview = None;
        }
    }

    fn activate(&mut self, comp_idx: usize, tabbed: &[usize]) {
        if let Some(tab_idx) = self.tab_index_of(comp_idx, tabbed) {
            self.active = tab_idx;
        }
    }

    fn set_active(&mut self, tab: usize, tabs_count: usize) {
        if tabs_count > 0 {
            self.active = tab.min(tabs_count - 1);
        }
    }

    // --- Clamp / adjust after removal ---

    fn clamp(&mut self, tabs_count: usize) {
        if tabs_count == 0 {
            self.active = 0;
        } else if self.active >= tabs_count {
            self.active = tabs_count - 1;
        }
    }

    /// After removing a tab at `removed_tab_idx`, shift active left if needed.
    fn adjust_after_removal(&mut self, removed_tab_idx: Option<usize>, tabs_count: usize) {
        if let Some(rt) = removed_tab_idx {
            if rt < self.active {
                self.active -= 1;
            } else {
                self.clamp(tabs_count);
            }
        }
    }

    // --- Post-broadcast stabilization ---

    fn stabilize(&mut self, prev_active: usize, prev_comp: Option<usize>, tabbed: &[usize]) {
        if self.active == prev_active {
            if let Some(ci) = prev_comp {
                if let Some(ti) = self.tab_index_of(ci, tabbed) {
                    self.active = ti;
                }
            }
        }
    }

    // --- Preview state machine ---

    fn save_preview(&mut self) {
        if self.pre_preview.is_none() {
            self.pre_preview = Some(self.active);
        }
    }

    fn restore_preview(&mut self, tabs_count: usize) {
        if let Some(tab) = self.pre_preview.take() {
            if tabs_count > 0 {
                self.active = tab.min(tabs_count - 1);
            }
        }
    }

    fn clear_preview(&mut self) {
        self.pre_preview = None;
    }

    // --- LRU tracking ---

    fn register(&mut self) {
        self.last_touched.push(Instant::now());
    }

    fn remove(&mut self, comp_idx: usize) {
        self.last_touched.remove(comp_idx);
    }

    fn touch(&mut self, comp_idx: usize) {
        if comp_idx < self.last_touched.len() {
            self.last_touched[comp_idx] = Instant::now();
        }
    }

    fn touch_active(&mut self, tabbed: &[usize]) {
        if let Some(idx) = self.active_component(tabbed) {
            self.touch(idx);
        }
    }
}

struct Env {
    db: Option<Connection>,
    root: PathBuf,
    viewport_height: usize,
    clipboard: Arc<dyn Clipboard>,
    waker: Option<Waker>,
}

fn make_ctx<'a>(env: &'a Env, docs: &'a mut DocStore) -> Context<'a> {
    Context {
        db: env.db.as_ref(),
        root: &env.root,
        viewport_height: env.viewport_height,
        clipboard: env.clipboard.as_ref(),
        waker: env.waker.clone(),
        kv: std::collections::HashMap::new(),
        docs,
    }
}

pub struct Shell {
    pub components: Vec<Box<dyn Component>>,
    tabs: TabManager,
    pub message: Option<(String, Instant)>,
    chord: ChordState,
    keymap: Keymap,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub debug: bool,
    pub theme: Theme,
    debug_flash: Option<(String, Instant)>,
    pub modal: Option<Modal>,
    pub rename_modal: Option<RenameModal>,
    last_persist: Instant,
    pending_flush: bool,
    env: Env,
    pub file_statuses: FileStatusStore,
    pub lsp_status: Option<led_core::LspStatus>,
    pub docs: DocStore,
}

impl Shell {
    pub fn new(keymap: Keymap, theme: Theme, db: Option<Connection>, root: PathBuf) -> Self {
        Self {
            components: Vec::new(),
            tabs: TabManager::new(),
            message: None,
            chord: ChordState::None,
            keymap,
            focus: PanelSlot::Side,
            show_side_panel: true,
            debug: false,
            theme,
            debug_flash: None,
            modal: None,
            rename_modal: None,
            last_persist: Instant::now(),
            pending_flush: false,
            file_statuses: FileStatusStore::default(),
            lsp_status: None,
            docs: DocStore::new(),
            env: Env {
                db,
                root,
                viewport_height: 24,
                clipboard: Arc::new(ArboardClipboard::new()),
                waker: None,
            },
        }
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.env.waker = Some(waker);
    }

    pub fn register(&mut self, component: Box<dyn Component>) {
        // Save pre_preview_tab before registering a preview buffer
        let is_preview = component.tab().map_or(false, |t| t.preview);
        if is_preview {
            self.tabs.save_preview();
        }
        // Dedup by path: if a tab with the same path exists, just focus it
        if let Some(path) = component.tab().and_then(|t| t.path) {
            if let Some(idx) = self
                .components
                .iter()
                .position(|c| c.tab().and_then(|t| t.path).as_ref() == Some(&path))
            {
                let tabbed = self.tabbed_components();
                self.tabs.activate(idx, &tabbed);
                self.notify_active_buffer();
                return;
            }
        }
        let has_tab = component.tab().is_some();
        // Evict LRU clean tabs if the tab bar would overflow
        if has_tab && !is_preview {
            self.evict_for_new_tab(&component);
        }
        self.components.push(component);
        self.tabs.register();
        let last = self.components.len() - 1;
        if has_tab {
            {
                let mut ctx = make_ctx(&self.env, &mut self.docs);
                self.components[last].handle_action(Action::RestoreSession, &mut ctx);
            }
            let tabbed = self.tabbed_components();
            self.tabs.activate(last, &tabbed);
            self.notify_active_buffer();
        }
    }

    // --- Tab helpers ---

    /// Get all components that have tabs, in order.
    fn tabbed_components(&self) -> Vec<usize> {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.tab().is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// Get the component index for the active tab.
    pub fn active_tab_component_idx(&self) -> Option<usize> {
        let tabbed = self.tabbed_components();
        self.tabs.active_component(&tabbed)
    }

    /// Get a mutable reference to the active tab's component.
    pub fn active_buffer_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        let idx = self.active_tab_component_idx()?;
        Some(&mut self.components[idx])
    }

    /// Find the component claiming the Side panel (highest priority).
    pub fn side_component(&self) -> Option<&Box<dyn Component>> {
        let idx = self.side_component_idx()?;
        Some(&self.components[idx])
    }

    pub fn side_component_idx(&self) -> Option<usize> {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.panel_claims().iter().any(|cl| cl.slot == PanelSlot::Side))
            .max_by_key(|(_, c)| {
                c.panel_claims()
                    .iter()
                    .filter(|cl| cl.slot == PanelSlot::Side)
                    .map(|cl| cl.priority)
                    .max()
                    .unwrap_or(0)
            })
            .map(|(i, _)| i)
    }

    pub fn status_bar_component_idx(&self) -> Option<usize> {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.panel_claims()
                    .iter()
                    .any(|cl| cl.slot == PanelSlot::StatusBar)
            })
            .max_by_key(|(_, c)| {
                c.panel_claims()
                    .iter()
                    .filter(|cl| cl.slot == PanelSlot::StatusBar)
                    .map(|cl| cl.priority)
                    .max()
                    .unwrap_or(0)
            })
            .map(|(i, _)| i)
    }

    pub fn status_bar_component(&self) -> Option<&Box<dyn Component>> {
        let idx = self.status_bar_component_idx()?;
        Some(&self.components[idx])
    }

    pub fn overlay_component_idx(&self) -> Option<usize> {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.panel_claims()
                    .iter()
                    .any(|cl| cl.slot == PanelSlot::Overlay)
            })
            .max_by_key(|(_, c)| {
                c.panel_claims()
                    .iter()
                    .filter(|cl| cl.slot == PanelSlot::Overlay)
                    .map(|cl| cl.priority)
                    .max()
                    .unwrap_or(0)
            })
            .map(|(i, _)| i)
    }

    pub fn components(&self) -> &[Box<dyn Component>] {
        &self.components
    }

    pub fn active_tab(&self) -> usize {
        self.tabs.active
    }

    pub fn has_tabs(&self) -> bool {
        !self.tabbed_components().is_empty()
    }

    // --- Key event handling ---

    pub fn handle_key_event(&mut self, key: KeyEvent) -> InputResult {
        let combo = KeyCombo::from_key_event(&key);

        // Clear transient message on any keypress
        self.message = None;

        // Debug flash
        if self.debug {
            let display = if let ChordState::Pending(prefix) = &self.chord {
                format!("{} -> {}", prefix.display_name(), combo.display_name())
            } else {
                combo.display_name()
            };
            self.debug_flash = Some((display, Instant::now()));
        }

        // Handle rename modal input
        if self.rename_modal.is_some() {
            match classify_rename_key(&key) {
                RenameKeyResult::Cancel => {
                    self.rename_modal = None;
                    self.set_message("Aborted");
                }
                RenameKeyResult::Submit => {
                    let modal = self.rename_modal.take().unwrap();
                    if !modal.input.is_empty() {
                        let effects = vec![Effect::Emit(Event::LspRename {
                            path: modal.path,
                            row: modal.row,
                            col: modal.col,
                            new_name: modal.input,
                        })];
                        self.process_effects(effects);
                    }
                }
                RenameKeyResult::Backspace => {
                    if let Some(ref mut modal) = self.rename_modal {
                        modal.input.pop();
                    }
                }
                RenameKeyResult::Char(c) => {
                    if let Some(ref mut modal) = self.rename_modal {
                        modal.input.push(c);
                    }
                }
                RenameKeyResult::Ignore => {}
            }
            return InputResult::Continue;
        }

        // Handle modal input
        if self.modal.is_some() {
            let input = self.modal.as_ref().unwrap().input.as_str();
            match classify_modal_key(&key, input) {
                ModalKeyResult::Cancel => {
                    self.modal = None;
                    self.set_message("Aborted");
                }
                ModalKeyResult::Submit { confirmed } => {
                    if confirmed {
                        let modal = self.modal.take().unwrap();
                        match modal.action {
                            PendingAction::KillBuffer => self.kill_current_buffer(),
                            PendingAction::Confirmed(action) => {
                                let effects = self.dispatch_action(action);
                                self.process_effects(effects);
                            }
                        }
                    } else {
                        self.set_message("Aborted");
                    }
                    self.modal = None;
                }
                ModalKeyResult::Backspace => {
                    if let Some(ref mut modal) = self.modal {
                        modal.input.pop();
                    }
                }
                ModalKeyResult::Char(c) => {
                    if let Some(ref mut modal) = self.modal {
                        modal.input.push(c);
                    }
                }
                ModalKeyResult::Ignore => {}
            }
            return InputResult::Continue;
        }

        // Handle chord state
        if let ChordState::Pending(prefix) = self.chord {
            self.chord = ChordState::None;
            if let Some(action) = self.keymap.lookup_chord(&prefix, &combo) {
                return self.execute_action(action);
            }
            self.set_message("Unknown chord");
            return InputResult::Continue;
        }

        let context: Option<String> = match self.focus {
            PanelSlot::Side => self
                .side_component()
                .and_then(|c| c.context_name())
                .map(|s| s.to_string()),
            PanelSlot::StatusBar => self
                .status_bar_component()
                .and_then(|c| c.context_name())
                .map(|s| s.to_string()),
            PanelSlot::Overlay => self
                .overlay_component_idx()
                .and_then(|i| self.components[i].context_name())
                .map(|s| s.to_string()),
            PanelSlot::Main => None,
        };

        match self.keymap.lookup(&combo, context.as_deref()) {
            KeymapLookup::Action(action) => self.execute_action(action),
            KeymapLookup::ChordPrefix => {
                self.chord = ChordState::Pending(combo);
                self.message = None;
                InputResult::Continue
            }
            KeymapLookup::Unbound => {
                // Printable character fallback: insert if no ctrl/alt modifier
                let allow_insert = match self.focus {
                    PanelSlot::Main => self.has_tabs(),
                    PanelSlot::StatusBar => self
                        .status_bar_component()
                        .and_then(|c| c.context_name())
                        .is_some(),
                    PanelSlot::Side => {
                        self.side_component().and_then(|c| c.context_name()) == Some("file_search")
                    }
                    PanelSlot::Overlay => false,
                };
                if allow_insert {
                    let has_ctrl_alt = key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                    if let KeyCode::Char(c) = key.code {
                        if !has_ctrl_alt {
                            return self.execute_action(Action::InsertChar(c));
                        }
                    }
                }
                InputResult::Continue
            }
        }
    }

    fn execute_action(&mut self, action: Action) -> InputResult {
        match action {
            // Shell-level actions
            Action::ToggleFocus => {
                if self.show_side_panel {
                    let new_focus = match self.focus {
                        PanelSlot::Main => PanelSlot::Side,
                        PanelSlot::Side if self.has_tabs() => PanelSlot::Main,
                        _ => return InputResult::Continue,
                    };
                    self.set_focus(new_focus);
                }
            }
            Action::ToggleSidePanel => {
                self.show_side_panel = !self.show_side_panel;
                if !self.show_side_panel && self.has_tabs() {
                    self.set_focus(PanelSlot::Main);
                }
            }
            Action::Quit => return InputResult::Quit,
            Action::Suspend => return InputResult::Suspend,

            Action::PrevTab => {
                let count = self.tabbed_components().len();
                self.tabs.prev(count);
                self.notify_active_buffer();
            }
            Action::NextTab => {
                let count = self.tabbed_components().len();
                self.tabs.next(count);
                self.notify_active_buffer();
            }

            Action::KillBuffer => {
                if self.has_tabs() {
                    if let Some(idx) = self.active_tab_component_idx() {
                        let tab = self.components[idx].tab();
                        if tab.as_ref().map_or(false, |t| t.path.is_none()) {
                            // Virtual buffer (e.g. *Messages*) — let the component handle hiding
                            let effects = self.dispatch_action(Action::KillBuffer);
                            self.process_effects(effects);
                        } else if tab.map_or(false, |t| t.dirty) {
                            self.modal = Some(Modal {
                                prompt: "Buffer modified; kill anyway? (yes/no)".into(),
                                input: String::new(),
                                action: PendingAction::KillBuffer,
                            });
                        } else {
                            self.kill_current_buffer();
                        }
                    }
                }
            }

            Action::OpenFileSearch => {
                self.show_side_panel = true;
                // Always dispatch to the active buffer (not the focused component)
                // so it can grab selected text and emit FileSearchOpened
                let idx = self.active_tab_component_idx();
                let effects = if let Some(idx) = idx {
                    let mut ctx = make_ctx(&self.env, &mut self.docs);
                    self.components[idx].handle_action(Action::OpenFileSearch, &mut ctx)
                } else {
                    vec![Effect::Emit(Event::FileSearchOpened {
                        selected_text: None,
                    })]
                };
                self.process_effects(effects);
                self.set_focus(PanelSlot::Side);
            }

            Action::FindFile => {
                let dir = self
                    .active_buffer_mut()
                    .and_then(|c| c.tab())
                    .and_then(|t| t.path)
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                    .unwrap_or_else(|| self.env.root.clone());
                let effects = vec![Effect::Emit(Event::FindFileOpened { dir })];
                self.process_effects(effects);
                self.set_focus(PanelSlot::StatusBar);
            }

            Action::Abort => {
                if self.modal.is_some() {
                    self.modal = None;
                    self.set_message("Aborted");
                } else {
                    self.message = None;
                    // Dispatch to active buffer to clear mark
                    let effects = self.dispatch_action(Action::Abort);
                    self.process_effects(effects);
                }
            }

            Action::OpenMessages => {
                self.process_effects(vec![Effect::Emit(Event::OpenMessages)]);
                if let Some(idx) = self
                    .components
                    .iter()
                    .position(|c| c.tab().map_or(false, |t| t.label == "*Messages*"))
                {
                    let tabbed = self.tabbed_components();
                    self.tabs.activate(idx, &tabbed);
                    self.set_focus(PanelSlot::Main);
                }
            }

            Action::JumpBack => {
                if let Some((path, row, col, scroll_offset)) = self.active_buffer_position() {
                    self.process_effects(vec![Effect::Emit(Event::JumpBack {
                        path,
                        row,
                        col,
                        scroll_offset,
                    })]);
                }
            }
            Action::JumpForward => {
                self.process_effects(vec![Effect::Emit(Event::JumpForward)]);
            }

            // All other actions → dispatch to focused component
            _ => {
                let effects = self.dispatch_action(action);
                self.process_effects(effects);
            }
        }
        InputResult::Continue
    }

    fn dispatch_action(&mut self, action: Action) -> Vec<Effect> {
        if self.focus == PanelSlot::Main {
            let tabbed = self.tabbed_components();
            self.tabs.touch_active(&tabbed);
        }
        self.pending_flush = true;

        let idx = match self.focus {
            PanelSlot::Main => self.active_tab_component_idx(),
            PanelSlot::StatusBar => self
                .status_bar_component_idx()
                .or_else(|| self.active_tab_component_idx()),
            PanelSlot::Side => self.side_component_idx(),
            PanelSlot::Overlay => self.overlay_component_idx(),
        };

        if let Some(idx) = idx {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            self.components[idx].handle_action(action, &mut ctx)
        } else {
            vec![]
        }
    }

    fn process_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Emit(event) => {
                    // Intercept SetDiagnostics to track per-file severity
                    if let Event::SetDiagnostics {
                        ref path,
                        ref diagnostics,
                    } = event
                    {
                        let worst = worst_diagnostic_severity(diagnostics);
                        self.file_statuses
                            .set_diagnostic_severity(path.clone(), worst);
                    }
                    if matches!(&event, Event::ConfirmSearch { .. }) {
                        self.tabs.clear_preview();
                    }
                    let tabbed = self.tabbed_components();
                    let prev_comp = self.tabs.active_component(&tabbed);
                    let prev_active = self.tabs.active;
                    let mut more_effects = Vec::new();
                    let mut ctx = make_ctx(&self.env, &mut self.docs);
                    for comp in &mut self.components {
                        more_effects.extend(comp.handle_event(&event, &mut ctx));
                    }
                    self.process_effects(more_effects);
                    let tabbed = self.tabbed_components();
                    self.tabs.stabilize(prev_active, prev_comp, &tabbed);
                    if matches!(&event, Event::PreviewClosed) {
                        self.tabs.restore_preview(tabbed.len());
                    }
                    if matches!(&event, Event::PreviewPromoted) {
                        self.tabs.clear_preview();
                    }
                }
                Effect::Spawn(component) => {
                    self.register(component);
                }
                Effect::SetMessage(msg) => {
                    self.set_message(&msg);
                }
                Effect::SetLspStatus(status) => {
                    self.lsp_status = Some(status);
                }
                Effect::FocusPanel(slot) => {
                    self.set_focus(slot);
                }
                Effect::ConfirmAction { prompt, action } => {
                    self.modal = Some(Modal {
                        prompt,
                        input: String::new(),
                        action: PendingAction::Confirmed(action),
                    });
                }
                Effect::ActivateBuffer(path) => {
                    self.tabs.save_preview();
                    if let Some(idx) = self.components.iter().position(|c| {
                        c.tab().and_then(|t| t.path).as_deref() == Some(path.as_path())
                    }) {
                        let tabbed = self.tabbed_components();
                        self.tabs.activate(idx, &tabbed);
                    }
                }
                Effect::KillPreview => {
                    if let Some(idx) = find_preview_idx(&self.components) {
                        self.components.remove(idx);
                        self.tabs.remove(idx);
                        self.tabs.clamp(self.tabbed_components().len());
                    }
                }
                Effect::SetFileStatuses { statuses, branch } => {
                    self.file_statuses.set_file_statuses(statuses);
                    self.file_statuses.branch = branch;
                }
                Effect::SetLineStatuses { path, statuses } => {
                    self.file_statuses.set_line_statuses(path, statuses);
                }
                Effect::Quit => {
                    // Handled at top level
                }
                Effect::PromptRename {
                    prompt,
                    initial,
                    path,
                    row,
                    col,
                } => {
                    self.rename_modal = Some(RenameModal {
                        prompt,
                        input: initial,
                        path,
                        row,
                        col,
                    });
                }
            }
        }
    }

    fn kill_current_buffer(&mut self) {
        if let Some(comp_idx) = self.active_tab_component_idx() {
            let label = self.components[comp_idx]
                .tab()
                .map(|t| t.label.clone())
                .unwrap_or_default();
            // Let the component clean up its persisted state
            {
                let mut ctx = make_ctx(&self.env, &mut self.docs);
                self.components[comp_idx].handle_action(Action::KillBuffer, &mut ctx);
            }
            // Emit BufferClosed before removing the component
            if let Some(path) = self.components[comp_idx].tab().and_then(|t| t.path.clone()) {
                let effects = vec![Effect::Emit(Event::BufferClosed(path.clone()))];
                self.process_effects(effects);
            }
            self.components.remove(comp_idx);
            self.tabs.remove(comp_idx);
            let tabbed = self.tabbed_components();
            self.tabs.clamp(tabbed.len());
            if tabbed.is_empty() {
                self.set_focus(PanelSlot::Side);
            }
            self.set_message(&format!("Killed {label}"));
            self.notify_active_buffer();
        }
    }

    fn tab_display_width(tab: &led_core::TabDescriptor) -> u16 {
        let char_count = tab.label.chars().count() + 1; // +1 for lead char
        (char_count.min(15) + 1) as u16 // +1 for trailing space
    }

    fn total_tab_bar_width(&self) -> u16 {
        let tabs = self.tabbed_components();
        let mut width: u16 = 0;
        for (i, &comp_idx) in tabs.iter().enumerate() {
            if let Some(tab) = self.components[comp_idx].tab() {
                if i > 0 {
                    width += 1; // gap
                }
                width += Self::tab_display_width(&tab);
            }
        }
        width
    }

    fn evict_for_new_tab(&mut self, new_component: &Box<dyn Component>) {
        if self.tabs.bar_width == 0 {
            return;
        }
        let gutter_offset: u16 = 1; // GUTTER_WIDTH - 1
        let available = self.tabs.bar_width.saturating_sub(gutter_offset);
        let new_tab_width = new_component
            .tab()
            .map(|t| Self::tab_display_width(&t))
            .unwrap_or(0);

        loop {
            let existing_width = self.total_tab_bar_width();
            let gap = if existing_width > 0 { 1u16 } else { 0 };
            let total = existing_width + gap + new_tab_width;
            if total <= available {
                break;
            }
            // Find oldest non-dirty, non-preview, non-active tabbed component
            let active_comp_idx = self.active_tab_component_idx();
            let tabbed = self.tabbed_components();
            let candidate = tabbed
                .iter()
                .filter(|&&ci| {
                    Some(ci) != active_comp_idx
                        && self.components[ci]
                            .tab()
                            .map_or(true, |t| !t.dirty && !t.preview)
                })
                .min_by_key(|&&ci| self.tabs.last_touched[ci])
                .copied();
            if let Some(ci) = candidate {
                let removed_tab_idx = self.tabs.tab_index_of(ci, &tabbed);
                self.components.remove(ci);
                self.tabs.remove(ci);
                self.tabs
                    .adjust_after_removal(removed_tab_idx, self.tabbed_components().len());
            } else {
                break; // no eviction candidates
            }
        }
    }

    pub fn set_tab_bar_width(&mut self, width: u16) {
        self.tabs.bar_width = width;
    }

    pub fn notify_active_buffer(&mut self) {
        let tabbed = self.tabbed_components();
        self.tabs.touch_active(&tabbed);
        if let Some(idx) = self.tabs.active_component(&tabbed) {
            let path = self.components[idx].tab().and_then(|t| t.path);
            let effects = vec![Effect::Emit(led_core::Event::TabActivated { path })];
            self.process_effects(effects);
        }
    }

    // --- Session helpers ---

    pub fn save_all_sessions(&mut self) {
        let kv = {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            for i in 0..self.components.len() {
                self.components[i].handle_action(Action::SaveSession, &mut ctx);
            }
            std::mem::take(&mut ctx.kv)
        };
        if let Some(conn) = self.env.db.as_ref() {
            crate::session::save_kv(conn, &self.env.root, &kv);
        }
    }

    pub fn restore_sidepanel_sessions(&mut self) {
        let kv = self
            .env
            .db
            .as_ref()
            .map(|conn| crate::session::load_kv(conn, &self.env.root))
            .unwrap_or_default();
        for i in 0..self.components.len() {
            if self.components[i].tab().is_some() {
                continue;
            }
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            ctx.kv = kv.clone();
            self.components[i].handle_action(Action::RestoreSession, &mut ctx);
        }
    }

    pub fn capture_session(&self) -> SessionSnapshot {
        SessionSnapshot {
            active_tab: self.tabs.active,
            focus: self.focus,
            show_side_panel: self.show_side_panel,
        }
    }

    pub fn activate_buffer_by_path(&mut self, path: &std::path::Path) {
        if let Some(idx) = self
            .components
            .iter()
            .position(|c| c.tab().and_then(|t| t.path).as_deref() == Some(path))
        {
            let tabbed = self.tabbed_components();
            self.tabs.activate(idx, &tabbed);
        }
    }

    pub fn set_active_tab(&mut self, tab: usize) {
        let count = self.tabbed_components().len();
        self.tabs.set_active(tab, count);
    }

    pub fn set_focus(&mut self, focus: PanelSlot) {
        if self.focus == focus {
            return;
        }
        let old = self.focus;
        self.focus = focus;
        self.notify_focus_change(old, focus);
    }

    fn notify_focus_change(&mut self, old: PanelSlot, new: PanelSlot) {
        let mut effects = Vec::new();

        let old_idx = match old {
            PanelSlot::Main => self.active_tab_component_idx(),
            PanelSlot::StatusBar => self
                .status_bar_component_idx()
                .or_else(|| self.active_tab_component_idx()),
            PanelSlot::Side => self.side_component_idx(),
            PanelSlot::Overlay => self.overlay_component_idx(),
        };
        if let Some(idx) = old_idx {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            effects.extend(self.components[idx].handle_action(Action::FocusLost, &mut ctx));
        }

        let new_idx = match new {
            PanelSlot::Main => self.active_tab_component_idx(),
            PanelSlot::StatusBar => self
                .status_bar_component_idx()
                .or_else(|| self.active_tab_component_idx()),
            PanelSlot::Side => self.side_component_idx(),
            PanelSlot::Overlay => self.overlay_component_idx(),
        };
        if let Some(idx) = new_idx {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            effects.extend(self.components[idx].handle_action(Action::FocusGained, &mut ctx));
        }

        self.process_effects(effects);
    }

    pub fn set_message(&mut self, msg: &str) {
        self.message = Some((msg.to_string(), Instant::now()));
    }

    pub fn message_text(&self) -> Option<&str> {
        if let Some((ref text, instant)) = self.message {
            if instant.elapsed() < Duration::from_millis(1500) {
                return Some(text);
            }
        }
        None
    }

    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn set_viewport_height(&mut self, h: usize) {
        self.env.viewport_height = h;
    }

    pub fn debug_flash_text(&self) -> Option<&str> {
        if let Some((ref text, instant)) = self.debug_flash {
            if instant.elapsed().as_millis() < 500 {
                return Some(text);
            }
        }
        None
    }

    pub fn needs_redraw_in(&self) -> Option<Duration> {
        let mut soonest: Option<Duration> = None;
        let mut consider = |deadline: Duration, elapsed: Duration| {
            if elapsed < deadline {
                let remaining = deadline - elapsed;
                soonest = Some(match soonest {
                    Some(s) => s.min(remaining),
                    None => remaining,
                });
            }
        };
        if let Some((_, instant)) = &self.debug_flash {
            consider(Duration::from_millis(500), instant.elapsed());
        }
        if let Some((_, instant)) = &self.message {
            consider(Duration::from_millis(1500), instant.elapsed());
        }
        // Redraw periodically while LSP spinner is active.
        if self.lsp_status.as_ref().map_or(false, |s| s.busy) {
            return Some(soonest.map_or(Duration::from_millis(80), |s| {
                s.min(Duration::from_millis(80))
            }));
        }
        soonest
    }

    pub fn needs_persist_in(&self) -> Option<Duration> {
        if !self.pending_flush {
            return None;
        }
        let elapsed = self.last_persist.elapsed();
        let deadline = Duration::from_millis(200);
        if elapsed >= deadline {
            Some(Duration::ZERO)
        } else {
            Some(deadline - elapsed)
        }
    }

    pub fn needs_persist(&self) -> bool {
        self.pending_flush && self.last_persist.elapsed() >= Duration::from_millis(200)
    }

    pub fn flush_to_db(&mut self) {
        for i in 0..self.components.len() {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            self.components[i].handle_action(Action::Flush, &mut ctx);
        }
        self.pending_flush = false;
        self.last_persist = Instant::now();
    }

    pub fn db(&self) -> Option<&Connection> {
        self.env.db.as_ref()
    }

    pub fn waker(&self) -> Option<&Waker> {
        self.env.waker.as_ref()
    }

    fn active_buffer_position(&self) -> Option<(PathBuf, usize, usize, usize)> {
        let idx = self.active_tab_component_idx()?;
        let path = self.components[idx].tab()?.path?;
        let (row, col, scroll_offset) = self.components[idx].cursor_position()?;
        Some((path, row, col, scroll_offset))
    }

    pub fn emit(&mut self, event: Event) {
        self.process_effects(vec![Effect::Emit(event)]);
    }

    pub fn run_action(&mut self, action: Action) -> InputResult {
        self.execute_action(action)
    }

    pub fn tick(&mut self) {
        let mut all_effects = Vec::new();
        for i in 0..self.components.len() {
            let mut ctx = make_ctx(&self.env, &mut self.docs);
            let effects = self.components[i].handle_action(Action::Tick, &mut ctx);
            all_effects.extend(effects);
        }
        self.process_effects(all_effects);
    }
}

pub struct SessionSnapshot {
    pub active_tab: usize,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
}
