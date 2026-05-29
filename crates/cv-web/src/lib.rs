//! cv-web — WASM bindings for claurdvoyant.
//!
//! The browser app uploads a harness directory as a `.zip`; we unzip it entirely in memory (no
//! filesystem), sniff + parse each entry via [`cv_core::ingest`], and hand back the sessions as a
//! JSON string for the frontend.

use std::io::{Cursor, Read};
use wasm_bindgen::prelude::*;

/// Ingest the bytes of an uploaded `.zip` (a harness directory) and return a JSON array of sessions.
#[wasm_bindgen]
pub fn ingest_zip(bytes: &[u8]) -> Result<String, JsValue> {
    let reader = Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| JsValue::from_str(&format!("zip: {e}")))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| JsValue::from_str(&format!("zip entry {i}: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| JsValue::from_str(&format!("reading {name}: {e}")))?;
        files.push((name, buf));
    }

    let sessions = cv_core::ingest::ingest_files(files);
    serde_json::to_string(&sessions).map_err(|e| JsValue::from_str(&format!("json: {e}")))
}
