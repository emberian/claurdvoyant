// Entry point. Loads the wasm ingest module if present; otherwise falls back to
// the bundled sample dataset so the page is demoable immediately.
//
// WASM contract (built by the cv-web crate via wasm-pack --target web):
//   import init, { ingest_zip } from "./pkg/cv_web.js";
//   await init();                          // async initializer
//   const json = ingest_zip(uint8array);   // returns a JSON string: Session[]
//
// We don't statically `import` the pkg module (it may not exist on the static
// fallback deploy), so we use a dynamic import wrapped in try/catch.

import "./components/cv-app.js";

/** @type {null | ((bytes: Uint8Array) => string)} */
let wasmIngest = null;
let wasmStatus = "loading";

async function loadWasm() {
  try {
    const mod = await import("./pkg/cv_web.js");
    const init = mod.default;
    if (typeof init === "function") {
      await init();
    }
    if (typeof mod.ingest_zip !== "function") {
      throw new Error("cv_web.js loaded but ingest_zip export is missing");
    }
    wasmIngest = mod.ingest_zip;
    wasmStatus = "ready";
  } catch (err) {
    // Expected on the static fallback deploy (no pkg/ built). Demo via sample.js.
    console.info("[claurdvoyant] wasm module unavailable, using sample fallback:", err?.message ?? err);
    wasmStatus = "missing";
  }
}

const ready = loadWasm();

/**
 * Ingest a .zip's raw bytes into an array of Session objects.
 * Throws if the wasm module isn't available.
 * @param {Uint8Array} bytes
 * @returns {Promise<object[]>}
 */
export async function ingestZip(bytes) {
  await ready;
  if (!wasmIngest) {
    throw new Error(
      "The WASM ingest module isn't available in this build, so .zip parsing is disabled. " +
      "You can still explore the bundled sample dataset."
    );
  }
  const json = wasmIngest(bytes);
  const parsed = JSON.parse(json);
  if (!Array.isArray(parsed)) {
    throw new Error("ingest_zip did not return a JSON array");
  }
  return parsed;
}

/** @returns {Promise<"ready"|"missing">} resolves once wasm load is settled. */
export async function wasmReady() {
  await ready;
  return wasmStatus === "ready" ? "ready" : "missing";
}

/** Load the bundled sample sessions. @returns {Promise<object[]>} */
export async function loadSample() {
  const mod = await import("./sample.js");
  return mod.SAMPLE_SESSIONS ?? mod.default ?? [];
}
