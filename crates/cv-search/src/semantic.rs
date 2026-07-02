//! Semantic search via model2vec static embeddings + brute-force cosine, at **window** granularity.
//!
//! model2vec embeds text with a *static* distilled embedding table (no transformer forward pass,
//! no ONNX): tokenize, look up per-token vectors, mean-pool. It's tiny and CPU-fast, which makes
//! "embed every window up front, then cosine-scan in memory" a perfectly tasteful design even at
//! ~50–100k vectors. No ANN crate required.
//!
//! Sessions are embedded **per window**, not head-only: each session's searchable text is cut into
//! ~3 KiB windows with a 512-byte overlap ([`cut_windows`]), and every window gets its own vector
//! tagged with the message index it starts at — so a hit can say *where* in a 500-message session
//! the match lives (`cv show --range` can jump there), instead of only matching content that
//! happened to sit in the first 512 tokens. A session is capped at [`MAX_WINDOWS`] windows (its
//! `truncated` flag records when the cap or the parse budget cut coverage short).
//!
//! Embedding is **incremental**: the store carries each session's `(mtime, size)` freshness
//! signature (the same size-primary policy as the FTS index — see
//! [`cv_core::events::size_primary_freshness`]), so `cv index --semantic` re-embeds only
//! changed/new sessions, carries unchanged vectors over, drops vanished sessions, and rewrites the
//! store once at the end (atomic temp + rename). `--rebuild` forces a full re-embed.
//!
//! On first use the model (`MODEL_REPO`) is fetched from the HuggingFace Hub and cached by
//! `hf-hub` under `~/.cache/huggingface`. To pin a local copy (offline), set
//! `CV_SEARCH_MODEL=/path/to/model-dir`.
//!
//! # The store format (v3)
//!
//! Vectors are persisted in a small hand-rolled binary layout (fixed-width little-endian f32s)
//! instead of JSON: a query loads ~N·dim·4 bytes and reinterprets them, rather than JSON-parsing
//! millions of ASCII floats. Layout, all integers little-endian:
//!
//! ```text
//! [8]  magic  b"CVEMBED\x03"
//! u32  dim          (vector dimensionality)
//! u32  count        (number of window records)
//! u64  meta_len
//! […]  meta JSON    ({ model, sessions: [{id, harness, path, sigs, …}…], records: [{s, w, m}…] })
//! […]  vectors      (count × dim × f32 LE, record i's vector at offset i·dim)
//! ```
//!
//! The metadata stays JSON (small, schema-evolvable via serde defaults); only the bulk — the
//! vectors — is fixed-width. Older stores (v2's one-head-vector-per-session, or the original
//! all-JSON layout) carry no per-session signatures or window info, so they are treated as stale:
//! the version bump forces a one-time full re-embed rather than a lossy migration.

use crate::Hit;
use anyhow::{bail, Context, Result};
use cv_core::events::size_primary_freshness;
use model2vec_rs::model::StaticModel;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A small, strong static-embedding model (~30MB, 256-dim). Distilled from a sentence encoder.
const MODEL_REPO: &str = "minishlab/potion-base-8M";

/// Store header magic: `CVEMBED` + format version 3 (v1 was the implicit all-JSON store, v2 the
/// binary head-only-vector store — both stale now, see the module docs).
const MAGIC: &[u8; 8] = b"CVEMBED\x03";

/// Bytes of searchable text per embed window. model2vec truncates each input to 512 tokens
/// (≈2–3 KB of text), so ~3 KiB windows waste almost nothing while keeping the vector count sane.
const WINDOW_BYTES: usize = 3 * 1024;

/// Bytes shared between consecutive windows, so a passage straddling a cut still lands whole in
/// one of the two windows.
const WINDOW_OVERLAP: usize = 512;

/// How far consecutive window starts are apart.
const WINDOW_STRIDE: usize = WINDOW_BYTES - WINDOW_OVERLAP;

/// Max windows embedded per session, so one giant session can't blow up the store (256 windows ×
/// 256 dim × 4 B = 256 KiB of vectors). Coverage beyond the cap is dropped and the session's
/// `truncated` flag records it.
const MAX_WINDOWS: usize = 256;

/// Searchable bytes per session that the window cutter can cover at [`MAX_WINDOWS`] — also the
/// parse-time accumulation budget in [`crate::doc_for_ref`] (text past it can't be windowed).
pub(crate) const SESSION_TEXT_BUDGET: usize = (MAX_WINDOWS - 1) * WINDOW_STRIDE + WINDOW_BYTES;

/// Per-session metadata, persisted in the store's JSON block (one per session, however many
/// window records it owns).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SessionMeta {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    /// Source file, so a hit's snippet can be re-read live from the matched window's messages.
    path: String,
    /// A short preview of the body head — the snippet fallback when the source file is gone.
    preview: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
    /// File `(mtime_ns, size)` signature at embed time — the incremental skip key, under the same
    /// size-primary policy as the FTS index ([`size_primary_freshness`]).
    mtime: i64,
    size: i64,
    /// True when the windowing budget or [`MAX_WINDOWS`] cap cut coverage short: the vectors span
    /// a prefix of the session, not all of it.
    #[serde(default)]
    truncated: bool,
}

/// Per-vector window record (compact keys — there's one of these per vector): `s` indexes into
/// the sessions array, `w` is the window's ordinal within its session, `m` the 0-based index of
/// the message the window starts in (the `cv show --range` jump target).
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct WindowMeta {
    s: u32,
    w: u32,
    m: u32,
}

/// The JSON block inside the binary store (deserialize side; serialization uses a borrowed twin
/// in [`save_store`] to avoid cloning).
#[derive(serde::Deserialize)]
struct MetaBlock {
    model: String,
    sessions: Vec<SessionMeta>,
    records: Vec<WindowMeta>,
}

