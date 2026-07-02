//! cv-web — WASM bindings for clustervision.
//!
//! The browser app uploads a harness directory as a `.zip`; we unzip it entirely in memory (no
//! filesystem), sniff + parse each entry via [`cv_core::ingest`], and hand back the sessions as a
//! JSON string for the frontend.

use std::io::{Cursor, Read};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn warn(s: &str);
}

/// Everything is decompressed in memory, so cap what one zip may expand to: a hostile archive
/// (zip bomb / forged size headers) gets its oversized entries skipped — with a console warning —
/// instead of exhausting the wasm heap or aborting the whole ingest.
const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Ingest the bytes of an uploaded `.zip` (a harness directory) and return a JSON array of sessions.
#[wasm_bindgen]
pub fn ingest_zip(bytes: &[u8]) -> Result<String, JsValue> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| JsValue::from_str(&format!("zip: {e}")))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(archive.len());
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| JsValue::from_str(&format!("zip entry {i}: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let cap = MAX_ENTRY_BYTES.min(MAX_TOTAL_BYTES.saturating_sub(total));
        if entry.size() > cap {
            warn(&format!(
                "cv-web: skipping {name}: {} bytes exceeds the extraction cap ({cap})",
                entry.size()
            ));
            continue;
        }
        // The header's size is attacker-controlled, so cap the bytes actually decompressed (one
        // past `cap` distinguishes "fits exactly" from "lied and kept going") rather than trust it.
        let mut buf = Vec::new();
        (&mut entry)
            .take(cap.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| JsValue::from_str(&format!("reading {name}: {e}")))?;
        if buf.len() as u64 > cap {
            warn(&format!("cv-web: skipping {name}: decompressed past the extraction cap ({cap})"));
            continue;
        }
        total += buf.len() as u64;
        files.push((name, buf));
    }

    let sessions = cv_core::ingest::ingest_files(files);
    serde_json::to_string(&sessions).map_err(|e| JsValue::from_str(&format!("json: {e}")))
}
