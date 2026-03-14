use std::cell::Cell;
use std::sync::Arc;

use crossterm::event::KeyCode;
use led_core::PanelSlot;
use led_core::keys::{KeyCombo, KeymapLookup};
use led_core::rx::Stream;
use led_core::Action;
use led_terminal_in::TerminalInput;
use led_state::AppState;

use super::Mut;

/// Derive actions from terminal input + state (for keymap context).
pub fn actions_of(
    input: &Stream<TerminalInput>,
    state: &Stream<Arc<AppState>>,
) -> Stream<Mut> {
    let chord: Cell<Option<KeyCombo>> = Cell::new(None);

    input.sample_combine(state).filter_map(move |(input, state)| {
        match map_input(input, &state, &chord) {
            Some(TerminalEvent::Action(a)) => Some(Mut::Action(a)),
            Some(TerminalEvent::Resize(w, h)) => Some(Mut::Resize(w, h)),
            None => None,
        }
    }).stream()
}

enum TerminalEvent {
    Action(Action),
    Resize(u16, u16),
}

fn map_input(
    input: TerminalInput,
    state: &AppState,
    chord: &Cell<Option<KeyCombo>>,
) -> Option<TerminalEvent> {
    let combo = match input {
        TerminalInput::Key(combo) => combo,
        TerminalInput::Resize(w, h) => return Some(TerminalEvent::Resize(w, h)),
        _ => return None,
    };

    let keymap = state.keymap.as_ref()?;
    let context = resolve_context(state);

    if let Some(prefix) = chord.take() {
        if let Some(action) = keymap.lookup_chord(&prefix, &combo) {
            return Some(TerminalEvent::Action(action));
        }
        return None;
    }

    match keymap.lookup(&combo, context) {
        KeymapLookup::Action(action) => Some(TerminalEvent::Action(action)),
        KeymapLookup::ChordPrefix => {
            chord.set(Some(combo));
            None
        }
        KeymapLookup::Unbound => {
            if allow_char_insert(state) && !combo.ctrl && !combo.alt {
                if let KeyCode::Char(c) = combo.code {
                    return Some(TerminalEvent::Action(Action::InsertChar(c)));
                }
            }
            None
        }
    }
}

fn resolve_context(state: &AppState) -> Option<&'static str> {
    match state.focus {
        PanelSlot::Side => Some("browser"),
        PanelSlot::Main => None,
        PanelSlot::StatusBar => None,
        PanelSlot::Overlay => None,
    }
}

fn allow_char_insert(state: &AppState) -> bool {
    match state.focus {
        PanelSlot::Main => true,
        _ => false,
    }
}
