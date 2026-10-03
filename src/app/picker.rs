//! The interactive list pickers: a filterable single-select and a checkbox
//! multi-select, plus the key source that drives them and the row vocabulary
//! they are built from.
//!
//! Each picker is a model folded over messages. A keystroke is translated into
//! the intention it names before any state sees it, so `update` never handles a
//! keycode and can be driven straight from a test with no terminal. The two
//! keep separate models and message types: they share a shape but not a state
//! space, and a union would be half-dead in either.

use std::collections::HashMap;

use console::{Key, Term, measure_text_width, style};

use super::{CursorGuard, interactive_term};
use crate::AppResult;
use crate::grammar::Verb;

/// Source of key events for the interactive pickers. Abstracting input behind a
/// trait lets the event loops be driven by a scripted sequence in tests; the
/// real implementation is [`TermKeys`].
pub(crate) trait KeySource {
    fn read_key(&mut self) -> std::io::Result<Key>;
}

/// The real key source backing the interactive pickers. It holds the terminal in
/// raw mode for the picker's lifetime and lets `crossterm` parse key events.
pub(crate) struct TermKeys {
    term: Term,
    raw: Option<raw::RawMode>,
}

impl KeySource for TermKeys {
    fn read_key(&mut self) -> std::io::Result<Key> {
        if let Some(raw) = &self.raw {
            return raw.read_key();
        }
        self.term.read_key()
    }
}

/// A key source for an interactive prompt, or `None` in piped/CI runs. Mirrors
/// [`interactive_term`] but acquires raw mode so arrow keys are read reliably.
pub(crate) fn interactive_keys() -> Option<TermKeys> {
    let term = interactive_term()?;
    Some(TermKeys {
        term,
        // Acquiring raw mode can fail; fall back to `console`.
        raw: raw::RawMode::acquire().ok(),
    })
}

/// Raw-mode key reader. `console::read_key` re-arms raw mode on every keystroke
/// and has been fragile around split escape sequences; `crossterm` keeps raw
/// mode active and uses its battle-tested event parser instead.
mod raw {
    use std::io;

    use console::Key;
    use crossterm::{
        event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read},
        terminal::{disable_raw_mode, enable_raw_mode},
    };

    /// Zero-sized guard: enabling raw mode is process-global, and [`Drop`]
    /// disables it again.
    pub(crate) struct RawMode;

    impl RawMode {
        pub(crate) fn acquire() -> io::Result<Self> {
            enable_raw_mode()?;
            Ok(Self)
        }

        // `&self` is a capability token: holding the guard proves raw mode is
        // active, even though reading uses crossterm's global event source.
        #[expect(clippy::unused_self)]
        pub(crate) fn read_key(&self) -> io::Result<Key> {
            loop {
                let Event::Key(event) = read()? else {
                    continue;
                };
                if event.kind == KeyEventKind::Release {
                    continue;
                }
                return translate_key(event);
            }
        }
    }

    fn translate_key(event: KeyEvent) -> io::Result<Key> {
        if event.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(event.code, KeyCode::Char('c' | 'C'))
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }

        Ok(match event.code {
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Enter => Key::Enter,
            KeyCode::Left => Key::ArrowLeft,
            KeyCode::Right => Key::ArrowRight,
            KeyCode::Up => Key::ArrowUp,
            KeyCode::Down => Key::ArrowDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Delete => Key::Del,
            KeyCode::Insert => Key::Insert,
            KeyCode::Esc => Key::Escape,
            KeyCode::Char('a' | 'A') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                Key::Home
            }
            KeyCode::Char('e' | 'E') if event.modifiers.contains(KeyModifiers::CONTROL) => Key::End,
            KeyCode::Char(c)
                if !event
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Key::Char(c)
            }
            _ => Key::Unknown,
        })
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
}

/// Everything the git side found: which branches exist where, and which
/// worktree *Held*s each. Only facts — grouping them under headings and
/// deciding what each row says is [`sections`], which is pure and needs no repo
/// on disk.
pub(crate) struct Catalogue {
    /// The branch the worktree we're standing in has checked out.
    pub current: Option<String>,
    /// Display path of the worktree holding a branch, for those a worktree does.
    pub held: HashMap<String, String>,
    /// Checked out locally, in the order git listed them.
    pub local: Vec<String>,
    /// On the remote with no local counterpart.
    pub remote_only: Vec<String>,
}

/// A branch as the picker draws it: the name, whatever there is to say after
/// it, and whether the verb in play can do anything with it.
#[derive(Clone)]
pub(crate) struct Pick {
    pub name: String,
    pub is_current: bool,
    /// Dim text after the name — where the branch is held, or why the row is
    /// inert.
    pub annotation: String,
    /// Greyed and unselectable: this verb has nothing to do with the row. The
    /// row stays on screen regardless, since hiding a branch another worktree
    /// holds only turns "where did that branch go?" into a support question.
    pub disabled: bool,
}

pub(crate) struct Section {
    pub heading: &'static str,
    pub items: Vec<Pick>,
}

impl Catalogue {
    fn is_current(&self, name: &str) -> bool {
        self.current.as_deref() == Some(name)
    }

    /// The worktree holding this branch, where that is somewhere other than
    /// here. Git forbids the same branch in two worktrees and the one we're
    /// standing in holds the current branch, so "elsewhere" is exactly "held,
    /// and not current" — which is also why the row we're standing on is marked
    /// `*` and not with a path.
    fn elsewhere(&self, name: &str) -> Option<&str> {
        if self.is_current(name) {
            return None;
        }
        self.held.get(name).map(String::as_str)
    }
}

fn row(catalogue: &Catalogue, name: &str, verb: Verb) -> Pick {
    let (annotation, disabled) = match catalogue.elsewhere(name) {
        // `br` promises a checkout *here*, and git will not check out a branch a
        // second worktree already holds. So this is the one row a verb differs
        // over — everywhere else git leaves exactly one move legal.
        Some(path) if verb == Verb::Here => (format!("in {path} — use `perch`"), true),
        Some(path) => (path.to_string(), false),
        None => (String::new(), false),
    };
    Pick {
        name: name.to_string(),
        is_current: catalogue.is_current(name),
        annotation,
        disabled,
    }
}

