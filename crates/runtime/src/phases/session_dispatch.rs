//! Session-driver dispatch: one-shot Init when `fs.root` is known
//! and the session driver hasn't been initialised yet, plus the
//! Phase::Exiting-side Save dispatch.

use led_driver_session_core::SessionCmd;
use led_state_lifecycle::Phase;

use crate::apply::session::build_session_data;
use crate::phases::TickEnv;
use crate::Sources;

pub(crate) fn run(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        store,
        browser,
        jumps,
        fs,
        session,
        session_driver,
        lifecycle,
        ..
    } = sources;

    if !session.init_done
        && let Some(root) = fs.root.as_ref()
    {
        if let Some(cfg) = env.resolved_config_dir.clone() {
            env.drivers.session.execute(
                std::iter::once(&SessionCmd::Init {
                    root: root.clone(),
                    config_dir: cfg,
                }),
                session_driver,
            );
            session.init_done = true;
        } else {
            session.init_done = true;
            session.saved = true;
        }
    }

    if matches!(lifecycle.phase, Phase::Exiting)
        && session.primary
        && !session.saved
        && !session_driver.save_dispatched
    {
        // build_session_data returns DraftSession (Theme O role
        // newtype). The driver's wire ABI carries SessionData, so
        // the explicit `.0` unwrap is the seam where draft ⇒
        // about-to-be-persisted; the seal closes on the matching
        // `Saved` event where `last_saved` reads it back as
        // PersistedSession.
        let draft = build_session_data(tabs, edits, store, browser, jumps);
        env.drivers.session.execute(
            std::iter::once(&SessionCmd::SaveSession { data: draft.0 }),
            session_driver,
        );
    } else if matches!(lifecycle.phase, Phase::Exiting)
        && !session.primary
    {
        session.saved = true;
    }
}
