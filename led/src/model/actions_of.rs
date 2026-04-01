use std::cell::Cell;
use std::rc::Rc;

use crossterm::event::KeyCode;
use led_core::Action;
use led_core::PanelSlot;
use led_core::keys::{KeyCombo, KeymapLookup};
use led_core::rx::Stream;
use led_state::AppState;
use led_terminal_in::TerminalInput;

use super::Mut;

/// Derive actions from terminal input + state (for keymap context).
pub fn actions_of(input: &Stream<TerminalInput>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // Resize doesn't need state — extract it directly so it's never lost
    let resize_s = input.filter_map(|i| match i {
        TerminalInput::Resize(w, h) => Some(Mut::Resize(w, h)),
        _ => None,
    });

    // Key events need the keymap from state
    let chord: Cell<Option<KeyCombo>> = Cell::new(None);
    let chord_count: Cell<Option<usize>> = Cell::new(None);
    let macro_repeat: Cell<bool> = Cell::new(false);
    let key_input_s = input
        .filter_map(|i| match i {
            TerminalInput::Key(combo) => Some(combo),
            _ => None,
        })
        .sample_combine(state)
        .map(move |(combo, state)| map_key(combo, &state, &chord, &chord_count, &macro_repeat))
        .flat_map(|actions| actions);

    let merged: Stream<Mut> = Stream::new();
    resize_s.into(&merged);
    key_input_s.forward(&merged);
    merged
}

fn map_key(
    combo: KeyCombo,
    state: &AppState,
    chord: &Cell<Option<KeyCombo>>,
    chord_count: &Cell<Option<usize>>,
    macro_repeat: &Cell<bool>,
) -> Vec<Mut> {
    // Macro repeat mode: bare 'e' replays the macro
    if macro_repeat.get() {
        if !combo.ctrl && !combo.alt && combo.code == KeyCode::Char('e') {
            return vec![Mut::Action(Action::KbdMacroExecute)];
        } else {
            macro_repeat.set(false);
            // fall through to normal processing
        }
    }

    let Some(keymap) = state.keymap.as_ref() else {
        return vec![];
    };
    let context = resolve_context(state);

    if let Some(prefix) = chord.take() {
        // Accumulate digits into chord_count
        if !combo.ctrl && !combo.alt {
            if let KeyCode::Char(c @ '0'..='9') = combo.code {
                let current = chord_count.get().unwrap_or(0);
                chord_count.set(Some(current * 10 + (c as usize - '0' as usize)));
                chord.set(Some(prefix)); // keep chord prefix active
                return vec![];
            }
        }

        let count = chord_count.take();
        let action = keymap.lookup_chord(&prefix, &combo);

        if let Some(ref a) = action {
            if matches!(a, Action::KbdMacroExecute) {
                macro_repeat.set(true);
                // Emit count-set + execute as two Muts
                let mut muts = Vec::new();
                if let Some(n) = count {
                    muts.push(Mut::KbdMacroSetCount(n));
                }
                muts.push(Mut::Action(Action::KbdMacroExecute));
                return muts;
            }
        }

        return match action {
            Some(a) if !requires_editor_focus(&a) || state.focus == PanelSlot::Main => {
                vec![Mut::Action(a)]
            }
            _ => vec![],
        };
    }

    let action = match keymap.lookup(&combo, context) {
        KeymapLookup::Action(action) => Some(action),
        KeymapLookup::ChordPrefix => {
            chord.set(Some(combo));
            None
        }
        KeymapLookup::Unbound => {
            if allow_char_insert(state) && !combo.ctrl && !combo.alt {
                if let KeyCode::Char(c) = combo.code {
                    return vec![Mut::Action(Action::InsertChar(c))];
                }
            }
            None
        }
    };
    match action {
        Some(a) if !requires_editor_focus(&a) || state.focus == PanelSlot::Main => {
            vec![Mut::Action(a)]
        }
        _ => vec![],
    }
}

fn resolve_context(state: &AppState) -> Option<&'static str> {
    if state.file_search.is_some() {
        return Some("file_search");
    }
    match state.focus {
        PanelSlot::Side => Some("browser"),
        PanelSlot::Main => None,
        PanelSlot::StatusBar => None,
        PanelSlot::Overlay => None,
    }
}

fn requires_editor_focus(action: &Action) -> bool {
    matches!(
        action,
        Action::InsertChar(_)
            | Action::InsertNewline
            | Action::InsertTab
            | Action::DeleteBackward
            | Action::DeleteForward
            | Action::KillLine
            | Action::KillRegion
            | Action::Yank
            | Action::Undo
            | Action::Redo
            | Action::SortImports
    )
}

fn allow_char_insert(state: &AppState) -> bool {
    if state.file_search.is_some() || state.find_file.is_some() {
        return true;
    }
    matches!(state.focus, PanelSlot::Main)
}
