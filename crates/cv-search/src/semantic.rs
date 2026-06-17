//! Semantic search via model2vec static embeddings + brute-force cosine.
//!
//! model2vec embeds text with a *static* distilled embedding table (no transformer forward pass,
//! no ONNX): tokenize, look up per-token vectors, mean-pool. It's tiny and CPU-fast, which makes
//! "embed every session up front, then cosine-scan in memory" a perfectly tasteful design for the
//! few-thousand-session scale here. No ANN crate required.
//!
//! On first use the model (`MODEL_REPO`) is fetched from the HuggingFace Hub and cached by
//! `hf-hub` under `~/.cache/huggingface`. To pin a local copy (offline), set
//! `CV_SEARCH_MODEL=/path/to/model-dir`.
//!
//! # The store format
//!
//! Vectors are persisted in a small hand-rolled binary layout (fixed-width little-endian f32s)
//! instead of JSON: a query loads ~N·dim·4 bytes and reinterprets them, rather than JSON-parsing
//! hundreds of thousands of ASCII floats. Layout, all integers little-endian:
//!
//! ```text
//! [8]  magic  b"CVEMBED\x02"
//! u32  dim          (vector dimensionality)
//! u32  count        (number of records)
//! u64  meta_len
//! […]  meta JSON    ({ model, records: [{id, harness, cwd, title, preview, dates}…] })
//! […]  vectors      (count × dim × f32 LE, record i's vector at offset i·dim)
//! ```
//!
//! The metadata stays JSON (it's small, schema-evolvable via serde defaults); only the bulk —
//! the vectors — is fixed-width. A store written by an older build (pure JSON, starts with `{`)
//! still loads, and [`semantic_search`] migrates the legacy default `embeddings.json` to the
//! binary default path on first use.

use crate::Hit;
use anyhow::{bail, Context, Result};
use model2vec_rs::model::StaticModel;
use std::path::Path;

/// A small, strong static-embedding model (~30MB, 256-dim). Distilled from a sentence encoder.
const MODEL_REPO: &str = "minishlab/potion-base-8M";

/// Store header magic: `CVEMBED` + format version 2 (version 1 was the implicit all-JSON store).
const MAGIC: &[u8; 8] = b"CVEMBED\x02";

/// Per-session metadata, persisted as the store's JSON block (everything but the vector).
#[derive(serde::Serialize, serde::Deserialize)]
struct RecordMeta {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    /// A short preview of the body, for display in hits.
    preview: String,
    /// Unix-seconds session dates, surfaced on hits. `serde(default)` keeps stores written
    /// before these fields loading cleanly (their hits are just dateless until the next
    /// `embed_all`).
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
}

/// The JSON block inside the binary store (deserialize side; serialization uses a borrowed twin
/// in [`save_store`] to avoid cloning the records).
#[derive(serde::Deserialize)]
struct MetaBlock {
    model: String,
    records: Vec<RecordMeta>,
}

/// The in-memory store: metadata plus one flat `count × dim` vector block (record `i`'s vector is
/// `vectors[i*dim..(i+1)*dim]`).
struct Store {
    model: String,
    dim: usize,
    meta: Vec<RecordMeta>,
    vectors: Vec<f32>,
}

