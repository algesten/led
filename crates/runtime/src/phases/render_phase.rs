//! Render phase: paint the new frame if it differs from the last
//! one, and graduate `Phase::Starting` → `Phase::Running` on the
//! first successful paint (M20 fallback path).

use std::io::Write;

use led_driver_terminal_core::Frame;
use led_state_lifecycle::Phase;

use crate::phases::TickEnv;
use crate::Sources;

pub(crate) fn run<W: Write>(
    sources: &mut Sources,
    env: &TickEnv<'_>,
    stdout: &mut W,
    frame: Option<Frame>,
    last_frame: &mut Option<Frame>,
) -> std::io::Result<()> {
    let Sources { lifecycle, .. } = sources;
    if frame != *last_frame {
        if let Some(f) = &frame {
            env.drivers
                .output
                .execute(f, last_frame.as_ref(), env.theme, stdout)?;
            if lifecycle.phase == Phase::Starting {
                lifecycle.phase = Phase::Running;
            }
        }
        *last_frame = frame;
    }
    Ok(())
}
