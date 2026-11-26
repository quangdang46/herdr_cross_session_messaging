//! `herdr-pool clean [--older-than DAYS] [--dry-run]` — bound pool.db growth.
//!
//! Iterates every `<state>/pools/<scope_key>/pool.db` and prunes terminal
//! rows older than the cutoff (see [`PoolStore::prune`]). Per-pool failures
//! are reported and skipped so one corrupt db cannot block the rest.

use herdr_pool_store::{PoolStore, PruneReport};
use serde::Serialize;

use crate::context::{now_millis, CmdResult, Ctx, Failure};
use crate::output::emit_json;

const MILLIS_PER_DAY: i64 = 86_400_000;

#[derive(Serialize)]
struct PoolPruneJson {
    scope_key: String,
    messages: usize,
    agents: usize,
    threads: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct CleanJson {
    older_than_days: u64,
    dry_run: bool,
    pools: Vec<PoolPruneJson>,
    totals: PoolPruneJson,
}

pub fn run(ctx: &Ctx, older_than_days: u64, dry_run: bool) -> CmdResult<()> {
    let cutoff = now_millis() - (older_than_days as i64) * MILLIS_PER_DAY;
    let pools_dir = ctx.state_dir.join("pools");

    let mut reports: Vec<PruneReport> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    let entries: Vec<_> = match std::fs::read_dir(&pools_dir) {
        Ok(entries) => entries.flatten().collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(Failure::runtime(format!(
                "read pools dir {}: {e}",
                pools_dir.display()
            )))
        }
    };
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() || !path.join("pool.db").is_file() {
            continue;
        }
        let Some(scope_key) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        match PoolStore::open(scope_key, &ctx.state_dir) {
            Ok(store) => match store.prune(cutoff, dry_run) {
                Ok(report) => reports.push(report),
                Err(e) => errors.push((scope_key.to_string(), e.to_string())),
            },
            Err(e) => errors.push((scope_key.to_string(), e.to_string())),
        }
    }

    let totals = PruneReport {
        scope_key: String::new(),
        messages_deleted: reports.iter().map(|r| r.messages_deleted).sum(),
        agents_deleted: reports.iter().map(|r| r.agents_deleted).sum(),
        threads_deleted: reports.iter().map(|r| r.threads_deleted).sum(),
        dry_run,
    };

    if ctx.json {
        let pools: Vec<PoolPruneJson> = reports
            .iter()
            .map(|r| PoolPruneJson {
                scope_key: r.scope_key.clone(),
                messages: r.messages_deleted,
                agents: r.agents_deleted,
                threads: r.threads_deleted,
                error: None,
            })
            .chain(errors.iter().map(|(scope, err)| PoolPruneJson {
                scope_key: scope.clone(),
                messages: 0,
                agents: 0,
                threads: 0,
                error: Some(err.clone()),
            }))
            .collect();
        emit_json(&CleanJson {
            older_than_days,
            dry_run,
            pools,
            totals: PoolPruneJson {
                scope_key: String::new(),
                messages: totals.messages_deleted,
                agents: totals.agents_deleted,
                threads: totals.threads_deleted,
                error: None,
            },
        });
    } else {
        let verb = if dry_run { "would delete" } else { "deleted" };
        println!(
            "clean: cutoff {} ({} days{})",
            cutoff,
            older_than_days,
            if dry_run { ", dry-run" } else { "" }
        );
        for r in &reports {
            if r.messages_deleted + r.agents_deleted + r.threads_deleted == 0 {
                continue;
            }
            println!(
                "  {}: {} messages, {} agents, {} threads {}",
                r.scope_key, r.messages_deleted, r.agents_deleted, r.threads_deleted, verb
            );
        }
        for (scope, err) in &errors {
            eprintln!("  {scope}: prune failed: {err}");
        }
        println!(
            "totals: {} messages, {} agents, {} threads {}",
            totals.messages_deleted, totals.agents_deleted, totals.threads_deleted, verb
        );
    }
    Ok(())
}