/// The whole list, for any verb: every branch once, under the heading saying
/// where it lives. Headings with nothing under them are dropped, so a repo whose
/// branches are all checked out shows no *Remote*.
pub(crate) fn sections(catalogue: &Catalogue, verb: Verb) -> Vec<Section> {
    [
        ("Local", &catalogue.local),
        ("Remote", &catalogue.remote_only),
    ]
    .into_iter()
    .filter(|(_, names)| !names.is_empty())
    .map(|(heading, names)| Section {
        heading,
        items: names
            .iter()
            .map(|name| row(catalogue, name, verb))
            .collect(),
    })
    .collect()
}

enum RowKind {
    Heading(&'static str),
    Item(Pick),
    CreateNew(String),
}

struct RenderRow {
    kind: RowKind,
    section_idx: usize,
}

struct View {
    rows: Vec<RenderRow>,
    selectable: Vec<usize>,
    /// Width to pad names to so annotations share a column. Derived per view
    /// rather than per section, since filtering changes which rows are on
    /// screen and a column that only lined up within a section wouldn't be one.
    name_column: usize,
}

#[derive(Debug)]
pub(crate) enum Selection {
    Existing(String),
    Create(String),
}

#[derive(Clone, Copy)]
pub(crate) struct PickerOptions {
    pub prompt: &'static str,
    pub allow_create_from_filter: bool,
}

/// Subsequence match against a pre-lowered needle. Lowering happens at the
/// call site so the needle is normalized once per filter, not once per item.
fn fuzzy_match(needle_lower: &str, haystack: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let mut hi = haystack.chars().flat_map(char::to_lowercase);
    'next: for nc in needle_lower.chars() {
        for hc in hi.by_ref() {
            if hc == nc {
                continue 'next;
            }
        }
        return false;
    }
    true
}

fn build_view(sections: &[Section], filter: &str, opts: PickerOptions) -> View {
    let needle: String = filter.chars().flat_map(char::to_lowercase).collect();
    let mut rows: Vec<RenderRow> = Vec::new();
    let mut selectable: Vec<usize> = Vec::new();

    for (sec_idx, sec) in sections.iter().enumerate() {
        let matching: Vec<&Pick> = sec
            .items
            .iter()
            .filter(|p| fuzzy_match(&needle, &p.name))
            .collect();
        if matching.is_empty() {
            continue;
        }
        rows.push(RenderRow {
            kind: RowKind::Heading(sec.heading),
            section_idx: sec_idx,
        });
        for pick in matching {
            let idx = rows.len();
            let is_selectable = !pick.disabled;
            rows.push(RenderRow {
                kind: RowKind::Item(pick.clone()),
                section_idx: sec_idx,
            });
            if is_selectable {
                selectable.push(idx);
            }
        }
    }

    if opts.allow_create_from_filter && selectable.is_empty() && !filter.is_empty() {
        let idx = rows.len();
        rows.push(RenderRow {
            kind: RowKind::CreateNew(filter.to_string()),
            section_idx: 0,
        });
        selectable.push(idx);
    }

    let name_column = annotation_column(rows.iter().filter_map(|r| match &r.kind {
        RowKind::Item(p) => Some((p.name.as_str(), p.annotation.as_str())),
        _ => None,
    }));

    View {
        rows,
        selectable,
        name_column,
    }
}

fn cursor_selection(view: &View, cursor: usize) -> Option<Selection> {
    let &row_idx = view.selectable.get(cursor)?;
    match &view.rows[row_idx].kind {
        RowKind::Item(p) => Some(Selection::Existing(p.name.clone())),
        RowKind::CreateNew(name) => Some(Selection::Create(name.clone())),
        RowKind::Heading(_) => None,
    }
}

fn selectable_position(view: &View, name: &str) -> Option<usize> {
    view.selectable
        .iter()
        .position(|&i| matches!(&view.rows[i].kind, RowKind::Item(p) if p.name == name))
}

/// What a keystroke means to the single-select picker. The event loop folds
/// these into a [`PickModel`]; [`pick_msg`] is the only place that knows which
/// key carries which intention, so a rebinding is one line there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickMsg {
    Accepted,
    /// Escape is one gesture with two meanings — clear the filter, or give up —
    /// and which it is depends on model state the translation can't see. So the
    /// gesture is carried through and [`PickModel::update`] decides.
    Escaped,
    FilterPopped,
    FilterPushed(char),
    MovedToEnd,
    MovedToStart,
    /// Carries the page size, since how far a page reaches is a fact about the
    /// terminal at the moment the key was pressed, not about the model.
    PagedDown(usize),
    PagedUp(usize),
    SteppedDown,
    SteppedUp,
}

/// The keyboard contract, entire. `None` is an unbound key, which does nothing
/// — including for control characters, which are never filter input.
fn pick_msg(key: &Key, page: usize) -> Option<PickMsg> {
    Some(match key {
        Key::ArrowDown => PickMsg::SteppedDown,
        Key::ArrowUp => PickMsg::SteppedUp,
        Key::Backspace => PickMsg::FilterPopped,
        Key::Char(c) if !c.is_control() => PickMsg::FilterPushed(*c),
        Key::End => PickMsg::MovedToEnd,
        Key::Enter => PickMsg::Accepted,
        Key::Escape => PickMsg::Escaped,
        Key::Home => PickMsg::MovedToStart,
        Key::PageDown => PickMsg::PagedDown(page),
        Key::PageUp => PickMsg::PagedUp(page),
        _ => return None,
    })
}

/// Whether a picker carries on, and what it settled on if not. Both pickers
/// fold their own message type, but they end the same way, so this is the one
/// piece of them worth sharing.
enum Flow<T> {
    Continue,
    Stop(T),
}

/// Everything the single-select picker knows: what has been typed, where the
/// cursor sits, and the view the two derive. Holding the sections and options
/// alongside keeps [`Self::update`] able to re-derive the view on its own, so
/// the whole picker can be driven — and tested — without a terminal.
struct PickModel<'a> {
    cursor: usize,
    filter: String,
    opts: PickerOptions,
    sections: &'a [Section],
    view: View,
}

impl<'a> PickModel<'a> {
    /// Starts on `current` where the unfiltered view still offers it, so the
    /// picker opens on the branch you're already on.
    fn new(current: Option<&str>, sections: &'a [Section], opts: PickerOptions) -> Self {
        let view = build_view(sections, "", opts);
        let cursor = current
            .and_then(|c| selectable_position(&view, c))
            .unwrap_or(0);
        Self {
            cursor,
            filter: String::new(),
            opts,
            sections,
            view,
        }
    }

