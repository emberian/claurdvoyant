//! Full-text search via tantivy.
//!
//! Schema fields:
//! - `id`         STRING | STORED  — exact session id (the delete key on reindex)
//! - `harness`    STRING | STORED  — fielded filter, e.g. `harness:claude`
//! - `cwd`        TEXT   | STORED  — tokenized so path fragments are searchable
//! - `title`      TEXT   | STORED
//! - `body`       TEXT             — the big searchable blob; **indexed but not stored**
//! - `path`       STRING | STORED  — source file, so a hit can be snippeted by re-reading just
//!   that one session live (no full stored body — keeps the index lean)
//! - `preview`    STORED (only)    — ~240-char whitespace-collapsed head of the body, written on
//!   the session's **first** chunk doc only (~240 bytes/session): the snippet fallback when the
//!   source file has moved/changed
//! - `created_at`/`updated_at` i64 INDEXED | STORED — returned on hits + future sort/filter
//! - `mtime`      i64 STORED       — incremental-index key: unchanged `(id, mtime)` is skipped
//!
//! Indexing is **incremental** by default: only sessions whose file mtime changed (plus new ones)
//! are (re)written, and sessions whose files vanished are deleted. A full rebuild is `rebuild=true`.
//! Snippets are generated **live** from the top hits (re-reading the session, capped) rather than
//! from a stored copy of every body — which is what keeps the on-disk index small.

use crate::Hit;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

struct Fields {
    id: Field,
    harness: Field,
    cwd: Field,
    title: Field,
    body: Field,
    path: Field,
    preview: Field,
    created_at: Field,
    updated_at: Field,
    mtime: Field,
}

fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("id", STRING | STORED);
    b.add_text_field("harness", STRING | STORED);
    b.add_text_field("cwd", TEXT | STORED);
    b.add_text_field("title", TEXT | STORED);
    b.add_text_field("body", TEXT);
    b.add_text_field("path", STRING | STORED);
    b.add_text_field("preview", STORED); // stored-only: never searched, just the snippet fallback
    b.add_i64_field("created_at", INDEXED | STORED);
    b.add_i64_field("updated_at", INDEXED | STORED);
    b.add_i64_field("mtime", STORED);
    b.build()
}

fn fields_of(schema: &Schema) -> Result<Fields> {
    let get = |name: &str| schema.get_field(name);
    Ok(Fields {
        id: get("id")?,
        harness: get("harness")?,
        cwd: get("cwd")?,
        title: get("title")?,
        body: get("body")?,
        path: get("path")?,
        preview: get("preview")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
        mtime: get("mtime")?,
    })
}

/// Open the index at `dir`, creating it if absent. If an existing index has a **stale schema**
/// (missing the `path`/`mtime`/`preview` fields from before this layout — e.g. an index built by
/// an older `cv`), it's transparently rebuilt fresh so the binary self-heals on upgrade; the next
/// `cv index` repopulates it.
fn open_or_create(dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating tantivy dir {}", dir.display()))?;
    let index = match Index::open_in_dir(dir) {
        Ok(idx)
            if idx.schema().get_field("path").is_ok()
                && idx.schema().get_field("mtime").is_ok()
                && idx.schema().get_field("preview").is_ok() =>
        {
            idx
        }
        // Either no index yet, or one with an incompatible older schema → (re)create fresh.
        _ => {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir)
                .with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
            Index::create_in_dir(dir, build_schema())
                .with_context(|| format!("creating tantivy index in {}", dir.display()))?
        }
    };
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

const COMMIT_EVERY: usize = 256;