/// The in-memory store: session + window metadata plus one flat `count × dim` vector block
/// (record `i`'s vector is `vectors[i*dim..(i+1)*dim]`).
struct Store {
    model: String,
    dim: usize,
    sessions: Vec<SessionMeta>,
    records: Vec<WindowMeta>,
    vectors: Vec<f32>,
}

impl Store {
    fn vector(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

// ---- store I/O -------------------------------------------------------------------------------

/// Serialize + write the store atomically (temp + rename).
fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let meta_json = {
        // Serialize the metadata without cloning: a borrowed twin of `MetaBlock`.
        #[derive(serde::Serialize)]
        struct MetaBlockRef<'a> {
            model: &'a str,
            sessions: &'a [SessionMeta],
            records: &'a [WindowMeta],
        }
        serde_json::to_vec(&MetaBlockRef {
            model: &store.model,
            sessions: &store.sessions,
            records: &store.records,
        })
        .context("serializing embedding store metadata")?
    };

    let mut out = Vec::with_capacity(8 + 4 + 4 + 8 + meta_json.len() + store.vectors.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(store.dim as u32).to_le_bytes());
    out.extend_from_slice(&(store.records.len() as u32).to_le_bytes());
    out.extend_from_slice(&(meta_json.len() as u64).to_le_bytes());
    out.extend_from_slice(&meta_json);
    for v in &store.vectors {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, &out).with_context(|| format!("writing embedding store {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("moving embedding store into place at {}", path.display()))?;
    Ok(())
}

/// Load a v3 store. An older store (v2 binary or the original all-JSON layout) is an explicit
/// "rebuild me" error: those formats carry no window info or freshness signatures, so the version
/// bump forces a one-time full re-embed instead of a lossy migration.
fn load_store(path: &Path) -> Result<Store> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading embedding store {} — run `cv index --semantic` first",
            path.display()
        )
    })?;
    if bytes.first() == Some(&b'{') || (bytes.len() >= 8 && bytes[..7] == MAGIC[..7] && bytes[7] != MAGIC[7]) {
        bail!(
            "embedding store {} is an older head-only format — run `cv index --semantic` to rebuild \
             it with windowed embeddings (one-time full re-embed)",
            path.display()
        );
    }
    if bytes.len() < 24 || &bytes[..8] != MAGIC {
        bail!(
            "unrecognized embedding store {} (bad magic) — re-run `cv index --semantic`",
            path.display()
        );
    }
    let dim = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let meta_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let meta_end = 24usize
        .checked_add(meta_len)
        .filter(|&e| e <= bytes.len())
        .with_context(|| format!("truncated embedding store {}", path.display()))?;
    let meta: MetaBlock = serde_json::from_slice(&bytes[24..meta_end]).context("parsing embedding store metadata")?;
    let vec_bytes = &bytes[meta_end..];
    if meta.records.len() != count
        || vec_bytes.len() != count * dim * 4
        || meta.records.iter().any(|r| r.s as usize >= meta.sessions.len())
    {
        bail!(
            "corrupt embedding store {} ({} records, {} vector bytes, dim {dim}) — re-run `cv index --semantic`",
            path.display(),
            meta.records.len(),
            vec_bytes.len()
        );
    }
    let vectors: Vec<f32> = vec_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    Ok(Store {
        model: meta.model,
        dim,
        sessions: meta.sessions,
        records: meta.records,
        vectors,
    })
}

/// [`load_store`] for the *indexing* path: any unusable store (absent, older format, corrupt) is
/// simply `None` — a full re-embed, never an error that blocks indexing.
fn load_store_for_index(path: &Path) -> Option<Store> {
    if !path.exists() {
        return None;
    }
    match load_store(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("(embedding store not reusable: {e:#}; re-embedding everything)");
            None
        }
    }
}

// ---- windowing -------------------------------------------------------------------------------

/// One embed window cut from a session's text: the message it starts in, and its text.
struct Window {
    first_msg: u32,
    text: String,
}

/// Cut a session's per-message text pieces into overlapping embed windows.
///
/// Pieces are `(message index, searchable text)` in stream order. The concatenated text is cut
/// into [`WINDOW_BYTES`] windows every [`WINDOW_STRIDE`] bytes (so consecutive windows share
/// [`WINDOW_OVERLAP`] bytes and a passage straddling a cut lands whole in one of them), each
/// tagged with the message its start offset falls in. Whitespace-only windows are dropped. At
/// most [`MAX_WINDOWS`] windows are produced; the returned flag is true when that cap left text
/// uncovered.
fn cut_windows(pieces: &[(u32, String)]) -> (Vec<Window>, bool) {
    let mut text = String::new();
    // (start offset in `text`, message index) per piece, for the offset → message mapping.
    let mut bounds: Vec<(usize, u32)> = Vec::with_capacity(pieces.len());
    for (msg, t) in pieces {
        bounds.push((text.len(), *msg));
        text.push_str(t);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    let total = text.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut covered_end = total == 0;
    while start < total && out.len() < MAX_WINDOWS {
        let s = floor_char(&text, start);
        let e = ceil_char(&text, (start + WINDOW_BYTES).min(total));
        let slice = &text[s..e];
        if !slice.trim().is_empty() {
            let first_msg = match bounds.binary_search_by(|&(off, _)| off.cmp(&s)) {
                Ok(i) => bounds[i].1,
                Err(0) => 0,
                Err(i) => bounds[i - 1].1,
            };
            out.push(Window {
                first_msg,
                text: slice.to_string(),
            });
        }
        if e >= total {
            covered_end = true;
            break;
        }
        start += WINDOW_STRIDE;
    }
    (out, !covered_end)
}

// ---- embedding -------------------------------------------------------------------------------

/// The id of the model [`load_model`] would load right now: the `CV_SEARCH_MODEL` override (a
/// local model dir) when set, else [`MODEL_REPO`]. Recorded in the store by [`embed_all`] and
/// compared at query time — query and store vectors must come from the **same** model, or the
/// cosine ranking is cross-space noise.
fn model_id() -> String {
    std::env::var_os("CV_SEARCH_MODEL")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| MODEL_REPO.to_string())
}

