# Install & quick start

## Install

**Prebuilt binaries** (macOS / Linux / Windows · arm64 · x64 · x86) ship on every [release](https://github.com/emberian/cv/releases/latest). One-liner:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

This installs `cv`, `cv-mcp`, `cvd`, `cv-tui`, and `cv-search`.

Or build from source (needs a recent Rust toolchain):

```sh
git clone https://github.com/emberian/cv && cd cv
cargo build --release      # → target/release/{cv, cv-mcp, cvd, cv-tui, cv-search}
```

## 60-second tour

```sh
cv ls                       # list recent sessions across every harness
cv search "retry backoff"   # full-text search
cv show <id>                # print a transcript (prefix-match on the id is fine)
cv tree <id>                # the message thread as a tree
cv convert <id> --to codex  # port a session into another harness
cv scry                     # live-follow every agent on your machine
```

Most commands take a **session id prefix** (the first few characters are usually enough) and an
optional `--harness <name>` to disambiguate. Run `cv --help` or `cv <command> --help` for the full
flag set.

## The desktop app

Prefer a GUI? The desktop app reads **all your local sessions natively** and lays them out — a
Projects lens, an activity-heatmap timeline, compare, stats, a loom composer, a live fleet
dashboard, and sub-agent trees. See **[The desktop & web app](app.md)**.
