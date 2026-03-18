use std::cell::Cell;
use std::sync::Arc;

use crossterm::event::KeyCode;
use led_core::Action;
use led_core::PanelSlot;
use led_core::keys::{KeyCombo, KeymapLookup};
use led_core::rx::Stream;
use led_state::AppState;
use led_terminal_in::TerminalInput;

use super::Mut;

/// Derive actions from terminal input + state (for keymap context).
pub fn actions_of(input: &Stream<TerminalInput>, state: &Stream<Arc<AppState>>) -> Stream<Mut> {
    // Resize doesn't need state — extract it directly so it's never lost
    let resize_s = input.filter_map(|i| match i {
        TerminalInput::Resize(w, h) => Some(Mut::Resize(w, h)),
        _ => None,
    });

    // Key events need the keymap from state
    let chord: Cell<Option<KeyCombo>> = Cell::new(None);
    let key_input_s = input
        .filter_map(|i| match i {
            TerminalInput::Key(combo) => Some(combo),
            _ => None,
        })
        .sample_combine(state)
        .filter_map(move |(combo, state)| map_key(combo, &state, &chord))
        .map(|a| Mut::Action(a));

    resize_s.or(key_input_s)
}

fn map_key(combo: KeyCombo, state: &AppState, chord: &Cell<Option<KeyCombo>>) -> Option<Action> {
    let keymap = state.keymap.as_ref()?;
    let context = resolve_context(state);

    if let Some(prefix) = chord.take() {
        return keymap.lookup_chord(&prefix, &combo);
    }

    match keymap.lookup(&combo, context) {
        KeymapLookup::Action(action) => Some(action),
        KeymapLookup::ChordPrefix => {
            chord.set(Some(combo));
            None
        }
        KeymapLookup::Unbound => {
            if allow_char_insert(state) && !combo.ctrl && !combo.alt {
                if let KeyCode::Char(c) = combo.code {
                    return match c {
                        '}' | ')' | ']' => Some(Action::InsertCloseBracket(c)),
                        _ => Some(Action::InsertChar(c)),
                    };
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