impl Store {
    fn vector(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

// ---- legacy (pre-binary) store --------------------------------------------------------------

/// The old all-JSON store layout, kept so existing `embeddings.json` files load + migrate.
#[derive(serde::Deserialize)]
struct LegacyStore {
    model: String,
    records: Vec<LegacyRecord>,
}

#[derive(serde::Deserialize)]
struct LegacyRecord {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    preview: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
    vector: Vec<f32>,
}

impl From<LegacyStore> for Store {
    fn from(old: LegacyStore) -> Store {
        let dim = old.records.first().map(|r| r.vector.len()).unwrap_or(0);
        let mut meta = Vec::with_capacity(old.records.len());
        let mut vectors = Vec::with_capacity(old.records.len() * dim);
        for r in old.records {
            // A malformed row (wrong dim) would shear the flat block; skip it instead.
            if r.vector.len() != dim {
                continue;
            }
            vectors.extend_from_slice(&r.vector);
            meta.push(RecordMeta {
                id: r.id,
                harness: r.harness,
                cwd: r.cwd,
                title: r.title,
                preview: r.preview,
                created_at: r.created_at,
                updated_at: r.updated_at,
            });
        }
        Store { model: old.model, dim, meta, vectors }
    }
}

// ---- store I/O -------------------------------------------------------------------------------

/// Serialize + write the store atomically (temp + rename).
fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let meta_json = {
        // Serialize the metadata without cloning the records: a borrowed twin of `MetaBlock`.
        #[derive(serde::Serialize)]
        struct MetaBlockRef<'a> {
            model: &'a str,
            records: &'a [RecordMeta],
        }
        serde_json::to_vec(&MetaBlockRef { model: &store.model, records: &store.meta })
            .context("serializing embedding store metadata")?
    };

    let mut out = Vec::with_capacity(8 + 4 + 4 + 8 + meta_json.len() + store.vectors.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(store.dim as u32).to_le_bytes());
    out.extend_from_slice(&(store.meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&(meta_json.len() as u64).to_le_bytes());
    out.extend_from_slice(&meta_json);
    for v in &store.vectors {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, &out)
        .with_context(|| format!("writing embedding store {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving embedding store into place at {}", path.display()))?;
    Ok(())
}

/// Load a store: the binary format, or (transparently) a legacy all-JSON store.
fn load_store(path: &Path) -> Result<Store> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading embedding store {} — run `embed_all` first",
            path.display()
        )
    })?;
    if bytes.first() == Some(&b'{') {
        let legacy: LegacyStore =
            serde_json::from_slice(&bytes).context("parsing legacy JSON embedding store")?;
        return Ok(legacy.into());
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
    let meta: MetaBlock =
        serde_json::from_slice(&bytes[24..meta_end]).context("parsing embedding store metadata")?;
    let vec_bytes = &bytes[meta_end..];
    if meta.records.len() != count || vec_bytes.len() != count * dim * 4 {
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
    Ok(Store { model: meta.model, dim, meta: meta.records, vectors })
}

// ---- embedding -------------------------------------------------------------------------------

/// Load the embedding model, honoring `CV_SEARCH_MODEL` (local dir) before falling back to the Hub.
fn load_model() -> Result<StaticModel> {
    if let Some(dir) = std::env::var_os("CV_SEARCH_MODEL") {
        return StaticModel::from_pretrained(dir, None, None, None)
            .context("loading model from $CV_SEARCH_MODEL");
    }
    StaticModel::from_pretrained(MODEL_REPO, None, None, None).with_context(|| {
        format!("loading/downloading embedding model {MODEL_REPO} (set CV_SEARCH_MODEL for offline)")
    })
}

/// Sessions whose bodies are batched into one `model.encode` call. Bounds how many session body
/// heads are resident at once during embedding.
const EMBED_BATCH: usize = 128;

/// Discover + parse + embed every session, persisting vectors to `path` (binary store).
///
/// Streams the corpus in bounded batches via [`crate::stream_corpus`]: each session contributes
/// only its **head** (the model truncates to 512 tokens anyway — see `EMBED_HEAD_BYTES` there), at
/// most `EMBED_BATCH` heads are resident, and each is dropped right after its batch is encoded —
/// only the small metadata + the flat vector block survive.
pub fn embed_all(path: &Path) -> Result<usize> {
    let model = load_model()?;

    let mut store = Store {
        model: MODEL_REPO.to_string(),
        dim: 0,
        meta: Vec::new(),
        vectors: Vec::new(),
    };
    // Pending batch: `pending[i]` is the metadata for `bodies[i]`, vector not yet encoded.
    let mut pending: Vec<RecordMeta> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();

    crate::stream_corpus(|d| {
        pending.push(RecordMeta {
            id: d.id,
            harness: d.harness,
            cwd: d.cwd,
            title: d.title,
            preview: preview(&d.body, 200),
            created_at: d.created_at,
            updated_at: d.updated_at,
        });
        bodies.push(d.body);
        if bodies.len() >= EMBED_BATCH {
            encode_batch(&model, &mut pending, &mut bodies, &mut store);
        }
        Ok(())
    })?;
    encode_batch(&model, &mut pending, &mut bodies, &mut store); // final partial batch

    save_store(path, &store)?;
    Ok(store.meta.len())
}

/// Encode the pending batch of bodies (one `model.encode` call — model2vec truncates to 512 tokens
/// by default), appending each vector to the store's flat block alongside its metadata. Clears the
/// batch (freeing the bodies) before returning.
fn encode_batch(
    model: &StaticModel,
    pending: &mut Vec<RecordMeta>,
    bodies: &mut Vec<String>,
    store: &mut Store,
) {
    if bodies.is_empty() {
        return;
    }
    let vectors = model.encode(bodies.as_slice());
    for (rec, vector) in pending.drain(..).zip(vectors) {
        if store.dim == 0 {
            store.dim = vector.len();
        }
        if vector.len() != store.dim {
            continue; // defensive: a shape-mismatched vector would shear the flat block
        }
        store.vectors.extend_from_slice(&vector);
        store.meta.push(rec);
    }
    bodies.clear();
}

// ---- query -----------------------------------------------------------------------------------

/// Embed `query` and return the top-`k` sessions by cosine similarity.
///
/// If `path` doesn't exist but a legacy `embeddings.json` sits next to it (written by an older
/// build), that store is migrated to the binary format at `path` first — so upgrading doesn't
/// force a re-embed.
pub fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    migrate_legacy_sibling(path);
    let store = load_store(path)?;
    let model = load_model()?;
    let q = model.encode_single(query);
    Ok(rank(&store, &q, k))
}

