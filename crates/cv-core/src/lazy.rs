//! Lazy content: the IR holds large text as a **span** into its source file, not as owned bytes.
//!
//! A fully-parsed [`Session`](crate::ir::Session) used to *own* every byte of content
//! (`Block::Text { text: String }`, …) — 700 MB resident just to hold the IR of a transcript with
//! one 700 MB record, even if the consumer only wants its title. There's no need: the bytes live on
//! disk in a read-only transcript. So a content field is a [`Text`] that's either materialized
//! [`Inline`](Text::Inline) (small — most messages) or a [`Span`] (large), resolved **on demand**
//! against the source via a [`Resolver`] (which memory-maps the file, so even resolving touches only
//! the needed pages).
//!
//! Invariant: reading a `Span` as `&str` (via [`Deref`]) panics — you must [`resolve`](Text::resolve)
//! it against a [`Resolver`], or [`materialize`](crate::ir::Session::materialize) the session first.
//! Streaming consumers resolve per-block (peak = one field); whole-session consumers materialize
//! (peak = whole session, their choice). Below the inline threshold everything is `Inline`, where
//! `Deref`/`Display` just work.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Content at or below this many bytes is stored inline; larger content becomes a lazy [`Span`].
/// Small on purpose: most messages are well under it, so typical sessions are untouched and only
/// genuinely large fields (file dumps, pasted blobs, base64) go lazy.
pub const INLINE_MAX: usize = 4096;

/// A span of bytes in a source file, resolved on demand. `offset`/`len` cover the **raw** bytes; if
/// the source is a JSON string body, `escaped` marks that the slice needs JSON-unescaping on resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Source file. `None` ⇒ the owning [`Session`](crate::ir::Session)'s `source_path` (the common
    /// case); set only for adapters whose content lives in a sidecar (codex/grok update streams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    pub offset: u64,
    pub len: u64,
    /// The raw slice is a JSON string body needing unescaping (`true`) or literal text (`false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escaped: bool,
}

/// A piece of content text: materialized inline (small) or a lazy [`Span`] (large). See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    Inline(String),
    Span(Span),
}

impl Text {
    /// The inline string, or `None` if this is an unresolved span.
    pub fn inline_str(&self) -> Option<&str> {
        match self {
            Text::Inline(s) => Some(s),
            Text::Span(_) => None,
        }
    }

    pub fn is_span(&self) -> bool {
        matches!(self, Text::Span(_))
    }

    /// Resolve to the text, borrowing the inline string or (for a span) reading it from `resolver`.
    /// Cheap for inline and for unescaped spans (borrows the mmap); allocates only to unescape.
    pub fn resolve<'a>(&'a self, resolver: &'a Resolver) -> Cow<'a, str> {
        match self {
            Text::Inline(s) => Cow::Borrowed(s),
            Text::Span(sp) => resolver.resolve(sp),
        }
    }

    /// Length in bytes without resolving (a span's raw length — an upper bound on the unescaped len).
    pub fn len(&self) -> usize {
        match self {
            Text::Inline(s) => s.len(),
            Text::Span(sp) => sp.len as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Text {
    fn default() -> Self {
        Text::Inline(String::new())
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Text::Inline(s)
    }
}
impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Text::Inline(s.to_string())
    }
}

/// Read ergonomics: an inline `Text` derefs to its `str`. **Panics on an unresolved `Span`** — the
/// caller must `resolve`/`materialize` first. Inline content (the common case) is unaffected.
impl Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Text::Inline(s) => s,
            Text::Span(_) => {
                panic!("Text::Span read as &str without resolving — call Session::materialize() or Text::resolve(&resolver) first")
            }
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Text::Inline(s) => f.write_str(s),
            // Don't panic in Display; an unmaterialized span renders empty. Materialize for output.
            Text::Span(_) => Ok(()),
        }
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        self.inline_str() == Some(other)
    }
}
impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        self.inline_str() == Some(*other)
    }
}

