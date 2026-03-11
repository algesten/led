use std::sync::Arc;

use led_core::keys::{Keymap, Keys};
use led_core::{AStream, Alert};
use tokio_stream::StreamExt;

pub fn keymap_of(keys: impl AStream<Arc<Keys>>) -> impl AStream<Result<Arc<Keymap>, Alert>> {
    keys.map(|k| {
        k.as_ref()
            .clone()
            .into_keymap()
            .map(|km| Arc::new(km))
            .map_err(|e| Alert::Warn(e))
    })
}
