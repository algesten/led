use std::sync::Arc;

use led_core::StreamOpsExt;
use led_state::{AppState, Workspace};
use tokio_stream::{Stream, StreamExt};

use crate::Drivers;

pub fn model(drivers: Drivers, init: AppState) -> impl Stream<Item = Arc<AppState>> {
    let mut_workspace = drivers.workspace.map(|v| Mut::Workspace(v));

    mut_workspace
        .reduce(init, |state, m| match m {
            Mut::Workspace(v) => state.workspace = Some(v),
        })
        .map(|state| Arc::new(state))
}

enum Mut {
    Workspace(Workspace),
}
