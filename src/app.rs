use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Event, EventFilter, FontId, Id, Key, Modifiers, Rect, Sense, Stroke, Vec2,
    pos2, vec2,
};
use ropey::Rope;

use crate::doc::{self, Doc, Kind, Selection, Side};
use crate::find::{self, Needle};
use crate::platform;

/// Lines longer than this are truncated on screen (never on disk).
const MAX_COLS: usize = 2000;
const TAB: usize = 4;
const FONT_SIZE: f32 = 13.0;
/// Writes wait for a pause this long, so typing doesn't rewrite a huge file per key.
const SAVE_DELAY: Duration = Duration::from_millis(300);
/// Match offsets kept for Find next/previous (the count is always exact).
const MAX_MATCHES: usize = 1_000_000;

/// Writes the file on a background thread so the UI never blocks on I/O.
/// Bursts of changes collapse into one write of the latest text.
struct Saver {
    tx: Option<Sender<(Rope, PathBuf)>>,
    handle: Option<JoinHandle<()>>,
    status: Arc<Mutex<String>>,
}

impl Saver {
    fn new(ctx: egui::Context) -> Saver {
        let (tx, rx) = channel::<(Rope, PathBuf)>();
        let status = Arc::new(Mutex::new(String::new()));
        let st = status.clone();
        let handle = std::thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                // Wait for a pause, always keeping only the newest text.
                while let Ok(newer) = rx.recv_timeout(SAVE_DELAY).map_err(|e| e == RecvTimeoutError::Disconnected) {
                    job = newer;
                }
                let (rope, path) = job;
                *st.lock().unwrap() = match doc::save(&rope, &path) {
                    Ok(()) => "Saved".into(),
                    Err(e) => format!("Save failed: {e}"),
                };
                ctx.request_repaint();
            }
        });
        Saver { tx: Some(tx), handle: Some(handle), status }
    }

    fn save(&self, doc: &Doc) {
        *self.status.lock().unwrap() = "Saving…".into();
        if let Some(tx) = &self.tx {
            // Rope clones share their nodes: O(1), no copy of the text.
            let _ = tx.send((doc.rope.clone(), doc.path.clone()));
        }
    }

    /// Block until every pending write has hit the disk.
    fn flush(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Lines ticked for "Accept Selected" in one conflict.
struct Picks {
    /// `end - start` of the conflict when ticked; if it changes the ticks are dropped.
    len: usize,
    /// Ticked lines as offsets from the conflict's start line (ascending).
    offsets: Vec<usize>,
}

/// Last frame's scroll view.
#[derive(Default, Clone, Copy)]
struct View {
    offset: Vec2,
    size: Vec2,
    rows: usize,
    row_h: f32,
}

/// What a search was run for: query, match case, document version.
type FindKey = (String, bool, u64);

/// All matches for one [`FindKey`].
struct Found {
    key: FindKey,
    /// Byte offsets of the first [`MAX_MATCHES`] matches.
    starts: Vec<usize>,
    total: usize,
}

/// The find bar. Matches are counted on a background thread; the visible
/// ones are highlighted straight from the line text.
#[derive(Default)]
struct Find {
    open: bool,
    query: String,
    case: bool,
    /// Focus the query field and select its text next frame.
    focus: bool,
    found: Option<Found>,
    /// Search running in the background.
    pending: Option<FindKey>,
    slot: Arc<Mutex<Option<Found>>>,
    generation: Arc<AtomicU64>,
}

enum Mouse {
    Press(usize, bool),
    Drag(usize),
    Word(usize),
    Line(usize),
}

pub struct App {
    doc: Option<Doc>,
    error: Option<String>,
    saver: Saver,
    unresolved_out: Arc<AtomicUsize>,
    open_dialog: bool,
    /// Start line of the conflict ⌘1/2/3 act on when the cursor isn't in one
    /// (if that conflict is gone, the next one after it).
    active: Option<usize>,
    /// Ticked lines per conflict, keyed by the conflict's start line.
    picks: HashMap<usize, Picks>,
    sel: Selection,
    /// Display column kept while moving up and down.
    want_col: Option<usize>,
    /// Scroll so this line sits near the top quarter of the view.
    scroll_to: Option<usize>,
    /// Scroll just enough to show the cursor.
    reveal: bool,
    view: View,
    content_w: f32,
    line_buf: String,
    find: Find,
}

impl App {
    pub fn new(ctx: &egui::Context, loaded: Option<std::io::Result<Doc>>, unresolved_out: Arc<AtomicUsize>) -> App {
        let mut app = App {
            doc: None,
            error: None,
            saver: Saver::new(ctx.clone()),
            unresolved_out,
            open_dialog: false,
            active: None,
            picks: HashMap::new(),
            sel: (0, 0),
            want_col: None,
            scroll_to: None,
            reveal: false,
            view: View::default(),
            content_w: 0.0,
            line_buf: String::new(),
            find: Find::default(),
        };
        brighten_text(ctx);
        if let Some(r) = loaded {
            app.set_doc(ctx, r);
        }
        app
    }

    fn set_doc(&mut self, ctx: &egui::Context, r: std::io::Result<Doc>) {
        match r {
            Ok(doc) => {
                let name = doc.path.file_name().unwrap_or_default().to_string_lossy();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — mergefix")));
                self.unresolved_out.store(doc.conflicts.len(), Ordering::Relaxed);
                self.doc = Some(doc);
                self.error = None;
                self.picks.clear();
                self.active = None;
                self.sel = (0, 0);
                self.want_col = None;
                self.content_w = 0.0;
                *self.saver.status.lock().unwrap() = String::new();
                self.goto(true);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Bookkeeping after any change to the document: remap line-keyed state and save.
    fn changed(&mut self) {
        let Some(doc) = &mut self.doc else { return };
        let shifts = doc.take_shifts();
        let remap = |mut line: usize| {
            for s in &shifts {
                line = s.map(line)?;
            }
            Some(line)
        };
        if let Some(a) = self.active {
            // If its line was edited, keep pointing at that spot.
            self.active = Some(remap(a).unwrap_or_else(|| shifts.last().map_or(a, |s| s.first)));
        }
        self.picks = std::mem::take(&mut self.picks)
            .into_iter()
            .filter_map(|(start, p)| {
                let start = remap(start)?;
                let ci = doc.conflict_at(start)?;
                let c = doc.conflicts[ci];
                (c.start == start && c.end - c.start == p.len).then_some((start, p))
            })
            .collect();
        let len = doc.rope.len_chars();
        self.sel = (self.sel.0.min(len), self.sel.1.min(len));
        self.unresolved_out.store(doc.conflicts.len(), Ordering::Relaxed);
        self.saver.save(doc);
    }

    // ---- conflicts -------------------------------------------------------

    fn active_ci(&self) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        let line = self.active?;
        let ci = doc.conflicts.partition_point(|c| c.start < line);
        (ci < doc.conflicts.len()).then_some(ci)
    }

    /// The conflict keyboard accepts apply to: the one under the cursor, else the active one.
    fn target_ci(&self) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        doc.conflict_at(doc.rope.char_to_line(self.sel.1)).or_else(|| self.active_ci())
    }

    fn resolve(&mut self, ci: usize, side: Option<Side>) {
        let Some(doc) = &mut self.doc else { return };
        let start = doc.conflicts[ci].start;
        let at = match side {
            Some(side) => doc.resolve(ci, side, self.sel),
            None => {
                let lines: Vec<usize> = self.picks.get(&start).map_or(Vec::new(), |p| p.offsets.iter().map(|o| start + o).collect());
                doc.resolve_picked(ci, &lines, self.sel)
            }
        };
        self.picks.remove(&start);
        self.sel = (at, at);
        self.want_col = None;
        // The next conflict after this spot becomes the active one.
        self.active = Some(start);
        self.changed();
    }

    /// Jump to the next/previous conflict (wrapping) and put the cursor there.
    fn goto(&mut self, forward: bool) {
        let Some(doc) = &self.doc else { return };
        let n = doc.conflicts.len();
        if n == 0 {
            return;
        }
        let cursor_line = doc.rope.char_to_line(self.sel.1);
        let ci = match (self.active, forward) {
            (Some(a), true) => doc.conflicts.partition_point(|c| c.start <= a) % n,
            (None, true) => doc.conflicts.partition_point(|c| c.start < cursor_line) % n,
            (from, false) => {
                let from = from.unwrap_or(cursor_line);
                let i = doc.conflicts.partition_point(|c| c.start < from);
                (i + n - 1) % n
            }
        };
        let start = doc.conflicts[ci].start;
        let at = doc.rope.line_to_char(start);
        self.active = Some(start);
        self.scroll_to = Some(start);
        self.sel = (at, at);
        self.want_col = None;
    }

    fn undo(&mut self, redo: bool) {
        let Some(doc) = &mut self.doc else { return };
        let sel = if redo { doc.redo() } else { doc.undo() };
        if let Some(sel) = sel {
            self.sel = sel;
            self.want_col = None;
            self.reveal = true;
            self.changed();
        }
    }

    // ---- find ------------------------------------------------------------

    fn find_key(&self) -> Option<FindKey> {
        let doc = self.doc.as_ref()?;
        (self.find.open && !self.find.query.is_empty()).then(|| (self.find.query.clone(), self.find.case, doc.version))
    }

    /// Pick up finished background counts and start one if the results are stale.
    fn refresh_find(&mut self, ctx: &egui::Context) {
        if let Some(found) = self.find.slot.lock().unwrap().take() {
            self.find.found = Some(found);
        }
        let Some(key) = self.find_key() else { return };
        if self.find.found.as_ref().is_some_and(|f| f.key == key) || self.find.pending.as_ref() == Some(&key) {
            return;
        }
        self.find.pending = Some(key.clone());
        let generation = self.find.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let current = self.find.generation.clone();
        let slot = self.find.slot.clone();
        // Rope clones share their nodes: O(1), no copy of the text.
        let rope = self.doc.as_ref().unwrap().rope.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let needle = Needle::new(&key.0, key.1).unwrap();
            let stale = || current.load(Ordering::Relaxed) != generation;
            if let Some((starts, total)) = find::find_all(&rope, &needle, MAX_MATCHES, stale)
                && !stale()
            {
                *slot.lock().unwrap() = Some(Found { key, starts, total });
                ctx.request_repaint();
            }
        });
    }

    /// Results for the current query and text, searching now if the background one isn't done.
    fn fresh_found(&mut self) -> Option<&Found> {
        let key = self.find_key()?;
        if let Some(found) = self.find.slot.lock().unwrap().take() {
            self.find.found = Some(found);
        }
        if self.find.found.as_ref().is_none_or(|f| f.key != key) {
            let needle = Needle::new(&key.0, key.1)?;
            let (starts, total) = find::find_all(&self.doc.as_ref()?.rope, &needle, MAX_MATCHES, || false)?;
            self.find.found = Some(Found { key, starts, total });
        }
        self.find.found.as_ref()
    }

    /// Select the next/previous match after/before the selection (wrapping).
    fn find_step(&mut self, forward: bool) {
        let (a, b) = self.ordered();
        let Some(rope) = self.doc.as_ref().map(|d| d.rope.clone()) else { return };
        let (from, to) = (rope.char_to_byte(a), rope.char_to_byte(b));
        let len = self.find.query.len();
        let Some(found) = self.fresh_found() else { return };
        let starts = &found.starts;
        if starts.is_empty() {
            return;
        }
        let i = if forward {
            starts.partition_point(|&s| s < to) % starts.len()
        } else {
            starts.partition_point(|&s| s < from).checked_sub(1).unwrap_or(starts.len() - 1)
        };
        let at = starts[i];
        self.select_match(at, len);
    }

    /// While typing a query: select the first match at or after the selection start.
    fn find_incremental(&mut self) {
        let Some(doc) = &self.doc else { return };
        let Some(needle) = Needle::new(&self.find.query, self.find.case) else { return };
        let from = doc.rope.char_to_byte(self.ordered().0);
        let mut hit = None;
        needle.scan(&doc.rope, from, |m| {
            hit = Some(m);
            false
        });
        if hit.is_none() {
            needle.scan(&doc.rope, 0, |m| {
                hit = Some(m);
                false
            });
        }
        if let Some(at) = hit {
            self.select_match(at, needle.len());
        }
    }

    fn select_match(&mut self, at: usize, len: usize) {
        let rope = &self.doc.as_ref().unwrap().rope;
        let (a, b) = (rope.byte_to_char(at), rope.byte_to_char(at + len));
        let line = rope.char_to_line(a);
        self.sel = (a, b);
        self.want_col = None;
        self.reveal = true;
        let View { offset, size, row_h, .. } = self.view;
        let y = line as f32 * row_h;
        if y < offset.y || y + row_h > offset.y + size.y {
            self.scroll_to = Some(line);
        }
    }

    /// "3 of 120", "120 matches" or "No results" for the find bar.
    fn find_count(&self) -> Option<(String, bool)> {
        let key = self.find_key()?;
        let found = self.find.found.as_ref().filter(|f| f.key.0 == key.0 && f.key.1 == key.1)?;
        if found.total == 0 {
            return Some(("No results".into(), false));
        }
        let rope = &self.doc.as_ref()?.rope;
        let (a, b) = self.ordered();
        let (from, to) = (rope.char_to_byte(a), rope.char_to_byte(b));
        let index = (found.key == key && to - from == key.0.len()).then(|| found.starts.binary_search(&from).ok()).flatten();
        Some(match index {
            Some(i) => (format!("{} of {}", i + 1, found.total), true),
            None if found.total == 1 => ("1 match".into(), true),
            None => (format!("{} matches", found.total), true),
        })
    }

    fn open_find(&mut self) {
        // Seed the query with a short single-line selection.
        if let Some(t) = self.selected_text().filter(|t| t.chars().count() <= 200 && !t.contains(['\n', '\r'])) {
            self.find.query = t;
        }
        self.find.open = true;
        self.find.focus = true;
    }

    fn find_bar(&mut self, ui: &mut egui::Ui) {
        let id = Id::new("find");
        ui.horizontal(|ui| {
            ui.label("Find");
            let focus = std::mem::take(&mut self.find.focus);
            if focus {
                // Select the whole query so typing replaces it.
                let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
                let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.find.query.chars().count()));
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), id);
            }
            let field = egui::TextEdit::singleline(&mut self.find.query).id(id).hint_text("Search the file").desired_width(280.0).font(FontId::monospace(FONT_SIZE));
            let resp = ui.add(field);
            if focus {
                resp.request_focus();
            }
            if resp.changed() {
                self.find_incremental();
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let back = ui.input(|i| i.modifiers.shift);
                self.find_step(!back);
                resp.request_focus();
            }
            if ui.toggle_value(&mut self.find.case, "Aa").on_hover_text("Match case").changed() {
                self.find_incremental();
                resp.request_focus();
            }
            let has = self.find_count().is_some_and(|(_, any)| any);
            if ui.add_enabled(has, egui::Button::new("⬆")).on_hover_text(keys("Previous match (⇧⏎ / ⇧⌘G)", "Previous match (Shift+Enter / Ctrl+Shift+G)")).clicked() {
                self.find_step(false);
            }
            if ui.add_enabled(has, egui::Button::new("⬇")).on_hover_text(keys("Next match (⏎ / ⌘G)", "Next match (Enter / Ctrl+G)")).clicked() {
                self.find_step(true);
            }
            match self.find_count() {
                Some((text, true)) => _ = ui.label(text),
                Some((text, false)) => _ = ui.colored_label(warn_color(ui), text),
                None if self.find.query.is_empty() => {}
                None => _ = ui.weak("Searching…"),
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✖").on_hover_text("Close (Esc)").clicked() {
                    self.find.open = false;
                }
            });
        });
    }

    // ---- editing ---------------------------------------------------------

    fn ordered(&self) -> (usize, usize) {
        (self.sel.0.min(self.sel.1), self.sel.0.max(self.sel.1))
    }

    /// Replace the selection with `text`.
    fn insert(&mut self, text: &str, typing: bool) {
        let Some(doc) = &mut self.doc else { return };
        let (a, b) = (self.sel.0.min(self.sel.1), self.sel.0.max(self.sel.1));
        let end = a + text.chars().count();
        doc.edit(a..b, text, self.sel, (end, end), typing);
        self.sel = (end, end);
        self.want_col = None;
        self.reveal = true;
        self.changed();
    }

    /// Delete the selection, or from the cursor to `to` if nothing is selected.
    fn delete_to(&mut self, to: usize) {
        let (a, b) = self.ordered();
        let Some(doc) = &mut self.doc else { return };
        let (a, b) = if a != b { (a, b) } else { (to.min(a), to.max(a)) };
        if a == b {
            return;
        }
        doc.edit(a..b, "", self.sel, (a, a), true);
        self.sel = (a, a);
        self.want_col = None;
        self.reveal = true;
        self.changed();
    }

    fn move_to(&mut self, to: usize, extend: bool) {
        self.sel = if extend { (self.sel.0, to) } else { (to, to) };
        self.reveal = true;
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.ordered();
        (a != b).then(|| self.doc.as_ref().unwrap().rope.slice(a..b).to_string())
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        if self.doc.is_none() {
            ctx.input_mut(|i| self.open_dialog |= i.consume_key(Modifiers::COMMAND, Key::O));
            return;
        }
        // While typing a query only app-wide shortcuts reach us; the field gets the rest.
        let in_find = self.find.open && ctx.memory(|m| m.has_focus(Id::new("find")));
        for event in ctx.input(|i| i.events.clone()) {
            match event {
                Event::Key { key, pressed: true, modifiers, .. } if in_find => {
                    let global = matches!(key, Key::F | Key::G | Key::O | Key::S | Key::Num1 | Key::Num2 | Key::Num3);
                    if matches!(key, Key::Escape | Key::F7) || (modifiers.command && global) {
                        self.key(key, modifiers);
                    }
                }
                _ if in_find => {}
                Event::Text(t) if !t.chars().any(char::is_control) => self.insert(&t, true),
                Event::Paste(t) => {
                    let doc = self.doc.as_ref().unwrap();
                    let mut t = t.replace("\r\n", "\n");
                    if doc.newline == "\r\n" {
                        t = t.replace('\n', "\r\n");
                    }
                    self.insert(&t, false);
                }
                Event::Copy => {
                    if let Some(t) = self.selected_text() {
                        ctx.copy_text(t);
                    }
                }
                Event::Cut => {
                    if let Some(t) = self.selected_text() {
                        ctx.copy_text(t);
                        self.insert("", false);
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => self.key(key, modifiers),
                _ => {}
            }
        }
    }

    fn key(&mut self, key: Key, m: Modifiers) {
        let doc = self.doc.as_ref().unwrap();
        let rope = &doc.rope;
        let (a, b) = self.ordered();
        let head = self.sel.1;
        let line = rope.char_to_line(head);
        let line_start = rope.line_to_char(line);
        let line_end = line_start + doc.line_len(line);
        let len = rope.len_chars();
        let shift = m.shift;
        // Word moves are ⌥ on macOS and Ctrl elsewhere; ⌘ jumps to line ends on macOS.
        let mac = cfg!(target_os = "macos");
        let (word, line_end_mod) = if mac { (m.alt, m.command) } else { (m.ctrl, false) };
        match key {
            Key::ArrowLeft | Key::ArrowRight => {
                let left = key == Key::ArrowLeft;
                let to = if line_end_mod {
                    if left { line_start } else { line_end }
                } else if word {
                    if left { word_left(rope, head) } else { word_right(rope, head) }
                } else if a != b && !shift {
                    if left { a } else { b }
                } else if left {
                    prev_char(rope, head)
                } else {
                    next_char(rope, head)
                };
                self.want_col = None;
                self.move_to(to, shift);
            }
            Key::ArrowUp if m.alt => self.goto(false),
            Key::ArrowDown if m.alt => self.goto(true),
            Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
                let up = matches!(key, Key::ArrowUp | Key::PageUp);
                if m.command && matches!(key, Key::ArrowUp | Key::ArrowDown) {
                    self.want_col = None;
                    self.move_to(if up { 0 } else { len }, shift);
                    return;
                }
                let step = if matches!(key, Key::PageUp | Key::PageDown) { self.view.rows.max(1) } else { 1 };
                let target = if up { line.saturating_sub(step) } else { (line + step).min(doc.lines() - 1) };
                let col = self.want_col.unwrap_or_else(|| col_of(&doc.line_text(line), head - line_start));
                let to = rope.line_to_char(target) + char_at_col(&doc.line_text(target), col);
                self.move_to(to, shift);
                self.want_col = Some(col);
            }
            Key::Home | Key::End => {
                let to = match (key, m.command) {
                    (Key::Home, true) => 0,
                    (Key::Home, false) => line_start,
                    (_, true) => len,
                    _ => line_end,
                };
                self.want_col = None;
                self.move_to(to, shift);
            }
            Key::Backspace => {
                let to = if line_end_mod {
                    if head == line_start { prev_char(rope, head) } else { line_start }
                } else if word {
                    word_left(rope, head)
                } else {
                    prev_char(rope, head)
                };
                self.delete_to(to);
            }
            Key::Delete => {
                let to = if word { word_right(rope, head) } else { next_char(rope, head) };
                self.delete_to(to);
            }
            Key::Enter if !m.command => {
                // Keep the current line's indentation.
                let indent: String = doc.line_text(line).chars().take(head - line_start).take_while(|c| *c == ' ' || *c == '\t').collect();
                let text = format!("{}{indent}", doc.newline);
                self.insert(&text, true);
            }
            Key::Tab if !m.command => self.insert("\t", true),
            Key::Escape if self.find.open => self.find.open = false,
            Key::Escape => self.sel = (head, head),
            Key::F if m.command => self.open_find(),
            Key::G if m.command && self.find.open && !self.find.query.is_empty() => self.find_step(!shift),
            Key::G if m.command => self.open_find(),
            Key::A if m.command => self.sel = (0, len),
            Key::Z if m.command => self.undo(shift),
            Key::Y if m.command => self.undo(true),
            Key::O if m.command => self.open_dialog = true,
            Key::S if m.command => self.saver.save(doc),
            Key::Num1 | Key::Num2 | Key::Num3 if m.command => {
                let side = match key {
                    Key::Num1 => Side::Current,
                    Key::Num2 => Side::Incoming,
                    _ => Side::Both,
                };
                if let Some(ci) = self.target_ci() {
                    self.resolve(ci, Some(side));
                    self.goto(true);
                }
            }
            Key::F7 => self.goto(!shift),
            _ => {}
        }
    }

    // ---- UI --------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if platform::has_open_dialog() && ui.button("Open…").on_hover_text(keys("⌘O", "Ctrl+O")).clicked() {
                self.open_dialog = true;
            }
            let Some(doc) = &self.doc else {
                ui.label("mergefix");
                return;
            };
            ui.strong(doc.path.file_name().unwrap_or_default().to_string_lossy());
            ui.separator();
            let left = doc.conflicts.len();
            let total = doc.total.max(left);
            if total == 0 {
                ui.label("No conflict markers found");
            } else if left == 0 {
                ui.colored_label(Color32::from_rgb(80, 180, 100), format!("All {total} conflicts resolved ✔"));
            } else {
                ui.label(format!("{left} of {total} conflicts left"));
            }
            ui.separator();
            let (has_open, can_undo, can_redo) = (left > 0, doc.can_undo(), doc.can_redo());
            if ui.add_enabled(has_open, egui::Button::new("◀ Prev")).on_hover_text(keys("⌥↑ / Shift+F7", "Alt+Up / Shift+F7")).clicked() {
                self.goto(false);
            }
            if ui.add_enabled(has_open, egui::Button::new("Next ▶")).on_hover_text(keys("⌥↓ / F7", "Alt+Down / F7")).clicked() {
                self.goto(true);
            }
            ui.separator();
            if ui.add_enabled(can_undo, egui::Button::new("Undo")).on_hover_text(keys("⌘Z", "Ctrl+Z")).clicked() {
                self.undo(false);
            }
            if ui.add_enabled(can_redo, egui::Button::new("Redo")).on_hover_text(keys("⇧⌘Z", "Ctrl+Y")).clicked() {
                self.undo(true);
            }
            if ui.button("Find").on_hover_text(keys("⌘F", "Ctrl+F")).clicked() {
                self.open_find();
            }
            ui.separator();
            ui.weak(keys("⌘1 current · ⌘2 incoming · ⌘3 both · tick lines to pick", "Ctrl+1 current · Ctrl+2 incoming · Ctrl+3 both · tick lines to pick"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status = self.saver.status.lock().unwrap().clone();
                if status.starts_with("Save failed") {
                    ui.colored_label(Color32::RED, status);
                } else {
                    ui.weak(status);
                }
                if self.doc.as_ref().is_some_and(|d| d.lossy) {
                    ui.colored_label(warn_color(ui), "Not valid UTF-8: invalid bytes are replaced on save");
                }
            });
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = &self.doc else {
            ui.centered_and_justified(|ui| {
                ui.label(match &self.error {
                    Some(e) => format!("Could not open file: {e}"),
                    None if platform::has_open_dialog() => format!("Drop a file with merge conflicts here, or click Open… ({})", keys("⌘O", "Ctrl+O")),
                    None => "Drop a file with merge conflicts here, or run: mergefix <file>".into(),
                });
            });
            return;
        };

        let font = FontId::monospace(FONT_SIZE);
        let (row_h, char_w) = ui.ctx().fonts_mut(|f| (f.row_height(&font).ceil() + 3.0, f.glyph_width(&font, ' ')));
        let total = doc.lines();
        // Gutter = line number + checkbox column (both pinned when scrolled sideways).
        let gutter_w = (total.max(1).ilog10() + 1) as f32 * char_w + 40.0;
        let text_x = gutter_w + 8.0;
        let pal = Palette::new(ui.visuals().dark_mode);
        let text_color = ui.visuals().text_color();
        let weak_color = ui.visuals().weak_text_color();
        let check_fill = ui.visuals().selection.bg_fill;
        let check_mark = ui.visuals().selection.stroke.color;
        let sel_color = ui.visuals().selection.bg_fill.gamma_multiply(0.45);
        let min_w = text_x + doc.max_line_len.min(MAX_COLS) as f32 * char_w + 24.0;
        // Rows span at least the whole view so conflict colours reach the right edge.
        self.content_w = self.content_w.max(min_w).max(self.view.size.x);

        let head_line = doc.rope.char_to_line(self.sel.1);
        let head_col = col_of(&doc.line_text(head_line), self.sel.1 - doc.rope.line_to_char(head_line));
        let mut area = egui::ScrollArea::both().auto_shrink(false).id_salt("editor");
        // The view's size is unknown until the first frame; scroll once it is.
        if self.view.size.y > 0.0
            && let Some(line) = self.scroll_to.take()
        {
            let y = line as f32 * row_h - self.view.size.y * 0.25;
            area = area.vertical_scroll_offset(y.max(0.0)).horizontal_scroll_offset(0.0);
        } else if std::mem::take(&mut self.reveal) {
            let View { offset, size, .. } = self.view;
            let (y, x) = (head_line as f32 * row_h, text_x + head_col as f32 * char_w);
            if y < offset.y {
                area = area.vertical_scroll_offset(y);
            } else if y + row_h > offset.y + size.y {
                area = area.vertical_scroll_offset(y + row_h - size.y);
            }
            if x < offset.x + text_x {
                area = area.horizontal_scroll_offset((x - text_x - 4.0).max(0.0));
            } else if x > offset.x + size.x - 24.0 {
                area = area.horizontal_scroll_offset(x - size.x + 24.0);
            }
        }

        let editor_id = Id::new("editor");
        let (sa, sb) = self.ordered();
        let active = self.active_ci();
        let mut action = None;
        let mut mouse = None;
        let mut toggle = None;
        let mut toggle_side = None;
        let mut widest = self.content_w;
        let needle = if self.find.open { Needle::new(&self.find.query, self.find.case) } else { None };
        let mut find_buf = Vec::new();
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        // Allow scrolling half a screen past the last line, so the view doesn't jump
        // when lines near the end are deleted.
        let padding = self.view.rows / 2;
        let out = area.show_rows(ui, row_h, total + padding, |ui, range| {
            let painter = ui.painter().clone();
            let clip = ui.clip_rect();
            let y0 = ui.max_rect().top() - range.start as f32 * row_h;
            let x_text = ui.max_rect().left() + text_x;

            // One widget for the text area; buttons and checkboxes drawn later sit on top.
            let resp = ui.interact(ui.max_rect().intersect(clip), editor_id, Sense::click_and_drag());
            let resp = resp.on_hover_cursor(CursorIcon::Text);
            if let Some(pos) = resp.interact_pointer_pos().or(resp.hover_pos()) {
                let line = (((pos.y - y0) / row_h).floor().max(0.0) as usize).min(total - 1);
                let col = ((pos.x - x_text) / char_w).round().max(0.0) as usize;
                let at = doc.rope.line_to_char(line) + char_at_col(&doc.line_text(line), col);
                let (pressed, shift) = ui.input(|i| (i.pointer.primary_pressed(), i.modifiers.shift));
                if resp.triple_clicked() {
                    mouse = Some(Mouse::Line(at));
                } else if resp.double_clicked() {
                    mouse = Some(Mouse::Word(at));
                } else if pressed && resp.hovered() {
                    mouse = Some(Mouse::Press(at, shift));
                } else if resp.dragged() {
                    mouse = Some(Mouse::Drag(at));
                }
            }

            for line in range {
                let (rect, _) = ui.allocate_exact_size(vec2(self.content_w, row_h), Sense::hover());
                if line >= total {
                    let gutter = Rect::from_min_size(pos2(rect.left().max(clip.left()), rect.top()), vec2(gutter_w, row_h));
                    painter.rect_filled(gutter, 0.0, pal.gutter);
                    continue;
                }
                let (kind, ci) = doc.kind(line);
                let text = doc.line_text(line);
                let line_start = doc.rope.line_to_char(line);
                let line_end = line_start + text.chars().count();
                let x_at = |chars: usize| rect.left() + text_x + col_of(&text, chars) as f32 * char_w;

                if let Some(bg) = pal.bg(kind) {
                    painter.rect_filled(rect, 0.0, bg);
                }
                if let Some(n) = &needle {
                    for m in n.find_in(&text, &mut find_buf) {
                        let s = text[..m].chars().count();
                        let e = s + text[m..m + n.len()].chars().count();
                        let r = Rect::from_x_y_ranges(x_at(s)..=x_at(e), rect.y_range().shrink(1.0));
                        painter.rect_filled(r, 2.0, pal.find);
                        if (line_start + s, line_start + e) == (sa, sb) {
                            painter.rect_stroke(r, 2.0, Stroke::new(1.5, pal.active), egui::StrokeKind::Outside);
                        }
                    }
                }
                // Selection (a little extra past the end when it includes the line break).
                if sa < sb && sa <= line_end && sb > line_start {
                    let (s, e) = (sa.max(line_start) - line_start, sb.min(line_end) - line_start);
                    let extra = if sb > line_end { char_w * 0.6 } else { 0.0 };
                    let r = Rect::from_x_y_ranges(x_at(s)..=x_at(e) + extra, rect.y_range());
                    painter.rect_filled(r, 0.0, sel_color);
                }

                expand_line(&text, &mut self.line_buf);
                let marker = matches!(kind, Kind::Start | Kind::Sep | Kind::End | Kind::BaseMarker);
                let start = ci.map(|ci| doc.conflicts[ci].start);
                let picks = start.and_then(|s| self.picks.get(&s)).filter(|p| !p.offsets.is_empty());
                let pickable = matches!(kind, Kind::Current | Kind::Incoming);
                let picked = picks.is_some_and(|p| p.offsets.binary_search(&(line - start.unwrap())).is_ok());
                let dimmed = marker || (pickable && picks.is_some() && !picked);
                let color = if dimmed { weak_color } else { text_color };
                let text_rect = painter.text(pos2(rect.left() + text_x, rect.center().y), Align2::LEFT_CENTER, &self.line_buf, font.clone(), color);

                if line == head_line {
                    let x = x_at(self.sel.1 - line_start);
                    painter.line_segment([pos2(x, rect.top() + 1.0), pos2(x, rect.bottom() - 1.0)], Stroke::new(2.0, text_color));
                }

                if kind == Kind::Start {
                    let ci = ci.unwrap();
                    let mut x = text_rect.right().max(rect.left() + text_x) + 16.0;
                    let mut buttons = vec![
                        ("Accept Current".to_string(), Some(Side::Current)),
                        ("Accept Incoming".to_string(), Some(Side::Incoming)),
                        ("Accept Both".to_string(), Some(Side::Both)),
                    ];
                    if let Some(p) = picks {
                        buttons.push((format!("Accept Selected ({})", p.offsets.len()), None));
                    }
                    for (i, (label, side)) in buttons.into_iter().enumerate() {
                        let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), text_color);
                        let r = Rect::from_min_size(pos2(x, rect.top() + 1.0), vec2(galley.size().x + 14.0, row_h - 2.0));
                        let resp = ui.interact(r, Id::new(("accept", line, i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                        let fill = if resp.hovered() { pal.button_hover } else { pal.button };
                        painter.rect(r, 4.0, fill, Stroke::new(1.0, pal.button_stroke), egui::StrokeKind::Inside);
                        painter.galley(pos2(r.left() + 7.0, r.center().y - galley.size().y / 2.0), galley, text_color);
                        if resp.clicked() {
                            action = Some((ci, side));
                        }
                        x = r.right() + 6.0;
                    }
                    widest = widest.max(x - rect.left() + 24.0);
                } else {
                    widest = widest.max(text_rect.right() - rect.left() + 24.0);
                }

                // Gutter stays pinned to the left edge when scrolled horizontally.
                let gx = rect.left().max(clip.left());
                let gutter = Rect::from_min_size(pos2(gx, rect.top()), vec2(gutter_w, row_h));
                painter.rect_filled(gutter, 0.0, pal.gutter);
                if ci.is_some() && ci == active {
                    let bar = Rect::from_min_size(gutter.right_top() - vec2(3.0, 0.0), vec2(3.0, row_h));
                    painter.rect_filled(bar, 0.0, pal.active);
                }
                let cb = Rect::from_center_size(pos2(gutter.right() - 15.0, rect.center().y), vec2(12.0, 12.0));
                // Line checkboxes on current/incoming lines; select-all boxes on the
                // `<<<<<<<` (current) and `=======` (incoming) lines.
                let check = if let (Some(start), Some(ci)) = (start, ci) {
                    let c = doc.conflicts[ci];
                    let side = match kind {
                        Kind::Start => Some((c.current(), "current")),
                        Kind::Sep => Some((c.incoming(), "incoming")),
                        _ => None,
                    }
                    .filter(|((a, b), _)| a < b);
                    let id = Id::new(("pick", line));
                    if pickable {
                        let resp = ui.interact(cb.expand(4.0), id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                        if resp.clicked() {
                            toggle = Some((start, c.end - start, line - start));
                        }
                        Some(if picked { Check::All } else { Check::None })
                    } else if let Some(((a, b), name)) = side {
                        let n = picks.map_or(0, |p| p.offsets.iter().filter(|o| (a..b).contains(&(start + **o))).count());
                        let all = n == b - a;
                        let tip = if all { format!("Untick all {name} lines") } else { format!("Tick all {name} lines") };
                        let resp = ui.interact(cb.expand(4.0), id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
                        if resp.clicked() {
                            toggle_side = Some((start, c.end - start, a - start, b - start, all));
                        }
                        Some(if all { Check::All } else if n > 0 { Check::Some } else { Check::None })
                    } else {
                        None
                    }
                } else {
                    None
                };
                draw_check(&painter, cb, check, weak_color, check_fill, check_mark);
                painter.text(pos2(gutter.right() - 30.0, rect.center().y), Align2::RIGHT_CENTER, (line + 1).to_string(), font.clone(), weak_color);
            }
        });
        self.content_w = widest;
        self.view = View { offset: out.state.offset, size: out.inner_rect.size(), rows: (out.inner_rect.height() / row_h) as usize, row_h };

        // Keep keyboard focus on the text so Tab/arrows/Enter never reach the buttons.
        let find_open = self.find.open;
        ui.memory_mut(|m| {
            if !(find_open && m.has_focus(Id::new("find"))) {
                m.request_focus(editor_id);
            }
            m.set_focus_lock_filter(editor_id, EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        });

        let doc = self.doc.as_ref().unwrap();
        match mouse {
            Some(Mouse::Press(at, shift)) => {
                self.sel = if shift { (self.sel.0, at) } else { (at, at) };
                self.want_col = None;
                // Clicking into a conflict makes it the one ⌘1/2/3 act on.
                if let Some(ci) = doc.conflict_at(doc.rope.char_to_line(at)) {
                    self.active = Some(doc.conflicts[ci].start);
                }
            }
            Some(Mouse::Drag(at)) => {
                self.sel.1 = at;
                self.reveal = true;
            }
            Some(Mouse::Word(at)) => self.sel = word_at(&doc.rope, at),
            Some(Mouse::Line(at)) => {
                let line = doc.rope.char_to_line(at);
                let end = if line + 1 < doc.lines() { doc.rope.line_to_char(line + 1) } else { doc.rope.len_chars() };
                self.sel = (doc.rope.line_to_char(line), end);
            }
            None => {}
        }
        if let Some((start, len, off)) = toggle {
            let p = self.picks.entry(start).or_insert(Picks { len, offsets: Vec::new() });
            match p.offsets.binary_search(&off) {
                Ok(i) => _ = p.offsets.remove(i),
                Err(i) => p.offsets.insert(i, off),
            }
            self.active = Some(start);
        }
        if let Some((start, len, a, b, all)) = toggle_side {
            let p = self.picks.entry(start).or_insert(Picks { len, offsets: Vec::new() });
            if all {
                p.offsets.retain(|o| !(a..b).contains(o));
            } else {
                p.offsets.extend(a..b);
                p.offsets.sort_unstable();
                p.offsets.dedup();
            }
            self.active = Some(start);
        }
        if let Some((ci, side)) = action {
            self.resolve(ci, side);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped.or_else(platform::take_opened) {
            // The OS may re-deliver the file we were launched with; don't reset for it.
            if self.doc.as_ref().is_none_or(|d| d.path != path) {
                self.set_doc(&ctx, Doc::load(path));
            }
        }
        self.handle_input(&ctx);
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.find.open && self.doc.is_some() {
            self.refresh_find(&ctx);
            egui::Panel::top("find").show(ui, |ui| self.find_bar(ui));
        }
        // Run the modal panel outside the egui pass that requested it.
        if std::mem::take(&mut self.open_dialog)
            && let Some(path) = platform::open_dialog()
        {
            self.set_doc(&ctx, Doc::load(path));
        }
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0))
            .show(ui, |ui| self.editor(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.saver.flush();
    }
}

// ---- text helpers ----------------------------------------------------------

/// Display column of the char at `chars` in `text` (tabs to 4-column stops).
fn col_of(text: &str, chars: usize) -> usize {
    text.chars().take(chars).fold(0, |col, ch| if ch == '\t' { col + TAB - col % TAB } else { col + 1 })
}

/// Char index in `text` closest to display column `col`.
fn char_at_col(text: &str, col: usize) -> usize {
    let mut c = 0;
    for (i, ch) in text.chars().enumerate() {
        let next = if ch == '\t' { c + TAB - c % TAB } else { c + 1 };
        if col < next {
            return if col - c <= next - col { i } else { i + 1 };
        }
        c = next;
    }
    text.chars().count()
}

fn prev_char(rope: &Rope, at: usize) -> usize {
    match at {
        0 => 0,
        _ if at >= 2 && rope.char(at - 1) == '\n' && rope.char(at - 2) == '\r' => at - 2,
        _ => at - 1,
    }
}

fn next_char(rope: &Rope, at: usize) -> usize {
    let len = rope.len_chars();
    if at >= len {
        len
    } else if rope.char(at) == '\r' && at + 1 < len && rope.char(at + 1) == '\n' {
        at + 2
    } else {
        at + 1
    }
}

fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        2
    } else if c.is_whitespace() {
        0
    } else {
        1
    }
}

/// Start of the word before `at` (skipping whitespace, stopping at line breaks).
fn word_left(rope: &Rope, at: usize) -> usize {
    let mut p = at;
    while p > 0 && class(rope.char(p - 1)) == 0 && rope.char(p - 1) != '\n' {
        p -= 1;
    }
    if p == at && p > 0 && rope.char(p - 1) == '\n' {
        return prev_char(rope, p);
    }
    if p > 0 {
        let k = class(rope.char(p - 1));
        while p > 0 && class(rope.char(p - 1)) == k && k != 0 {
            p -= 1;
        }
    }
    p
}

fn word_right(rope: &Rope, at: usize) -> usize {
    let len = rope.len_chars();
    let mut p = at;
    while p < len && class(rope.char(p)) == 0 && rope.char(p) != '\n' && rope.char(p) != '\r' {
        p += 1;
    }
    if p == at && p < len && matches!(rope.char(p), '\n' | '\r') {
        return next_char(rope, p);
    }
    if p < len {
        let k = class(rope.char(p));
        while p < len && class(rope.char(p)) == k && k != 0 {
            p += 1;
        }
    }
    p
}

/// Selection covering the word (or run of punctuation/space) at `at`.
fn word_at(rope: &Rope, at: usize) -> Selection {
    let len = rope.len_chars();
    if at >= len || rope.char(at) == '\n' {
        return (at, at);
    }
    let k = class(rope.char(at));
    let (mut a, mut b) = (at, at);
    while a > 0 && class(rope.char(a - 1)) == k && rope.char(a - 1) != '\n' {
        a -= 1;
    }
    while b < len && class(rope.char(b)) == k && rope.char(b) != '\n' {
        b += 1;
    }
    (a, b)
}

/// Expand tabs to 4-column stops and truncate very long lines (display only).
fn expand_line(text: &str, out: &mut String) {
    out.clear();
    let mut col = 0;
    for ch in text.chars() {
        if col >= MAX_COLS {
            out.push('…');
            break;
        }
        if ch == '\t' {
            let n = TAB - col % TAB;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
}

enum Check {
    None,
    Some,
    All,
}

fn draw_check(painter: &egui::Painter, cb: Rect, check: Option<Check>, weak: Color32, fill: Color32, mark: Color32) {
    match check {
        Some(Check::None) => {
            painter.rect_stroke(cb, 3.0, Stroke::new(1.0, weak), egui::StrokeKind::Inside);
        }
        Some(Check::Some) => {
            painter.rect_filled(cb, 3.0, fill);
            let y = cb.center().y;
            painter.line_segment([pos2(cb.left() + 3.0, y), pos2(cb.right() - 3.0, y)], Stroke::new(1.8, mark));
        }
        Some(Check::All) => {
            painter.rect_filled(cb, 3.0, fill);
            let pts = vec![
                pos2(cb.left() + 2.5, cb.center().y),
                pos2(cb.left() + 5.0, cb.bottom() - 3.0),
                pos2(cb.right() - 2.5, cb.top() + 3.0),
            ];
            painter.add(egui::Shape::line(pts, Stroke::new(1.8, mark)));
        }
        None => {}
    }
}

/// A shortcut label in the platform's style.
fn keys(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { other }
}

/// egui's default text is quite dim; raise its contrast in both themes.
fn brighten_text(ctx: &egui::Context) {
    for (theme, text, weak, button) in [(egui::Theme::Dark, 220, 155, 235), (egui::Theme::Light, 25, 105, 15)] {
        ctx.style_mut_of(theme, |s| {
            let v = &mut s.visuals;
            v.widgets.noninteractive.fg_stroke.color = Color32::from_gray(text);
            v.widgets.inactive.fg_stroke.color = Color32::from_gray(button);
            v.weak_text_color = Some(Color32::from_gray(weak));
        });
    }
}

fn warn_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(240, 180, 60) } else { Color32::from_rgb(170, 100, 0) }
}

struct Palette {
    current: Color32,
    current_marker: Color32,
    incoming: Color32,
    incoming_marker: Color32,
    base: Color32,
    base_marker: Color32,
    gutter: Color32,
    active: Color32,
    find: Color32,
    button: Color32,
    button_hover: Color32,
    button_stroke: Color32,
}

impl Palette {
    fn new(dark: bool) -> Palette {
        let c = Color32::from_rgb;
        if dark {
            Palette {
                current: c(28, 58, 40),
                current_marker: c(36, 92, 58),
                incoming: c(26, 44, 74),
                incoming_marker: c(34, 66, 118),
                base: c(48, 46, 40),
                base_marker: c(70, 66, 54),
                gutter: c(24, 24, 24),
                active: c(240, 180, 60),
                find: Color32::from_rgba_unmultiplied(230, 170, 40, 80),
                button: c(50, 50, 50),
                button_hover: c(75, 75, 75),
                button_stroke: c(90, 90, 90),
            }
        } else {
            Palette {
                current: c(222, 244, 228),
                current_marker: c(180, 228, 192),
                incoming: c(222, 234, 252),
                incoming_marker: c(182, 206, 246),
                base: c(240, 238, 228),
                base_marker: c(222, 218, 200),
                gutter: c(242, 242, 242),
                active: c(220, 140, 20),
                find: Color32::from_rgba_unmultiplied(255, 200, 0, 110),
                button: c(250, 250, 250),
                button_hover: c(225, 225, 225),
                button_stroke: c(180, 180, 180),
            }
        }
    }

    fn bg(&self, kind: Kind) -> Option<Color32> {
        Some(match kind {
            Kind::Plain => return None,
            Kind::Start => self.current_marker,
            Kind::Current => self.current,
            Kind::BaseMarker => self.base_marker,
            Kind::Base => self.base,
            Kind::Sep => self.base_marker,
            Kind::Incoming => self.incoming,
            Kind::End => self.incoming_marker,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_with_tabs() {
        let t = "\t\tab\tc";
        assert_eq!(col_of(t, 2), 8);
        assert_eq!(col_of(t, 5), 12);
        assert_eq!(char_at_col(t, 8), 2);
        assert_eq!(char_at_col(t, 9), 3);
        assert_eq!(char_at_col(t, 3), 1, "nearest boundary inside a tab");
        assert_eq!(char_at_col(t, 100), 6);
    }

    #[test]
    fn word_motion() {
        let r = Rope::from_str("foo.bar  baz\nqux");
        assert_eq!(word_right(&r, 0), 3);
        assert_eq!(word_right(&r, 3), 4);
        assert_eq!(word_right(&r, 7), 12);
        assert_eq!(word_left(&r, 12), 9);
        assert_eq!(word_left(&r, 13), 12, "line start goes back over the newline");
        assert_eq!(word_at(&r, 5), (4, 7));
    }
}
