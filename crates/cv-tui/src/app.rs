//! Application state + input handling for cv-tui.
//!
//! `App` is a plain state struct; [`App::on_key`] is the single dispatch point that mutates it in
//! response to a key. Rendering ([`crate::ui::draw`]) only *reads* `App` (plus a tiny bit of cached
//! layout it writes back, like the visible transcript height for paging). Keeping all the logic
//! here makes the event loop trivial and the UI a pure projection of state.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use cv_core::board::{self, BoardMessage};
use cv_core::harness::{self, Adapter};
use cv_core::ir::{Harness, Session, SessionRef};

/// Which top-level view is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The session list (with optional transcript preview when one is selected).
    List,
    /// A focused, full-width transcript reader.
    Transcript,
    /// The coordination board (fleet channel + active claims).
    Board,
}

/// Transient input mode layered on top of the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal navigation.
    Normal,
    /// Typing into the `/` substring/fuzzy filter line (filters the visible list).
    Filter,
    /// Typing a full-text/semantic search query (`s`).
    Search,
    /// The `?` help overlay is up.
    Help,
}

/// A row in the list: either a discovered session ref or a search hit (which we resolve to a ref).
#[derive(Debug, Clone)]
pub struct Row {
    pub r#ref: SessionRef,
    /// For search results: a one-line snippet/score to show under the row.
    pub snippet: Option<String>,
    pub score: Option<f32>,
}

/// A line of the rendered transcript, pre-classified so the UI can color it without re-parsing.
#[derive(Debug, Clone)]
pub struct TLine {
    pub text: String,
    pub kind: LineKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    RoleUser,
    RoleAssistant,
    RoleSystem,
    RoleTool,
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    ToolError,
    Meta,
}

pub struct App {
    pub should_quit: bool,
    pub view: View,
    pub mode: Mode,

    /// Every discovered session, newest-first (the unfiltered master list).
    pub all_rows: Vec<Row>,
    /// Indices into `all_rows` that pass the current filter, in display order.
    pub filtered: Vec<usize>,
    /// Cursor within `filtered`.
    pub selected: usize,
    /// How many list rows are visible (set by the renderer) — used for PgUp/PgDn + scroll.
    pub list_height: usize,
    /// First visible filtered-index (scroll offset for the list).
    pub list_offset: usize,

    /// The `/` filter text.
    pub filter: String,
    /// The `s` search query buffer (while typing) and whether the current list is search results.
    pub search_input: String,
    pub showing_search: bool,

    /// The currently-opened transcript, if any (parsed lazily on Enter).
    pub open: Option<OpenSession>,
    /// Transcript scroll offset (top visible line index).
    pub transcript_scroll: usize,
    /// Visible transcript height (set by renderer) for paging math.
    pub transcript_height: usize,

    /// Board state.
    pub board_msgs: Vec<BoardMessage>,
    pub board_claims: Vec<(String, String, chrono::DateTime<chrono::Utc>)>,
    pub board_channel: String,

    /// A transient status/error line shown in the footer.
    pub status: Option<String>,
}

/// A parsed, opened session plus its pre-rendered transcript lines.
pub struct OpenSession {
    pub session: Session,
    pub lines: Vec<TLine>,
}

impl App {
    pub fn new() -> App {
        App {
            should_quit: false,
            view: View::List,
            mode: Mode::Normal,
            all_rows: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            list_height: 20,
            list_offset: 0,
            filter: String::new(),
            search_input: String::new(),
            showing_search: false,
            open: None,
            transcript_scroll: 0,
            transcript_height: 20,
            board_msgs: Vec::new(),
            board_claims: Vec::new(),
            board_channel: "fleet".to_string(),
            status: None,
        }
    }

    // ───────────────────────────── data loading ─────────────────────────────

    /// (Re)discover all sessions, newest-first, and reset the list to the unfiltered set.
    pub fn refresh_sessions(&mut self) {
        let mut refs = cv_core::discover_all();
        // Newest-first by updated_at (fall back to created_at).
        refs.sort_by(|a, b| {
            let ka = a.updated_at.or(a.created_at);
            let kb = b.updated_at.or(b.created_at);
            kb.cmp(&ka)
        });
        self.all_rows = refs
            .into_iter()
            .map(|r| Row {
                r#ref: r,
                snippet: None,
                score: None,
            })
            .collect();
        self.showing_search = false;
        self.search_input.clear();
        self.recompute_filter();
        self.status = Some(format!("{} sessions", self.all_rows.len()));
    }

