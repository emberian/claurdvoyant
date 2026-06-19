# Search (full-text & semantic)

clustervision indexes every session you've ever run — across every harness — so you
can find that one conversation again. There are two ways to look: **full-text**
search (you remember a word that was said) and **semantic** search (you only
remember what it was *about*). 🔮

- **Full-text** is a real inverted index ([tantivy], Lucene-style): tokenization,
  BM25 ranking, fielded filters, phrase and boolean operators, highlighted
  snippets. Fast and exact — it finds documents that contain your words.
- **Semantic** is meaning-based: every session is embedded into a vector, your
  query is embedded too, and results are ranked by cosine similarity. It finds
  the *right* session even when it shares no words with your query.

Both are exposed through the everyday [`cv`](cli.md) CLI; there's also a small
standalone `cv-search` binary that talks to the same on-disk indexes.

## Building the index

Search runs against a prebuilt index, so build it once (and refresh it whenever
you've accumulated new sessions):

```sh
cv index              # build/refresh the full-text index
cv index --semantic   # also build semantic embeddings
```

```text
✦ building full-text index…
indexed 412 session(s) → /Users/you/.clustervision/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 412 session(s) → /Users/you/.clustervision/embeddings.json
```

`cv index` discovers and parses every session from every supported harness, then
rebuilds the full-text index from scratch (a reindex clears prior contents — it's
a full rebuild, not an incremental update, which keeps it simple and always
correct). Add `--semantic` to *also* compute embeddings; that step downloads a
small embedding model (~30 MB) the first time it runs, then caches it.

The equivalent low-level commands on the standalone binary are:

```sh
cv-search index    # full-text only
cv-search embed    # semantic embeddings only (requires the `semantic` build feature)
```

### What gets indexed

Each session becomes one document built from its **entire textual content**, not
just metadata: the title, every message's text and thinking blocks, tool-call
names and their JSON inputs, tool results, and referenced file paths — all
concatenated into one searchable blob. Alongside that body, each document stores
the session `id`, its `harness`, the working directory (`cwd`, tokenized so path
fragments are searchable), the `title`, and created/updated timestamps.

### Where the index lives

Both indexes live under `$CLUSTERVISION_HOME` (default `~/.clustervision`):

| Index            | Path                                  |
| ---------------- | ------------------------------------- |
| Full-text        | `~/.clustervision/tantivy/`            |
| Semantic vectors | `~/.clustervision/embeddings.json`     |

Set `CLUSTERVISION_HOME` to relocate them.

## Full-text search

```sh
cv search "tantivy bm25"
cv search "kubernetes" --harness claude --limit 50
```

```text
claude    a1f3c2d9  2026-05-21  Rust tantivy index
          … We built a full text search engine with <b>tantivy</b> and BM25 scoring.
```

Bare terms search across the title, body, and cwd. The query supports tantivy's
full syntax:

- **Fielded filters** — `harness:claude foo` restricts to a harness.
- **Phrases** — `"formal verification"` matches the exact phrase.
- **Booleans** — `lean AND proof`, `rust OR zig`.

Terms are **conjunctive by default**: every term must match (better precision
than OR). Results are ranked by BM25, with a highlighted snippet drawn from the
matching region of the body. `--limit` caps the number of rows shown (default
**20**); `--harness` filters by harness after ranking.

The standalone binary exposes the same query engine:

```sh
cv-search text "harness:codex pandas" --limit 5
```

Here `--limit` defaults to **10**.

### How `cv search` resolves

`cv search` prefers the tantivy index when it exists — and when present it's
*authoritative*: an empty result means "no match", not "go scan everything."
Only when no index is built does it scan sessions live (slow — that's what the
index is for). When you see `(no index yet — scanning live; run cv index for
instant search)`, build the index.

> Older releases also kept a SQLite full-text index as a middle tier; it's
> retired (tantivy is canonical). If a stale `index.sqlite` lingers in your
> clustervision home, `cv search` prints a note that it's safe to delete.

## Semantic / meaning search

Keyword search only finds documents that contain your words. Semantic search
finds documents that *mean* what you asked, even with **zero word overlap**.

> 🔮 Suppose you once spent an afternoon with Lean proving a theorem, but the
> word "formalizing" never appears in that transcript. A full-text search for
> `"formalizing proofs"` finds nothing. A *semantic* search for the same phrase
> ranks that Lean session right at the top — because "formalizing proofs" and
> "proving theorems in Lean" live near each other in meaning-space.

Two front doors, both backed by the embeddings store:

```sh
cv search "formalizing proofs" --semantic   # semantic mode of the normal search
cv recall "formalizing proofs"              # recall: ranked spans + excerpts
cv-search semantic "formalizing proofs" -k 5
```

`cv search --semantic` embeds your query, ranks every stored session vector by
cosine similarity, and prints the same row format as full-text search. It needs
embeddings to exist — run `cv index --semantic` first.

### `cv recall` — relevant spans, not just metadata

`cv recall` is the meaning-search built for *re-finding context*. It returns the
most relevant past **message spans** for a query, not just a list of session
titles:

```sh
cv recall "how did we handle retry backoff" -k 5
cv recall "auth token refresh" -k 3 --harness codex
```

```text
claude    a1f3c2d9   0.812  Wiring up the HTTP client  ·  ~/proj/api
      assistant: I added exponential backoff with jitter capped at 30s …
      user: can we make the cap configurable?
      assistant: yes — pulled it into RetryConfig.max_delay …
```

Under the hood, recall ranks sessions semantically, then for each hit produces a
compact excerpt. It prefers the stored preview; if none is available it loads the
session, finds the single message that best matches your query, and renders a
small window (the matching message plus its neighbors) — so you get the actual
*conversation around the relevant moment*, with role labels, rather than a bare
metadata row.

`-k` controls how many results to return (default **5**; note it's `-k`, not
`--limit`). `--harness` filters to one harness — recall over-fetches when a
harness filter is set so it can still fill `k` results.

For the MCP version of this same capability — recall surfaced to an agent as a
tool it can call mid-conversation — see [MCP recall](mcp.md).

### Graceful degradation: semantic → keyword

`cv recall` does the right thing if you never ran `cv index --semantic`. When the
embeddings store is missing, semantic search fails cleanly and recall **falls
back to keyword mode** automatically, printing a hint:

```text
(semantic search unavailable: …; falling back to keyword mode — run
 `cv index --semantic` for semantic recall)
```

You still get spans and excerpts — just ranked by keyword match instead of
meaning. Build the semantic index to unlock true meaning-based recall.

> Note: `cv search --semantic` is stricter — it does **not** silently degrade. If
> there are no embeddings it errors and tells you to run `cv index --semantic`.
> Only `cv recall` falls back to keyword.

## How semantic embeddings work

Semantic search uses [model2vec] *static* embeddings (model
`minishlab/potion-base-8M`, ~30 MB, 256-dim). "Static" means there's no
transformer forward pass and no ONNX runtime: text is tokenized, per-token
vectors are looked up in a distilled table and mean-pooled. It's tiny and
CPU-fast, so clustervision just embeds every session up front and does a
brute-force in-memory cosine scan at query time — no approximate-nearest-neighbor
index needed at the ~1k-session scale.

The model is fetched from the HuggingFace Hub on first use and cached under
`~/.cache/huggingface`. To pin a local copy and run fully offline, point
`CV_SEARCH_MODEL` at a model directory:

```sh
export CV_SEARCH_MODEL=/path/to/potion-base-8M
cv index --semantic
```

## Quick reference

| Command                          | What it does                                        | Limit flag (default) |
| -------------------------------- | --------------------------------------------------- | -------------------- |
| `cv index`                       | Build/refresh the full-text index                   | —                    |
| `cv index --semantic`            | Also build semantic embeddings                      | —                    |
| `cv search <query>`              | Full-text search (BM25)                             | `--limit` (20)       |
| `cv search <query> --semantic`   | Semantic search (no keyword fallback)               | `--limit` (20)       |
| `cv recall <query>`              | Semantic recall → spans/excerpts (keyword fallback) | `-k` (5)             |
| `cv-search text <query>`         | Full-text search (standalone binary)                | `--limit` (10)       |
| `cv-search semantic <query>`     | Semantic search (standalone binary)                 | `-k` (10)            |
| `cv-search index` / `embed`      | Build full-text / embeddings (standalone)           | —                    |

See also: [the CLI](cli.md) for the full command surface, and
[MCP recall](mcp.md) for exposing semantic recall to agents.

[tantivy]: https://github.com/quickwit-oss/tantivy
[model2vec]: https://github.com/MinishLab/model2vec
