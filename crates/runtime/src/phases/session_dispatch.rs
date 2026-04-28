//! Session-driver dispatch: one-shot Init when `fs.root` is known
//! and the session driver hasn't been initialised yet, plus the
//! Phase::Exiting-side Save dispatch.

use led_driver_session_core::SessionCmd;
use led_state_lifecycle::Phase;

use crate::apply::session::build_session_data;
use crate::phases::TickEnv;
use crate::Atoms;

pub(crate) fn run(atoms: &mut Atoms, env: &TickEnv<'_>) {
    let Atoms {
        tabs,
        edits,
        store,
        browser,
        jumps,
        fs,
        session,
        session_save_dispatched,
        lifecycle,
        ..
    } = atoms;

    if !session.init_done
        && let Some(root) = fs.root.as_ref()
    {
        if let Some(cfg) = env.resolved_config_dir.clone() {
            env.drivers.session.execute(std::iter::once(&SessionCmd::Init {
                root: root.clone(),
                config_dir: cfg,
            }));
            session.init_done = true;
        } else {
            session.init_done = true;
            session.saved = true;
        }
    }

    if matches!(lifecycle.phase, Phase::Exiting)
        && session.primary
        && !session.saved
        && !*session_save_dispatched
    {
        let data = build_session_data(tabs, edits, store, browser, jumps);
        env.drivers.session.execute(std::iter::once(&SessionCmd::SaveSession {
            data,
        }));
        *session_save_dispatched = true;
    } else if matches!(lifecycle.phase, Phase::Exiting)
        && !session.primary
    {
        session.saved = true;
    }
}