/// Load the embedding model, honoring `CV_SEARCH_MODEL` (local dir) before falling back to the Hub.
fn load_model() -> Result<StaticModel> {
    if let Some(dir) = std::env::var_os("CV_SEARCH_MODEL").filter(|v| !v.is_empty()) {
        return StaticModel::from_pretrained(dir, None, None, None).context("loading model from $CV_SEARCH_MODEL");
    }
    StaticModel::from_pretrained(MODEL_REPO, None, None, None)
        .with_context(|| format!("loading/downloading embedding model {MODEL_REPO} (set CV_SEARCH_MODEL for offline)"))
}

/// Windows batched into one `model.encode` call. Bounds how many window texts are resident at
/// once during embedding (128 × ~3 KiB ≈ 400 KB).
const EMBED_BATCH: usize = 128;

/// What one incremental sweep did — surfaced by [`embed_all`]'s summary line.
struct EmbedStats {
    /// Sessions discovered on disk (the return value of `embed_all`, matching `index_all`).
    total: usize,
    /// Sessions parsed + (re-)embedded this run.
    embedded: usize,
    /// Sessions whose vectors were carried over unchanged (freshness signature matched).
    carried: usize,
    /// Previously-stored sessions no longer on disk, dropped from the store.
    removed: usize,
}

/// Discover, window, and embed every session, persisting vectors to `path` (binary v3 store).
///
/// Incremental by default: a stored session whose `(mtime, size)` signature still matches (under
/// the FTS index's size-primary policy) keeps its vectors without a re-parse; changed/new sessions
/// are re-windowed and re-embedded; vanished ones are dropped. The store is rewritten once at the
/// end (atomic temp + rename). `rebuild = true` ignores the existing store and re-embeds all.
/// The model is loaded lazily — a fully-fresh sweep never touches it. Returns the number of
/// sessions discovered.
pub fn embed_all(path: &Path, rebuild: bool) -> Result<usize> {
    let id = model_id();
    let mut model: Option<StaticModel> = None;
    let mut embed = |texts: &[String]| -> Result<Vec<Vec<f32>>> {
        if model.is_none() {
            model = Some(load_model()?);
        }
        Ok(model.as_ref().expect("just loaded").encode(texts))
    };
    let stats = embed_pass(path, rebuild, &id, &mut embed, cv_core::discover_all())?;
    if !rebuild {
        eprintln!(
            "  ↳ embeddings: {} re-embedded, {} carried over, {} removed",
            stats.embedded, stats.carried, stats.removed
        );
    }
    Ok(stats.total)
}

/// The incremental embed sweep behind [`embed_all`], with the corpus and the embedder injected —
/// production drives it with `discover_all()` + the real model, tests with fixture refs + a fake
/// embedder (so no model download or `CV_SEARCH_MODEL` setup is ever needed in a test).
///
/// `embed` maps a batch of window texts to one vector each; batches are at most [`EMBED_BATCH`]
/// windows. Records/vectors stay row-aligned throughout: queued window texts are flushed before
/// any carried-over session copies its old vector rows in.
fn embed_pass<F>(
    path: &Path,
    rebuild: bool,
    model: &str,
    embed: &mut F,
    refs: impl IntoIterator<Item = cv_core::SessionRef>,
) -> Result<EmbedStats>
where
    F: FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
{
    // The old store, reusable only when same-format and same-model (cross-model vectors would be
    // cosine noise; a model switch is a full re-embed).
    let old = if rebuild {
        None
    } else {
        load_store_for_index(path).filter(|s| s.model == model)
    };
    // id → old session index, and each old session's record rows, for O(1) carry-over.
    let mut old_by_id: HashMap<&str, u32> = HashMap::new();
    let mut old_recs: Vec<Vec<usize>> = Vec::new();
    if let Some(o) = &old {
        old_recs = vec![Vec::new(); o.sessions.len()];
        for (i, s) in o.sessions.iter().enumerate() {
            old_by_id.insert(&s.id, i as u32);
        }
        for (ri, r) in o.records.iter().enumerate() {
            old_recs[r.s as usize].push(ri);
        }
    }

    let mut store = Store {
        model: model.to_string(),
        dim: old.as_ref().map(|o| o.dim).unwrap_or(0),
        sessions: Vec::new(),
        records: Vec::new(),
        vectors: Vec::new(),
    };
    // Window texts queued for the next `embed` call; their records are already in `store.records`
    // (the trailing `pending.len()` rows), vectors owed on flush.
    let mut pending: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stats = EmbedStats {
        total: 0,
        embedded: 0,
        carried: 0,
        removed: 0,
    };

    for r in refs {
        stats.total += 1;
        seen.insert(r.id.clone());
        // Signature *before* the parse: content appended mid-parse re-embeds next run, never skips.
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);

        // Fresh in the old store → carry session + vectors over untouched, no re-parse.
        if let (Some(o), Some(&osidx)) = (&old, old_by_id.get(r.id.as_str())) {
            let om = &o.sessions[osidx as usize];
            if embed_is_fresh(&r, Some((om.mtime, om.size)), mtime, size) {
                flush_pending(&mut store, &mut pending, embed)?; // keep vector rows record-aligned
                let nsidx = store.sessions.len() as u32;
                store.sessions.push(om.clone());
                for &ri in &old_recs[osidx as usize] {
                    store.records.push(WindowMeta {
                        s: nsidx,
                        ..o.records[ri]
                    });
                    store.vectors.extend_from_slice(o.vector(ri));
                }
                stats.carried += 1;
                continue;
            }
        }

        // Changed or new: stream-parse into per-message pieces, cut windows, queue for embedding.
        // A parse failure leaves the session out of the store entirely (retried next run).
        let Some(doc) = crate::doc_for_ref(&r) else { continue };
        let (windows, cut_short) = cut_windows(&doc.pieces);
        let nsidx = store.sessions.len() as u32;
        store.sessions.push(SessionMeta {
            id: doc.id,
            harness: doc.harness,
            cwd: doc.cwd,
            title: doc.title,
            path: doc.path,
            preview: windows.first().map(|w| preview(&w.text, 200)).unwrap_or_default(),
            created_at: doc.created_at,
            updated_at: doc.updated_at,
            mtime,
            size,
            truncated: doc.truncated || cut_short,
        });
        for (w, win) in windows.into_iter().enumerate() {
            store.records.push(WindowMeta {
                s: nsidx,
                w: w as u32,
                m: win.first_msg,
            });
            pending.push(win.text);
            if pending.len() >= EMBED_BATCH {
                flush_pending(&mut store, &mut pending, embed)?;
            }
        }
        stats.embedded += 1;
    }
    flush_pending(&mut store, &mut pending, embed)?; // final partial batch

    if let Some(o) = &old {
        stats.removed = o.sessions.iter().filter(|s| !seen.contains(&s.id)).count();
    }
    save_store(path, &store)?;
    Ok(stats)
}

