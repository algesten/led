use std::path::PathBuf;
use std::sync::Arc;

use futures::future::ready;
use futures::stream::select_all;
use futures::stream::{Stream, StreamExt};
use led_core::State;

use crate::Drivers;

pub fn model(drivers: Drivers, init: State) -> impl Stream<Item = Arc<State>> {
    let mut_workspace = drivers.workspace.map(|v| Mut::Workspace(v));

    let out = select_all([mut_workspace]);

    out.scan(init, |state, m| {
        match m {
            Mut::Workspace(v) => state.workspace = Some(v),
        }

        // This immutable copy is what we are releasing.
        let copy = Arc::new(state.clone());

        ready(Some(copy))
    })
}

enum Mut {
    Workspace(PathBuf),
}
