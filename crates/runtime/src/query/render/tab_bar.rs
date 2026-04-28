//! Tab-bar slice of the render frame.

use led_driver_terminal_core::TabBarModel;
use std::sync::Arc;

use crate::query::inputs::*;

/// Tab-bar slice of the render frame.
///
/// Labels are wrapped in `Arc` so cache-hit clones of [`TabBarModel`]
/// (inside `Frame`, deep inside `render_frame`'s cache slot) are a
/// pointer copy.
///
/// Format per label: `<prefix><name>` where `<prefix>` is `●`
/// (filled circle) when the buffer is dirty, else a space. The painter
/// wraps each label in `" <label> "`, so the two cases render as
/// `"  foo.rs "` (clean) and `" ●foo.rs "` (dirty) — the `●`
/// replaces the second leading space, matching the legacy goldens.
#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    edits: EditedBuffersInput<'b>,
) -> TabBarModel {
    let labels: Vec<String> = tabs
        .open
        .iter()
        .map(|t| {
            let base = t
                .path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.display().to_string());
            let dirty = edits
                .buffers
                .get(&t.path)
                .map(|b| b.dirty())
                .unwrap_or(false);
            let mut s = String::with_capacity(base.len() + "\u{25cf}".len());
            if dirty {
                s.push('\u{25cf}'); // ●
            } else {
                s.push(' ');
            }
            s.push_str(&base);
            s
        })
        .collect();
    let active = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id));
    TabBarModel {
        labels: Arc::new(labels),
        active,
    }
}