/// Encode the queued window texts (one `embed` call) and append their vectors, restoring the
/// records ↔ vectors row alignment. A wrong-shaped embedder result is an error — silently skipping
/// a vector would shear the flat block against its records.
fn flush_pending<F>(store: &mut Store, pending: &mut Vec<String>, embed: &mut F) -> Result<()>
where
    F: FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
{
    if pending.is_empty() {
        return Ok(());
    }
    let vectors = embed(pending)?;
    anyhow::ensure!(
        vectors.len() == pending.len(),
        "embedder returned {} vectors for {} texts",
        vectors.len(),
        pending.len()
    );
    for v in vectors {
        if store.dim == 0 {
            store.dim = v.len();
        }
        anyhow::ensure!(
            !v.is_empty() && v.len() == store.dim,
            "embedding dim {} does not match store dim {}",
            v.len(),
            store.dim
        );
        store.vectors.extend_from_slice(&v);
    }
    pending.clear();
    Ok(())
}

/// Whether a stored session is still fresh given its stored `(mtime, size)` and the current file
/// signature — the exact policy the FTS index uses: size-primary for append-only transcript logs
/// (a pure mtime bump skips), size **and** mtime for rewriteable stores, and a 0 size (unreadable)
/// is never fresh.
fn embed_is_fresh(r: &cv_core::SessionRef, stored: Option<(i64, i64)>, mtime: i64, size: i64) -> bool {
    stored.is_some_and(|(stored_mtime, stored_size)| {
        stored_size == size && size != 0 && (size_primary_freshness(r) || (stored_mtime == mtime && mtime != 0))
    })
}

// ---- query -----------------------------------------------------------------------------------

/// A semantic hit with its **location**: which message the best-matching window starts at, so the
/// caller can point `cv show --range` (or a reader) at the right part of the session. `hit` is the
/// ordinary session-level [`Hit`] (id, title, score, snippet, dates).
pub struct SemanticHit {
    pub hit: Hit,
    /// 0-based index of the message the matched window starts in — a `cv show <id> --range N-`
    /// jump target.
    pub first_msg: usize,
    /// The matched window's ordinal within its session (0-based).
    pub window: usize,
}

/// Embed `query` and return the top-`k` sessions by cosine similarity over **all** stored window
/// vectors, deduped to each session's best window (so one session can't fill the result list) and
/// carrying that window's message index.
///
/// The store's recorded model must match the model the query would embed under (including a
/// `CV_SEARCH_MODEL` override), and the query vector's dimensionality must match the store's —
/// cosine across different embedding spaces is noise, so a mismatch is an error with a re-embed
/// hint rather than silently garbage rankings.
pub fn semantic_search_located(path: &Path, query: &str, k: usize) -> Result<Vec<SemanticHit>> {
    let store = load_store(path)?;
    if store.records.is_empty() {
        return Ok(Vec::new());
    }
    let current = model_id();
    if store.model != current {
        bail!(
            "embedding store {} was built with model {:?}, but the current model is {current:?} \
             — run `cv index --semantic` to re-embed (or set CV_SEARCH_MODEL to match)",
            path.display(),
            store.model
        );
    }
    let model = load_model()?;
    let q = model.encode_single(query);
    if q.len() != store.dim {
        bail!(
            "query embedding has dim {}, but store {} has dim {} — run `cv index --semantic` to re-embed",
            q.len(),
            path.display(),
            store.dim
        );
    }
    Ok(to_hits(&store, rank(&store, &q, k), query))
}

/// [`semantic_search_located`] flattened to plain [`Hit`]s, for callers that don't need the
/// window location (`cv recall`, `cv pack`, the TUI, the demo bin).
pub fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    Ok(semantic_search_located(path, query, k)?
        .into_iter()
        .map(|h| h.hit)
        .collect())
}

