# Sharing transcripts: `cv share`

`cv share` is asciinema for agent sessions: one command, one `.html` file, and your whole transcript is something you can post to a forum, attach to a bug report, or DM to a friend. The recipient needs **nothing installed** — they double-click the file and read it in any browser, offline.

```sh
cv share da9174f4                       # → ./da9174f4….html, redacted
cv share da91 --out incident.html      # pick the filename
cv share da91 --harness codex          # disambiguate, like everywhere else
cv share da91 --no-redact              # keep secrets (loud stderr warning)
```

## What you get

A single self-contained document — inline CSS, inline JS, inline data. No CDN links, no fetches, no telemetry; a `Content-Security-Policy` meta tag in the file forbids the document from loading anything remote even if it wanted to. Open it from `file://` and everything works.

The page itself:

- **a sticky header** — 🔮, the session title, a color-coded harness badge, model, date, working directory (shortened to `~/…`), short id, and a live message count;
- **role-styled message cards** with timestamps — user / assistant / system / tool each get their own accent;
- **collapsible folds** for thinking blocks, tool calls, and tool results — each fold's summary line previews the content (the command being run, the file being edited, the result size) so a collapsed transcript still *reads*;
- **code blocks** with a tiny built-in syntax highlighter (no JS dependencies — it's ~40 lines of inline script operating on text only);
- **keyboard nav**: `j`/`k` move between messages, `x` toggles the focused message's folds, and the header button expands/collapses everything;
- **dark theme by default** (the 🔮 violet), light theme automatically via `prefers-color-scheme`;
- **a footer** crediting the `clustervision` version that produced the file.

Every session-derived string is HTML-escaped on the way in — a transcript containing `<script>` renders as text, never as markup.

## Redaction is the default

Before anything is written, every message passes through the same scrubber as [`cv redact`](cli.md#cv-redact): API keys, private-key blocks, JWTs, emails, `password = "…"` assignments, and keyword-adjacent high-entropy blobs are replaced with `[REDACTED:kind]` placeholders. The artifact wears its provenance honestly:

- redacted (the default): a **🛡 redacted** badge in the header, and a footer line — *"this transcript was redacted — N secrets scrubbed"*;
- `--no-redact`: a **⚠ unredacted** badge, a *"shared without redaction"* footer, and a stderr warning at generation time.

The scrub counts are printed to stderr either way you'd expect:

```
🔮 wrote incident.html — 400 message(s), 593.6 KB
🛡 redacted 13 item(s): 0 api_key, 0 private_key, 0 jwt, 13 email, 0 blob, 0 assignment
```

Redaction is a best-effort pattern scrubber, not a guarantee — for anything sensitive, skim the artifact before posting it. (It's a single readable HTML file; skimming is the point.)

## Big sessions

Rendering **streams**: each message is parsed, scrubbed, rendered, written to disk, and dropped, so peak memory stays at roughly the largest single message no matter how long the transcript is. A multi-thousand-message session shares fine.

The honest limit is the *output* size, not memory: the artifact contains the full session, so a session with megabytes of tool output becomes an HTML file of megabytes (a ~2,700-message codex session lands around 7 MB). Browsers open that without complaint, but it's a chunky attachment. There's no `--range` on `share` yet; if you want to share a slice of a huge session, [`splice`](cli.md#cv-splice) the span you care about into a new session and share *that*.
