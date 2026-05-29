//! Semantic search via model2vec static embeddings + brute-force cosine.
//!
//! model2vec embeds text with a *static* distilled embedding table (no transformer forward pass,
//! no ONNX): tokenize, look up per-token vectors, mean-pool. It's tiny and CPU-fast, which makes
//! "embed every session up front, then cosine-scan in memory" a perfectly tasteful design for the
//! ~1k-session scale here. No ANN crate required.
//!
//! On first use the model (`MODEL_REPO`) is fetched from the HuggingFace Hub and cached by
//! `hf-hub` under `~/.cache/huggingface`. To pin a local copy (offline), set
//! `CV_SEARCH_MODEL=/path/to/model-dir`.

use crate::Hit;
use anyhow::{Context, Result};
use model2vec_rs::model::StaticModel;
use std::path::Path;

/// A small, strong static-embedding model (~30MB, 256-dim). Distilled from a sentence encoder.
const MODEL_REPO: &str = "minishlab/potion-base-8M";

/// The persisted embedding store: model id + one record per session.
#[derive(serde::Serialize, serde::Deserialize)]
struct Store {
    model: String,
    records: Vec<Record>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    id: String,
    harness: String,
    cwd: Option<String>,
    title: Option<String>,
    /// A short preview of the body, for display in hits.
    preview: String,
    vector: Vec<f32>,
}

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

/// Discover + parse + embed every session, persisting vectors to `path` (JSON).
pub fn embed_all(path: &Path) -> Result<usize> {
    let docs = crate::build_corpus();
    let model = load_model()?;

    // model2vec encodes a batch in one call; truncation to 512 tokens is the library default.
    let texts: Vec<String> = docs.iter().map(|d| d.body.clone()).collect();
    let vectors = model.encode(&texts);

    let records = docs
        .into_iter()
        .zip(vectors)
        .map(|(d, vector)| Record {
            id: d.id,
            harness: d.harness,
            cwd: d.cwd,
            title: d.title,
            preview: preview(&d.body, 200),
            vector,
        })
        .collect::<Vec<_>>();

    let store = Store {
        model: MODEL_REPO.to_string(),
        records,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_vec(&store).context("serializing embedding store")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing embedding store {}", path.display()))?;
    Ok(store.records.len())
}

/// Embed `query` and return the top-`k` sessions by cosine similarity.
pub fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading embedding store {} — run `embed_all` first",
            path.display()
        )
    })?;
    let store: Store = serde_json::from_slice(&bytes).context("parsing embedding store")?;

    let model = load_model()?;
    let q = model.encode_single(query);

    let mut scored: Vec<(f32, &Record)> = store
        .records
        .iter()
        .map(|r| (cosine(&q, &r.vector), r))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);

    Ok(scored
        .into_iter()
        .map(|(score, r)| Hit {
            id: r.id.clone(),
            harness: r.harness.clone(),
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            score,
            snippet: r.preview.clone(),
        })
        .collect())
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