/// Brute-force cosine over every window vector, deduped to the best-scoring window per session,
/// sorted by score, truncated to `k`. Returns `(score, record index)` pairs. Split from the model
/// + snippet plumbing so ranking and dedup are testable without an embedding model.
fn rank(store: &Store, q: &[f32], k: usize) -> Vec<(f32, usize)> {
    // Best (score, record) seen per session.
    let mut best: HashMap<u32, (f32, usize)> = HashMap::new();
    for (ri, rec) in store.records.iter().enumerate() {
        let score = cosine(q, store.vector(ri));
        let e = best.entry(rec.s).or_insert((score, ri));
        if score > e.0 {
            *e = (score, ri);
        }
    }
    let mut scored: Vec<(f32, usize)> = best.into_values().collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);
    scored
}

/// Project ranked `(score, record)` pairs to [`SemanticHit`]s: snippet from a live bounded re-read
/// of the matched window's messages, falling back to the stored head preview when the source file
/// is unreadable (suffixed `(source file moved)` when it's gone entirely, like FTS hits).
fn to_hits(store: &Store, ranked: Vec<(f32, usize)>, query: &str) -> Vec<SemanticHit> {
    ranked
        .into_iter()
        .map(|(score, ri)| {
            let rec = store.records[ri];
            let s = &store.sessions[rec.s as usize];
            let snippet = match window_snippet(&s.path, &s.harness, rec.m, query).filter(|t| !t.trim().is_empty()) {
                Some(t) => t,
                None => {
                    let gone = !Path::new(&s.path).exists();
                    match (gone, s.preview.is_empty()) {
                        (true, true) => "(source file moved)".to_string(),
                        (true, false) => format!("{} (source file moved)", s.preview),
                        (false, _) => s.preview.clone(),
                    }
                }
            };
            SemanticHit {
                hit: Hit {
                    id: s.id.clone(),
                    harness: s.harness.clone(),
                    cwd: s.cwd.clone(),
                    title: s.title.clone(),
                    score,
                    snippet,
                    created_at: s.created_at,
                    updated_at: s.updated_at,
                    // Semantic store doesn't fold the sub-agent forest (yet); no provenance to carry.
                    parent_id: None,
                    agent_id: None,
                    workflow: None,
                },
                first_msg: rec.m as usize,
                window: rec.w as usize,
            }
        })
        .collect()
}

