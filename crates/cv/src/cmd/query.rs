//! CLI glue for the query calculus (`cv_core::query`): parse `-q` strings, the `cv query` reference
//! command, and a [`SessionFacts`] resolver that answers the external predicates (`text:` via the
//! tantivy index, `tool:`/`touched:`/`has:` from the parsed session's events) so the engine can be
//! evaluated against a real session.

use anyhow::{anyhow, Result};
use cv_core::events::{self, Event};
use cv_core::ir::{Block, Session, SessionRef};
use cv_core::query::{ExtFacts, Facts, FieldId, SessionQuery, Tri};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

/// Parse an optional `-q` string into a query, surfacing parse errors (which already point at
/// `cv query`).
pub(crate) fn build(query: Option<String>) -> Result<Option<SessionQuery>> {
    match query {
        Some(q) => Ok(Some(SessionQuery::parse(&q).map_err(|e| anyhow!(e))?)),
        None => Ok(None),
    }
}

/// `cv query` — print the full language reference (or the machine schema with `--json`).
pub(crate) fn cmd_query(json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&cv_core::query::schema_json())?);
    } else {
        print!("{}", cv_core::query::reference());
    }
    Ok(())
}

/// Pre-resolved full-text results: each distinct `text:` needle → the set of session ids that match
/// it in the tantivy index. Computed once before the scan so per-session evaluation is a set lookup.
pub(crate) struct TextSets(HashMap<String, HashSet<String>>);

impl TextSets {
    /// An empty set (no `text:` needles) — for callers with no query.
    pub(crate) fn empty() -> TextSets {
        TextSets(HashMap::new())
    }

    /// Run one full-text search per distinct `text:` needle in the query. If there's no index, warn
    /// once and treat every `text:` needle as matching nothing.
    pub(crate) fn resolve(query: &SessionQuery) -> TextSets {
        let mut map = HashMap::new();
        let needles = query.needles(FieldId::Text);
        if needles.is_empty() {
            return TextSets(map);
        }
        if !cv_search::default_tantivy_dir().exists() {
            eprintln!("cv query: text: needs a full-text index — run `cv index` (treating text: as no match)");
            for n in needles {
                map.insert(n, HashSet::new());
            }
            return TextSets(map);
        }
        for n in needles {
            // A generous cap: we want the full matching set, not a top-K ranking.
            let ids = cv_search::text_search(None, &n, 100_000)
                .map(|hits| hits.into_iter().map(|h| h.id).collect())
                .unwrap_or_default();
            map.insert(n, ids);
        }
        TextSets(map)
    }
}

/// The external-predicate resolver for one parsed session: `text:` from the precomputed [`TextSets`],
/// and `tool:`/`touched:`/`has:` from the session's events (extracted in-memory, lazily, once).
pub(crate) struct SessionFacts<'a> {
    session: &'a Session,
    text_sets: &'a TextSets,
    events: OnceCell<Vec<Event>>,
}

impl<'a> SessionFacts<'a> {
    pub(crate) fn new(session: &'a Session, text_sets: &'a TextSets) -> Self {
        SessionFacts { session, text_sets, events: OnceCell::new() }
    }

    fn events(&self) -> &[Event] {
        self.events.get_or_init(|| {
            let cwd = self.session.cwd.as_deref();
            self.session
                .messages
                .iter()
                .enumerate()
                .flat_map(|(i, m)| events::extract(m, i, cwd))
                .collect()
        })
    }
}

impl ExtFacts for SessionFacts<'_> {
    fn text(&self, needle: &str) -> Tri {
        // The id key is the session id; `needle` is already lowercased by the parser, matching the
        // map keys we inserted under.
        Tri::of(self.text_sets.0.get(needle).is_some_and(|s| s.contains(&self.session.id)))
    }

    fn tool(&self, name: &str) -> Tri {
        Tri::of(
            self.events()
                .iter()
                .any(|e| e.tool.as_deref().is_some_and(|t| t.to_lowercase().contains(name))),
        )
    }

    fn touched(&self, path: &str) -> Tri {
        Tri::of(self.events().iter().any(|e| {
            matches!(e.kind, "file_edit" | "file_read")
                && e.target.as_deref().is_some_and(|t| t.to_lowercase().contains(path))
        }))
    }

    fn has(&self, flag: &str) -> Tri {
        Tri::of(match flag {
            "errors" => self.events().iter().any(|e| e.kind == "error"),
            "tools" => self.events().iter().any(|e| e.tool.is_some()),
            "images" => self
                .session
                .messages
                .iter()
                .any(|m| m.content.iter().any(|b| matches!(b, Block::Image { .. }))),
            "subagents" => {
                self.session.harness == cv_core::ir::Harness::Claude
                    && self
                        .session
                        .source_path
                        .as_deref()
                        .is_some_and(|p| !cv_core::harness::claude::subagent_refs(p).is_empty())
            }
            _ => false,
        })
    }
}

/// Full evaluation of `query` against a parsed session + its ref, using a [`SessionFacts`] resolver.
pub(crate) fn matches_full(
    query: &SessionQuery,
    r: &SessionRef,
    session: &Session,
    text_sets: &TextSets,
) -> bool {
    let facts = SessionFacts::new(session, text_sets);
    query.matches(&Facts { r, session: Some(session), ext: &facts })
}
