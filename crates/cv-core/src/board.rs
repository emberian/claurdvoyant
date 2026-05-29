//! 📣 The coordination board — a lightweight append-only message bus for agents.
//!
//! claurdvoyant lets agents *read* each other's sessions; the board lets them *talk*: post status,
//! leave notes, request/answer, broadcast events. A "channel" is a room (often a project path or a
//! named topic). Messages are appended to `$CLAURDVOYANT_HOME/board/<channel>.jsonl`. The daemon
//! (`cvd`) can mirror session activity onto channels to build a fleet activity feed; the MCP server
//! exposes post/read and an await-until-regex on top of `read`.
//!
//! OWNED BY the messageboard work. The storage API lives here (dependency-light, no regex); the
//! regex/await loop lives in the `cv` CLI and `cv-mcp` on top of [`read`].
//!
//! # Concurrency & atomicity
//!
//! The board is designed to be safe under many simultaneous writers across *processes* (parallel
//! Claude Codes, Codex, a cloud fleet), not just threads. Each [`post`] does two things:
//!
//! 1. Acquires a per-channel advisory lock by `create_new`-ing a `<channel>.lock` file and spinning
//!    with a short backoff until it succeeds (the lock is removed when the [`ChannelLock`] guard
//!    drops, including on panic). This serializes writers to a given channel.
//! 2. Opens the channel file in append mode and writes the serialized message **plus its trailing
//!    newline in a single `write_all`**. A single `write` of less than `PIPE_BUF` bytes to a file
//!    opened `O_APPEND` is atomic on POSIX, so even without the lock individual lines never tear or
//!    interleave; the lock additionally guarantees ordering and covers the (rare) over-`PIPE_BUF`
//!    line. We then `flush`.
//!
//! Readers ([`read`]) tolerate partially-written/garbage lines by skipping anything that fails to
//! parse, so a reader concurrent with a writer never errors — at worst it misses an in-flight line.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Root dir for board channels: `$CLAURDVOYANT_HOME/board` (or `~/.claurdvoyant/board`).
pub fn board_dir() -> PathBuf {
    std::env::var_os("CLAURDVOYANT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claurdvoyant")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("board")
}

/// A single message on a board channel. One of these is serialized per line of `<channel>.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardMessage {
    /// Unique, time-sortable id (uuid v7).
    pub id: String,
    /// The channel (room) this message belongs to. Stored un-slugged; the slug is the filename.
    pub channel: String,
    /// Who posted it (agent/session/human identifier — caller's choice).
    pub from: String,
    /// When it was posted.
    pub ts: DateTime<Utc>,
    /// Message kind: `"msg"` | `"status"` | `"event"` (free-form, but those are the conventions).
    pub kind: String,
    /// The message text / payload.
    pub body: String,
    /// Optional tags for filtering. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional reference to a session (e.g. a `SessionRef` id) the message is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
}

/// Slugify a channel name into a safe filename stem: keep `[A-Za-z0-9._-]`, replace the rest with `-`.
fn slug(channel: &str) -> String {
    let s: String = channel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Avoid empty / dot-only stems that would collide with the directory or hidden files.
    if s.is_empty() || s.chars().all(|c| c == '.') {
        format!("-{s}")
    } else {
        s
    }
}

/// Path to a channel's jsonl file inside `dir`.
fn channel_path(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{}.jsonl", slug(channel)))
}

/// Path to a channel's lockfile inside `dir`.
fn lock_path(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{}.lock", slug(channel)))
}

/// RAII guard for a per-channel advisory lockfile. Removes the lockfile on drop.
struct ChannelLock {
    path: PathBuf,
}

