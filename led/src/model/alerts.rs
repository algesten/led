use led_core::{AStream, Alert, FanoutStreamExt, StreamOpsExt};
use tokio_stream::StreamExt;

use crate::model::Mut;

pub fn all_alerts(alert_s: impl AStream<Alert>) -> (impl AStream<Mut>, impl AStream<Mut>) {
    let b = alert_s.broadcast();

    let i = b.latest().map(|a| match a {
        Alert::Info(v) => Some(v),
        Alert::Warn(_) => None,
    });
    let w = b.latest().map(|a| match a {
        Alert::Info(_) => None,
        Alert::Warn(v) => Some(v),
    });

    (i.map(Mut::Info), w.map(Mut::Warn))
}