/// Max searchable bytes per tantivy document. A session's body is flushed into **multiple** docs of
/// at most this size (all sharing the session id), so the index never holds a whole large session's
/// body at once — search dedups hits back to one row per id. 4 MB keeps per-doc memory bounded while
/// staying well above almost every individual message.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Discover + parse every session and bring the index at `dir` up to date.
///
/// Incremental by default: a session whose `(id, file-mtime)` already matches the index is **skipped**
/// (no re-parse cost beyond the metadata scan, no rewrite); changed sessions are replaced; new ones
/// added; and sessions whose source files no longer exist are deleted. Pass `rebuild = true` to clear
/// and rebuild from scratch. Returns the number of sessions discovered (not just the changed ones).
///
/// **Chunked ingestion:** each session streams message-by-message into a bounded body buffer that
/// flushes a tantivy document every [`CHUNK_BYTES`], so a large session is indexed as several small
/// docs (same id) and the indexer never holds its whole body. Commits periodically so segments flush
/// rather than accumulating in the writer's heap.
///
/// **Event ride-along:** the same single adapter pass also feeds the event catalog
/// ([`cv_core::events`] — file edits/reads, commands, errors → `cv events` / `cv touched`) via a
/// [`cv_core::TeeSink`]. Events keep their own mtime skip-table (`event_sync`), so an FTS-only
/// `--rebuild` doesn't force an event re-ingest and a tantivy-fresh session whose events are
/// missing gets an events-only catch-up pass; when both are stale the session is read exactly once.
/// Event persistence is best-effort (sqlite errors never fail indexing).
pub fn index_all(dir: &Path, rebuild: bool) -> Result<usize> {
    use cv_core::ParseOptions;

    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("creating tantivy index writer")?;

    if rebuild {
        writer.delete_all_documents().context("clearing index")?;
        writer.commit().context("commit after clear")?;
    }

    let existing: HashMap<String, i64> = if rebuild {
        HashMap::new()
    } else {
        read_indexed_mtimes(&index, &f).unwrap_or_default()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut since_commit = 0usize;
    let mut changed = 0usize;
    let mut total = 0usize;

    // One query for the whole event/offsets skip-tables — the per-session checks would open (and
    // run DDL on) a fresh sqlite connection per call, which a ~2,400-session sweep pays ~2,400×.
    let event_sync = cv_core::events::SyncTable::load();
    let offset_sync = cv_core::offsets::SyncTable::load();

    for r in cv_core::discover_all() {
        total += 1;
        seen.insert(r.id.clone());
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        let fts_fresh = existing
            .get(&r.id)
            .is_some_and(|&mt| mt == mtime && mtime != 0);
        let events_stale = event_sync.needs_ingest(&r, mtime);
        // Message byte offsets (seekable `cv show --range` — see [`cv_core::offsets`]) ride the
        // same pass, for the harnesses whose adapters can stamp them.
        let offsets_stale =
            cv_core::offsets::supported(r.harness) && offset_sync.needs_record(&r, mtime, size);
        // Unchanged on all axes → skip entirely (no re-parse beyond the metadata scan).
        if fts_fresh && !events_stale && !offsets_stale {
            continue;
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let mut events = events_stale.then(|| cv_core::events::EventSink::new(r.cwd.clone()));
        let mut offsets = offsets_stale.then(cv_core::offsets::OffsetSink::new);
        // The recording pass needs per-message offset stamps; otherwise plain lazy. Identical for
        // every other consumer on the tee (the stamp is one small `extra` entry nobody reads).
        let opts = if offsets.is_some() {
            ParseOptions::lazy_offsets()
        } else {
            ParseOptions::lazy()
        };

        // FTS docs current but events/offsets missing/stale (e.g. first run after upgrading, or
        // after a catalog wipe): a catalog-only pass that never touches tantivy.
        if fts_fresh {
            let res = match (events.as_mut(), offsets.as_mut()) {
                (Some(es), Some(os)) => {
                    let mut tee = cv_core::TeeSink::new(es, os);
                    adapter.stream(&r, &opts, &mut tee)
                }
                (Some(es), None) => adapter.stream(&r, &opts, es),
                (None, Some(os)) => adapter.stream(&r, &opts, os),
                (None, None) => unreachable!("skip above covers fresh+fresh"),
            };
            match res {
                Ok(_) => {
                    if let Some(es) = &events {
                        cv_core::events::record(&r, es.events(), mtime);
                    }
                    if let Some(os) = &offsets {
                        cv_core::offsets::record(&r, os, mtime, size);
                    }
                }
                Err(e) => eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness),
            }
            continue;
        }

        if existing.contains_key(&r.id) {
            // Changed: drop *all* of this session's docs (delete by the shared id term) before re-add.
            writer.delete_term(Term::from_field_text(f.id, &r.id));
        }
        let docs = match index_session(
            &mut writer,
            &f,
            adapter.as_ref(),
            &r,
            mtime,
            events.as_mut(),
            offsets.as_mut(),
        ) {
            Ok(docs) => docs,
            Err(e) => {
                eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness);
                continue;
            }
        };
        // Only stamp events/offsets after a *complete* pass — a parse error above leaves the sync
        // rows untouched so the session is retried next run.
        if let Some(es) = events {
            cv_core::events::record(&r, es.events(), mtime);
        }
        if let Some(os) = offsets {
            cv_core::offsets::record(&r, &os, mtime, size);
        }
        changed += 1;
        since_commit += docs;
        if since_commit >= COMMIT_EVERY {
            writer.commit().context("interim commit")?;
            since_commit = 0;
        }
    }

    // Reap sessions that disappeared from disk since the last index (incremental only).
    let mut removed = 0usize;
    if !rebuild {
        for id in existing.keys() {
            if !seen.contains(id) {
                writer.delete_term(Term::from_field_text(f.id, id));
                removed += 1;
            }
        }
    }

    writer.commit().context("committing index")?;
    if !rebuild {
        eprintln!(
            "  ↳ index: {changed} changed/new, {removed} removed, {} unchanged",
            total.saturating_sub(changed)
        );
    }
    Ok(total)
}

