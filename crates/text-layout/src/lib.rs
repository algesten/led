//! Rope-coupled text-layout helpers. Soft-wrap, grapheme-cluster
//! walks, display-cell math. Lifted out of `core/` per the
//! architecture audit's rule that core/ holds only primitives.
//!
//! Consumers import the same flat symbol names that used to live on
//! `led_core::` — see [`grapheme`] and [`wrap`] for the per-module
//! API surface. `SubLine` itself stays in `led-core` (the per-tab
//! `Scroll`, jumps stack, and session driver all carry one without
//! wanting a transitive dep on rope-coupled layout).

pub mod grapheme;
pub mod wrap;

pub use grapheme::{
    TAB_STOP, char_to_grapheme_col, display_col_to_grapheme, grapheme_col_to_char,
    grapheme_col_to_utf16_units, grapheme_display_width, line_grapheme_len,
    prefix_display_width, utf16_units_to_grapheme_col,
};
pub use wrap::{
    SubLineRange, col_to_sub_line, is_continued, line_layout, sub_line_cells_to_grapheme_col,
    sub_line_count, sub_line_range,
};
