//! Completion checks cv RUNS at `done` time — the machinery that lets a non-code `Done` be
//! OBSERVED, not just attested. This extends the substrate's first law (observed, not attested)
//! to revision-less tasks: instead of taking `Done { observed: "trust me" }` on the agent's word,
//! `cv task done` can carry a check that cv itself executes. A pass records HOW the completion was
//! verified ([`super::model::DoneCheck`]); a failure REFUSES the done (the task stays open) so a
//! false completion never lands.
//!
//! **This is the I/O half of the feature and lives OUTSIDE the pure modules by design** (the
//! purity fence forbids `std::process`/`std::net`/`std::fs` in `reduce`/`model`/`stats`/
//! `provenance`). Running a check reads ambient machine state, so it belongs here and in the
//! CLI/MCP surfaces that call it — the reducer only ever sees the recorded *result*.
//!
//! Three kinds land: a shell command (exit 0 = pass), a file (exists and non-empty = pass), and a
//! plain-`http://` GET (2xx = pass). `https://` is intentionally unsupported by the built-in GET
//! (no TLS in cv-core's dependency fence) — the error names the `--check-cmd 'curl …'` escape.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};

use super::model::{DoneCheck, DoneCheckKind};

/// How much of a failing command's output to echo back (so a refused done is diagnosable without
/// flooding the terminal with an entire test log).
const OUTPUT_TAIL: usize = 2000;
/// Network timeout for the built-in HTTP check — a check must not hang the `done` verb.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// A completion check requested on a `done` surface, before it has run. The CLI/MCP layer builds
/// one of these from the mutually-exclusive `--check-*` inputs and calls [`CheckSpec::run`].
#[derive(Clone, Debug, PartialEq)]
pub enum CheckSpec {
    /// A shell command; exit 0 = pass.
    Cmd(String),
    /// A path that must exist and be non-empty.
    File(PathBuf),
    /// A `http://` URL; a 2xx GET = pass.
    Http(String),
}

impl CheckSpec {
    /// Run the check. `Ok(DoneCheck)` records how the completion was OBSERVED (a passing check) and
    /// is what the caller attaches to the `Done` event. `Err` means the predicate did NOT hold —
    /// the caller must REFUSE the done (leave the task open) and surface the message, which carries
    /// the check's own stderr/detail. `repo` is the task's repo dir when it has one: commands run
    /// there and relative file paths resolve against it; otherwise the process cwd is used.
    pub fn run(&self, repo: Option<&Path>) -> anyhow::Result<DoneCheck> {
        match self {
            CheckSpec::Cmd(cmd) => run_cmd(cmd, repo),
            CheckSpec::File(path) => run_file(path, repo),
            CheckSpec::Http(url) => run_http(url),
        }
    }
}

#[cfg(windows)]
const SHELL: (&str, &str) = ("cmd", "/C");
#[cfg(not(windows))]
const SHELL: (&str, &str) = ("sh", "-c");

fn run_cmd(cmd: &str, repo: Option<&Path>) -> anyhow::Result<DoneCheck> {
    let (program, flag) = SHELL;
    let mut command = Command::new(program);
    command.arg(flag).arg(cmd);
    if let Some(dir) = repo {
        command.current_dir(dir);
    }
    let out = command
        .output()
        .with_context(|| format!("could not run completion check command {cmd:?}"))?;
    if out.status.success() {
        return Ok(DoneCheck {
            kind: DoneCheckKind::Cmd { cmd: cmd.to_string() },
            result: "exit 0".into(),
        });
    }
    let code = out
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    bail!(
        "completion check FAILED (exit {code}); done refused, task stays open.\n\
         --- check: {cmd}\n{}{}",
        tail(stderr.trim_end()),
        tail(stdout.trim_end()),
    )
}

fn run_file(path: &Path, repo: Option<&Path>) -> anyhow::Result<DoneCheck> {
    let resolved = if path.is_relative() {
        match repo {
            Some(dir) => dir.join(path),
            None => path.to_path_buf(),
        }
    } else {
        path.to_path_buf()
    };
    let meta = std::fs::metadata(&resolved).map_err(|e| {
        anyhow!(
            "completion check FAILED: {} does not exist ({e}); done refused, task stays open",
            resolved.display()
        )
    })?;
    if meta.is_dir() {
        bail!(
            "completion check FAILED: {} is a directory, not a file; done refused, task stays open",
            resolved.display()
        );
    }
    if meta.len() == 0 {
        bail!(
            "completion check FAILED: {} exists but is empty; done refused, task stays open",
            resolved.display()
        );
    }
    Ok(DoneCheck {
        // Record the path the caller named (the spec), not the repo-resolved absolute form.
        kind: DoneCheckKind::File { path: path.to_path_buf() },
        result: format!("exists, {} bytes", meta.len()),
    })
}