/// One-time upgrade: when `path` is absent but the old default `embeddings.json` exists alongside
/// it, convert that JSON store into the binary format at `path`. Best-effort (a failure just means
/// `load_store` reports the missing file); the legacy file is left untouched.
fn migrate_legacy_sibling(path: &Path) {
    if path.exists() || path.file_name().is_none() {
        return;
    }
    let legacy = path.with_file_name("embeddings.json");
    if legacy == path || !legacy.exists() {
        return;
    }
    if let Ok(store) = load_store(&legacy) {
        if save_store(path, &store).is_ok() {
            eprintln!(
                "(migrated legacy embedding store {} → {})",
                legacy.display(),
                path.display()
            );
        }
    }
}

/// Brute-force cosine ranking of the store against an already-embedded query, mapping the top-`k`
/// records to [`Hit`]s. Split from [`semantic_search`] so the record→hit projection (dates,
/// preview-as-snippet) is testable without loading the embedding model.
fn rank(store: &Store, q: &[f32], k: usize) -> Vec<Hit> {
    let mut scored: Vec<(f32, usize)> = (0..store.meta.len())
        .map(|i| (cosine(q, store.vector(i)), i))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);

    scored
        .into_iter()
        .map(|(score, i)| {
            let r = &store.meta[i];
            Hit {
                id: r.id.clone(),
                harness: r.harness.clone(),
                cwd: r.cwd.clone(),
                title: r.title.clone(),
                score,
                snippet: r.preview.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at,
                // Semantic store doesn't fold the sub-agent forest (yet); no provenance to carry.
                parent_id: None,
                agent_id: None,
                workflow: None,
            }
        })
        .collect()
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

