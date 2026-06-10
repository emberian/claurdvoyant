//! Per-message byte offsets — the seekable-session store (REARCH Phase 2).
//!
//! Recorded into `catalog.db` during the ingest pass; lets windowed consumers seek straight to
//! a message range instead of streaming from byte 0. Implementation lands with the offsets agent.
