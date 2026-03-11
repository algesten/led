use std::sync::Arc;

use led_core::AStream;
use led_state::AppState;

pub struct Ui;

impl Ui {
    pub fn close() {}
}

pub fn driver(state: impl AStream<Arc<AppState>>) -> Ui {
    Ui
}
