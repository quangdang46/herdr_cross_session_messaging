//! `herdr-pool inbox [--unread]` and `herdr-pool inbox read <id>`.
//!
//! `inbox` lists messages addressed to this agent (`pending`, `delivered`,
//! `acknowledged`; `--unread` restricts to pending+delivered).
//! `inbox read <id>` / `message read <id>` mark the message acknowledged
//! (when it is `pending` or `delivered`) and print the full content.

use std::io::Write;
use std::time::Duration;

use herdr_plugin::paths::EnvLookup;
use herdr_pool_core::PoolContext;
use herdr_pool_store::Message;
use serde::Serialize;

use crate::cli::InboxAction;
use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx, Failure};
use crate::output::{emit_json, fmt_time, snippet, table_string, MessageJson};

#[derive(Serialize)]
struct InboxJson {
    unread_only: bool,
    messages: Vec<MessageJson>,
}

pub fn run(
    ctx: &Ctx,
    env: &EnvLookup<'_>,
    unread: bool,
    follow: bool,
    action: Option<&InboxAction>,
) -> CmdResult<()> {
    match action {
        Some(InboxAction::Read { id }) => read_message(ctx, env, id),
        None => list_inbox(ctx, env, unread, follow),
    }
}

fn list_inbox(ctx: &Ctx, env: &EnvLookup<'_>, unread: bool, follow: bool) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    if follow && !ctx.json {
        return follow_inbox(&pool, &self_record.agent_id, unread);
    }

    let messages = pool.pool.messages().inbox(&self_record.agent_id, unread)?;

    if ctx.json {
        emit_json(&InboxJson {
            unread_only: unread,
            messages: messages.iter().map(MessageJson::from).collect(),
        });
        return Ok(());
    }

    if messages.is_empty() {
        println!(
            "{}",
            if unread {
                "no unread messages"
            } else {
                "inbox is empty"
            }
        );
        return Ok(());
    }
    println!("{}", inbox_table(&messages));
    Ok(())
}

fn inbox_rows(messages: &[Message]) -> Vec<Vec<String>> {
    messages
        .iter()
        .map(|m| {
            vec![
                m.id.clone(),
                m.from_agent.clone(),
                m.kind.clone(),
                m.status.to_string(),
                fmt_time(m.created_at),
                snippet(&m.content, 60),
            ]
        })
        .collect()
}

fn inbox_table(messages: &[Message]) -> String {
    table_string(
        &["ID", "FROM", "KIND", "STATUS", "TIME", "SNIPPET"],
        &inbox_rows(messages),
    )
}

/// `inbox --follow`: redraw the inbox every 2s until `q`/Esc or the popup
/// is closed. Interactive keys (raw mode, tty only): `u` toggles the
/// unread-only filter.
fn follow_inbox(pool: &PoolContext, agent_id: &str, mut unread: bool) -> CmdResult<()> {
    use std::io::IsTerminal;

    let keys_enabled = std::io::stdin().is_terminal();
    if keys_enabled {
        let _ = crossterm::terminal::enable_raw_mode();
    }
    let stdout = std::io::stdout();
    let mut last_render = std::time::Instant::now() - Duration::from_secs(3);
    loop {
        if keys_enabled {
            while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
                let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() else {
                    continue;
                };
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                        let _ = crossterm::terminal::disable_raw_mode();
                        return Ok(());
                    }
                    crossterm::event::KeyCode::Char('u') => {
                        unread = !unread;
                        last_render = std::time::Instant::now() - Duration::from_secs(3);
                    }
                    _ => {}
                }
            }
        }
        if last_render.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        last_render = std::time::Instant::now();
        // A transient store error must not kill the popup — show it and
        // keep redrawing.
        let body = match pool.pool.messages().inbox(agent_id, unread) {
            Ok(messages) if messages.is_empty() => {
                if unread {
                    "no unread messages".to_string()
                } else {
                    "inbox is empty".to_string()
                }
            }
            Ok(messages) => inbox_table(&messages),
            Err(err) => format!("inbox error: {err}"),
        };
        let scope = if unread { "unread" } else { "all" };
        let frame = format!(
            "\x1b[2J\x1b[Hherdr-pool inbox — live · {scope} (q close · u toggle)\n{}\n",
            fmt_time(crate::context::now_millis())
        );
        let mut out = stdout.lock();
        let _ = out.write_all(frame.as_bytes());
        let _ = out.write_all(body.as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
        drop(out);
    }
}

/// Shared `read` path for `inbox read` and `message read`.
///
/// Marks the message `acknowledged` when it is `delivered` or still
/// `pending` (an explicit read consumes pending mail so the delivery
/// worker never prompts it again) and only when addressed to the caller.
/// `failed`/`dead`/`acknowledged` mail is shown as-is.
pub fn read_message(ctx: &Ctx, env: &EnvLookup<'_>, id: &str) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    let msg = pool
        .pool
        .messages()
        .message(id)?
        .ok_or_else(|| Failure::usage(format!("message '{id}' not found")))?;

    if msg.to_agent != self_record.agent_id {
        return Err(Failure::usage(format!(
            "message '{id}' is not addressed to you ({})",
            self_record.agent_id
        )));
    }

    // Acknowledge from `delivered` or `pending`; failed/dead/acknowledged
    // mail has nothing left to consume.
    let was_pending = msg.status == herdr_pool_store::MessageStatus::Pending;
    if matches!(
        msg.status,
        herdr_pool_store::MessageStatus::Delivered | herdr_pool_store::MessageStatus::Pending
    ) {
        pool.pool.messages().mark_acknowledged(id)?;
    }
    let msg = pool.pool.messages().message(id)?.unwrap_or(msg);

    if ctx.json {
        emit_json(&MessageJson::from(&msg));
    } else {
        println!("id:         {}", msg.id);
        println!("thread:     {}", msg.thread_id.as_deref().unwrap_or("-"));
        println!("from:       {}", msg.from_agent);
        println!("status:     {}", msg.status);
        println!("created:    {}", fmt_time(msg.created_at));
        if let Some(t) = msg.delivered_at {
            println!("delivered:  {}", fmt_time(t));
        }
        if let Some(t) = msg.acked_at {
            println!("acknowledged: {}", fmt_time(t));
        }
        println!("---");
        println!("{}", msg.content);
        if was_pending {
            eprintln!("note: message was still pending delivery; read consumed it");
        }
    }
    Ok(())
}

/// Fetch a message for `message get` (no ack, no recipient check beyond scope).
pub fn get_message(ctx: &Ctx, env: &EnvLookup<'_>, id: &str) -> CmdResult<Message> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let _ = upsert_self(&pool, &identity)?;
    pool.pool
        .messages()
        .message(id)?
        .ok_or_else(|| Failure::usage(format!("message '{id}' not found")))
}
