use std::cell::Cell;
use std::sync::Arc;

use crossterm::event::KeyCode;
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
fn resolve_context(_state: &AppState) -> Option<&'static str> {
    // TODO: once AppState has focus/panel state, resolve context from it:
    //
    // match state.focus {
    //     PanelSlot::Side => {
    //         if state.file_search.active {
    //             Some("file_search")
    //         } else {
    //             Some("browser")
    //         }
    //     }
    //     PanelSlot::Overlay => None,
    //     PanelSlot::StatusBar => None,
    //     PanelSlot::Main => None,
    // }
    None
}

/// Whether the current focus allows unbound keys to be inserted as characters.
fn allow_char_insert(_state: &AppState) -> bool {
    // TODO: once AppState has focus/panel state:
    //
    // match state.focus {
    //     PanelSlot::Main => state.has_tabs(),
    //     PanelSlot::StatusBar => {
    //         // Allow insert when find-file or similar input is active
    //         state.status_bar_context().is_some()
    //     }
    //     PanelSlot::Side => {
    //         // Allow insert in file search query input
    //         state.file_search.active
    //     }
    //     PanelSlot::Overlay => false,
    // }
    true
}