    /// Re-derives the view and keeps the cursor on the branch it was on, rather
    /// than the position it held — narrowing the list must not move the
    /// selection under the user. Where that branch is filtered away there is
    /// nothing to follow, so the cursor returns to the top.
    fn refilter(&mut self) {
        let preserved = match cursor_selection(&self.view, self.cursor) {
            Some(Selection::Existing(name)) => Some(name),
            _ => None,
        };
        self.view = build_view(self.sections, &self.filter, self.opts);
        self.cursor = preserved
            .as_deref()
            .and_then(|n| selectable_position(&self.view, n))
            .unwrap_or(0);
    }

    fn update(&mut self, msg: PickMsg) -> Flow<Option<Selection>> {
        let len = self.view.selectable.len();
        match msg {
            PickMsg::Accepted => {
                return Flow::Stop(cursor_selection(&self.view, self.cursor));
            }
            // Nothing left to clear means the user is asking to leave.
            PickMsg::Escaped => {
                if self.filter.is_empty() {
                    return Flow::Stop(None);
                }
                self.filter.clear();
                self.refilter();
            }
            PickMsg::FilterPopped => {
                if self.filter.pop().is_some() {
                    self.refilter();
                }
            }
            PickMsg::FilterPushed(c) => {
                self.filter.push(c);
                self.refilter();
            }
            PickMsg::MovedToEnd => self.cursor = len.saturating_sub(1),
            PickMsg::MovedToStart => self.cursor = 0,
            // Paging clamps where stepping wraps: a page is as far as you can
            // see, and running out of list is no reason to reappear at the
            // other end.
            PickMsg::PagedDown(page) => {
                self.cursor = (self.cursor + page).min(len.saturating_sub(1));
            }
            PickMsg::PagedUp(page) => self.cursor = self.cursor.saturating_sub(page),
            PickMsg::SteppedDown => {
                if len > 0 {
                    self.cursor = (self.cursor + 1) % len;
                }
            }
            PickMsg::SteppedUp => {
                if len > 0 {
                    self.cursor = if self.cursor == 0 {
                        len - 1
                    } else {
                        self.cursor - 1
                    };
                }
            }
        }
        Flow::Continue
    }
}

/// The single-select picker. `keys` is taken by value so the raw mode it holds
/// is released when this returns: under raw mode a newline moves down without
/// returning to column 0, so anything a caller printed while still holding the
/// key source would staircase across the terminal.
///
/// This is the shell around [`PickModel`]: read a key, translate it, fold it in,
/// redraw. Everything that decides anything lives in the model.
pub(crate) fn pick(
    current: Option<&str>,
    sections: &[Section],
    opts: PickerOptions,
    mut keys: impl KeySource,
) -> AppResult<Option<Selection>> {
    let term = Term::stderr();
    let _cursor_guard = CursorGuard::hide();

    let mut model = PickModel::new(current, sections, opts);
    if model.view.selectable.is_empty() && !opts.allow_create_from_filter {
        return Ok(None);
    }

    let mut drawn = render(&term, &model.view, model.cursor, &model.filter, opts.prompt);

    loop {
        let key = keys.read_key()?;
        let Some(msg) = pick_msg(&key, page_size(&term)) else {
            continue;
        };
        if let Flow::Stop(selection) = model.update(msg) {
            let _ = term.clear_last_lines(drawn);
            return Ok(selection);
        }

        let _ = term.clear_last_lines(drawn);
        drawn = render(&term, &model.view, model.cursor, &model.filter, opts.prompt);
    }
}

fn page_size(term: &Term) -> usize {
    let h = term.size().0 as usize;
    h.saturating_sub(2).max(1)
}

fn render(term: &Term, view: &View, cursor: usize, filter: &str, prompt_label: &str) -> usize {
    let (rows_term, cols_term) = term.size();
    let height = rows_term as usize;
    let width = cols_term as usize;

    let prompt = format!(
        "{} {} {} {}",
        style("?").green().bold(),
        style(prompt_label).bold(),
        style("(type to filter):").dim(),
        filter,
    );
    render_line(&prompt);
    let mut drawn = visual_rows(&prompt, width);

    if view.selectable.is_empty() {
        let line = style("  (no matches)").dim().to_string();
        render_line(&line);
        drawn += visual_rows(&line, width);
        return drawn;
    }

    // Reserve one trailing line of headroom. If the render filled the full
    // terminal height, the final newline would scroll the screen up by a line
    // each redraw; `clear_last_lines` then can't reach the scrolled-off prompt
    // (cursor-up clamps at the top row), so stale prompt lines pile up and the
    // live prompt scrolls out of view.
    let viewport_h = height.saturating_sub(drawn + 1).max(3);
    let total_rows = view.rows.len();
    let cursor_row = view.selectable.get(cursor).copied().unwrap_or(0);

    let cursor_section = view.rows[cursor_row].section_idx;
    let cursor_heading_row = view
        .rows
        .iter()
        .position(|r| r.section_idx == cursor_section && matches!(r.kind, RowKind::Heading(_)));

    let mut scroll = if total_rows <= viewport_h || cursor_row + 1 < viewport_h {
        0
    } else {
        cursor_row + 1 - viewport_h
    };

    let sticky = cursor_heading_row.is_some_and(|h| h < scroll);
    let content_h = if sticky {
        viewport_h.saturating_sub(1).max(1)
    } else {
        viewport_h
    };

    if sticky && cursor_row >= scroll + content_h {
        scroll = cursor_row + 1 - content_h;
    }

    if sticky
        && let Some(h) = cursor_heading_row
        && let RowKind::Heading(text) = &view.rows[h].kind
    {
        let line = style(clip_end(text, width)).bold().dim().to_string();
        render_line(&line);
        drawn += visual_rows(&line, width);
    }

    let end = (scroll + content_h).min(total_rows);
    for r in scroll..end {
        let line = format_row(&view.rows[r], r == cursor_row, view.name_column, width);
        render_line(&line);
        drawn += visual_rows(&line, width);
    }

    drawn
}

/// Columns every item row spends before the name: two of indent, the cursor,
/// and the `*` column, each followed by a space.
const ROW_INDENT: usize = 6;

