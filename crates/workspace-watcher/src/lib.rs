use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use led_core::{Action, Component, Context, DrawContext, Effect, Event, PanelClaim, Waker};
use notify::{EventKind, RecursiveMode, Watcher};
use ratatui::Frame;
use ratatui::layout::Rect;

pub struct WorkspaceWatcher {
    changed: Arc<AtomicBool>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl WorkspaceWatcher {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let changed = Arc::new(AtomicBool::new(false));
        let flag = changed.clone();

        let watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                let Ok(ev) = res else { return };
                match ev.kind {
                    EventKind::Create(_) | EventKind::Remove(_) => {}
                    _ => return,
                }
                // Skip .git internal changes
                if ev
                    .paths
                    .iter()
                    .all(|p| p.components().any(|c| c.as_os_str() == ".git"))
                {
                    return;
                }
                flag.store(true, Ordering::Relaxed);
                if let Some(ref w) = waker {
                    w();
                }
            })
            .ok()
            .and_then(|mut w| {
                w.watch(&root, RecursiveMode::Recursive).ok()?;
                Some(w)
            });

        Self {
            changed,
            _watcher: watcher,
        }
    }
}

impl Component for WorkspaceWatcher {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        if matches!(action, Action::Tick) && self.changed.swap(false, Ordering::Relaxed) {
            vec![Effect::Emit(Event::WorkspaceChanged)]
        } else {
            vec![]
        }
    }

    fn handle_event(&mut self, _event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}
