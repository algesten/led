//! LSP one-shot Init dispatch. The first tick after `fs.root` is
//! known (and we're not in `--no-workspace` mode) emits a single
//! `LspCmd::Init`; subsequent ticks see `lsp_driver.init_sent =
//! true` (set by `LspDriver::execute`) and skip.

use led_driver_lsp_core::LspCmd;

use crate::phases::TickEnv;
use crate::Sources;

pub(crate) fn run(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        fs,
        lsp_driver,
        ..
    } = sources;

    if !lsp_driver.init_sent
        && !env.no_workspace
        && let Some(root) = fs.root.as_ref()
    {
        env.drivers.lsp.execute(
            std::iter::once(&LspCmd::Init {
                root: root.clone(),
            }),
            lsp_driver,
        );
    }
}
