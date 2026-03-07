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
    Action, Clipboard, Component, Context, Effect, Event, FileStatusStore, PanelSlot, Waker,
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

pub struct PickerModal {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    pub source_path: PathBuf,
}

struct Env {
    db: Option<Connection>,
    root: PathBuf,
    viewport_height: usize,
    clipboard: Arc<dyn Clipboard>,
    waker: Option<Waker>,
}

impl Env {
    fn ctx(&self) -> Context<'_> {
        Context {
            db: self.db.as_ref(),
            root: &self.root,
            viewport_height: self.viewport_height,
            clipboard: self.clipboard.as_ref(),
            waker: self.waker.clone(),
            kv: std::collections::HashMap::new(),
        }
    }
}

pub struct Shell {
    components: Vec<Box<dyn Component>>,
    last_touched: Vec<Instant>,
    active_tab: usize,
    pub message: Option<String>,
    chord: ChordState,
    keymap: Keymap,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub debug: bool,
    pub theme: Theme,
    debug_flash: Option<(String, Instant)>,
    pub modal: Option<Modal>,
    pub rename_modal: Option<RenameModal>,
    pub picker_modal: Option<PickerModal>,
    last_persist: Instant,
    pending_flush: bool,
    pre_preview_tab: Option<usize>,
    tab_bar_width: u16,
    env: Env,
    pub file_statuses: FileStatusStore,
}