/// Serializes as a plain JSON string when inline (so a materialized `Session` round-trips
/// byte-identically), or as the span object when not. Whole-session JSON paths
/// [`materialize`](crate::ir::Session::materialize) first, so spans don't reach serialization there.
impl Serialize for Text {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Inline(t) => s.serialize_str(t),
            Text::Span(sp) => sp.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string (inline text) or a span object")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Text, E> {
                Ok(Text::Inline(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Text, E> {
                Ok(Text::Inline(v))
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Text, A::Error> {
                let sp = Span::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Text::Span(sp))
            }
        }
        d.deserialize_any(V)
    }
}

/// Resolves [`Span`]s against their source file(s). Memory-maps each source on first use (with the
/// `mmap` feature) so resolving slices pages on demand rather than reading the whole file; without
/// `mmap` (e.g. wasm32) it falls back to a positioned ranged read.
pub struct Resolver {
    primary: Option<PathBuf>,
    #[cfg(feature = "mmap")]
    maps: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<memmap2::Mmap>>>,
}

impl Resolver {
    /// A resolver rooted at a session's primary source (its `source_path`).
    pub fn new(primary: Option<PathBuf>) -> Self {
        Resolver {
            primary,
            #[cfg(feature = "mmap")]
            maps: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn path_for<'a>(&'a self, sp: &'a Span) -> Option<&'a Path> {
        sp.source.as_deref().or(self.primary.as_deref())
    }

    /// Resolve a span to its text. Borrows the mapped bytes for unescaped spans; allocates only to
    /// UTF-8-decode or JSON-unescape.
    pub fn resolve<'a>(&'a self, sp: &'a Span) -> Cow<'a, str> {
        match self.raw_bytes(sp) {
            None => Cow::Borrowed(""),
            Some(bytes) => {
                if sp.escaped {
                    Cow::Owned(unescape_json_body(&bytes))
                } else {
                    match bytes {
                        Cow::Borrowed(b) => String::from_utf8_lossy(b),
                        Cow::Owned(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
                    }
                }
            }
        }
    }

    #[cfg(feature = "mmap")]
    fn raw_bytes<'a>(&'a self, sp: &'a Span) -> Option<Cow<'a, [u8]>> {
        let path = self.path_for(sp)?.to_path_buf();
        {
            let mut maps = self.maps.borrow_mut();
            if !maps.contains_key(&path) {
                let m = std::fs::File::open(&path)
                    .ok()
                    .and_then(|f| unsafe { memmap2::Mmap::map(&f).ok() });
                maps.insert(path.clone(), m);
            }
        }
        let maps = self.maps.borrow();
        let mmap = maps.get(&path)?.as_ref()?;
        let start = sp.offset as usize;
        let end = start.checked_add(sp.len as usize)?;
        if end > mmap.len() {
            return None;
        }
        let slice: &[u8] = &mmap[start..end];
        // SAFETY: the backing Mmap is owned by `self.maps` for `self`'s lifetime `'a` and entries are
        // never removed, so the slice stays valid for `'a`.
        let slice: &'a [u8] = unsafe { std::mem::transmute::<&[u8], &'a [u8]>(slice) };
        Some(Cow::Borrowed(slice))
    }

    #[cfg(not(feature = "mmap"))]
    fn raw_bytes<'a>(&'a self, sp: &'a Span) -> Option<Cow<'a, [u8]>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path_for(sp)?;
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(sp.offset)).ok()?;
        let mut buf = vec![0u8; sp.len as usize];
        f.read_exact(&mut buf).ok()?;
        Some(Cow::Owned(buf))
    }
}

/// Unescape a JSON string *body* (the bytes between the quotes) into its text, by re-quoting and
/// letting `serde_json` parse it — correct for all JSON escapes. Falls back to a lossy decode.
fn unescape_json_body(raw: &[u8]) -> String {
    if !raw.contains(&b'\\') {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut quoted = Vec::with_capacity(raw.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(raw);
    quoted.push(b'"');
    serde_json::from_slice::<String>(&quoted)
        .unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrips_as_string() {
        let t = Text::from("hello");
        assert_eq!(&*t, "hello");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"hello\"");
        let back: Text = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(back, Text::Inline("hello".into()));
    }

    #[test]
    fn unescape_handles_escapes() {
        assert_eq!(unescape_json_body(b"a\\nb"), "a\nb");
        assert_eq!(unescape_json_body(b"plain"), "plain");
        assert_eq!(unescape_json_body(b"quote \\\" mark"), "quote \" mark");
    }
}
