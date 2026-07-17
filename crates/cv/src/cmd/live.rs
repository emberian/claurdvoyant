//! `cv scry` / `cv board` — live activity and the agent coordination board.

use crate::util::{home_rel, parse_harness, short_id};
use anyhow::Result;
use clap::Subcommand;
use cv_core::ir::truncate;
use cv_core::watch::{Filter, Watcher};
use std::time::Duration;

#[derive(Subcommand)]
pub(crate) enum BoardCmd {
    /// Post a message to a channel.
    Post {
        channel: String,
        body: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
    },
    /// Read messages from a channel.
    Read {
        channel: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List all channels.
    Channels,
    /// Follow a channel live; with --match, exit when a body contains the substring.
    Watch {
        channel: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long = "match")]
        pattern: Option<String>,
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
    /// Post a question others can `reply` to; prints the request id (the correlation key).
    Request {
        channel: String,
        body: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
    },
    /// Answer a request by its id.
    Reply {
        channel: String,
        /// The request (or message) id this answers.
        in_reply_to: String,
        body: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
    },
    /// Collect all replies to a request id.
    Replies { channel: String, request_id: String },
    /// Requests with zero replies, oldest first, with age — dropped questions made visible.
    Unanswered {
        channel: String,
        /// Only requests posted within this window.
        #[arg(long = "within-secs", default_value_t = 86_400)]
        within_secs: u64,
        #[arg(long)]
        json: bool,
    },
    /// Try to claim a task `key` (a soft lease). Exits non-zero on contention.
    Claim {
        channel: String,
        key: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
        /// Lease duration in seconds.
        #[arg(long = "ttl-secs", default_value_t = 300)]
        ttl_secs: u64,
    },
    /// Release a held claim on `key`.
    Release {
        channel: String,
        key: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
    },
    /// List the currently-active (un-expired) claims on a channel.
    Claims { channel: String },
    /// List agents that heartbeat on a channel recently (also posts your own heartbeat).
    Who {
        channel: String,
        #[arg(long = "within-secs", default_value_t = cv_core::board::WHO_WINDOW_SECS)]
        within_secs: u64,
    },
    /// Acknowledge a message id with a tiny ack note.
    Ack {
        channel: String,
        message_id: String,
        /// Sender endpoint. Default: $CV_ENDPOINT, else "cv".
        #[arg(long)]
        from: Option<String>,
    },
}

pub(crate) fn cmd_scry(harness: Option<String>, cwd: Option<String>, interval: f64, existing: bool) -> Result<()> {
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let mut watcher = Watcher::new(filter, existing);
    eprintln!("✦ scrying for agent activity… (Ctrl-C to stop)");
    watcher.run(Duration::from_secs_f64(interval.max(0.25)), |ev| {
        let r = &ev.reference;
        let tag = match ev.kind {
            cv_core::watch::EventKind::New => "✷ new",
            cv_core::watch::EventKind::Updated => "   +",
        };
        let where_ = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into());
        println!(
            "{tag}  {:8} {:8}  {}  ({} msg)",
            r.harness.as_str(),
            short_id(&r.id),
            where_,
            ev.new_messages.len()
        );
        for m in &ev.new_messages {
            if let Some(t) = m.text() {
                println!("      {} {}", cv_core::render::role_label(m.role), truncate(&t, 100));
            }
        }
    });
}

/// Board identity (G4): explicit `--from`, else the spawner-set `CV_ENDPOINT`, else `"cv"`. The
/// board is chat, so unlike identity-bearing task verbs an unresolvable identity is never an
/// error — the env var just makes the bare command attribute correctly. Shared resolution lives
/// in [`cv_core::task::actor`]; only the CLI's default sink is chosen here.
fn board_from(explicit: Option<String>) -> String {
    cv_core::task::actor(explicit, "cv")
}

