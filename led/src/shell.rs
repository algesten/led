use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::Connection;

use std::sync::Arc;

use crate::config::{KeyCombo, Keymap, KeymapLookup};
use crate::theme::Theme;
use led_core::{Action, Clipboard, Component, Context, Effect, Event, PanelSlot, Waker};

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
    last_persist: Instant,
    pending_flush: bool,
    pub single_file_mode: bool,
    pre_preview_tab: Option<usize>,
    tab_bar_width: u16,
    env: Env,
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
            last_persist: Instant::now(),
            pending_flush: false,
            single_file_mode: false,
            pre_preview_tab: None,
            tab_bar_width: 0,
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
        if has_tab && self.single_file_mode {
            self.single_file_mode = false;
        }
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

        // Debug flash
        if self.debug {
            let display = if let ChordState::Pending(prefix) = &self.chord {
                format!("{} -> {}", prefix.display_name(), combo.display_name())
            } else {
                combo.display_name()
            };
            self.debug_flash = Some((display, Instant::now()));
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
            PanelSlot::Main | PanelSlot::StatusBar => None,
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
                    PanelSlot::Main | PanelSlot::StatusBar => self.has_tabs(),
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
                    self.notify_active_buffer();
                }
            }
            Action::NextTab => {
                let tabs = self.tabbed_components();
                if tabs.len() > 1 {
                    self.active_tab = (self.active_tab + 1) % tabs.len();
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
            PanelSlot::Main | PanelSlot::StatusBar => {
                if let Some(idx) = self.active_tab_component_idx() {
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
                    let mut more_effects = Vec::new();
                    let mut ctx = self.env.ctx();
                    for comp in &mut self.components {
                        more_effects.extend(comp.handle_event(&event, &mut ctx));
                    }
                    self.process_effects(more_effects);
                    // Post-broadcast hook for preview tab management
                    match &event {
                        Event::PreviewClosed => {
                            if let Some(tab) = self.pre_preview_tab.take() {
                                let tabs = self.tabbed_components();
                                if !tabs.is_empty() {
                                    self.active_tab = tab.min(tabs.len() - 1);
                                }
                            }
                        }
                        Event::ConfirmSearch { .. } => {
                            self.pre_preview_tab = None;
                        }
                        _ => {}
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
                Effect::Quit => {
                    // Handled at top level
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

    fn notify_active_buffer(&mut self) {
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
            PanelSlot::Main | PanelSlot::StatusBar => self.active_tab_component_idx(),
            PanelSlot::Side => self.side_component_idx(),
        };
        if let Some(idx) = old_idx {
            let mut ctx = self.env.ctx();
            effects.extend(self.components[idx].handle_action(Action::FocusLost, &mut ctx));
        }

        let new_idx = match new {
            PanelSlot::Main | PanelSlot::StatusBar => self.active_tab_component_idx(),
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
