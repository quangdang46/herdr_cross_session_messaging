//! `herdr-pool message read|get <id>` — inspect a single message.
//!
//! `read` shares the inbox read path (acknowledge + full content, human or
//! `--json`). `get` always emits the full [`MessageJson`] record as JSON —
//! it is the scripting/debugging surface and ignores the human formatter.

use herdr_plugin::paths::EnvLookup;

use crate::cli::MessageAction;
use crate::commands::inbox::{get_message, read_message};
use crate::context::{CmdResult, Ctx};
use crate::output::{emit_json, MessageJson};

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, action: &MessageAction) -> CmdResult<()> {
    match action {
        MessageAction::Read { id } => read_message(ctx, env, id),
        MessageAction::Get { id } => {
            let msg = get_message(ctx, env, id)?;
            emit_json(&MessageJson::from(&msg));
            Ok(())
        }
    }
}
