use std::cell::Cell;
use std::sync::Arc;

use crossterm::event::KeyCode;
use led_core::PanelSlot;
use led_core::keys::{KeyCombo, KeymapLookup};
use led_core::{AStream, Action, StreamOpsExt};
use led_input::TerminalInput;
use led_state::AppState;
use tokio_stream::StreamExt;

pub fn actions_of(
    state: impl AStream<Arc<AppState>>,
    input: impl AStream<TerminalInput>,
) -> impl AStream<Action> {
    let chord: Cell<Option<KeyCombo>> = Cell::new(None);

    input
        .sample_combine(state)
        .filter_map(move |(input, state)| map_input(input, &state, &chord))
}

fn map_input(
    input: TerminalInput,
    state: &AppState,
    chord: &Cell<Option<KeyCombo>>,
) -> Option<Action> {
    let combo = match input {
        TerminalInput::Key(combo) => combo,
        // TODO: TerminalInput::Resize -> update viewport dimensions in state
        // TODO: TerminalInput::FocusGained -> trigger git refresh
        // TODO: TerminalInput::FocusLost -> (no action needed currently)
        // TODO: TerminalInput::Mouse -> mouse handling
        _ => return None,
    };

    let keymap = state.keymap.as_ref()?;

    // Determine context from current focus
    let context = resolve_context(state);

    // Handle chord state: if we're waiting for a chord's second key
    if let Some(prefix) = chord.take() {
        if let Some(action) = keymap.lookup_chord(&prefix, &combo) {
            return Some(action);
        }
        // Unknown chord second key — swallow it
        return None;
    }

    // Main keymap lookup
    match keymap.lookup(&combo, context) {
        KeymapLookup::Action(action) => Some(action),

        KeymapLookup::ChordPrefix => {
            chord.set(Some(combo));
            None
        }

        KeymapLookup::Unbound => {
            // Fallback: insert printable characters when in an insertable context
            if allow_char_insert(state) && !combo.ctrl && !combo.alt {
                if let KeyCode::Char(c) = combo.code {
                    return Some(Action::InsertChar(c));
                }
            }
            None
        }
    }
}

/// Determine the keymap context name from the current focus and active component.
fn resolve_context(state: &AppState) -> Option<&'static str> {
    match state.focus {
        PanelSlot::Side => {
            // TODO: if state.file_search.active { Some("file_search") } else
            Some("browser")
        }
        PanelSlot::Main => None,
        PanelSlot::StatusBar => None,
        PanelSlot::Overlay => None,
    }
}

/// Whether the current focus allows unbound keys to be inserted as characters.
fn allow_char_insert(state: &AppState) -> bool {
    match state.focus {
        PanelSlot::Main => true, // TODO: gate on state.has_tabs() once tabs exist
        PanelSlot::StatusBar => false, // TODO: allow when find-file input is active
        PanelSlot::Side => false, // TODO: allow when file search query is active
        PanelSlot::Overlay => false,
    }
}
