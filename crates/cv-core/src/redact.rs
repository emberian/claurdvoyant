//! Redact secrets / PII from a [`Session`] before sharing or export.
//!
//! The contribution loop ("send us your logs") and any enterprise use both hinge on being able to
//! scrub a transcript first. This returns a copy with secrets replaced by placeholders.
//!
//! OWNED BY the redact work. (Skeleton.)

use crate::ir::Session;

/// Return a copy of `session` with secrets/PII scrubbed from all message + tool content.
pub fn redact(session: &Session) -> Session {
    session.clone()
}