/// Re-read ~one window's worth of text starting at message `first_msg` of the session at `path`
/// (streamed, span-resolution capped at the remaining budget — the same bounded-re-read discipline
/// as the FTS `live_snippet`), and cut a ~200-char display snippet from it: around the first query
/// term when one matches, else the head. `None` when the file can't be parsed anymore.
fn window_snippet(path: &str, harness: &str, first_msg: u32, query: &str) -> Option<String> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, SessionRef};
    let h = cv_core::Harness::parse(harness)?;
    let adapter = cv_core::harness::for_harness(h)?;
    let sref = SessionRef {
        id: String::new(),
        harness: h,
        path: path.into(),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };

    /// Skips `skip` messages, then accumulates searchable text up to [`WINDOW_BYTES`].
    struct WindowAcc {
        skip: u32,
        idx: u32,
        buf: String,
        resolver: cv_core::Resolver,
    }
    impl WindowAcc {
        fn push_text(&mut self, text: &cv_core::Text) {
            if let Some(sp) = text.as_span() {
                let remaining = WINDOW_BYTES.saturating_sub(self.buf.len()) as u64;
                if remaining > 0 {
                    self.buf.push_str(&self.resolver.resolve_prefix(sp, remaining));
                }
            } else if let Some(s) = text.inline_str() {
                self.buf.push_str(s);
            }
        }
    }
    impl MessageSink for WindowAcc {
        fn message(&mut self, m: Message) -> Flow {
            use std::fmt::Write as _;
            let i = self.idx;
            self.idx += 1;
            if i < self.skip {
                return Flow::Continue;
            }
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
            if self.buf.len() >= WINDOW_BYTES {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    let mut acc = WindowAcc {
        skip: first_msg,
        idx: 0,
        buf: String::new(),
        resolver: cv_core::Resolver::new(Some(path.into())),
    };
    adapter.stream(&sref, &ParseOptions::lazy(), &mut acc).ok()?;
    Some(snippet_from(&acc.buf, query))
}

/// A ~200-char display snippet from window text: centered on the first (case-insensitive) query
/// term found, else the head. Terms are the query's bare lowercased word tokens.
fn snippet_from(body: &str, query: &str) -> String {
    let lc = body.to_lowercase();
    for term in query.split_whitespace() {
        let term = term.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if term.is_empty() {
            continue;
        }
        if let Some(pos) = lc.find(&term) {
            let start = floor_char(body, pos.saturating_sub(40));
            let end = ceil_char(body, (pos + term.len() + 120).min(body.len()));
            return preview(&body[start..end], 200);
        }
    }
    preview(body, 200)
}

/// Cosine similarity. model2vec L2-normalizes by default, so this is ~a dot product, but we
/// normalize defensively so unnormalized vectors still rank sanely.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Whitespace-collapsed head of `s`, at most `max_chars` chars, `…`-suffixed when truncated.
fn preview(s: &str, max_chars: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max_chars {
        let t: String = t.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        t
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cv-emb-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A deterministic fake embedder (no model download, no `CV_SEARCH_MODEL`): 4-dim vectors from
    /// simple text statistics, counting every text embedded into `calls`.
    fn fake_embed(calls: &mut usize) -> impl FnMut(&[String]) -> Result<Vec<Vec<f32>>> + '_ {
        move |texts: &[String]| {
            *calls += texts.len();
            Ok(texts
                .iter()
                .map(|t| {
                    let sum: u32 = t.bytes().map(u32::from).sum();
                    vec![t.len() as f32, (sum % 97) as f32, 1.0, 0.0]
                })
                .collect())
        }
    }

    // -- fixtures (same shapes as the fts tests: minimal real Claude transcripts) --------------

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

    /// Rewrite a file with byte-identical content: mtime bumps, size doesn't — the "mass mtime
    /// bump, no real change" condition the size-primary freshness policy exists for.
    fn bump_mtime(path: &str) {
        let content = std::fs::read(path).unwrap();
        std::fs::write(path, &content).unwrap();
    }

    /// Append one user message line (size grows) — a real content change.
    fn append(path: &str, extra: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "type": "user",
            "uuid": "append",
            "message": { "role": "user", "content": extra }
        });
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    // -- store fixture builders ------------------------------------------------------------------

    fn smeta(id: &str, path: &str, sig: (i64, i64)) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            harness: "claude".into(),
            cwd: None,
            title: Some(format!("title-{id}")),
            path: path.into(),
            preview: format!("preview-{id}"),
            created_at: Some(100),
            updated_at: Some(200),
            mtime: sig.0,
            size: sig.1,
            truncated: false,
        }
    }

    // -- window cutting (pure) -----------------------------------------------------------------

    #[test]
    fn cut_windows_small_session_is_one_window() {
        let pieces = vec![(0u32, "hello world".to_string()), (1, "second message".to_string())];
        let (windows, truncated) = cut_windows(&pieces);
        assert_eq!(windows.len(), 1);
        assert!(!truncated);
        assert_eq!(windows[0].first_msg, 0);
        assert!(windows[0].text.contains("hello world"));
        assert!(windows[0].text.contains("second message"));
        // Nothing to window: no vectors, not truncated.
        let (windows, truncated) = cut_windows(&[]);
        assert!(windows.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn cut_windows_overlaps_and_maps_first_msg() {
        // Two 3000-byte messages: window starts at 0 / STRIDE / 2·STRIDE land in msg 0, msg 0
        // (still before byte 3001), and msg 1 respectively.
        let pieces = vec![(0u32, "a".repeat(3000)), (7, "b".repeat(3000))];
        let (windows, truncated) = cut_windows(&pieces);
        assert!(!truncated);
        assert_eq!(windows.len(), 3, "6 KB of text at stride {WINDOW_STRIDE} → 3 windows");
        assert_eq!(
            windows.iter().map(|w| w.first_msg).collect::<Vec<_>>(),
            vec![0, 0, 7],
            "window start offsets map back to the message they fall in"
        );
        // Consecutive windows share exactly the overlap: window 1 starts WINDOW_STRIDE into
        // window 0, so window 0's tail past that point re-appears as window 1's head.
        let w0_tail = &windows[0].text[WINDOW_STRIDE..];
        assert_eq!(w0_tail.len(), WINDOW_OVERLAP);
        assert!(
            windows[1].text.starts_with(w0_tail),
            "overlap must repeat across the cut"
        );
    }

    #[test]
    fn cut_windows_caps_at_max_windows_and_reports_truncation() {
        // More text than MAX_WINDOWS can cover → exactly MAX_WINDOWS windows + the truncated flag.
        let pieces = vec![(0u32, "x".repeat(SESSION_TEXT_BUDGET + 10 * WINDOW_STRIDE))];
        let (windows, truncated) = cut_windows(&pieces);
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert!(truncated, "text beyond the window cap must be reported as truncated");
        // Just-under-budget text fits without truncation.
        let pieces = vec![(0u32, "y".repeat(SESSION_TEXT_BUDGET - WINDOW_BYTES))];
        let (windows, truncated) = cut_windows(&pieces);
        assert!(windows.len() < MAX_WINDOWS);
        assert!(!truncated);
    }

    // -- store round-trip ------------------------------------------------------------------------

    /// The v3 binary store round-trips exactly: session metadata (incl. the `(mtime, size)`
    /// freshness sigs and the truncated flag), window records, dim, and every vector bit.
    #[test]
    fn store_v3_roundtrips_with_sigs_and_windows() {
        let dir = tmpdir();
        let path = dir.join("embeddings.bin");

        let mut a = smeta("a", "/tmp/a.jsonl", (111, 4096));
        a.truncated = true;
        let s = Store {
            model: "test-model".into(),
            dim: 3,
            sessions: vec![a, smeta("b", "/tmp/b.jsonl", (222, 8192))],
            records: vec![
                WindowMeta { s: 0, w: 0, m: 0 },
                WindowMeta { s: 0, w: 1, m: 7 },
                WindowMeta { s: 1, w: 0, m: 3 },
            ],
            vectors: vec![0.25, -1.5, 3.0, f32::MIN_POSITIVE, 0.0, -0.0, 1.0, 2.0, 3.0],
        };
        save_store(&path, &s).unwrap();
        let back = load_store(&path).unwrap();
        assert_eq!(back.model, "test-model");
        assert_eq!(back.dim, 3);
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.sessions[0].id, "a");
        assert_eq!((back.sessions[0].mtime, back.sessions[0].size), (111, 4096));
        assert!(back.sessions[0].truncated);
        assert_eq!((back.sessions[1].mtime, back.sessions[1].size), (222, 8192));
        assert!(!back.sessions[1].truncated);
        assert_eq!(back.sessions[1].title.as_deref(), Some("title-b"));
        assert_eq!(back.records.len(), 3);
        assert_eq!((back.records[1].s, back.records[1].w, back.records[1].m), (0, 1, 7));
        assert_eq!(back.vectors, s.vectors); // bit-exact through to_le_bytes/from_le_bytes

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Pre-v3 stores (v2 binary head-only, or the original all-JSON layout) are stale: the query
    /// path errors with a rebuild hint, and the indexing path treats them as absent (full re-embed).
    #[test]
    fn older_store_formats_are_stale_not_migrated() {
        let dir = tmpdir();

        let v2 = dir.join("v2.bin");
        std::fs::write(&v2, b"CVEMBED\x02rest-of-a-v2-store").unwrap();
        let err = load_store(&v2).err().expect("v2 must not load").to_string();
        assert!(err.contains("cv index --semantic"), "rebuild hint expected: {err}");
        assert!(
            load_store_for_index(&v2).is_none(),
            "index path re-embeds over a v2 store"
        );

        let legacy = dir.join("embeddings.json");
        std::fs::write(&legacy, r#"{"model":"m","records":[]}"#).unwrap();
        let err = load_store(&legacy)
            .err()
            .expect("legacy JSON must not load")
            .to_string();
        assert!(err.contains("cv index --semantic"), "rebuild hint expected: {err}");
        assert!(load_store_for_index(&legacy).is_none());

        assert!(load_store_for_index(&dir.join("absent.bin")).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Garbage on disk fails loudly with a re-index hint instead of mis-parsing.
    #[test]
    fn bad_magic_is_an_error() {
        let dir = tmpdir();
        let path = dir.join("embeddings.bin");
        std::fs::write(&path, b"not a store at all").unwrap();
        let err = load_store(&path).err().expect("bad magic must fail").to_string();
        assert!(err.contains("bad magic"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- incremental embedding -------------------------------------------------------------------

    /// The incremental core: first run embeds everything; a pure mtime bump (size unchanged on an
    /// append-only log) carries every vector over with ZERO embed calls; a real append re-embeds
    /// exactly the appended session; a vanished session is dropped; `rebuild` re-embeds all.
    #[test]
    fn incremental_skips_fresh_reembeds_changed_drops_vanished() {
        let dir = tmpdir();
        let store_path = dir.join("embeddings.bin");
        let p1 = write_claude(&dir, "s1", "alpha session one body");
        let p2 = write_claude(&dir, "s2", "beta session two body");
        let refs = || vec![sref("s1", "one", p1.clone()), sref("s2", "two", p2.clone())];

        // First run: both sessions are new → both parsed + embedded (one window each).
        let mut calls = 0usize;
        let stats = embed_pass(&store_path, false, "fake", &mut fake_embed(&mut calls), refs()).unwrap();
        assert_eq!((stats.embedded, stats.carried, stats.removed), (2, 0, 0));
        assert_eq!(calls, 2, "two small sessions → one window text embedded each");
        let store = load_store(&store_path).unwrap();
        assert_eq!(store.sessions.len(), 2);
        assert_eq!(store.records.len(), 2);
        assert_eq!(store.vectors.len(), 2 * store.dim);

        // Pure mtime bump on both (byte-identical rewrite): size-primary freshness must carry
        // everything over without a single embed call.
        bump_mtime(&p1);
        bump_mtime(&p2);
        let mut calls = 0usize;
        let stats = embed_pass(&store_path, false, "fake", &mut fake_embed(&mut calls), refs()).unwrap();
        assert_eq!(
            (stats.embedded, stats.carried, stats.removed),
            (0, 2, 0),
            "pure mtime bump must re-embed NOTHING"
        );
        assert_eq!(calls, 0, "carried-over sessions must not touch the embedder");
        let carried = load_store(&store_path).unwrap();
        assert_eq!(carried.vectors, store.vectors, "carried vectors are bit-identical");

        // Real append to one session (size grows) → exactly that one re-embeds.
        append(&p2, "gamma appended tail");
        let mut calls = 0usize;
        let stats = embed_pass(&store_path, false, "fake", &mut fake_embed(&mut calls), refs()).unwrap();
        assert_eq!((stats.embedded, stats.carried), (1, 1));
        assert_eq!(calls, 1, "only the appended session's window re-embeds");
        // The re-embedded session's stored sig caught up to the new size.
        let store = load_store(&store_path).unwrap();
        let s2 = store.sessions.iter().find(|s| s.id == "s2").unwrap();
        assert_eq!(s2.size, std::fs::metadata(&p2).unwrap().len() as i64);

        // A session missing from the sweep is dropped from the store.
        let mut calls = 0usize;
        let stats = embed_pass(
            &store_path,
            false,
            "fake",
            &mut fake_embed(&mut calls),
            vec![sref("s1", "one", p1.clone())],
        )
        .unwrap();
        assert_eq!((stats.embedded, stats.carried, stats.removed), (0, 1, 1));
        let store = load_store(&store_path).unwrap();
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.sessions[0].id, "s1");
        assert_eq!(store.records.len(), 1);

        // --rebuild ignores the fresh store and re-embeds everything.
        let mut calls = 0usize;
        let stats = embed_pass(&store_path, true, "fake", &mut fake_embed(&mut calls), refs()).unwrap();
        assert_eq!((stats.embedded, stats.carried), (2, 0));
        assert_eq!(calls, 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A store built under a different model id is never carried over — same-id-different-space
    /// vectors would be cosine noise, so a model switch is a full re-embed.
    #[test]
    fn model_change_forces_full_reembed() {
        let dir = tmpdir();
        let store_path = dir.join("embeddings.bin");
        let p = write_claude(&dir, "m1", "some session body");
        let refs = || vec![sref("m1", "one", p.clone())];

        let mut calls = 0usize;
        embed_pass(&store_path, false, "model-a", &mut fake_embed(&mut calls), refs()).unwrap();
        let mut calls = 0usize;
        let stats = embed_pass(&store_path, false, "model-b", &mut fake_embed(&mut calls), refs()).unwrap();
        assert_eq!((stats.embedded, stats.carried), (1, 0), "model switch must re-embed");
        assert_eq!(load_store(&store_path).unwrap().model, "model-b");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end through the real parse + window pipeline: a session big enough for several
    /// windows stores one vector per window, each tagged with the message index its window starts
    /// at — so a late-session hit can point past the head.
    #[test]
    fn big_session_gets_multiple_located_windows() {
        let dir = tmpdir();
        let store_path = dir.join("embeddings.bin");
        // ~5 messages × ~2.5 KB → ~12.5 KB of text → several windows across several messages.
        let m = "lorem ipsum dolor sit amet ".repeat(100);
        let msgs: Vec<(&str, &str)> = vec![
            ("user", m.as_str()),
            ("assistant", m.as_str()),
            ("user", m.as_str()),
            ("assistant", m.as_str()),
            ("user", m.as_str()),
        ];
        let p = write_claude_msgs(&dir, "big", &msgs);
        let mut calls = 0usize;
        embed_pass(
            &store_path,
            false,
            "fake",
            &mut fake_embed(&mut calls),
            vec![sref("big", "big session", p)],
        )
        .unwrap();

        let store = load_store(&store_path).unwrap();
        assert_eq!(store.sessions.len(), 1);
        assert!(
            store.records.len() >= 4,
            "a ~12.5 KB session must produce several window vectors, got {}",
            store.records.len()
        );
        assert_eq!(calls, store.records.len());
        assert!(!store.sessions[0].truncated, "well under the window cap");
        // Window ordinals are sequential and the later windows start in later messages.
        assert!(store.records.windows(2).all(|w| w[1].w == w[0].w + 1));
        assert_eq!(store.records.first().unwrap().m, 0);
        assert!(
            store.records.last().unwrap().m > 0,
            "a late window must be tagged with a later message index"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // -- ranking / dedup -------------------------------------------------------------------------

    /// Every window competes, but results dedup to each session's best window: one row per
    /// session, carrying the winning window's message index for the location display.
    #[test]
    fn rank_dedups_to_best_window_per_session() {
        let store = Store {
            model: "test".into(),
            dim: 2,
            sessions: vec![
                smeta("a", "/nonexistent/a.jsonl", (1, 2)),
                smeta("b", "/nonexistent/b.jsonl", (3, 4)),
            ],
            records: vec![
                WindowMeta { s: 0, w: 0, m: 2 },  // a, mediocre window
                WindowMeta { s: 0, w: 1, m: 41 }, // a, best window — must win the dedup
                WindowMeta { s: 1, w: 0, m: 7 },  // b, its only window
            ],
            vectors: vec![
                0.6, 0.8, // a/w0: cosine 0.6 against [1,0]
                1.0, 0.0, // a/w1: cosine 1.0
                0.8, 0.6, // b/w0: cosine 0.8
            ],
        };
        let ranked = rank(&store, &[1.0, 0.0], 10);
        assert_eq!(ranked.len(), 2, "3 windows dedup to 2 sessions");
        assert_eq!(ranked[0].1, 1, "session a is represented by its BEST window");
        assert!((ranked[0].0 - 1.0).abs() < 1e-6);
        assert_eq!(ranked[1].1, 2, "session b second");

        // k truncates after dedup, keeping the strongest sessions.
        let ranked = rank(&store, &[1.0, 0.0], 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].1, 1);

        // Projection carries the winning window's location + dates, and falls back to the stored
        // preview (with the moved-file marker) since the paths don't exist.
        let hits = to_hits(&store, rank(&store, &[1.0, 0.0], 10), "anything");
        assert_eq!(hits[0].hit.id, "a");
        assert_eq!(hits[0].first_msg, 41);
        assert_eq!(hits[0].window, 1);
        assert_eq!(hits[0].hit.created_at, Some(100));
        assert_eq!(hits[0].hit.updated_at, Some(200));
        assert_eq!(hits[0].hit.snippet, "preview-a (source file moved)");
        assert_eq!(hits[1].hit.id, "b");
        assert_eq!(hits[1].first_msg, 7);
    }

    /// The live window snippet re-reads the session *starting at the recorded message*: text from
    /// skipped earlier messages must not appear, and a query term in the window is centered.
    #[test]
    fn window_snippet_starts_at_the_recorded_message() {
        let dir = tmpdir();
        let p = write_claude_msgs(
            &dir,
            "snip",
            &[
                ("user", "zqearly text that must be skipped"),
                ("assistant", "middle chatter"),
                ("user", "the window starts here and mentions zqtarget prominently"),
            ],
        );

        let s = window_snippet(&p, "claude", 2, "zqtarget").expect("snippet");
        assert!(s.contains("zqtarget"), "term in the window should surface: {s:?}");
        assert!(
            !s.contains("zqearly"),
            "messages before the window must be skipped: {s:?}"
        );

        // No term match → head of the window, still not the skipped messages.
        let s = window_snippet(&p, "claude", 2, "kubernetes").expect("snippet");
        assert!(s.starts_with("the window starts here"), "head fallback expected: {s:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Unit guard on the freshness predicate: size-primary only for append-only logs, size+mtime
    /// for rewriteable stores, and unknown/zero sizes are never fresh (same policy as FTS).
    #[test]
    fn embed_is_fresh_matches_the_fts_policy() {
        let append_only = sref("append", "append", "/tmp/append.jsonl".into());
        let non_append = cv_core::SessionRef {
            id: "cline-task".into(),
            harness: cv_core::Harness::Cline,
            path: "/tmp/task/api_conversation_history.json".into(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        assert!(embed_is_fresh(&append_only, Some((111, 4096)), 222, 4096));
        assert!(!embed_is_fresh(&append_only, Some((111, 4096)), 111, 8192));
        assert!(!embed_is_fresh(&append_only, Some((111, 0)), 111, 0));
        assert!(!embed_is_fresh(&append_only, None, 111, 4096));
        assert!(embed_is_fresh(&non_append, Some((111, 4096)), 111, 4096));
        assert!(!embed_is_fresh(&non_append, Some((111, 4096)), 222, 4096));
    }
}