impl Shell {
    pub fn new(keymap: Keymap, theme: Theme, db: Option<Connection>, root: PathBuf) -> Self {
        Self {
            components: Vec::new(),
            last_touched: Vec::new(),
            active_tab: 0,
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
            picker_modal: None,
            last_persist: Instant::now(),
            pending_flush: false,
            pre_preview_tab: None,
            tab_bar_width: 0,
            file_statuses: FileStatusStore::default(),
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
        if is_preview && self.pre_preview_tab.is_none() {
            self.pre_preview_tab = Some(self.active_tab);
        }
        // Dedup by path: if a tab with the same path exists, just focus it
        if let Some(path) = component.tab().and_then(|t| t.path) {
            if let Some(idx) = self
                .components
                .iter()
                .position(|c| c.tab().and_then(|t| t.path).as_ref() == Some(&path))
            {
                self.activate_tab_for_component(idx);
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
        self.last_touched.push(Instant::now());
        let last = self.components.len() - 1;
        if has_tab {
            let mut ctx = self.env.ctx();
            self.components[last].handle_action(Action::RestoreSession, &mut ctx);
            self.active_tab = self.tabbed_index_of(last).unwrap_or(0);
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

    /// Map a component index to its tab index.
    fn tabbed_index_of(&self, component_idx: usize) -> Option<usize> {
        self.tabbed_components()
            .iter()
            .position(|&i| i == component_idx)
    }

    /// Get the component index for the active tab.
    fn active_tab_component_idx(&self) -> Option<usize> {
        self.tabbed_components().get(self.active_tab).copied()
    }

    /// Get a mutable reference to the active tab's component.
    pub fn active_buffer_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        let idx = self.active_tab_component_idx()?;
        Some(&mut self.components[idx])
    }

    fn activate_tab_for_component(&mut self, component_idx: usize) {
        if let Some(tab_idx) = self.tabbed_index_of(component_idx) {
            self.active_tab = tab_idx;
        }
    }

    /// Find the component claiming the Side panel (highest priority).
    pub fn side_component(&self) -> Option<&Box<dyn Component>> {
        let idx = self.side_component_idx()?;
        Some(&self.components[idx])
    }

    fn side_component_idx(&self) -> Option<usize> {
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

    pub fn side_component_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        let idx = self.side_component_idx()?;
        Some(&mut self.components[idx])
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

    pub fn status_bar_component_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        let idx = self.status_bar_component_idx()?;
        Some(&mut self.components[idx])
    }

    pub fn components(&self) -> &[Box<dyn Component>] {
        &self.components
    }

    pub fn active_tab(&self) -> usize {
        self.active_tab
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
            let is_abort = matches!(key.code, KeyCode::Esc)
                || (key.code == KeyCode::Char('g')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort {
                self.rename_modal = None;
                self.message = Some("Aborted.".into());
            } else if key.code == KeyCode::Enter {
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
            } else if key.code == KeyCode::Backspace {
                if let Some(ref mut modal) = self.rename_modal {
                    modal.input.pop();
                }
            } else if let KeyCode::Char(c) = key.code {
                let has_ctrl_alt = key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if !has_ctrl_alt {
                    if let Some(ref mut modal) = self.rename_modal {
                        modal.input.push(c);
                    }
                }
            }
            return InputResult::Continue;
        }

        // Handle picker modal input
        if self.picker_modal.is_some() {
            let is_abort = matches!(key.code, KeyCode::Esc)
                || (key.code == KeyCode::Char('g')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort {
                self.picker_modal = None;
            } else if key.code == KeyCode::Enter {
                let modal = self.picker_modal.take().unwrap();
                let effects = vec![Effect::Emit(Event::LspCodeActionResolve {
                    path: modal.source_path,
                    index: modal.selected,
                })];
                self.process_effects(effects);
            } else if key.code == KeyCode::Up {
                if let Some(ref mut modal) = self.picker_modal {
                    if modal.selected > 0 {
                        modal.selected -= 1;
                    }
                }
            } else if key.code == KeyCode::Down {
                if let Some(ref mut modal) = self.picker_modal {
                    if modal.selected + 1 < modal.items.len() {
                        modal.selected += 1;
                    }
                }
            }
            return InputResult::Continue;
        }

        // Handle modal input
        if self.modal.is_some() {
            let is_abort = matches!(key.code, KeyCode::Esc)
                || (key.code == KeyCode::Char('g')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort {
                self.modal = None;
                self.message = Some("Aborted.".into());
            } else if key.code == KeyCode::Enter {
                let confirmed = self.modal.as_ref().unwrap().input == "yes";
                if confirmed {
                    // Take the modal to own the pending action
                    let modal = self.modal.take().unwrap();
                    match modal.action {
                        PendingAction::KillBuffer => self.kill_current_buffer(),
                        PendingAction::Confirmed(action) => {
                            let effects = self.dispatch_action(action);
                            self.process_effects(effects);
                        }
                    }
                } else {
                    self.message = Some("Aborted.".into());
                }
                self.modal = None;
            } else if key.code == KeyCode::Backspace {
                if let Some(ref mut modal) = self.modal {
                    modal.input.pop();
                }
            } else if let KeyCode::Char(c) = key.code {
                let has_ctrl_alt = key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if !has_ctrl_alt {
                    if let Some(ref mut modal) = self.modal {
                        modal.input.push(c);
                    }
                }
            }
            return InputResult::Continue;
        }

        // Handle chord state
        if let ChordState::Pending(prefix) = self.chord {
            self.chord = ChordState::None;
            if let Some(action) = self.keymap.lookup_chord(&prefix, &combo) {
                return self.execute_action(action);
            }
            self.message = Some("Unknown chord.".into());
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
                let tabs = self.tabbed_components();
                if tabs.len() > 1 {
                    if self.active_tab == 0 {
                        self.active_tab = tabs.len() - 1;
                    } else {
                        self.active_tab -= 1;
                    }
                    self.pre_preview_tab = None;
                    self.notify_active_buffer();
                }
            }
            Action::NextTab => {
                let tabs = self.tabbed_components();
                if tabs.len() > 1 {
                    self.active_tab = (self.active_tab + 1) % tabs.len();
                    self.pre_preview_tab = None;
                    self.notify_active_buffer();
                }
            }

            Action::KillBuffer => {
                if self.has_tabs() {
                    // Check if the active tab component is dirty
                    let is_dirty = self
                        .active_tab_component_idx()
                        .and_then(|idx| self.components[idx].tab())
                        .map_or(false, |t| t.dirty);
                    if is_dirty {
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

            Action::OpenFileSearch => {
                self.show_side_panel = true;
                // Always dispatch to the active buffer (not the focused component)
                // so it can grab selected text and emit FileSearchOpened
                let effects = if let Some(idx) = self.active_tab_component_idx() {
                    let mut ctx = self.env.ctx();
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
                    self.message = Some("Aborted.".into());
                } else {
                    self.message = None;
                    // Dispatch to active buffer to clear mark
                    let effects = self.dispatch_action(Action::Abort);
                    self.process_effects(effects);
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
            self.touch_active_buffer();
        }
        self.pending_flush = true;
        let mut ctx = self.env.ctx();

        match self.focus {
            PanelSlot::Main => {
                if let Some(idx) = self.active_tab_component_idx() {
                    self.components[idx].handle_action(action, &mut ctx)
                } else {
                    vec![]
                }
            }
            PanelSlot::StatusBar => {
                if let Some(idx) = self.status_bar_component_idx() {
                    self.components[idx].handle_action(action, &mut ctx)
                } else if let Some(idx) = self.active_tab_component_idx() {
                    self.components[idx].handle_action(action, &mut ctx)
                } else {
                    vec![]
                }
            }
            PanelSlot::Side => {
                if let Some(idx) = self.side_component_idx() {
                    self.components[idx].handle_action(action, &mut ctx)
                } else {
                    vec![]
                }
            }
        }
    }

    fn process_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Emit(event) => {
                    // Intercept ShowCodeActions to open picker modal
                    if let Event::ShowCodeActions {
                        ref path,
                        ref actions,
                    } = event
                    {
                        self.picker_modal = Some(PickerModal {
                            title: "Code Actions".into(),
                            items: actions.iter().map(|a| a.title.clone()).collect(),
                            selected: 0,
                            source_path: path.clone(),
                        });
                        continue;
                    }
                    // Pre-broadcast: clear preview state before ConfirmSearch
                    // effects run, so the FocusLost → PreviewClosed cascade
                    // won't restore the pre-preview tab.
                    if matches!(&event, Event::ConfirmSearch { .. }) {
                        self.pre_preview_tab = None;
                    }
                    let mut more_effects = Vec::new();
                    let mut ctx = self.env.ctx();
                    for comp in &mut self.components {
                        more_effects.extend(comp.handle_event(&event, &mut ctx));
                    }
                    self.process_effects(more_effects);
                    // Post-broadcast hook for preview tab management
                    if let Event::PreviewClosed = &event {
                        if let Some(tab) = self.pre_preview_tab.take() {
                            let tabs = self.tabbed_components();
                            if !tabs.is_empty() {
                                self.active_tab = tab.min(tabs.len() - 1);
                            }
                        }
                    }
                    if let Event::PreviewPromoted = &event {
                        self.pre_preview_tab = None;
                    }
                }
                Effect::Spawn(component) => {
                    self.register(component);
                }
                Effect::SetMessage(msg) => {
                    self.message = Some(msg);
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
                    if self.pre_preview_tab.is_none() {
                        self.pre_preview_tab = Some(self.active_tab);
                    }
                    if let Some(idx) = self.components.iter().position(|c| {
                        c.tab().and_then(|t| t.path).as_deref() == Some(path.as_path())
                    }) {
                        self.activate_tab_for_component(idx);
                    }
                }
                Effect::KillPreview => {
                    if let Some(idx) = self
                        .components
                        .iter()
                        .position(|c| c.tab().map_or(false, |t| t.preview))
                    {
                        self.components.remove(idx);
                        self.last_touched.remove(idx);
                        let tabs = self.tabbed_components();
                        if tabs.is_empty() {
                            self.active_tab = 0;
                        } else if self.active_tab >= tabs.len() {
                            self.active_tab = tabs.len() - 1;
                        }
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
                Effect::ShowPicker {
                    title,
                    items,
                    source_path,
                } => {
                    self.picker_modal = Some(PickerModal {
                        title,
                        items,
                        selected: 0,
                        source_path,
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
            // Emit BufferClosed before removing the component
            if let Some(path) = self.components[comp_idx].tab().and_then(|t| t.path.clone()) {
                let effects = vec![Effect::Emit(Event::BufferClosed(path.clone()))];
                self.process_effects(effects);
                // Clear cursor/scroll data from the DB so reopening starts fresh
                if let Some(conn) = self.env.db.as_ref() {
                    let root_str = self.env.root.to_string_lossy();
                    let file_str = path.to_string_lossy();
                    let _ = conn.execute(
                        "DELETE FROM buffers WHERE root_path = ?1 AND file_path = ?2",
                        rusqlite::params![&*root_str, &*file_str],
                    );
                }
            }
            self.components.remove(comp_idx);
            self.last_touched.remove(comp_idx);

            let tabs = self.tabbed_components();
            if tabs.is_empty() {
                self.active_tab = 0;
                self.set_focus(PanelSlot::Side);
            } else if self.active_tab >= tabs.len() {
                self.active_tab = tabs.len() - 1;
            }
            self.message = Some(format!("Killed {label}."));
            self.notify_active_buffer();
        }
    }

    fn touch_active_buffer(&mut self) {
        if let Some(idx) = self.active_tab_component_idx() {
            self.last_touched[idx] = Instant::now();
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
        if self.tab_bar_width == 0 {
            return;
        }
        let gutter_offset: u16 = 1; // GUTTER_WIDTH - 1
        let available = self.tab_bar_width.saturating_sub(gutter_offset);
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
            let tabs = self.tabbed_components();
            let candidate = tabs
                .iter()
                .filter(|&&ci| {
                    Some(ci) != active_comp_idx
                        && self.components[ci]
                            .tab()
                            .map_or(true, |t| !t.dirty && !t.preview)
                })
                .min_by_key(|&&ci| self.last_touched[ci])
                .copied();
            if let Some(ci) = candidate {
                // Adjust active_tab before removal
                let removed_tab_idx = self.tabbed_index_of(ci);
                self.components.remove(ci);
                self.last_touched.remove(ci);
                // Fix active_tab after removal
                let tabs = self.tabbed_components();
                if let Some(rt) = removed_tab_idx {
                    if rt < self.active_tab {
                        self.active_tab -= 1;
                    } else if self.active_tab >= tabs.len() && !tabs.is_empty() {
                        self.active_tab = tabs.len() - 1;
                    }
                }
            } else {
                break; // no eviction candidates
            }
        }
    }

    pub fn set_tab_bar_width(&mut self, width: u16) {
        self.tab_bar_width = width;
    }

    pub fn notify_active_buffer(&mut self) {
        self.touch_active_buffer();
        if let Some(idx) = self.active_tab_component_idx() {
            let path = self.components[idx].tab().and_then(|t| t.path);
            let effects = vec![Effect::Emit(led_core::Event::TabActivated { path })];
            self.process_effects(effects);
        }
    }

    // --- Session helpers ---

    pub fn save_all_sessions(&mut self) {
        let mut ctx = self.env.ctx();
        for i in 0..self.components.len() {
            self.components[i].handle_action(Action::SaveSession, &mut ctx);
        }
        if let Some(conn) = self.env.db.as_ref() {
            crate::session::save_kv(conn, &self.env.root, &ctx.kv);
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
            let mut ctx = self.env.ctx();
            ctx.kv = kv.clone();
            self.components[i].handle_action(Action::RestoreSession, &mut ctx);
        }
    }

    pub fn capture_session(&self) -> SessionSnapshot {
        SessionSnapshot {
            active_tab: self.active_tab,
            focus: self.focus,
            show_side_panel: self.show_side_panel,
        }
    }

    pub fn set_active_tab(&mut self, tab: usize) {
        let tabs = self.tabbed_components();
        if !tabs.is_empty() {
            self.active_tab = tab.min(tabs.len() - 1);
        }
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
        };
        if let Some(idx) = old_idx {
            let mut ctx = self.env.ctx();
            effects.extend(self.components[idx].handle_action(Action::FocusLost, &mut ctx));
        }

        let new_idx = match new {
            PanelSlot::Main => self.active_tab_component_idx(),
            PanelSlot::StatusBar => self
                .status_bar_component_idx()
                .or_else(|| self.active_tab_component_idx()),
            PanelSlot::Side => self.side_component_idx(),
        };
        if let Some(idx) = new_idx {
            let mut ctx = self.env.ctx();
            effects.extend(self.components[idx].handle_action(Action::FocusGained, &mut ctx));
        }

        self.process_effects(effects);
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
        if let Some((_, instant)) = &self.debug_flash {
            let elapsed = instant.elapsed();
            let deadline = Duration::from_millis(500);
            if elapsed < deadline {
                return Some(deadline - elapsed);
            }
        }
        None
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
            let mut ctx = self.env.ctx();
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

    pub fn tick(&mut self) {
        let mut all_effects = Vec::new();
        for i in 0..self.components.len() {
            let mut ctx = self.env.ctx();
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