/// Columns between a name and its annotation.
const ANNOTATION_GAP: usize = 2;

/// Shortens `text` to `width` columns, keeping the head and marking the cut —
/// a branch name reads from the left.
fn clip_end(text: &str, width: usize) -> String {
    if measure_text_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    // The ellipsis has to fit too, so it is spent before anything else.
    let mut used = 1;
    for c in text.chars() {
        let w = measure_text_width(c.encode_utf8(&mut [0; 4]));
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Shortens `text` to `width` columns, keeping the *tail* — a worktree path is
/// identified by where it ends, and so is the hint on a disabled row.
fn clip_start(text: &str, width: usize) -> String {
    if measure_text_width(text) <= width {
        return text.to_string();
    }
    let mut tail = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let w = measure_text_width(c.encode_utf8(&mut [0; 4]));
        if used + w > width {
            break;
        }
        tail.push(c);
        used += w;
    }
    let mut out = String::from("…");
    out.extend(tail.into_iter().rev());
    out
}

/// Renders a row, clipped to fit `width` in one terminal line.
///
/// Fitting is not cosmetic: [`render`] budgets its viewport in logical rows, so
/// a row that wrapped would spend more of the screen than it was allotted, push
/// the prompt off the top, and strand output where `clear_last_lines` can't
/// reach it. Annotations are worktree paths, which are long, so this is the
/// ordinary case rather than an edge one.
fn format_row(row: &RenderRow, is_cursor: bool, name_column: usize, width: usize) -> String {
    let cursor = if is_cursor { ">" } else { " " };
    match &row.kind {
        RowKind::Heading(text) => style(clip_end(text, width)).bold().dim().to_string(),
        RowKind::Item(pick) => {
            // The `*` reserves its column on every row, so names line up and
            // stepping down the list doesn't shuffle them sideways.
            let mark = if pick.is_current { "*" } else { " " };
            let body = width.saturating_sub(ROW_INDENT);

            // Only the terminal clips a name; `name_column` is a padding target
            // that a longer name simply overruns, pushing its annotation right,
            // exactly as `align_labels` leaves an over-wide name unpadded.
            let name = clip_end(&pick.name, body.max(1));
            let name_w = measure_text_width(&name);
            // The column is a shared target, so it is capped against this
            // terminal before it is spent: one name wide enough to fill the row
            // would otherwise pad every *other* row out to the edge, dropping
            // annotations that had room. Half the body splits the row evenly,
            // and the outsized name overruns its own padding instead — which is
            // already what a name longer than the column does.
            let column = name_column.min(body / 2);
            // Padded here rather than by `align_labels`, which joins the pair
            // into one string: the name and its annotation are styled
            // separately, so they have to stay separate.
            let pad_w = column
                .saturating_sub(name_w)
                .min(body.saturating_sub(name_w));
            let pad = " ".repeat(pad_w);
            let room = body.saturating_sub(name_w + pad_w + ANNOTATION_GAP);

            // Below a few columns there is no annotation worth reading, and a
            // bare name beats a lone ellipsis.
            let line = match pick.annotation.as_str() {
                "" => format!("  {cursor} {mark} {name}"),
                _ if room < 2 => format!("  {cursor} {mark} {name}"),
                annotation if pick.disabled => {
                    // A disabled row is dimmed whole below, so dimming the
                    // annotation here as well would only double the escapes.
                    format!(
                        "  {cursor} {mark} {name}{pad}  {}",
                        clip_start(annotation, room)
                    )
                }
                annotation => format!(
                    "  {cursor} {mark} {name}{pad}  {}",
                    style(clip_start(annotation, room)).dim()
                ),
            };
            if pick.disabled {
                style(line).dim().to_string()
            } else {
                line
            }
        }
        RowKind::CreateNew(name) => {
            let label = clip_end(
                &format!("Create new: {name}"),
                width.saturating_sub(ROW_INDENT),
            );
            format!(
                "  {cursor} {} {}",
                style("+").green().bold(),
                style(label).italic()
            )
        }
    }
}

/// The width names are padded to so annotations share a column. Only rows that
/// actually carry an annotation drive it — a list with none needs no column, and
/// padding for an absent one is just trailing whitespace.
fn annotation_column<'a>(rows: impl Iterator<Item = (&'a str, &'a str)>) -> usize {
    rows.filter(|(_, annotation)| !annotation.is_empty())
        .map(|(name, _)| measure_text_width(name))
        .max()
        .unwrap_or(0)
}

/// Pads (name, annotation) pairs so annotations line up in a column. Rows
/// without an annotation are left bare, so an unannotated list gains no
/// trailing whitespace.
pub(crate) fn align_labels(rows: &[(String, String)]) -> Vec<String> {
    let width = annotation_column(rows.iter().map(|(n, a)| (n.as_str(), a.as_str())));

    rows.iter()
        .map(|(name, annotation)| {
            if annotation.is_empty() {
                return name.clone();
            }
            let pad = " ".repeat(width.saturating_sub(measure_text_width(name)));
            format!("{name}{pad}  {annotation}")
        })
        .collect()
}

fn visual_rows(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let w = measure_text_width(text);
    if w == 0 { 1 } else { w.div_ceil(width) }
}

fn render_line(line: &str) {
    // Emit `\r\n` explicitly: raw mode disables the terminal's `\n`→`\r\n`
    // translation. Routing through `eprint!` (rather than a raw fd write) keeps
    // libtest's output capture working, so passing picker tests stay quiet.
    eprint!("{line}\r\n");
}

/// What a keystroke means to the multi-select picker. Deliberately not shared
/// with [`PickMsg`]: the two pickers have the same shape but not the same state
/// space, and a union of the two would be half-dead in either.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MultiMsg {
    Accepted,
    AllSelected,
    /// Cancels the picker, distinct from accepting an empty tick list.
    Escaped,
    NoneSelected,
    SteppedDown,
    SteppedUp,
    Toggled,
}

/// The keyboard contract for the multi-select. `None` is an unbound key.
fn multi_msg(key: &Key) -> Option<MultiMsg> {
    Some(match key {
        Key::ArrowDown => MultiMsg::SteppedDown,
        Key::ArrowLeft => MultiMsg::NoneSelected,
        Key::ArrowRight => MultiMsg::AllSelected,
        Key::ArrowUp => MultiMsg::SteppedUp,
        Key::Char(' ') => MultiMsg::Toggled,
        Key::Enter => MultiMsg::Accepted,
        Key::Escape => MultiMsg::Escaped,
        _ => return None,
    })
}