    /// Recompute `filtered` from `all_rows` + `filter`, clamping the selection.
    pub fn recompute_filter(&mut self) {
        let q = self.filter.to_ascii_lowercase();
        self.filtered = self
            .all_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| q.is_empty() || row_matches(row, &q))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.list_offset = 0;
    }

    /// The currently-selected row, if any.
    pub fn current_row(&self) -> Option<&Row> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.all_rows.get(i))
    }

    // ───────────────────────────── opening / parsing ─────────────────────────────

    /// Parse the selected session into the IR + pre-rendered lines, and switch to the reader.
    pub fn open_selected(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        self.status = Some(format!("loading {}…", short_id(&row.r#ref.id)));
        match load_session(&row.r#ref) {
            Ok(session) => {
                let lines = render_lines(&session);
                self.open = Some(OpenSession { session, lines });
                self.transcript_scroll = 0;
                self.view = View::Transcript;
                self.status = None;
            }
            Err(e) => {
                self.status = Some(format!("failed to open: {e:#}"));
            }
        }
    }

    // ───────────────────────────── search ─────────────────────────────

    /// Run text search (with a semantic fallback merged in) and replace the list with hits.
    fn run_search(&mut self) {
        let q = self.search_input.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.status = Some(format!("searching “{q}”…"));

        let mut rows: Vec<Row> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Full-text first (always available).
        match cv_search::text_search(None, &q, 50) {
            Ok(hits) => {
                for h in hits {
                    if let Some(row) = hit_to_row(&h, self) {
                        if seen.insert(row.r#ref.id.clone()) {
                            rows.push(row);
                        }
                    }
                }
            }
            Err(e) => {
                self.status = Some(format!("text search failed: {e:#}"));
            }
        }

        // Try semantic and merge any *new* hits after the FTS ones (best-effort).
        if let Some(extra) = try_semantic_search(&q) {
            for h in extra {
                if let Some(row) = hit_to_row(&h, self) {
                    if seen.insert(row.r#ref.id.clone()) {
                        rows.push(row);
                    }
                }
            }
        }

        let n = rows.len();
        if rows.is_empty() {
            // Keep the old list but tell the user nothing matched.
            self.status = Some(format!("no results for “{q}”"));
            return;
        }
        self.all_rows = rows;
        self.showing_search = true;
        self.filter.clear();
        self.selected = 0;
        self.recompute_filter();
        self.view = View::List;
        self.status = Some(format!("{n} results for “{q}” (Enter opens, r resets)"));
    }

    // ───────────────────────────── board ─────────────────────────────

    fn refresh_board(&mut self) {
        match board::read(&self.board_channel, None, 50) {
            Ok(m) => self.board_msgs = m,
            Err(e) => self.status = Some(format!("board read failed: {e:#}")),
        }
        self.board_claims = board::active_claims(&self.board_channel).unwrap_or_default();
    }

    // ───────────────────────────── navigation helpers ─────────────────────────────

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
        // Keep the selection within the visible window.
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + self.list_height {
            self.list_offset = self.selected + 1 - self.list_height;
        }
    }

    fn scroll_transcript(&mut self, delta: isize) {
        let max = self
            .open
            .as_ref()
            .map(|o| o.lines.len().saturating_sub(1))
            .unwrap_or(0);
        let next = (self.transcript_scroll as isize + delta).clamp(0, max as isize);
        self.transcript_scroll = next as usize;
    }

    // ───────────────────────────── input dispatch ─────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        // Ignore key-release events (Windows/Kitty send them); we only act on press/repeat.
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Help overlay swallows everything until dismissed.
        if self.mode == Mode::Help {
            self.mode = Mode::Normal;
            return;
        }

        match self.mode {
            Mode::Filter => self.on_key_filter(key),
            Mode::Search => self.on_key_search(key),
            Mode::Normal => self.on_key_normal(key),
            Mode::Help => {}
        }
    }

    fn on_key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.recompute_filter();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if self.current_row().is_some() {
                    self.open_selected();
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.recompute_filter();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.recompute_filter();
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_input.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.run_search();
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char(c) => self.search_input.push(c),
            _ => {}
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        // Global keys that work in any view.
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return;
            }
            KeyCode::Tab => {
                self.cycle_view();
                return;
            }
            _ => {}
        }

        match self.view {
            View::List => self.on_key_list(key),
            View::Transcript => self.on_key_transcript(key),
            View::Board => self.on_key_board(key),
        }
    }

    fn on_key_list(&mut self, key: KeyEvent) {
        let page = self.list_height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('r') => self.refresh_sessions(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                self.search_input.clear();
                self.mode = Mode::Search;
            }
            KeyCode::Char('b') => {
                self.refresh_board();
                self.view = View::Board;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Enter => self.open_selected(),
            _ => {}
        }
    }

    fn on_key_transcript(&mut self, key: KeyEvent) {
        let page = self.transcript_height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                self.view = View::List;
            }
            KeyCode::Char('r') => {
                // Re-parse the open session from disk.
                if let Some(open) = &self.open {
                    let r = SessionRef {
                        id: open.session.id.clone(),
                        harness: open.session.harness,
                        path: open
                            .session
                            .source_path
                            .clone()
                            .unwrap_or_default(),
                        cwd: open.session.cwd.clone(),
                        title: open.session.title.clone(),
                        created_at: open.session.created_at,
                        updated_at: open.session.updated_at,
                        message_count: open.session.messages.len(),
                    };
                    if let Ok(session) = load_session(&r) {
                        let lines = render_lines(&session);
                        self.open = Some(OpenSession { session, lines });
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_transcript(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_transcript(1),
            KeyCode::PageUp => self.scroll_transcript(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_transcript(page),
            KeyCode::Home | KeyCode::Char('g') => self.transcript_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_transcript(isize::MAX / 2),
            _ => {}
        }
    }

    fn on_key_board(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => self.view = View::List,
            KeyCode::Char('r') => self.refresh_board(),
            KeyCode::Char('b') => self.view = View::List,
            _ => {}
        }
    }

    fn cycle_view(&mut self) {
        self.view = match self.view {
            View::List => {
                if self.open.is_some() {
                    View::Transcript
                } else {
                    self.refresh_board();
                    View::Board
                }
            }
            View::Transcript => {
                self.refresh_board();
                View::Board
            }
            View::Board => View::List,
        };
    }
}

// ───────────────────────────── free helpers ─────────────────────────────

/// Load + parse a session ref into the IR via its harness adapter.
fn load_session(r: &SessionRef) -> anyhow::Result<Session> {
    let adapter: Box<dyn Adapter> = harness::for_harness(r.harness)
        .ok_or_else(|| anyhow::anyhow!("no adapter for harness {}", r.harness))?;
    adapter.parse(r)
}

/// Try semantic search if the feature/index is available; never propagate errors (best-effort).
fn try_semantic_search(q: &str) -> Option<Vec<cv_search::Hit>> {
    // `semantic_search` only exists when cv-search is built with its (default-on) `semantic`
    // feature. We call it directly; if the embedding store is missing it simply errors and we
    // fall back to the FTS-only results.
    match std::panic::catch_unwind(|| cv_search::semantic_search(None, q, 50)) {
        Ok(Ok(hits)) => Some(hits),
        _ => None,
    }
}

/// Resolve a search [`Hit`] to a [`Row`] by finding its ref among the master/all lists.
/// We discover refs once (here) so a hit always carries enough to be opened.
fn hit_to_row(h: &cv_search::Hit, app: &App) -> Option<Row> {
    // Prefer a ref we already discovered (cheap); else discover by id.
    if let Some(row) = app.all_rows.iter().find(|r| r.r#ref.id == h.id) {
        return Some(Row {
            r#ref: row.r#ref.clone(),
            snippet: Some(h.snippet.clone()),
            score: Some(h.score),
        });
    }
    let harness = Harness::parse(&h.harness)?;
    let (r, _) = cv_core::find(&h.id, Some(harness)).ok().flatten()?;
    Some(Row {
        r#ref: r,
        snippet: Some(h.snippet.clone()),
        score: Some(h.score),
    })
}

/// Does a list row match a lowercased query? Substring across harness/id/title/cwd, plus a
/// lenient subsequence ("fuzzy") fallback over the same haystack.
fn row_matches(row: &Row, q_lower: &str) -> bool {
    let r = &row.r#ref;
    let hay = format!(
        "{} {} {} {}",
        r.harness.as_str(),
        r.id,
        r.title.as_deref().unwrap_or(""),
        r.cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
    )
    .to_ascii_lowercase();
    hay.contains(q_lower) || is_subsequence(q_lower, &hay)
}

/// True if every char of `needle` appears in `hay` in order (a cheap fuzzy match).
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut hc = hay.chars();
    'outer: for nc in needle.chars() {
        if nc == ' ' {
            continue;
        }
        for h in hc.by_ref() {
            if h == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// First 8 chars of an id, for compact display.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Replace a leading `$HOME` with `~` for compact cwd display.
pub fn tildify(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

/// Pre-render a session into classified lines (the shape of `cv_core::render::to_plain`, but kept
/// structured so the UI can color + wrap them). Thinking and tool blocks are shortened.
fn render_lines(s: &Session) -> Vec<TLine> {
    use cv_core::ir::Block;
    let mut out: Vec<TLine> = Vec::new();

    // A small header so the reader has context.
    out.push(TLine {
        text: format!("{}  ·  {}", s.harness.as_str(), s.label()),
        kind: LineKind::Meta,
    });
    if let Some(cwd) = &s.cwd {
        out.push(TLine {
            text: format!("cwd: {}", tildify(cwd)),
            kind: LineKind::Meta,
        });
    }
    if let Some(m) = &s.model {
        out.push(TLine {
            text: format!("model: {m}"),
            kind: LineKind::Meta,
        });
    }
    out.push(TLine {
        text: String::new(),
        kind: LineKind::Meta,
    });

    for m in &s.messages {
        let (label, kind) = match m.role {
            cv_core::ir::Role::User => ("user", LineKind::RoleUser),
            cv_core::ir::Role::Assistant => ("assistant", LineKind::RoleAssistant),
            cv_core::ir::Role::System => ("system", LineKind::RoleSystem),
            cv_core::ir::Role::Tool => ("tool", LineKind::RoleTool),
        };
        out.push(TLine {
            text: format!("── {label} ──"),
            kind,
        });

        for b in &m.content {
            match b {
                Block::Text { text } => {
                    push_multiline(&mut out, text, LineKind::Text);
                }
                Block::Thinking { text, .. } => {
                    // Collapse thinking to a short preview (a few lines max).
                    let preview = preview_text(text, 6);
                    out.push(TLine {
                        text: format!("🧠 {}", preview.first().cloned().unwrap_or_default()),
                        kind: LineKind::Thinking,
                    });
                    for extra in preview.iter().skip(1) {
                        out.push(TLine {
                            text: format!("   {extra}"),
                            kind: LineKind::Thinking,
                        });
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    out.push(TLine {
                        text: format!("🔧 {name}  {}", short_json(input, 200)),
                        kind: LineKind::ToolUse,
                    });
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let kind = if *is_error {
                        LineKind::ToolError
                    } else {
                        LineKind::ToolResult
                    };
                    let head = if *is_error { "↩ error" } else { "↩ result" };
                    out.push(TLine {
                        text: head.to_string(),
                        kind,
                    });
                    for l in preview_text(content, 12) {
                        out.push(TLine {
                            text: format!("  {l}"),
                            kind,
                        });
                    }
                }
                Block::File { path, source, .. } => {
                    let label = path
                        .as_deref()
                        .or(source.as_deref())
                        .unwrap_or("?");
                    out.push(TLine {
                        text: format!("[file: {label}]"),
                        kind: LineKind::Meta,
                    });
                }
                Block::Image { .. } => out.push(TLine {
                    text: "[image]".to_string(),
                    kind: LineKind::Meta,
                }),
            }
        }
        out.push(TLine {
            text: String::new(),
            kind: LineKind::Text,
        });
    }
    out
}

/// Split `text` on newlines into classified lines.
fn push_multiline(out: &mut Vec<TLine>, text: &str, kind: LineKind) {
    for line in text.split('\n') {
        out.push(TLine {
            text: line.to_string(),
            kind,
        });
    }
}

/// First `max` non-trivial lines of `text`, with a trailing "…" marker if truncated.
fn preview_text(text: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let truncated = lines.len() > max;
    lines.truncate(max);
    if truncated {
        lines.push("… (truncated)".to_string());
    }
    lines
}

/// Compact one-line JSON, truncated to `max` chars.
fn short_json(v: &impl ToString, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}
