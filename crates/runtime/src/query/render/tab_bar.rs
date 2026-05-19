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
///
/// `cols` is the tab-bar's on-screen width — the runtime already
/// computed the layout by the time this memo runs (see
/// `render_frame`). It feeds the "scroll the active tab into view"
/// derivation that returns `TabBarModel.scroll_start`; the painter
/// then walks `labels.iter().skip(scroll_start)` instead of building
/// its own `Vec<u16>` of widths and re-deriving the start index
/// every frame.
#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    edits: EditedBuffersInput<'b>,
    cols: u16,
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
    let scroll_start = scroll_active_into_view(&labels, active, cols);
    TabBarModel {
        labels: Arc::new(labels),
        active,
        scroll_start,
    }
}

/// "Scroll the active tab into view" — pure derivation over
/// `(labels, active, cols)`. Mirrors the legacy inline loop in
/// `paint_tab_bar` exactly: walk from `start = 0` upward, in each
/// iteration fit as many labels as `cols` admits, stop once the
/// active tab is inside the visible window.
///
/// Per-label width is `2 + clamp(label_chars, cols)`, matching the
/// painter's `" <label> "` framing (leading + trailing space).
fn scroll_active_into_view(labels: &[String], active: Option<usize>, cols: u16) -> usize {
    let Some(active) = active else { return 0 };
    let widths: Vec<u16> = labels
        .iter()
        .map(|l| 2 + l.chars().count().min(cols as usize) as u16)
        .collect();
    let mut start = 0usize;
    loop {
        let mut used = 0u16;
        let mut last_visible = start;
        for (i, w) in widths.iter().enumerate().skip(start) {
            let next = used.saturating_add(*w);
            if next > cols {
                break;
            }
            used = next;
            last_visible = i;
        }
        if active <= last_visible || start >= widths.len() {
            break;
        }
        start += 1;
    }
    start
}