/// Everything the multi-select knows: which rows are ticked and where the
/// cursor is. The rows themselves are fixed for the picker's life, so they stay
/// with the caller.
struct MultiModel {
    cursor: usize,
    disabled: Vec<bool>,
    selected: Vec<bool>,
}

impl MultiModel {
    fn new(items: &[MultiItem]) -> Self {
        let selected = items
            .iter()
            .map(|item| item.selected && !item.disabled)
            .collect();
        Self {
            cursor: 0,
            disabled: items.iter().map(|item| item.disabled).collect(),
            selected,
        }
    }

    fn update(&mut self, msg: MultiMsg) -> Flow<Option<Vec<usize>>> {
        match msg {
            MultiMsg::Accepted => return Flow::Stop(Some(self.ticked())),
            MultiMsg::AllSelected => {
                for (selected, disabled) in self.selected.iter_mut().zip(&self.disabled) {
                    *selected = !disabled;
                }
            }
            MultiMsg::Escaped => return Flow::Stop(None),
            MultiMsg::NoneSelected => self.selected.fill(false),
            // Unlike the single-select, this is a fixed list of things to act
            // on rather than a ring: running off an end stays put, where
            // wrapping would put a space over something nobody looked at.
            MultiMsg::SteppedDown => {
                if self.cursor + 1 < self.selected.len() {
                    self.cursor += 1;
                }
            }
            MultiMsg::SteppedUp => self.cursor = self.cursor.saturating_sub(1),
            MultiMsg::Toggled => {
                if !self.disabled.get(self.cursor).copied().unwrap_or(true)
                    && let Some(row) = self.selected.get_mut(self.cursor)
                {
                    *row = !*row;
                }
            }
        }
        Flow::Continue
    }

    fn ticked(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter(|(_, s)| **s)
            .map(|(i, _)| i)
            .collect()
    }
}

pub(crate) struct MultiItem {
    pub(crate) label: String,
    pub(crate) selected: bool,
    pub(crate) disabled: bool,
}