/// Stream one session into bounded tantivy docs through [`ChunkSink`] — teeing the same pass into
/// `events` and/or `offsets` when given, so the transcript is read once for all consumers.
/// `lazy()` keeps large content as spans: the chunk sink chunk-resolves them and the other sinks
/// never read them at all; with an offsets sink the pass runs under `lazy_offsets()` so the
/// adapter stamps each message's byte offset for it to harvest. Returns the docs written.
fn index_session(
    writer: &mut IndexWriter,
    f: &Fields,
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    mtime: i64,
    events: Option<&mut cv_core::events::EventSink>,
    offsets: Option<&mut cv_core::offsets::OffsetSink>,
) -> Result<usize> {
    use cv_core::ParseOptions;
    let opts = if offsets.is_some() {
        ParseOptions::lazy_offsets()
    } else {
        ParseOptions::lazy()
    };
    let mut sink = ChunkSink::new(writer, f, r, mtime);
    match (events, offsets) {
        (Some(es), Some(os)) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, es);
            let mut tee = cv_core::TeeSink::new(&mut tee, os);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (Some(es), None) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, es);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (None, Some(os)) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, os);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (None, None) => {
            adapter.stream(r, &opts, &mut sink)?;
        }
    }
    sink.finish()?;
    Ok(sink.docs)
}

/// Streams one session's messages into bounded-size tantivy docs (all sharing the session id), so a
/// large session never materializes its whole body in the index.
struct ChunkSink<'w> {
    writer: &'w mut IndexWriter,
    f: &'w Fields,
    id: String,
    harness: String,
    cwd: Option<String>,
    path: String,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    mtime: i64,
    disc_title: Option<String>,
    meta_title: Option<String>,
    meta_received: bool,
    first_user: Option<String>,
    resolver: cv_core::Resolver,
    buf: String,
    /// Whitespace-collapsed head of the body, capped at [`PREVIEW_CHARS`]; stored on the first doc.
    preview: String,
    docs: usize,
    err: Option<anyhow::Error>,
}

/// Length cap (chars) of the stored head-of-body preview — the snippet fallback when a hit's
/// source file is gone at query time. ~240 bytes/session keeps the index lean.
const PREVIEW_CHARS: usize = 240;

impl<'w> ChunkSink<'w> {
    fn new(writer: &'w mut IndexWriter, f: &'w Fields, r: &cv_core::SessionRef, mtime: i64) -> Self {
        ChunkSink {
            writer,
            f,
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            path: r.path.display().to_string(),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            mtime,
            disc_title: r.title.clone(),
            meta_title: None,
            meta_received: false,
            first_user: None,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            buf: String::new(),
            preview: String::new(),
            docs: 0,
            err: None,
        }
    }

    /// Session label for display: the parsed title when the adapter delivered metadata (authoritative,
    /// e.g. codex's None → first-user fallback), else the discovery-time title (claude's ai-title).
    fn title(&self) -> String {
        let primary = if self.meta_received {
            self.meta_title.as_deref()
        } else {
            self.disc_title.as_deref()
        };
        cv_core::label_from(primary, self.first_user.as_deref())
    }

    /// Flush the current buffer as one document. `force` emits even an empty buffer (so a session with
    /// no body still gets a doc — needed for incremental mtime tracking).
    fn flush(&mut self, force: bool) {
        if self.err.is_some() || (self.buf.is_empty() && !force) {
            return;
        }
        let mut doc = TantivyDocument::default();
        doc.add_text(self.f.id, &self.id);
        doc.add_text(self.f.harness, &self.harness);
        if let Some(cwd) = &self.cwd {
            doc.add_text(self.f.cwd, cwd);
        }
        doc.add_text(self.f.title, self.title());
        doc.add_text(self.f.body, &self.buf);
        doc.add_text(self.f.path, &self.path);
        // The preview rides only the session's first doc — one copy per session, not per chunk.
        if self.docs == 0 && !self.preview.is_empty() {
            doc.add_text(self.f.preview, &self.preview);
        }
        if let Some(t) = self.created_at {
            doc.add_i64(self.f.created_at, t);
        }
        if let Some(t) = self.updated_at {
            doc.add_i64(self.f.updated_at, t);
        }
        doc.add_i64(self.f.mtime, self.mtime);
        if let Err(e) = self.writer.add_document(doc) {
            self.err = Some(e.into());
        }
        self.buf.clear();
        self.docs += 1;
    }