pub(crate) fn cmd_board(action: BoardCmd) -> Result<()> {
    use cv_core::board;
    match action {
        BoardCmd::Post {
            channel,
            body,
            from,
            kind,
            tags,
            session_ref,
        } => {
            let from = board_from(from);
            let m = board::post(&channel, &from, &body, kind.as_deref(), tags, session_ref)?;
            println!("✦ posted {} to #{}", short_id(&m.id), channel);
        }
        BoardCmd::Read {
            channel,
            since,
            limit,
            json,
        } => {
            let msgs = board::read(&channel, since.as_deref(), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msgs)?);
            } else {
                for m in &msgs {
                    print_board_msg(m);
                }
                if msgs.is_empty() {
                    println!("(no messages on #{channel})");
                }
            }
        }
        BoardCmd::Channels => {
            for c in board::channels()? {
                println!("#{c}");
            }
        }
        BoardCmd::Watch {
            channel,
            since,
            pattern,
            interval,
        } => {
            eprintln!("✦ watching #{channel} … (Ctrl-C to stop)");
            let mut cursor = since;
            loop {
                for m in board::read(&channel, cursor.as_deref(), 0)? {
                    cursor = Some(m.id.clone());
                    print_board_msg(&m);
                    if let Some(p) = &pattern {
                        if m.body.contains(p.as_str()) {
                            println!("✓ matched {p:?} — done");
                            return Ok(());
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs_f64(interval.max(0.25)));
            }
        }
        BoardCmd::Request { channel, body, from } => {
            let m = board::request(&channel, &board_from(from), &body)?;
            println!("✦ requested {} on #{}", short_id(&m.id), channel);
            println!("  ↳ reply with: cv board reply {channel} {} <body>", m.id);
        }
        BoardCmd::Reply {
            channel,
            in_reply_to,
            body,
            from,
        } => {
            let m = board::reply(&channel, &board_from(from), &in_reply_to, &body)?;
            println!(
                "✦ replied {} to {} on #{}",
                short_id(&m.id),
                short_id(&in_reply_to),
                channel
            );
        }
        BoardCmd::Replies { channel, request_id } => {
            let msgs = board::replies(&channel, &request_id)?;
            for m in &msgs {
                print_board_msg(m);
            }
            if msgs.is_empty() {
                println!("(no replies to {} on #{channel})", short_id(&request_id));
            }
        }
        BoardCmd::Unanswered { channel, within_secs, json } => {
            let rows = board::unanswered_requests(&channel, Duration::from_secs(within_secs))?;
            if json {
                let out: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(m, age)| {
                        serde_json::json!({ "message": m, "age_secs": age.num_seconds() })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                use cv_core::sanitize::sanitize_line;
                let now = chrono::Utc::now();
                for (m, _) in &rows {
                    println!(
                        "{:>4}  {}  {}  {}",
                        cv_core::task::age_short(m.ts, now),
                        short_id(&m.id),
                        sanitize_line(&m.from),
                        sanitize_line(&m.body)
                    );
                }
                if rows.is_empty() {
                    println!("(no unanswered requests on #{channel} in the last {within_secs}s)");
                }
            }
        }
        BoardCmd::Claim {
            channel,
            key,
            from,
            ttl_secs,
        } => {
            let from = board_from(from);
            let lease = board::claim(&channel, &from, &key, Duration::from_secs(ttl_secs))?;
            match lease {
                Some(l) => {
                    println!(
                        "GRANTED  {key} → {} (expires {})",
                        cv_core::sanitize::sanitize_line(&l.owner),
                        crate::util::fmt_local(l.expires_at, "%Y-%m-%d %H:%M:%S")
                    );
                }
                None => {
                    let holder = board::active_claims(&channel)?
                        .into_iter()
                        .find(|(k, _, _)| *k == key)
                        .map(|(_, owner, _)| owner)
                        .unwrap_or_else(|| "someone".into());
                    println!("CONTENDED  {key} is held by {}", cv_core::sanitize::sanitize_line(&holder));
                    std::process::exit(1);
                }
            }
        }
        BoardCmd::Release { channel, key, from } => {
            let from = board_from(from);
            // We can only `release` a real `Lease` (its dir/channel fields are private). But we must
            // not *acquire* a free key just to drop it — that would print "released" for something we
            // never held. So first confirm `from` actually owns a live claim on `key`; only then
            // re-claim (which renews our own lease) and release it.
            let owned = board::active_claims(&channel)?
                .into_iter()
                .any(|(k, owner, _)| k == key && owner == from);
            if !owned {
                println!("(you don't hold {key} on #{channel} — nothing to release)");
            } else {
                match board::claim(&channel, &from, &key, Duration::from_secs(1))? {
                    Some(lease) => {
                        board::release(&lease)?;
                        println!("✦ released {key} on #{channel}");
                    }
                    None => {
                        println!("(claim {key} on #{channel} is held by another agent — nothing to release)");
                    }
                }
            }
        }
        BoardCmd::Claims { channel } => {
            let claims = board::active_claims(&channel)?;
            for (key, owner, expires) in &claims {
                println!(
                    "{:30}  {:16}  expires {}",
                    cv_core::sanitize::sanitize_line(key),
                    cv_core::sanitize::sanitize_line(owner),
                    crate::util::fmt_local(*expires, "%Y-%m-%d %H:%M:%S")
                );
            }
            if claims.is_empty() {
                println!("(no active claims on #{channel})");
            }
        }
        BoardCmd::Who { channel, within_secs } => {
            // Record our own presence first (as CV_ENDPOINT when set), then list heartbeaters.
            board::heartbeat(&channel, &board_from(None))?;
            let agents = board::who(&channel, Duration::from_secs(within_secs))?;
            for a in &agents {
                println!("{}", cv_core::sanitize::sanitize_line(a));
            }
            if agents.is_empty() {
                println!("(nobody seen on #{channel} in the last {within_secs}s)");
            }
        }
        BoardCmd::Ack {
            channel,
            message_id,
            from,
        } => {
            let m = board::ack(&channel, &board_from(from), &message_id)?;
            println!("✦ acked {} on #{channel}", short_id(&message_id));
            let _ = m;
        }
    }
    Ok(())
}

/// Board messages are untrusted, forever-replayed text — sanitize every field at the terminal
/// seam (G5). `--json` output stays raw: serde_json escapes control chars for that transport.
fn print_board_msg(m: &cv_core::board::BoardMessage) {
    use cv_core::sanitize::sanitize_line;
    println!(
        "{}  {}  ({}) {}",
        crate::util::fmt_local(m.ts, "%H:%M:%S"),
        sanitize_line(&m.from),
        sanitize_line(&m.kind),
        sanitize_line(&m.body)
    );
}