/// The multi-select picker. Escape returns `None`; accepting returns `Some` with
/// the selected indices, including an empty vector when no rows are selected.
/// `legend` is an optional dim line under the prompt, for glossing anything in
/// the rows that isn't self-explanatory; callers pass `None` where nothing needs
/// it, so a plain list stays plain. `keys` is taken by value for the same reason
/// as [`pick`]: raw mode ends with the call, not with the caller's scope.
pub(crate) fn multi_select(
    prompt: &str,
    legend: Option<&str>,
    items: &[MultiItem],
    mut keys: impl KeySource,
) -> AppResult<Option<Vec<usize>>> {
    let term = Term::stderr();
    let mut model = MultiModel::new(items);
    let header = format!("{} {}", style("?").green().bold(), style(prompt).bold());
    let legend = legend.map(|l| format!("  {}", style(l).dim()));

    let _cursor_guard = CursorGuard::hide();

    let draw = |cursor: usize, selected: &[bool]| -> usize {
        let (rows_term, cols_term) = term.size();
        let (height, width) = (rows_term as usize, cols_term as usize);
        let mut rows = visual_rows(&header, width);
        render_line(&header);
        if let Some(legend) = &legend {
            rows += visual_rows(legend, width);
            render_line(legend);
        }

        // Scroll a window of items around the cursor and reserve a trailing line
        // of headroom, so a long list never overflows the screen and scrolls the
        // prompt out of `clear_last_lines`' reach (see `render`).
        let viewport = height.saturating_sub(rows + 1).max(1);
        let total = items.len();
        let scroll = if total <= viewport || cursor + 1 < viewport {
            0
        } else {
            (cursor + 1 - viewport).min(total - viewport)
        };
        let end = (scroll + viewport).min(total);
        for i in scroll..end {
            let arrow = if i == cursor { ">" } else { " " };
            let check = if selected[i] { "[x]" } else { "[ ]" };
            let line = format!("  {arrow} {check} {}", items[i].label);
            let line = if items[i].disabled {
                style(line).dim().to_string()
            } else {
                line
            };
            rows += visual_rows(&line, width);
            render_line(&line);
        }
        rows
    };

    let clear = |n: usize| {
        let _ = term.clear_last_lines(n);
    };

    let mut drawn = draw(model.cursor, &model.selected);

    loop {
        let Some(msg) = multi_msg(&keys.read_key()?) else {
            continue;
        };
        if let Flow::Stop(selection) = model.update(msg) {
            clear(drawn);
            return Ok(selection);
        }

        clear(drawn);
        drawn = draw(model.cursor, &model.selected);
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    /// Drives an event loop from a fixed list of keys. Once exhausted it yields
    /// `Escape` so a test that under-specifies its script bails out of the loop
    /// rather than hanging.
    struct ScriptedKeys(std::vec::IntoIter<Key>);

    impl ScriptedKeys {
        fn new(keys: Vec<Key>) -> Self {
            Self(keys.into_iter())
        }
    }

    impl KeySource for ScriptedKeys {
        fn read_key(&mut self) -> std::io::Result<Key> {
            Ok(self.0.next().unwrap_or(Key::Escape))
        }
    }

    fn section(heading: &'static str, names: &[&str]) -> Section {
        Section {
            heading,
            items: names
                .iter()
                .map(|n| Pick {
                    name: (*n).to_string(),
                    is_current: false,
                    annotation: String::new(),
                    disabled: false,
                })
                .collect(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// A catalogue with nothing in it, to be filled in by the struct-update
    /// syntax so each test states only the facts it is about.
    fn catalogue() -> Catalogue {
        Catalogue {
            current: None,
            held: HashMap::new(),
            local: Vec::new(),
            remote_only: Vec::new(),
        }
    }

    fn held(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(b, p)| ((*b).to_string(), (*p).to_string()))
            .collect()
    }

    /// The whole list as the user reads it: headings, then one line per row
    /// carrying its marker and, where it is inert, `(disabled)`.
    fn listing(catalogue: &Catalogue, verb: Verb) -> Vec<String> {
        sections(catalogue, verb)
            .iter()
            .flat_map(|s| {
                std::iter::once(s.heading.to_string()).chain(s.items.iter().map(|p| {
                    let mark = if p.is_current { "*" } else { " " };
                    let inert = if p.disabled { " (disabled)" } else { "" };
                    format!("{mark} {} {}{inert}", p.name, p.annotation)
                        .trim_end()
                        .to_string()
                }))
            })
            .collect()
    }

    /// Keys for typing a literal string into the filter.
    fn typed(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    const SELECT_OPTS: PickerOptions = PickerOptions {
        prompt: "Test",
        allow_create_from_filter: false,
    };

    const CREATE_OPTS: PickerOptions = PickerOptions {
        prompt: "Test",
        allow_create_from_filter: true,
    };

    fn run_pick(sections: &[Section], opts: PickerOptions, keys: Vec<Key>) -> Option<Selection> {
        pick(None, sections, opts, ScriptedKeys::new(keys)).expect("pick should not error")
    }

    fn picked_name(sel: Option<Selection>) -> Option<String> {
        match sel {
            Some(Selection::Existing(name)) => Some(name),
            _ => None,
        }
    }

    /// How the picker came to a stop, as the tests care about it.
    #[derive(Debug, Eq, PartialEq)]
    enum Stopped {
        Cancelled,
        Picked(String),
    }

    /// Drives the model directly, with no terminal and no key source — the
    /// point of splitting `update` out. Returns how the last message stopped
    /// the picker, or `None` if it didn't stop.
    fn step(model: &mut PickModel<'_>, msgs: &[PickMsg]) -> Option<Stopped> {
        let mut out = None;
        for &msg in msgs {
            if let Flow::Stop(selection) = model.update(msg) {
                out = Some(match picked_name(selection) {
                    Some(name) => Stopped::Picked(name),
                    None => Stopped::Cancelled,
                });
            }
        }
        out
    }

    /// The cursor is where the model says it is, not where the terminal drew it.
    fn at_cursor(model: &PickModel<'_>) -> Option<String> {
        picked_name(cursor_selection(&model.view, model.cursor))
    }

    /// Stepping wraps: the list is a ring, so falling off the bottom lands on
    /// the top.
    #[test]
    fn stepping_past_the_last_row_wraps_to_the_first() {
        let sections = vec![section("Local", &["a", "b", "c"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        step(&mut model, &[PickMsg::MovedToEnd, PickMsg::SteppedDown]);
        assert_eq!(at_cursor(&model).as_deref(), Some("a"));
    }

    /// Paging clamps where stepping wraps — a page is "as far as you can see",
    /// and running out of list is not a reason to reappear at the other end.
    #[test]
    fn paging_past_the_last_row_clamps_to_it() {
        let sections = vec![section("Local", &["a", "b", "c"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        step(&mut model, &[PickMsg::PagedDown(10)]);
        assert_eq!(at_cursor(&model).as_deref(), Some("c"));
        step(&mut model, &[PickMsg::PagedUp(10)]);
        assert_eq!(at_cursor(&model).as_deref(), Some("a"));
    }

    /// Typing re-derives the view, and the cursor follows the branch it was on
    /// rather than the position it held — otherwise narrowing the list silently
    /// moves the selection under the user.
    #[test]
    fn filtering_keeps_the_cursor_on_the_branch_it_was_on() {
        let sections = vec![section("Local", &["alpha", "beta", "beta-two"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        step(&mut model, &[PickMsg::SteppedDown]);
        assert_eq!(at_cursor(&model).as_deref(), Some("beta"));
        step(&mut model, &[PickMsg::FilterPushed('b')]);
        assert_eq!(at_cursor(&model).as_deref(), Some("beta"));
    }

    /// Where the branch it was on is filtered away, there is nothing to follow,
    /// so the cursor goes to the top of what's left.
    #[test]
    fn filtering_away_the_cursors_branch_falls_back_to_the_first_row() {
        let sections = vec![section("Local", &["alpha", "beta"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        step(&mut model, &[PickMsg::SteppedDown]);
        assert_eq!(at_cursor(&model).as_deref(), Some("beta"));
        // `l` is in "alpha" and not in "beta", so the cursor's branch goes.
        step(&mut model, &[PickMsg::FilterPushed('l')]);
        assert_eq!(at_cursor(&model).as_deref(), Some("alpha"));
    }

    /// Escape is one gesture with two meanings, and only the model knows which:
    /// it clears a filter if there is one, and otherwise cancels.
    #[test]
    fn escape_clears_the_filter_before_it_cancels() {
        let sections = vec![section("Local", &["alpha", "beta"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        step(&mut model, &[PickMsg::FilterPushed('a')]);
        assert!(
            step(&mut model, &[PickMsg::Escaped]).is_none(),
            "the first escape has a filter to clear, so the picker stays open"
        );
        assert_eq!(model.filter, "");
        assert_eq!(
            step(&mut model, &[PickMsg::Escaped]),
            Some(Stopped::Cancelled),
            "with nothing left to clear, escape cancels"
        );
    }

    #[test]
    fn accepting_stops_with_the_row_under_the_cursor() {
        let sections = vec![section("Local", &["a", "b"])];
        let mut model = PickModel::new(None, &sections, SELECT_OPTS);
        let stopped = step(&mut model, &[PickMsg::SteppedDown, PickMsg::Accepted]);
        assert_eq!(stopped, Some(Stopped::Picked("b".to_string())));
    }

    /// The bindings live in one table, so this is the whole keyboard contract.
    /// Control characters are not filter input, and an unbound key means
    /// nothing rather than something harmless.
    #[test]
    fn each_key_maps_to_the_intention_it_names() {
        let page = 7;
        let cases = [
            (Key::ArrowDown, Some(PickMsg::SteppedDown)),
            (Key::ArrowUp, Some(PickMsg::SteppedUp)),
            (Key::Backspace, Some(PickMsg::FilterPopped)),
            (Key::Char('x'), Some(PickMsg::FilterPushed('x'))),
            (Key::Char('\t'), None),
            (Key::End, Some(PickMsg::MovedToEnd)),
            (Key::Enter, Some(PickMsg::Accepted)),
            (Key::Escape, Some(PickMsg::Escaped)),
            (Key::Home, Some(PickMsg::MovedToStart)),
            (Key::PageDown, Some(PickMsg::PagedDown(page))),
            (Key::PageUp, Some(PickMsg::PagedUp(page))),
            (Key::Unknown, None),
        ];
        for (key, want) in cases {
            assert_eq!(pick_msg(&key, page), want, "for {key:?}");
        }
    }

    /// As for [`pick_msg`], this is the multi-select's whole keyboard contract.
    #[test]
    fn each_multi_select_key_maps_to_the_intention_it_names() {
        let cases = [
            (Key::ArrowDown, Some(MultiMsg::SteppedDown)),
            (Key::ArrowLeft, Some(MultiMsg::NoneSelected)),
            (Key::ArrowRight, Some(MultiMsg::AllSelected)),
            (Key::ArrowUp, Some(MultiMsg::SteppedUp)),
            (Key::Char(' '), Some(MultiMsg::Toggled)),
            (Key::Char('x'), None),
            (Key::Enter, Some(MultiMsg::Accepted)),
            (Key::Escape, Some(MultiMsg::Escaped)),
            (Key::Home, None),
        ];
        for (key, want) in cases {
            assert_eq!(multi_msg(&key), want, "for {key:?}");
        }
    }

    /// Toggling is about the row under the cursor and nothing else — the bug
    /// this guards is a select-all that also flips the current row back.
    #[test]
    fn toggling_flips_only_the_row_under_the_cursor() {
        let mut model = MultiModel::new(&multi_items(
            &["a", "b", "c"],
            &[false, false, false],
            &[false, false, false],
        ));
        model.update(MultiMsg::SteppedDown);
        model.update(MultiMsg::Toggled);
        assert_eq!(model.selected, vec![false, true, false]);
    }

    /// The multi-select is a fixed list of things to act on, not a ring: running
    /// off either end stays put rather than jumping to the far side, where a
    /// space would then tick something the user never looked at.
    #[test]
    fn multi_select_stepping_stops_at_both_ends() {
        let mut model =
            MultiModel::new(&multi_items(&["a", "b"], &[false, false], &[false, false]));
        model.update(MultiMsg::SteppedUp);
        assert_eq!(model.cursor, 0);
        model.update(MultiMsg::SteppedDown);
        model.update(MultiMsg::SteppedDown);
        assert_eq!(model.cursor, 1);
    }

    #[test]
    fn type_to_filter_then_enter_selects_match() {
        let sections = vec![section("Local", &["main", "feature", "develop"])];
        let mut keys = typed("feat");
        keys.push(Key::Enter);
        let sel = run_pick(&sections, SELECT_OPTS, keys);
        assert_eq!(picked_name(sel).as_deref(), Some("feature"));
    }

    #[test]
    fn non_matching_filter_with_enter_creates() {
        let sections = vec![section("Local", &["main"])];
        let mut keys = typed("xyz");
        keys.push(Key::Enter);
        let sel = run_pick(&sections, CREATE_OPTS, keys);
        assert_matches!(sel, Some(Selection::Create(name)) if name == "xyz");
    }

    #[test]
    fn cursor_navigation_skips_headings() {
        let sections = vec![section("Local", &["l1"]), section("Remote", &["r1", "r2"])];
        // From l1, one ArrowDown should land on r1, stepping over the "Remote"
        // heading row rather than onto it.
        let sel = run_pick(&sections, SELECT_OPTS, vec![Key::ArrowDown, Key::Enter]);
        assert_eq!(picked_name(sel).as_deref(), Some("r1"));
    }

    fn run_multi_select_with_disabled(
        items: &[&str],
        defaults: &[bool],
        disabled: &[bool],
        keys: Vec<Key>,
    ) -> Option<Vec<usize>> {
        multi_select(
            "Test",
            None,
            &multi_items(items, defaults, disabled),
            ScriptedKeys::new(keys),
        )
        .expect("multi_select should not error")
    }

    fn run_multi_select(items: &[&str], defaults: &[bool], keys: Vec<Key>) -> Option<Vec<usize>> {
        run_multi_select_with_disabled(items, defaults, &vec![false; items.len()], keys)
    }

    fn multi_items(items: &[&str], selected: &[bool], disabled: &[bool]) -> Vec<MultiItem> {
        items
            .iter()
            .zip(selected)
            .zip(disabled)
            .map(|((label, selected), disabled)| MultiItem {
                label: (*label).to_string(),
                selected: *selected,
                disabled: *disabled,
            })
            .collect()
    }

    #[test]
    fn multi_select_space_toggles_returns_index_set() {
        let got = run_multi_select(
            &["a", "b", "c"],
            &[false, false, false],
            vec![
                Key::Char(' '),
                Key::ArrowDown,
                Key::ArrowDown,
                Key::Char(' '),
                Key::Enter,
            ],
        );
        assert_eq!(got, Some(vec![0, 2]));
    }

    #[test]
    fn multi_select_right_selects_all() {
        let got = run_multi_select(
            &["a", "b"],
            &[false, false],
            vec![Key::ArrowRight, Key::Enter],
        );
        assert_eq!(got, Some(vec![0, 1]));
    }

    #[test]
    fn multi_select_cannot_toggle_a_disabled_row() {
        let got = run_multi_select_with_disabled(
            &["held", "free"],
            &[false, false],
            &[true, false],
            vec![Key::Char(' '), Key::Enter],
        );
        assert_eq!(got, Some(Vec::new()));
    }

    #[test]
    fn multi_select_all_skips_disabled_rows() {
        let got = run_multi_select_with_disabled(
            &["held", "free"],
            &[false, false],
            &[true, false],
            vec![Key::ArrowRight, Key::Enter],
        );
        assert_eq!(got, Some(vec![1]));
    }

    #[test]
    fn multi_select_left_selects_none() {
        let got = run_multi_select(&["a", "b"], &[true, true], vec![Key::ArrowLeft, Key::Enter]);
        assert_eq!(got, Some(Vec::new()));
    }

    #[test]
    fn multi_select_escape_cancels() {
        let got = run_multi_select(&["a", "b"], &[true, true], vec![Key::Escape]);
        assert_eq!(got, None);
    }

    /// One list, whichever verb is picking: every branch is on it, in the same
    /// order, with the worktree-backed ones carrying their path. Only what
    /// selecting a row *does* differs, and `br` is the only verb where that
    /// leaves a row with nothing to do.
    #[test]
    fn every_verb_draws_the_same_list() {
        let catalogue = Catalogue {
            current: Some("main".to_string()),
            held: held(&[("feature", "~/wt/repo/feature")]),
            local: names(&["main", "feature", "spike"]),
            remote_only: names(&["colleagues-work"]),
        };

        let listed = [
            "Local",
            "* main",
            "  feature ~/wt/repo/feature",
            "  spike",
            "Remote",
            "  colleagues-work",
        ];
        assert_eq!(listing(&catalogue, Verb::Go), listed);
        assert_eq!(listing(&catalogue, Verb::Worktree), listed);
        assert_eq!(
            listing(&catalogue, Verb::Here),
            [
                "Local",
                "* main",
                "  feature in ~/wt/repo/feature — use `perch` (disabled)",
                "  spike",
                "Remote",
                "  colleagues-work",
            ],
        );
    }

    /// The worktree we're standing in holds the current branch, so annotating it
    /// would put a path on every list's `*` row saying only "you are here" — and
    /// under `br` would grey out the one row that is always checkoutable.
    #[test]
    fn the_branch_we_are_on_is_marked_but_never_pathed() {
        let catalogue = Catalogue {
            current: Some("main".to_string()),
            held: held(&[("main", "~/repo")]),
            local: names(&["main"]),
            ..catalogue()
        };
        assert_eq!(listing(&catalogue, Verb::Go), ["Local", "* main"]);
        assert_eq!(listing(&catalogue, Verb::Here), ["Local", "* main"]);
    }

    /// A heading with nothing under it is not a heading.
    #[test]
    fn empty_headings_are_dropped() {
        let catalogue = Catalogue {
            local: names(&["only"]),
            ..catalogue()
        };
        assert_eq!(listing(&catalogue, Verb::Go), ["Local", "  only"]);
    }

    /// A greyed row is on screen but out of reach, so the cursor steps over it
    /// exactly as it steps over a heading.
    #[test]
    fn a_disabled_row_cannot_be_selected() {
        let catalogue = Catalogue {
            held: held(&[("feature", "~/wt/repo/feature")]),
            local: names(&["feature", "spike"]),
            ..catalogue()
        };
        let sections = sections(&catalogue, Verb::Here);
        let sel = run_pick(&sections, SELECT_OPTS, vec![Key::Enter]);
        assert_eq!(picked_name(sel).as_deref(), Some("spike"));
    }

    /// Strips styling so a row asserts as the user sees it.
    fn plain(s: &str) -> String {
        console::strip_ansi_codes(s).into_owned()
    }

    fn item_row(name: &str, annotation: &str, disabled: bool) -> RenderRow {
        RenderRow {
            kind: RowKind::Item(Pick {
                name: name.to_string(),
                is_current: false,
                annotation: annotation.to_string(),
                disabled,
            }),
            section_idx: 0,
        }
    }

    /// The viewport budgets one terminal line per row, so a row that wrapped
    /// would spend screen it was never allotted — pushing the prompt off the
    /// top and stranding output `clear_last_lines` can't reach.
    #[test]
    fn a_row_never_exceeds_the_terminal_width() {
        let long = "/Users/someone/Developer/Personal/worktrees/repo/feature";
        for width in [10, 20, 40, 80] {
            for disabled in [false, true] {
                let row = item_row("feature", long, disabled);
                let drawn = plain(&format_row(&row, true, 7, width));
                assert!(
                    measure_text_width(&drawn) <= width,
                    "{width}-column row overflowed (disabled={disabled}): {drawn:?}"
                );
            }
        }
    }

    /// A path is identified by where it ends, so the tail is what survives.
    #[test]
    fn a_clipped_annotation_keeps_its_tail() {
        let row = item_row(
            "feature",
            "/very/long/path/to/worktrees/repo/feature",
            false,
        );
        let drawn = plain(&format_row(&row, false, 7, 40));
        assert!(
            drawn.ends_with("worktrees/repo/feature"),
            "should keep the identifying tail, got: {drawn:?}"
        );
        assert!(drawn.contains('…'), "and mark the cut, got: {drawn:?}");
    }

    /// A branch name reads from the left, and it outranks its annotation: with
    /// too little room for both, the annotation goes rather than being cut to a
    /// lone ellipsis.
    #[test]
    fn a_narrow_row_keeps_the_name_and_drops_the_annotation() {
        let row = item_row("feature", "/very/long/path", false);
        let drawn = plain(&format_row(&row, false, 7, 14));
        assert_eq!(drawn, "      feature");
    }

    /// The alignment column is shared, so an over-wide name must pay for itself
    /// rather than charging every other row: a single long branch used to set a
    /// column past the terminal edge, and the short rows around it lost the
    /// annotations they had room for.
    #[test]
    fn one_over_wide_name_does_not_cost_the_short_rows_their_annotations() {
        let column = 100;
        let row = item_row("feature", "/Users/someone/worktrees/repo/feature", false);
        let drawn = plain(&format_row(&row, false, column, 80));
        assert!(
            drawn.ends_with("worktrees/repo/feature"),
            "the short row should keep the annotation it has room for, got: {drawn:?}"
        );
    }

    #[test]
    fn clipping_marks_the_cut_at_the_end_it_took_from() {
        assert_eq!(clip_end("abcdefgh", 4), "abc…");
        assert_eq!(clip_start("abcdefgh", 4), "…fgh");
        assert_eq!(clip_end("short", 10), "short", "untouched where it fits");
        assert_eq!(clip_start("short", 10), "short");
    }

    #[test]
    fn align_labels_pads_annotations_into_a_column() {
        let rows = vec![
            ("short".to_string(), "(+ worktree)".to_string()),
            ("much-longer-name".to_string(), "↑1".to_string()),
        ];
        let got = align_labels(&rows);
        assert_eq!(got[0], "short             (+ worktree)");
        assert_eq!(got[1], "much-longer-name  ↑1");
    }

    #[test]
    fn align_labels_leaves_unannotated_rows_bare() {
        let rows = vec![
            ("a".to_string(), String::new()),
            ("bb".to_string(), "↑1".to_string()),
        ];
        let got = align_labels(&rows);
        assert_eq!(got[0], "a", "no trailing padding on a bare row");
        assert_eq!(got[1], "bb  ↑1");
    }
}
