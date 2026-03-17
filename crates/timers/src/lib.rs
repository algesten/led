use std::collections::HashMap;
use std::time::Duration;

use led_core::rx::Stream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Cancel any existing timer with this name, start a new one.
    Replace,
    /// If a timer with this name is already running, do nothing.
    KeepExisting,
    /// Every Set creates a new independent timer. Cancel cancels all.
    Independent,
    /// Fire repeatedly at the given interval until cancelled.
    Repeated,
}

#[derive(Clone)]
pub enum TimersOut {
    Set {
        name: &'static str,
        duration: Duration,
        schedule: Schedule,
    },
    Cancel {
        name: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimersIn {
    pub name: &'static str,
}

pub fn driver(out: Stream<TimersOut>) -> Stream<TimersIn> {
    let stream: Stream<TimersIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<TimersOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<TimersIn>(64);

    // Bridge: rx::Stream → channel
    out.on(move |opt: Option<&TimersOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: manage named timers
    tokio::spawn(async move {
        let mut timers: HashMap<&'static str, Vec<JoinHandle<()>>> = HashMap::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TimersOut::Set {
                    name,
                    duration,
                    schedule,
                } => match schedule {
                    Schedule::Replace => {
                        cancel_all(&mut timers, name);
                        let handle = spawn_oneshot(name, duration, &result_tx);
                        timers.insert(name, vec![handle]);
                    }
                    Schedule::KeepExisting => {
                        // Prune finished handles before checking
                        if let Some(handles) = timers.get_mut(name) {
                            handles.retain(|h| !h.is_finished());
                        }
                        let active = timers.get(name).is_some_and(|v| !v.is_empty());
                        if !active {
                            let handle = spawn_oneshot(name, duration, &result_tx);
                            timers.insert(name, vec![handle]);
                        }
                    }
                    Schedule::Independent => {
                        let handle = spawn_oneshot(name, duration, &result_tx);
                        timers.entry(name).or_default().push(handle);
                    }
                    Schedule::Repeated => {
                        cancel_all(&mut timers, name);
                        let handle = spawn_repeated(name, duration, &result_tx);
                        timers.insert(name, vec![handle]);
                    }
                },
                TimersOut::Cancel { name } => {
                    cancel_all(&mut timers, name);
                }
            }
        }
    });

    // Bridge: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn cancel_all(timers: &mut HashMap<&'static str, Vec<JoinHandle<()>>>, name: &'static str) {
    if let Some(handles) = timers.remove(name) {
        for handle in handles {
            handle.abort();
        }
    }
}

fn spawn_oneshot(
    name: &'static str,
    duration: Duration,
    tx: &mpsc::Sender<TimersIn>,
) -> JoinHandle<()> {
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        tx.send(TimersIn { name }).await.ok();
    })
}

fn spawn_repeated(
    name: &'static str,
    duration: Duration,
    tx: &mpsc::Sender<TimersIn>,
) -> JoinHandle<()> {
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(duration);
        interval.tick().await; // first tick is immediate — skip it
        loop {
            interval.tick().await;
            if tx.send(TimersIn { name }).await.is_err() {
                break;
            }
        }
    })
}