fn run_http(url: &str) -> anyhow::Result<DoneCheck> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        if url.starts_with("https://") {
            anyhow!(
                "completion check: https is not supported by the built-in checker (cv-core carries no \
                 TLS); use --check-cmd 'curl -fsS {url}' instead"
            )
        } else {
            anyhow!("completion check: url must start with http:// (got {url:?})")
        }
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().context("invalid port in http:// url")?),
        None => (authority, 80u16),
    };
    if host.is_empty() {
        bail!("completion check: http:// url has no host (got {url:?})");
    }
    let mut stream = TcpStream::connect((host, port))
        .with_context(|| format!("completion check FAILED: cannot connect to {host}:{port}; done refused"))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: cv-done-check\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .context("completion check: request write failed")?;
    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .context("completion check: response read failed")?;
    let head = String::from_utf8_lossy(&resp);
    let status_line = head.lines().next().unwrap_or_default().trim();
    // Status line: `HTTP/1.x NNN Reason`.
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("completion check: unparseable HTTP status line {status_line:?}"))?;
    if (200..300).contains(&code) {
        Ok(DoneCheck {
            kind: DoneCheckKind::Http { url: url.to_string() },
            result: format!("{code} {}", reason(status_line)),
        })
    } else {
        bail!("completion check FAILED: GET {url} returned {code}; done refused, task stays open")
    }
}

/// The reason phrase off a status line (`HTTP/1.1 200 OK` → `OK`), for the recorded result.
fn reason(status_line: &str) -> String {
    status_line.splitn(3, ' ').nth(2).unwrap_or("OK").trim().to_string()
}

/// Keep only the last [`OUTPUT_TAIL`] bytes of a command's captured output, on a char boundary,
/// with a leading marker when truncated. Empty input yields empty output (no stray newline).
fn tail(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if s.len() <= OUTPUT_TAIL {
        return format!("{s}\n");
    }
    let mut cut = s.len() - OUTPUT_TAIL;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…(truncated)…\n{}\n", &s[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-check-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cmd_exit_zero_is_observed_pass() {
        let check = CheckSpec::Cmd("true".into()).run(None).unwrap();
        assert_eq!(check.kind, DoneCheckKind::Cmd { cmd: "true".into() });
        assert_eq!(check.result, "exit 0");
    }

    #[test]
    fn cmd_nonzero_is_refused_with_stderr() {
        let err = CheckSpec::Cmd("echo boom 1>&2; false".into()).run(None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("FAILED"), "{msg}");
        assert!(msg.contains("boom"), "the check's stderr is surfaced: {msg}");
    }

    #[test]
    fn cmd_runs_in_the_repo_dir_when_given() {
        let dir = tmp_dir("cwd");
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        // `test -f marker.txt` only passes if the command ran with `dir` as cwd.
        CheckSpec::Cmd("test -f marker.txt".into()).run(Some(&dir)).unwrap();
        // Without the repo dir it runs in the process cwd, where marker.txt does not exist.
        assert!(CheckSpec::Cmd("test -f marker.txt".into()).run(None).is_err());
    }

    #[test]
    fn file_present_and_nonempty_passes() {
        let dir = tmp_dir("file");
        let path = dir.join("design.md");
        std::fs::write(&path, "content").unwrap();
        let check = CheckSpec::File(path.clone()).run(None).unwrap();
        assert_eq!(check.kind, DoneCheckKind::File { path: path.clone() });
        assert_eq!(check.result, "exists, 7 bytes");
    }

    #[test]
    fn missing_file_is_refused() {
        let dir = tmp_dir("missing");
        let err = CheckSpec::File(dir.join("nope.md")).run(None).unwrap_err();
        assert!(format!("{err}").contains("does not exist"), "{err}");
    }

    #[test]
    fn empty_file_is_refused() {
        let dir = tmp_dir("empty");
        let path = dir.join("empty.md");
        std::fs::write(&path, "").unwrap();
        let err = CheckSpec::File(path).run(None).unwrap_err();
        assert!(format!("{err}").contains("empty"), "{err}");
    }

    #[test]
    fn relative_file_resolves_against_the_repo_dir() {
        let dir = tmp_dir("relfile");
        std::fs::write(dir.join("out.txt"), "ok").unwrap();
        let check = CheckSpec::File(PathBuf::from("out.txt")).run(Some(&dir)).unwrap();
        // The recorded path is the caller's spec, not the resolved absolute path.
        assert_eq!(check.kind, DoneCheckKind::File { path: PathBuf::from("out.txt") });
    }

    /// Serve one HTTP request with the given status line, then close. Returns the bound URL.
    fn serve_once(status_line: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                let body = "ok";
                let resp = format!(
                    "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}/health"), handle)
    }

    #[test]
    fn http_2xx_is_observed_pass() {
        let (url, handle) = serve_once("HTTP/1.1 200 OK");
        let check = CheckSpec::Http(url.clone()).run(None).unwrap();
        assert_eq!(check.kind, DoneCheckKind::Http { url });
        assert_eq!(check.result, "200 OK");
        handle.join().unwrap();
    }

    #[test]
    fn http_non_2xx_is_refused() {
        let (url, handle) = serve_once("HTTP/1.1 503 Service Unavailable");
        let err = CheckSpec::Http(url).run(None).unwrap_err();
        assert!(format!("{err}").contains("503"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn https_names_the_curl_escape() {
        let err = CheckSpec::Http("https://example.com".into()).run(None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("https is not supported") && msg.contains("--check-cmd"), "{msg}");
    }
}