impl ChannelLock {
    /// Acquire the lock by `create_new`-ing the lockfile, spinning with backoff if it's held.
    fn acquire(path: PathBuf) -> Result<ChannelLock> {
        // Spin for a bounded number of attempts so a crashed holder's stale lock can't wedge us
        // forever. Total wait is ~ sum of backoffs below (~ a few seconds) before we steal it.
        let mut backoff = Duration::from_millis(1);
        let max_backoff = Duration::from_millis(50);
        for _ in 0..400 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_f) => return Ok(ChannelLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(max_backoff);
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("creating board lockfile {}", path.display())
                    });
                }
            }
        }
        // Presume the holder died and left a stale lock; steal it so the board never deadlocks.
        // (A racing thief just re-creates it; the append itself is still O_APPEND-atomic.)
        let _ = fs::remove_file(&path);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("stealing stale board lockfile {}", path.display()))?;
        Ok(ChannelLock { path })
    }
}

impl Drop for ChannelLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Post a message to `channel` (in the default [`board_dir`]). See [`post_to_dir`].
pub fn post(
    channel: &str,
    from: &str,
    body: &str,
    kind: Option<&str>,
    tags: Vec<String>,
    session_ref: Option<String>,
) -> Result<BoardMessage> {
    post_to_dir(&board_dir(), channel, from, body, kind, tags, session_ref)
}

/// Core of [`post`], parameterized on the board directory (for testing and embedding).
///
/// Generates a uuid v7 `id` and `ts = Utc::now()`, defaults `kind` to `"msg"`, creates `dir` if
/// needed, then appends the serialized message under a per-channel lock (see module docs).
pub fn post_to_dir(
    dir: &Path,
    channel: &str,
    from: &str,
    body: &str,
    kind: Option<&str>,
    tags: Vec<String>,
    session_ref: Option<String>,
) -> Result<BoardMessage> {
    fs::create_dir_all(dir)
        .with_context(|| format!("creating board dir {}", dir.display()))?;

    let msg = BoardMessage {
        id: uuid::Uuid::now_v7().to_string(),
        channel: channel.to_string(),
        from: from.to_string(),
        ts: Utc::now(),
        kind: kind.unwrap_or("msg").to_string(),
        body: body.to_string(),
        tags,
        session_ref,
    };

    // Serialize to a single line (no embedded newlines: serde_json::to_string is single-line).
    let mut line = serde_json::to_string(&msg).context("serializing board message")?;
    line.push('\n');

    let path = channel_path(dir, channel);
    let _lock = ChannelLock::acquire(lock_path(dir, channel))?;

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening board channel {}", path.display()))?;
    // Single write_all of `line` keeps the record atomic vs. other appenders (O_APPEND).
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to board channel {}", path.display()))?;
    f.flush()
        .with_context(|| format!("flushing board channel {}", path.display()))?;

    Ok(msg)
}

/// Read messages from `channel` (in the default [`board_dir`]). See [`read_from_dir`].
pub fn read(channel: &str, since: Option<&str>, limit: usize) -> Result<Vec<BoardMessage>> {
    read_from_dir(&board_dir(), channel, since, limit)
}

/// Core of [`read`], parameterized on the board directory.
///
/// Reads the channel's jsonl, **skipping** any line that fails to parse (tolerates concurrent
/// writers / corruption). Returns messages in file (chronological) order.
///
/// - A missing channel returns an empty `Vec` (not an error).
/// - `since` is a message **id**: when given, only messages *after* the line with that id are
///   returned (cursor/tail semantics for polling). If the id isn't found, all messages are returned.
/// - `limit == 0` means unlimited; otherwise the most recent `limit` messages (after `since`) are
///   returned, still in chronological order.
pub fn read_from_dir(
    dir: &Path,
    channel: &str,
    since: Option<&str>,
    limit: usize,
) -> Result<Vec<BoardMessage>> {
    let path = channel_path(dir, channel);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("opening board channel {}", path.display()))
        }
    };

    let mut msgs: Vec<BoardMessage> = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // tolerate read hiccups
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<BoardMessage>(line) {
            msgs.push(m);
        }
        // else: malformed / partially-written line — skip.
    }

    // Apply the `since` cursor: drop everything up to and including the matching id.
    if let Some(cursor) = since {
        if let Some(pos) = msgs.iter().position(|m| m.id == cursor) {
            msgs.drain(..=pos);
        }
        // If not found, leave msgs as-is (return everything).
    }

    // Apply the limit: keep the most recent `limit`, preserving chronological order.
    if limit != 0 && msgs.len() > limit {
        let start = msgs.len() - limit;
        msgs.drain(..start);
    }

    Ok(msgs)
}

