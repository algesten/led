//! Render phase: paint the new frame if it differs from the last
//! one, and graduate `Phase::Starting` → `Phase::Running` on the
//! first successful paint (M20 fallback path).

use std::io::Write;

use led_driver_terminal_core::{Frame, FrameId, ScrollHint};
use led_state_lifecycle::Phase;

use crate::phases::TickEnv;
use crate::Sources;

pub(crate) fn run<W: Write>(
    sources: &mut Sources,
    env: &TickEnv<'_>,
    stdout: &mut W,
    frame: Option<Frame>,
    last_frame: &mut Option<Frame>,
    scroll_hints: &[ScrollHint],
) -> std::io::Result<()> {
    let Sources {
        lifecycle,
        paint_state,
        frame_id_seq,
        ..
    } = sources;
    if frame != *last_frame {
        if let Some(f) = frame {
            // Stamp a fresh `FrameId` only when the content
            // actually differs (the equality check above ignores
            // the id field — see `Frame`'s manual `PartialEq`).
            // Driver consumes the id to populate `paint_state.
            // in_flight` / `last_acked`.
            frame_id_seq.0 += 1;
            let stamped = Frame {
                id: FrameId(frame_id_seq.0),
                ..f
            };
            env.drivers.output.execute(
                &stamped,
                last_frame.as_ref(),
                scroll_hints,
                env.theme,
                paint_state,
                stdout,
            )?;
            if lifecycle.phase == Phase::Starting {
                lifecycle.phase = Phase::Running;
            }
            *last_frame = Some(stamped);
        } else {
            *last_frame = None;
        }
    }
    Ok(())
}