    /// Flush the trailing buffer (always emits ≥1 doc for the session), propagating any deferred error.
    fn finish(&mut self) -> Result<()> {
        self.flush(self.docs == 0);
        if let Some(e) = self.err.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl<'w> ChunkSink<'w> {
    /// Append `s` to the buffer, flushing a doc whenever it reaches a chunk.
    fn push_chunk(&mut self, s: &str) {
        self.extend_preview(s);
        self.buf.push_str(s);
        if self.buf.len() >= CHUNK_BYTES {
            self.flush(false);
        }
    }

    /// Grow the stored preview from the head of the body: words joined by single spaces (newlines
    /// and runs of whitespace collapse away), truncated at [`PREVIEW_CHARS`]. No-op once full, so
    /// the cost is bounded no matter how large the session is.
    fn extend_preview(&mut self, s: &str) {
        let mut n = self.preview.chars().count();
        if n >= PREVIEW_CHARS {
            return;
        }
        for word in s.split_whitespace() {
            if n >= PREVIEW_CHARS {
                break;
            }
            if !self.preview.is_empty() {
                self.preview.push(' ');
                n += 1;
            }
            for c in word.chars() {
                if n >= PREVIEW_CHARS {
                    break;
                }
                self.preview.push(c);
                n += 1;
            }
        }
    }

    /// Append a content `Text`: chunk-resolved from the source for a span (never owned whole), or the
    /// inline string. `r` is the session resolver (moved out of `self` to avoid a borrow conflict with
    /// the chunk callback, which mutates `self`).
    fn append_text(&mut self, r: &cv_core::Resolver, text: &cv_core::Text) {
        if let Some(sp) = text.as_span() {
            r.for_each_chunk(sp, CHUNK_BYTES, |s| self.push_chunk(s));
        } else if let Some(s) = text.inline_str() {
            self.push_chunk(s);
        }
    }

    /// Append one block's searchable text — identical projection to
    /// [`cv_core::stream::append_searchable`], but chunk-resolving span content.
    fn append_block(&mut self, r: &cv_core::Resolver, b: &cv_core::Block) {
        use cv_core::Block;
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                self.append_text(r, text);
                self.push_chunk("\n");
            }
            Block::ToolUse { name, input, .. } => {
                self.push_chunk(name);
                self.push_chunk(" ");
                self.push_chunk(&input.to_string());
                self.push_chunk("\n");
            }
            Block::ToolResult { content, .. } => {
                self.append_text(r, content);
                self.push_chunk("\n");
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    self.push_chunk(p);
                    self.push_chunk("\n");
                }
            }
            Block::Image { .. } => {}
        }
    }
}

impl cv_core::MessageSink for ChunkSink<'_> {
    fn meta(&mut self, s: &cv_core::Session) {
        self.meta_title = s.title.clone();
        self.meta_received = true;
        if self.cwd.is_none() {
            self.cwd = s.cwd.as_ref().map(|p| p.display().to_string());
        }
    }
    fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
        use cv_core::Block;
        if self.err.is_some() {
            return cv_core::Flow::Stop;
        }
        // Move the resolver out so the chunk callbacks (which mutate `self`) don't conflict with it.
        let r = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
        // First user text → the title fallback. Only its first 72 chars are ever read
        // (`label_from` truncates), so a span resolves just a 512-byte head, never the whole field.
        if self.first_user.is_none() && m.role == cv_core::Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    let s = match text.as_span() {
                        Some(sp) => r.resolve_prefix(sp, 512),
                        None => text.resolve(&r),
                    };
                    let t = s.trim();
                    if !t.is_empty() {
                        self.first_user = Some(t.to_string());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            self.append_block(&r, b);
        }
        self.resolver = r;
        cv_core::Flow::Continue
    }
}

/// Read `id → mtime` for every live document in the index — the incremental skip-set. Stored fields
/// are tiny now (no body is stored), so scanning them all is cheap.
fn read_indexed_mtimes(index: &Index, f: &Fields) -> Result<HashMap<String, i64>> {
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    let mut out = HashMap::new();
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(0).context("opening store reader")?;
        for doc_id in seg.doc_ids_alive() {
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let id = doc.get_first(f.id).and_then(|v| v.as_str()).map(str::to_string);
            let mt = doc.get_first(f.mtime).and_then(|v| v.as_i64());
            if let (Some(id), Some(mt)) = (id, mt) {
                out.insert(id, mt);
            }
        }
    }
    Ok(out)
}

/// Index an explicit set of sessions through the real production [`index_session`] path — the same
/// indexer [`index_all`] drives, minus only the global `discover_all()` scan. Full rebuild: clears
/// prior contents first. Used by tests so date-field (and chunk) indexing is exercised against the
/// code that actually ships rather than a parallel per-doc path. `catalog = true` additionally tees
/// each pass into the event catalog and (for supported harnesses) the message-offsets store, like
/// `index_all` does (only set it under an isolated `CLAURDVOYANT_HOME` — it writes the shared
/// catalog db).
#[cfg(test)]
pub(crate) fn index_refs(dir: &Path, refs: &[cv_core::SessionRef], catalog: bool) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("creating tantivy index writer")?;
    writer.delete_all_documents().context("clearing index")?;
    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        let mut es = catalog.then(|| cv_core::events::EventSink::new(r.cwd.clone()));
        let mut os = (catalog && cv_core::offsets::supported(r.harness))
            .then(cv_core::offsets::OffsetSink::new);
        index_session(&mut writer, &f, adapter.as_ref(), r, mtime, es.as_mut(), os.as_mut())
            .with_context(|| format!("indexing {}", r.id))?;
        if let Some(es) = es {
            cv_core::events::record(r, es.events(), mtime);
        }
        if let Some(os) = os {
            cv_core::offsets::record(r, &os, mtime, size);
        }
    }
    writer.commit().context("committing index")?;
    Ok(refs.len())
}

