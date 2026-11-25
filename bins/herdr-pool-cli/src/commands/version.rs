//! `herdr-pool version` — print version information.

use serde::Serialize;

use crate::context::{CmdResult, Ctx};
use crate::output::emit_json;

#[derive(Serialize)]
struct VersionJson {
    name: &'static str,
    version: &'static str,
    skill_version: &'static str,
}

pub fn run(ctx: &Ctx) -> CmdResult<()> {
    if ctx.json {
        emit_json(&VersionJson {
            name: "herdr-pool",
            version: env!("CARGO_PKG_VERSION"),
            skill_version: herdr_plugin::skill::SKILL_VERSION,
        });
    } else {
        println!("herdr-pool {}", env!("CARGO_PKG_VERSION"));
        println!("skill payload v{}", herdr_plugin::skill::SKILL_VERSION);
    }
    Ok(())
}
