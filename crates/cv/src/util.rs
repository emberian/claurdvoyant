//! Shared CLI helpers: harness/range parsing and small display utilities.

use anyhow::{bail, Context, Result};
use cv_core::ir::Harness;
use std::path::Path;

pub(crate) fn parse_harness(s: &Option<String>) -> Result<Option<Harness>> {
    match s {
        None => Ok(None),
        Some(s) => Harness::parse(s)
            .map(Some)
            .with_context(|| format!("unknown harness: {s}")),
    }
}

/// Parse `<start>-<end>` | `<start>-` | `-<end>` | `<start>` into a 0-based, end-exclusive
/// `(start, Option<end>)` window. `<start>` alone means just that single message.
pub(crate) fn parse_msg_range(spec: &str) -> Result<(usize, Option<usize>)> {
    let spec = spec.trim();
    match spec.split_once('-') {
        None => {
            let n: usize = spec
                .parse()
                .with_context(|| format!("bad range {spec:?}: expected <start>-<end>, <start>-, -<end>, or <n>"))?;
            Ok((n, Some(n + 1)))
        }
        Some((s, e)) => {
            let start = if s.trim().is_empty() {
                0
            } else {
                s.trim()
                    .parse()
                    .with_context(|| format!("bad range {spec:?}: start must be a number"))?
            };
            let end = if e.trim().is_empty() {
                None
            } else {
                Some(
                    e.trim()
                        .parse()
                        .with_context(|| format!("bad range {spec:?}: end must be a number"))?,
                )
            };
            if let Some(end) = end {
                if end < start {
                    bail!("bad range {spec:?}: end ({end}) is before start ({start})");
                }
            }
            Ok((start, end))
        }
    }
}

pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Format a UTC instant in the **local** timezone. Every human-facing time cv prints goes through
/// here (or [`fmt_local_ts`]): transcripts store UTC, but the user's other clocks — `git log`,
/// their memory of the evening — are local, and silently mixing the two skews forensics by the
/// UTC offset (a real 4-hour miss reconstructing a power-loss window prompted this).
pub(crate) fn fmt_local(d: chrono::DateTime<chrono::Utc>, fmt: &str) -> String {
    d.with_timezone(&chrono::Local).format(fmt).to_string()
}

/// [`fmt_local`] from unix seconds (the events catalog's timestamp shape).
pub(crate) fn fmt_local_ts(secs: i64, fmt: &str) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|d| fmt_local(d, fmt))
}

/// A session's `created → last-active` span, local time, fixed width (pads to [`SPAN_WIDTH`]).
/// Same-day sessions compress the right side to its time; a missing `created` shows only the
/// last-active instant. The span is what distinguishes a long-lived orchestrator from a one-shot —
/// and, after a crash, the resumed from the dropped.
pub(crate) const SPAN_WIDTH: usize = 31;
pub(crate) fn fmt_span(
    created: Option<chrono::DateTime<chrono::Utc>>,
    updated: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    const FMT: &str = "%y-%m-%d %H:%M";
    let s = match (created, updated) {
        (Some(c), Some(u)) => {
            let (lc, lu) = (c.with_timezone(&chrono::Local), u.with_timezone(&chrono::Local));
            if lc.date_naive() == lu.date_naive() {
                format!("{} → {}", lc.format(FMT), lu.format("%H:%M"))
            } else {
                format!("{} → {}", lc.format(FMT), lu.format(FMT))
            }
        }
        (Some(t), None) | (None, Some(t)) => fmt_local(t, FMT),
        (None, None) => "?".into(),
    };
    format!("{s:<SPAN_WIDTH$}")
}

pub(crate) fn home_rel(p: &Path) -> String {
    if let Some(home) = dirs_home() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

pub(crate) fn dim_cwd(p: Option<&Path>) -> String {
    p.map(home_rel).unwrap_or_else(|| "(no cwd)".into())
}

pub(crate) fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::parse_msg_range;

    #[test]
    fn msg_range_forms() {
        assert_eq!(parse_msg_range("10-13").unwrap(), (10, Some(13)));
        assert_eq!(parse_msg_range("10-").unwrap(), (10, None));
        assert_eq!(parse_msg_range("-13").unwrap(), (0, Some(13)));
        assert_eq!(parse_msg_range("5").unwrap(), (5, Some(6))); // single message
        assert_eq!(parse_msg_range(" 2 - 4 ").unwrap(), (2, Some(4))); // whitespace-tolerant
    }

    #[test]
    fn msg_range_rejects_inverted_and_garbage() {
        assert!(parse_msg_range("9-3").is_err()); // end before start
        assert!(parse_msg_range("abc").is_err());
        assert!(parse_msg_range("1-x").is_err());
    }
}
