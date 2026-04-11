use std::rc::Rc;

use led_core::Action;
use led_core::rx::Stream;
use led_state::AppState;

use super::reflow;
use super::{Mut, has_blocking_overlay, has_input_modal};

/// Reflow-paragraph stream: applies dprint-based markdown reflow to the
/// paragraph (or doc-comment block) at the cursor.
pub fn reflow_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    let parent_s = raw_actions
        .filter(|a| matches!(a, Action::ReflowParagraph))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .stream();

    let buf_s = parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let buf = s.buffers.get(&path)?;
            let file_path = buf.path()?.as_path().to_path_buf();
            let new_buf = reflow::reflow_buffer(buf, &file_path)?;
            Some(Mut::BufferUpdate(path, new_buf))
        })
        .stream();

    let alert_s = parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(path)?;
            let file_path = buf.path()?.as_path().to_path_buf();
            if reflow::reflow_buffer(buf, &file_path).is_none() {
                Some(Mut::Alert {
                    info: Some("Nothing to reflow".into()),
                })
            } else {
                None
            }
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    buf_s.forward(&merged);
    alert_s.forward(&merged);
    merged
}