fn preview(s: &str, max_chars: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, dates: Option<(i64, i64)>) -> RecordMeta {
        RecordMeta {
            id: id.into(),
            harness: "claude".into(),
            cwd: None,
            title: Some(format!("title-{id}")),
            preview: format!("preview-{id}"),
            created_at: dates.map(|(c, _)| c),
            updated_at: dates.map(|(_, u)| u),
        }
    }

    fn store(records: Vec<(RecordMeta, Vec<f32>)>) -> Store {
        let dim = records.first().map(|(_, v)| v.len()).unwrap_or(0);
        let mut s = Store { model: "test".into(), dim, meta: Vec::new(), vectors: Vec::new() };
        for (m, v) in records {
            assert_eq!(v.len(), dim);
            s.vectors.extend_from_slice(&v);
            s.meta.push(m);
        }
        s
    }

    /// Dates stored on a record surface on its hit; records embedded without dates stay `None`.
    #[test]
    fn rank_carries_dates_onto_hits() {
        let store = store(vec![
            (meta("dated", Some((100, 200))), vec![1.0, 0.0]),
            (meta("dateless", None), vec![0.0, 1.0]),
        ]);
        let hits = rank(&store, &[1.0, 0.0], 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "dated"); // cosine 1.0 ranks first
        assert_eq!(hits[0].created_at, Some(100));
        assert_eq!(hits[0].updated_at, Some(200));
        assert_eq!(hits[0].snippet, "preview-dated");
        assert_eq!(hits[1].id, "dateless");
        assert_eq!(hits[1].created_at, None);
        assert_eq!(hits[1].updated_at, None);
    }

    /// The binary store round-trips exactly: metadata, dim, and every vector bit.
    #[test]
    fn binary_store_roundtrips() {
        let dir = std::env::temp_dir().join(format!("cv-emb-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");

        let s = store(vec![
            (meta("a", Some((1, 2))), vec![0.25, -1.5, 3.0]),
            (meta("b", None), vec![f32::MIN_POSITIVE, 0.0, -0.0]),
        ]);
        save_store(&path, &s).unwrap();
        let back = load_store(&path).unwrap();
        assert_eq!(back.model, "test");
        assert_eq!(back.dim, 3);
        assert_eq!(back.meta.len(), 2);
        assert_eq!(back.meta[0].id, "a");
        assert_eq!(back.meta[0].created_at, Some(1));
        assert_eq!(back.meta[1].title.as_deref(), Some("title-b"));
        assert_eq!(back.vectors, s.vectors); // bit-exact through to_le_bytes/from_le_bytes
        // And the loaded store ranks like the in-memory one.
        assert_eq!(rank(&back, &[0.25, -1.5, 3.0], 1)[0].id, "a");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An embedding store written by an older build (pure JSON, with or without the date fields)
    /// must still load — and `migrate_legacy_sibling` upgrades it to the binary default path.
    #[test]
    fn legacy_json_store_loads_and_migrates() {
        let json = r#"{
            "model": "minishlab/potion-base-8M",
            "records": [{
                "id": "old", "harness": "claude", "cwd": null, "title": "t",
                "preview": "p", "vector": [0.5, 0.5]
            }]
        }"#;
        let dir = std::env::temp_dir().join(format!("cv-emb-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_path = dir.join("embeddings.json");
        std::fs::write(&legacy_path, json).unwrap();

        // Loads directly (dateless fields default to None), ranks, and maps to a hit.
        let store = load_store(&legacy_path).unwrap();
        assert_eq!(store.dim, 2);
        assert_eq!(store.meta.len(), 1);
        assert_eq!(store.meta[0].created_at, None);
        let hits = rank(&store, &[0.5, 0.5], 1);
        assert_eq!(hits[0].id, "old");
        assert_eq!(hits[0].created_at, None);

        // Sibling migration: a missing binary store next to the legacy JSON gets created.
        let bin_path = dir.join("embeddings.bin");
        migrate_legacy_sibling(&bin_path);
        assert!(bin_path.exists(), "legacy store should migrate to the binary path");
        let migrated = load_store(&bin_path).unwrap();
        assert_eq!(migrated.meta.len(), 1);
        assert_eq!(migrated.vectors, vec![0.5, 0.5]);
        assert!(legacy_path.exists(), "legacy file is left in place");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Garbage on disk fails loudly with a re-index hint instead of mis-parsing.
    #[test]
    fn bad_magic_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cv-emb-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");
        std::fs::write(&path, b"not a store at all").unwrap();
        let err = load_store(&path).err().expect("bad magic must fail").to_string();
        assert!(err.contains("bad magic"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