/// Convenience: read messages strictly newer than `after` by timestamp (in the default board dir).
pub fn read_since_ts(channel: &str, after: DateTime<Utc>) -> Result<Vec<BoardMessage>> {
    read_since_ts_from_dir(&board_dir(), channel, after)
}

/// Core of [`read_since_ts`], parameterized on the board directory.
pub fn read_since_ts_from_dir(
    dir: &Path,
    channel: &str,
    after: DateTime<Utc>,
) -> Result<Vec<BoardMessage>> {
    let mut msgs = read_from_dir(dir, channel, None, 0)?;
    msgs.retain(|m| m.ts > after);
    Ok(msgs)
}

/// List channel names (file stems of `*.jsonl`) under the default [`board_dir`].
pub fn channels() -> Result<Vec<String>> {
    channels_in_dir(&board_dir())
}

/// Core of [`channels`], parameterized on the board directory. Missing dir → empty list.
pub fn channels_in_dir(dir: &Path) -> Result<Vec<String>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing board dir {}", dir.display())),
    };

    let mut names = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway board dir under the system temp dir, unique per test.
    fn tmp_board() -> PathBuf {
        std::env::temp_dir().join(format!("cv-board-test-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn slug_sanitizes_unsafe_chars() {
        assert_eq!(slug("hello"), "hello");
        assert_eq!(slug("a.b_c-d"), "a.b_c-d");
        assert_eq!(slug("/Users/ember/pug"), "-Users-ember-pug");
        assert_eq!(slug("team chat!"), "team-chat-");
        assert_eq!(slug("café☕"), "caf--"); // non-ascii -> '-'
        // Empty / dot-only get a leading '-' so they don't collide with the dir.
        assert_eq!(slug(""), "-");
        assert_eq!(slug(".."), "-..");
    }

    #[test]
    fn slugged_channels_share_a_file() {
        let dir = tmp_board();
        post_to_dir(&dir, "team chat!", "a", "hi", None, vec![], None).unwrap();
        post_to_dir(&dir, "team-chat-", "b", "yo", None, vec![], None).unwrap();
        // Both slug to "team-chat-" so they land in the same channel file.
        let msgs = read_from_dir(&dir, "team chat!", None, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(channels_in_dir(&dir).unwrap(), vec!["team-chat-".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn post_then_read_in_order() {
        let dir = tmp_board();
        let m1 = post_to_dir(&dir, "ch", "alice", "first", None, vec![], None).unwrap();
        let m2 =
            post_to_dir(&dir, "ch", "bob", "second", Some("status"), vec!["x".into()], None).unwrap();
        let m3 = post_to_dir(
            &dir,
            "ch",
            "carol",
            "third",
            Some("event"),
            vec![],
            Some("sess-123".into()),
        )
        .unwrap();

        let msgs = read_from_dir(&dir, "ch", None, 0).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].id, m1.id);
        assert_eq!(msgs[0].body, "first");
        assert_eq!(msgs[0].kind, "msg"); // default kind
        assert_eq!(msgs[1].id, m2.id);
        assert_eq!(msgs[1].kind, "status");
        assert_eq!(msgs[1].tags, vec!["x".to_string()]);
        assert_eq!(msgs[2].id, m3.id);
        assert_eq!(msgs[2].kind, "event");
        assert_eq!(msgs[2].session_ref.as_deref(), Some("sess-123"));

        // ids should be monotonically sortable (uuid v7).
        assert!(m1.id < m2.id && m2.id < m3.id);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn since_cursor_returns_only_newer() {
        let dir = tmp_board();
        let m1 = post_to_dir(&dir, "ch", "a", "1", None, vec![], None).unwrap();
        let m2 = post_to_dir(&dir, "ch", "a", "2", None, vec![], None).unwrap();
        let m3 = post_to_dir(&dir, "ch", "a", "3", None, vec![], None).unwrap();

        let after_m1 = read_from_dir(&dir, "ch", Some(&m1.id), 0).unwrap();
        assert_eq!(after_m1.len(), 2);
        assert_eq!(after_m1[0].id, m2.id);
        assert_eq!(after_m1[1].id, m3.id);

        let after_m3 = read_from_dir(&dir, "ch", Some(&m3.id), 0).unwrap();
        assert!(after_m3.is_empty());

        // Unknown cursor -> return everything.
        let unknown = read_from_dir(&dir, "ch", Some("nope"), 0).unwrap();
        assert_eq!(unknown.len(), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limit_keeps_most_recent() {
        let dir = tmp_board();
        for i in 0..5 {
            post_to_dir(&dir, "ch", "a", &format!("m{i}"), None, vec![], None).unwrap();
        }
        let last2 = read_from_dir(&dir, "ch", None, 2).unwrap();
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].body, "m3");
        assert_eq!(last2[1].body, "m4");

        // since + limit compose: messages after index 0, limited to most-recent 2.
        let all = read_from_dir(&dir, "ch", None, 0).unwrap();
        let after_first = read_from_dir(&dir, "ch", Some(&all[0].id), 2).unwrap();
        assert_eq!(after_first.len(), 2);
        assert_eq!(after_first[1].body, "m4");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_channel_is_empty_not_error() {
        let dir = tmp_board();
        assert!(read_from_dir(&dir, "nope", None, 0).unwrap().is_empty());
        assert!(read_since_ts_from_dir(&dir, "nope", Utc::now()).unwrap().is_empty());
        assert!(channels_in_dir(&dir).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tmp_board();
        post_to_dir(&dir, "ch", "a", "good", None, vec![], None).unwrap();
        // Append complete-but-garbage lines, plus a final un-terminated partial line (which is the
        // only torn shape `post` can ever leave: an in-flight write at EOF).
        let path = channel_path(&dir, "ch");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"this is not json\n   \n{\"partial\": ").unwrap();
        f.flush().unwrap();

        let msgs = read_from_dir(&dir, "ch", None, 0).unwrap();
        // Only the one valid record survives; garbage + blank + partial are skipped.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "good");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_since_ts_filters_by_time() {
        let dir = tmp_board();
        post_to_dir(&dir, "ch", "a", "old", None, vec![], None).unwrap();
        let cut = Utc::now();
        std::thread::sleep(Duration::from_millis(5));
        post_to_dir(&dir, "ch", "a", "new", None, vec![], None).unwrap();

        let newer = read_since_ts_from_dir(&dir, "ch", cut).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].body, "new");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_appends_preserve_all_lines() {
        let dir = tmp_board();
        let n_threads = 8;
        let per_thread = 25;
        std::thread::scope(|s| {
            for t in 0..n_threads {
                let dir = dir.clone();
                s.spawn(move || {
                    for i in 0..per_thread {
                        post_to_dir(
                            &dir,
                            "race",
                            &format!("t{t}"),
                            &format!("msg-{t}-{i}"),
                            None,
                            vec![],
                            None,
                        )
                        .unwrap();
                    }
                });
            }
        });

        let msgs = read_from_dir(&dir, "race", None, 0).unwrap();
        assert_eq!(msgs.len(), n_threads * per_thread);
        // All ids unique (no torn/duplicated lines).
        let mut ids: Vec<_> = msgs.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n_threads * per_thread);
        // Lock should be released; lockfile gone.
        assert!(!lock_path(&dir, "race").exists());
        fs::remove_dir_all(&dir).ok();
    }
}