/// Run a full-text query, returning up to `limit` hits ranked by BM25, each with a live snippet and
/// its `created_at`/`updated_at` (so callers needn't re-discover the corpus just to date the rows).
/// If the hit's source file can't be re-read (moved/changed since indexing), the snippet falls back
/// to the head-of-body preview stored at index time, suffixed with `(source file moved)` when the
/// file is gone entirely.
///
/// The query supports tantivy's syntax: bare terms search title+body+cwd, and fielded terms like
/// `harness:claude`, phrases `"foo bar"`, and booleans `a AND b` work too.
pub fn text_search(dir: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
    if !dir.join("meta.json").exists() {
        anyhow::bail!(
            "no full-text index at {} — run `index_all` first",
            dir.display()
        );
    }
    let (index, f) = open_or_create(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();

    let mut parser = QueryParser::for_index(&index, vec![f.title, f.body, f.cwd]);
    parser.set_conjunction_by_default(); // all terms must match — better precision than OR
    let parsed = parser
        .parse_query(query)
        .with_context(|| format!("parsing query {query:?}"))?;

    // A session is several docs (body chunks) sharing one id, so over-fetch and dedup to one row per
    // id, keeping the best-scoring doc (TopDocs is score-ordered, so the first id occurrence wins).
    let fetch = (limit.saturating_mul(4)).max(64);
    let top = searcher
        .search(&parsed, &TopDocs::with_limit(fetch).order_by_score())
        .context("running search")?;

    let mut hits = Vec::with_capacity(limit);
    let mut seen: HashSet<String> = HashSet::new();
    for (score, addr) in top {
        if hits.len() >= limit {
            break;
        }
        let doc: TantivyDocument = searcher.doc(addr).context("loading stored doc")?;
        let get_str = |field: Field| -> Option<String> {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let id = get_str(f.id).unwrap_or_default();
        if !seen.insert(id.clone()) {
            continue; // another chunk of a session we already have
        }
        let harness = get_str(f.harness).unwrap_or_default();
        let path = get_str(f.path);
        let snippet = match path
            .as_deref()
            .and_then(|p| live_snippet(p, &harness, query))
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            // The source file is unreadable (moved/changed) or yielded nothing — fall back to the
            // preview stored at index time. It rides the session's *first* chunk doc, which may
            // not be the doc that scored this hit, hence the by-id lookup.
            None => {
                let preview = get_str(f.preview)
                    .filter(|s| !s.is_empty())
                    .or_else(|| stored_preview(&searcher, &f, &id))
                    .unwrap_or_default();
                let gone = path.as_deref().is_none_or(|p| !Path::new(p).exists());
                match (gone, preview.is_empty()) {
                    (true, true) => "(source file moved)".to_string(),
                    (true, false) => format!("{preview} (source file moved)"),
                    (false, _) => preview,
                }
            }
        };

        hits.push(Hit {
            id,
            harness,
            cwd: get_str(f.cwd),
            title: get_str(f.title),
            created_at: doc.get_first(f.created_at).and_then(|v| v.as_i64()),
            updated_at: doc.get_first(f.updated_at).and_then(|v| v.as_i64()),
            score,
            snippet,
        });
    }
    Ok(hits)
}

/// Fetch the stored head-of-body preview for session `id`. The preview lives only on the session's
/// **first** chunk doc, which for a multi-chunk session is usually not the doc that produced the
/// hit — so look it up across all of the id's docs. Only runs on the fallback path (live snippet
/// already failed), so the extra term query costs nothing in the common case.
fn stored_preview(searcher: &tantivy::Searcher, f: &Fields, id: &str) -> Option<String> {
    use tantivy::collector::DocSetCollector;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    let q = TermQuery::new(Term::from_field_text(f.id, id), IndexRecordOption::Basic);
    let docs = searcher.search(&q, &DocSetCollector).ok()?;
    for addr in docs {
        let doc: TantivyDocument = match searcher.doc(addr) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Some(p) = doc.get_first(f.preview).and_then(|v| v.as_str()) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Most text we'll scan from a session to find a snippet window. Matches are almost always early;
/// capping keeps snippeting a multi-GB hit bounded (and Phase-2 offsets will make it a seek).
const SNIPPET_SCAN_CAP: usize = 256 * 1024;

/// Generate a snippet for a hit by re-reading **just that one session** (streamed, capped), finding
/// the first query term, and windowing around it — instead of keeping a stored copy of every body.
///
/// Streams under [`ParseOptions::lazy`] and resolves span content **capped at the remaining scan
/// budget**, so a session fronted by a giant record (e.g. a 700 MB tool dump) costs at most
/// [`SNIPPET_SCAN_CAP`] here — the old `bulk()` + materialize path owned the whole record before
/// the cap check could run. Trade-off: a term that only occurs *beyond* the cap inside one giant
/// field now falls back to the head/preview snippet (it already did for terms in later messages).
fn live_snippet(path: &str, harness: &str, query: &str) -> Option<String> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, SessionRef};
    let h = cv_core::Harness::parse(harness)?;
    let adapter = cv_core::harness::for_harness(h)?;
    let sref = SessionRef {
        id: String::new(),
        harness: h,
        path: PathBuf::from(path),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };

    struct Acc {
        buf: String,
        resolver: cv_core::Resolver,
    }
    impl Acc {
        /// Append one content text: inline as-is; a span resolved to at most the remaining budget
        /// (never materialized whole — `resolve_prefix` also keeps a truncated escaped span clean).
        fn push_text(&mut self, text: &cv_core::Text) {
            if let Some(sp) = text.as_span() {
                let remaining = SNIPPET_SCAN_CAP.saturating_sub(self.buf.len()) as u64;
                if remaining == 0 {
                    return;
                }
                self.buf.push_str(&self.resolver.resolve_prefix(sp, remaining));
            } else if let Some(s) = text.inline_str() {
                self.buf.push_str(s);
            }
        }
    }
    impl MessageSink for Acc {
        fn message(&mut self, m: Message) -> Flow {
            // Same projection as `cv_core::stream::append_searchable`, but span-aware.
            use std::fmt::Write as _;
            for b in &m.content {
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        self.push_text(text);
                        self.buf.push('\n');
                    }
                    Block::ToolUse { name, input, .. } => {
                        self.buf.push_str(name);
                        self.buf.push(' ');
                        let _ = write!(self.buf, "{input}");
                        self.buf.push('\n');
                    }
                    Block::ToolResult { content, .. } => {
                        self.push_text(content);
                        self.buf.push('\n');
                    }
                    Block::File { path, source, .. } => {
                        if let Some(p) = path.as_deref().or(source.as_deref()) {
                            self.buf.push_str(p);
                            self.buf.push('\n');
                        }
                    }
                    Block::Image { .. } => {}
                }
            }
            if self.buf.len() >= SNIPPET_SCAN_CAP {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    let mut acc = Acc {
        buf: String::new(),
        resolver: cv_core::Resolver::new(Some(PathBuf::from(path))),
    };
    adapter.stream(&sref, &ParseOptions::lazy(), &mut acc).ok()?;
    Some(make_snippet(&acc.buf, query))
}

/// Window the body around the first matching query term; fall back to a head if nothing matches.
fn make_snippet(body: &str, query: &str) -> String {
    let lc = body.to_lowercase();
    for term in query_terms(query) {
        if let Some(pos) = lc.find(&term) {
            return window(body, pos, term.len());
        }
    }
    head(body, 200)
}

/// The bare, matchable terms of a tantivy query: drops `field:value` filters and boolean operators,
/// keeps lowercased word tokens. Used only to locate a snippet window, not for retrieval.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.contains(':'))
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn window(body: &str, pos: usize, len: usize) -> String {
    let start = floor_char(body, pos.saturating_sub(40));
    let end = ceil_char(body, (pos + len + 80).min(body.len()));
    let s = &body[start..end];
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        let t: String = s.chars().take(199).collect();
        format!("{t}…")
    } else {
        s
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Remove a single session from the index by id (handy for incremental updates).
#[allow(dead_code)]
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(15_000_000)?;
    writer.delete_term(Term::from_field_text(f.id, id));
    writer.commit()?;
    Ok(())
}

fn head(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cv-search-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a minimal one-message Claude transcript so `live_snippet` has a real file to re-read,
    /// and return its path. `body` becomes the single user message's text.
    fn write_claude(dir: &Path, id: &str, body: &str) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": id,
            "sessionId": id,
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p.display().to_string()
    }

    /// A [`cv_core::SessionRef`] pointing at a written Claude transcript, with the dates the index is
    /// expected to surface (`created_at`→1s, `updated_at`→2s, as the hit assertions check). The
    /// production `ChunkSink` reads these straight off the ref — Claude's adapter never emits a
    /// metadata title, so `title` here is the authoritative label.
    fn sref(id: &str, title: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            id: id.into(),
            harness: cv_core::Harness::Claude,
            path: path.into(),
            cwd: Some("/home/u/proj".into()),
            title: Some(title.into()),
            created_at: chrono::DateTime::from_timestamp(1, 0),
            updated_at: chrono::DateTime::from_timestamp(2, 0),
            message_count: 1,
        }
    }

    /// Like [`sref`] but with no discovery-time title — so the indexed label falls back to the first
    /// user message's text (the path Claude sessions without an `ai-title` line take).
    fn sref_untitled(id: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            title: None,
            ..sref(id, "", path)
        }
    }

    /// Write a multi-message Claude transcript (one JSONL record per `(role, content)`), returning its
    /// path. `role` is `"user"` or `"assistant"`; content is a bare string.
    fn write_claude_msgs(dir: &Path, id: &str, msgs: &[(&str, &str)]) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let mut out = String::new();
        for (i, (role, content)) in msgs.iter().enumerate() {
            let line = serde_json::json!({
                "type": role,
                "uuid": format!("{id}-{i}"),
                "sessionId": id,
                "message": { "role": role, "content": content }
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
        std::fs::write(&p, out).unwrap();
        p.display().to_string()
    }

    #[test]
    fn index_and_search_with_live_snippets() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let b1 = "We built a full text search engine with tantivy and BM25 scoring.";
        let b2 = "Loading dataframes and grouping by columns in pandas.";
        let p1 = write_claude(&sdir, "a1", b1);
        let p2 = write_claude(&sdir, "b2", b2);
        let refs = vec![
            sref("a1", "Rust tantivy index", p1),
            sref("b2", "Python pandas", p2),
        ];
        let n = index_refs(&dir, &refs, false).unwrap();
        assert_eq!(n, 2);

        // Term unique to the first doc; snippet is generated live from the session file.
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected exactly one match for 'tantivy'");
        assert_eq!(hits[0].id, "a1");
        assert!(
            hits[0].snippet.contains("tantivy"),
            "live snippet should mention the term: {:?}",
            hits[0].snippet
        );
        // Dates come straight off the index now (no corpus re-discovery).
        assert_eq!(hits[0].created_at, Some(1));
        assert_eq!(hits[0].updated_at, Some(2));

        let hits = text_search(&dir, "pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b2");

        // Fielded query restricts by harness.
        let hits = text_search(&dir, "harness:claude pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, "claude");

        // Conjunction-by-default: a term present in neither yields nothing.
        assert!(text_search(&dir, "kubernetes", 10).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn incremental_skips_unchanged_and_reaps_missing() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "x", "alpha beta gamma");
        // Seed the index with one session.
        index_refs(&dir, &[sref("x", "first", p.clone())], false).unwrap();
        assert_eq!(text_search(&dir, "alpha", 10).unwrap().len(), 1);

        // A direct full-rebuild replaces contents (old doc gone).
        let p2 = write_claude(&sdir, "y", "delta epsilon");
        index_refs(&dir, &[sref("y", "second", p2)], false).unwrap();
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "delta", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The core invariant of `ChunkSink`: a body larger than [`CHUNK_BYTES`] is flushed into several
    /// tantivy docs that all share the session id. Search must (a) still find a term living only in a
    /// late chunk, and (b) dedup a term present in *every* chunk back to a single row per id.
    #[test]
    fn oversized_body_is_chunked_yet_dedups_to_one_hit() {
        let dir = tmpdir();
        let sdir = tmpdir();
        // ~5.6 MB of a repeated common token (lands in every chunk) with a unique marker at the very
        // end (lands only in the final chunk) — forcing ≥2 docs for the one session.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "big", &body);
        index_refs(&dir, &[sref("big", "huge session", p)], false).unwrap();

        // Term in every chunk → multiple matching docs, but deduped to exactly one row for the id.
        let hits = text_search(&dir, "padding", 10).unwrap();
        assert_eq!(hits.len(), 1, "chunks of one session must dedup to a single hit");
        assert_eq!(hits[0].id, "big");

        // Term only in the trailing chunk → still indexed and findable (proves later chunks are added).
        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "big");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A session with no discovery title labels itself from the first user message, and the body
    /// accumulates across roles — so an assistant-only term is searchable too.
    #[test]
    fn title_falls_back_to_first_user_and_body_spans_roles() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude_msgs(
            &sdir,
            "m",
            &[
                ("user", "alpha question about tantivy"),
                ("assistant", "beta answer mentioning bm25"),
            ],
        );
        index_refs(&dir, &[sref_untitled("m", p)], false).unwrap();

        // Assistant-turn term is in the body (roles accumulate into one searchable blob).
        let hits = text_search(&dir, "bm25", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m");
        // No ai-title → label is the first user message's text.
        assert_eq!(hits[0].title.as_deref(), Some("alpha question about tantivy"));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// `live_snippet` must stay O(cap) on a session fronted by one giant field: a term within the
    /// scan cap windows normally, a term hiding *beyond* the cap inside the same giant field falls
    /// back to the head instead of materializing the whole record to find it.
    #[test]
    fn live_snippet_caps_giant_fields() {
        let sdir = tmpdir();
        let mut body = String::from("zqearly marker then padding ");
        body.push_str(&"pad ".repeat(2 * SNIPPET_SCAN_CAP / 4)); // 2× the cap of filler
        body.push_str(" zqlate");
        assert!(body.len() > SNIPPET_SCAN_CAP);
        let p = write_claude(&sdir, "giant", &body);

        let snip = live_snippet(&p, "claude", "zqearly").expect("snippet");
        assert!(snip.contains("zqearly"), "in-cap term should window: {snip:?}");

        let snip = live_snippet(&p, "claude", "zqlate").expect("snippet");
        assert!(
            !snip.contains("zqlate") && snip.starts_with("zqearly"),
            "beyond-cap term must fall back to the head, got: {snip:?}"
        );

        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn make_snippet_windows_match_then_falls_back_to_head() {
        let body = "the quick brown fox jumps over the lazy dog";
        let snip = make_snippet(body, "brown");
        assert!(snip.contains("brown"), "snippet should window the match: {snip:?}");
        // No term matches → fall back to the head of the body.
        assert_eq!(make_snippet("hello world", "kubernetes"), "hello world");
    }

    #[test]
    fn query_terms_drops_fields_and_booleans() {
        let terms = query_terms("harness:claude Tantivy AND bm25:score NOT pandas");
        assert_eq!(terms, vec!["tantivy", "pandas"]);
    }

    /// A query made *entirely* of fielded/boolean tokens has no bare terms to window on — the
    /// snippet must silently fall back to the head of the body, not panic or come back empty.
    #[test]
    fn all_fielded_query_snippets_fall_back_to_head() {
        assert!(query_terms("harness:claude AND cwd:proj").is_empty());
        assert_eq!(make_snippet("hello world", "harness:claude"), "hello world");
        assert_eq!(
            make_snippet("hello world", "harness:claude AND cwd:proj NOT id:x"),
            "hello world"
        );
        // End-to-end: a purely-fielded query still produces a non-empty (head) snippet on a hit.
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "ff", "fielded only query body text");
        index_refs(&dir, &[sref("ff", "fielded", p)], false).unwrap();
        let hits = text_search(&dir, "harness:claude", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("fielded only query body"),
            "head fallback snippet expected: {:?}",
            hits[0].snippet
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Fix for the blank-snippet hole: when a hit's source file has been deleted since indexing,
    /// the snippet falls back to the head-of-body preview stored on the first chunk doc, with a
    /// marker explaining why there's no live context.
    #[test]
    fn missing_source_falls_back_to_stored_preview_with_marker() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let body = "We built a full text\nsearch engine   with tantivy and BM25 scoring.";
        let p = write_claude(&sdir, "gone", body);
        index_refs(&dir, &[sref("gone", "doomed session", p.clone())], false).unwrap();

        // While the file is present, the snippet is live (no marker).
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("(source file moved)"));

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "hit should survive the file vanishing");
        // Preview is whitespace-collapsed: the newline and space-run become single spaces.
        assert!(
            hits[0]
                .snippet
                .contains("We built a full text search engine with tantivy"),
            "stored preview expected: {:?}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.ends_with("(source file moved)"),
            "marker expected: {:?}",
            hits[0].snippet
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The event ride-along, end to end through the production `index_session` tee: one indexing
    /// pass writes BOTH searchable tantivy docs and queryable event rows in the catalog db
    /// (isolated via `CLAURDVOYANT_HOME`).
    #[test]
    fn indexing_tees_events_into_the_catalog() {
        let dir = tmpdir();
        let home = tmpdir();
        std::env::set_var("CLAURDVOYANT_HOME", &home);

        // A claude transcript whose assistant turn edits a file and runs a command.
        let p = home.join("tee-evt.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u0", "sessionId": "tee-evt",
                "message": {"role": "user", "content": "rework the indexer"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "tee-evt",
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "on it — touching the indexer now"},
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "crates/cv-search/src/fts.rs",
                               "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test -p cv-search"}}
                ]}
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let mut r = sref("tee-evt", "indexer rework", p.display().to_string());
        r.cwd = Some("/repo".into());

        index_refs(&dir, &[r], true).unwrap();

        // The text landed in tantivy…
        let hits = text_search(&dir, "indexer", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "tee-evt");

        // …and the SAME pass landed events in the catalog.
        let rows = cv_core::events::events_for("claude", "tee-evt", None);
        let edit = rows.iter().find(|e| e.kind == "file_edit").expect("edit event");
        assert_eq!(edit.target.as_deref(), Some("/repo/crates/cv-search/src/fts.rs"));
        let cmd = rows.iter().find(|e| e.kind == "command").expect("command event");
        assert_eq!(cmd.target.as_deref(), Some("cargo test -p cv-search"));

        // The touched query resolves it by path suffix.
        let touched = cv_core::events::sessions_touching("crates/cv-search/src/fts.rs", true);
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "tee-evt");
        assert_eq!(touched[0].edits, 1);

        std::env::remove_var("CLAURDVOYANT_HOME");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// The preview rides only the session's *first* chunk doc, but a hit can score on a *later*
    /// chunk — the fallback must still find the preview via the by-id lookup. Also caps length.
    #[test]
    fn preview_fallback_found_even_when_hit_is_a_late_chunk() {
        let dir = tmpdir();
        let sdir = tmpdir();
        // Marker only in the trailing chunk, so the sole matching doc has no preview field.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "bigone", &body);
        index_refs(&dir, &[sref("bigone", "huge doomed session", p.clone())], false).unwrap();

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "bigone");
        assert!(
            hits[0].snippet.starts_with("padding padding"),
            "preview from the first chunk doc expected: {:?}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.ends_with("(source file moved)"));
        // The stored preview itself is capped (marker text rides on top of it).
        let stored = hits[0]
            .snippet
            .trim_end_matches(" (source file moved)")
            .chars()
            .count();
        assert!(stored <= PREVIEW_CHARS, "preview over cap: {stored} chars");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }
}
