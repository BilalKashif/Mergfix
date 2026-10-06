//! Document model: the file is a rope (cheap edits anywhere, O(1) snapshots for
//! the background saver) plus a sorted index of the conflicts in it. Every change,
//! typing and Accept buttons alike, is a text replacement; after each one only
//! the lines around it are re-scanned for conflict markers, so editing stays
//! instant no matter how big the file is.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use ropey::Rope;

const MARKER_LEN: usize = 7;
/// How far around an edit to look for markers it may have completed or broken.
const RESCAN: usize = 2000;

/// Line numbers (0-based) of the marker lines of one conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub start: usize,        // <<<<<<<
    pub base: Option<usize>, // ||||||| (diff3 / zdiff3 style)
    pub sep: usize,          // =======
    pub end: usize,          // >>>>>>>
}

impl Conflict {
    /// Lines `[a, b)` of the current (ours) side.
    pub fn current(&self) -> (usize, usize) {
        (self.start + 1, self.base.unwrap_or(self.sep))
    }
    /// Lines `[a, b)` of the incoming (theirs) side.
    pub fn incoming(&self) -> (usize, usize) {
        (self.sep + 1, self.end)
    }
}

/// What a line is, relative to conflicts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Plain,
    Start,
    Current,
    BaseMarker,
    Base,
    Sep,
    Incoming,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Current,
    Incoming,
    Both,
}

/// Cursor/selection as (anchor, head) char indices.
pub type Selection = (usize, usize);

/// Lines `first..=old_last` were replaced by lines `first..=old_last + delta`.
#[derive(Clone, Copy, Debug)]
pub struct Shift {
    pub first: usize,
    pub old_last: usize,
    pub delta: isize,
}

impl Shift {
    /// Where a line that existed before the edit is now; `None` if it was edited.
    pub fn map(&self, line: usize) -> Option<usize> {
        if line > self.old_last {
            Some((line as isize + self.delta) as usize)
        } else if line >= self.first {
            None
        } else {
            Some(line)
        }
    }
}

#[derive(Clone, Debug)]
struct Op {
    at: usize,
    removed: String,
    inserted: String,
}

/// One undo step: ops in the order they were applied.
struct Group {
    ops: Vec<Op>,
    before: Selection,
    after: Selection,
    typing: bool,
}

fn is_marker(line: &[u8], c: u8, label_ok: bool) -> bool {
    if line.len() < MARKER_LEN || line[..MARKER_LEN].iter().any(|&b| b != c) {
        return false;
    }
    match line.get(MARKER_LEN) {
        None | Some(b'\n') | Some(b'\r') => true,
        Some(b' ') => label_ok,
        _ => false,
    }
}

/// Incremental conflict-marker parser, fed one line at a time.
#[derive(Default)]
struct Parser {
    start: Option<usize>,
    base: Option<usize>,
    sep: Option<usize>,
}

impl Parser {
    fn feed(&mut self, i: usize, line: &[u8]) -> Option<Conflict> {
        match line.first() {
            Some(b'<') if is_marker(line, b'<', true) => {
                // A new start resets any half-parsed (malformed) conflict.
                *self = Parser { start: Some(i), ..Default::default() };
            }
            Some(b'|') if self.start.is_some() && self.sep.is_none() && is_marker(line, b'|', true) => {
                self.base = Some(i);
            }
            Some(b'=') if self.start.is_some() && self.sep.is_none() && is_marker(line, b'=', false) => {
                self.sep = Some(i);
            }
            Some(b'>') if self.sep.is_some() && is_marker(line, b'>', true) => {
                let c = Conflict { start: self.start.unwrap(), base: self.base, sep: self.sep.unwrap(), end: i };
                *self = Parser::default();
                return Some(c);
            }
            _ => {}
        }
        None
    }

    fn idle(&self) -> bool {
        self.start.is_none()
    }
}

pub struct Doc {
    pub path: PathBuf,
    pub rope: Rope,
    pub conflicts: Vec<Conflict>,
    /// Conflicts when the file was opened.
    pub total: usize,
    /// Line ending used for new lines ("\n" or "\r\n"), detected from the file.
    pub newline: &'static str,
    /// The file wasn't valid UTF-8; invalid bytes were replaced.
    pub lossy: bool,
    /// Longest line at load, in bytes (for the initial scroll width).
    pub max_line_len: usize,
    /// Bumped on every change.
    pub version: u64,
    undo: Vec<Group>,
    redo: Vec<Group>,
    shifts: Vec<Shift>,
}

impl Doc {
    /// Stream the file straight into the rope, so it's never held in memory twice.
    pub fn load(path: PathBuf) -> io::Result<Doc> {
        match Rope::from_reader(io::BufReader::with_capacity(1 << 20, fs::File::open(&path)?)) {
            Ok(rope) => Ok(Doc::from_rope(path, rope, false)),
            // Not UTF-8: fall back to replacing the invalid bytes (flagged in the UI).
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let bytes = fs::read(&path)?;
                let rope = Rope::from_str(&String::from_utf8_lossy(&bytes));
                Ok(Doc::from_rope(path, rope, true))
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    pub fn from_bytes(path: PathBuf, bytes: Vec<u8>) -> Doc {
        Doc::from_rope(path, Rope::from_str(std::str::from_utf8(&bytes).unwrap()), false)
    }

    fn from_rope(path: PathBuf, rope: Rope, lossy: bool) -> Doc {
        // One pass over the rope's chunks with a SIMD newline search: conflict markers,
        // longest line, and the file's line ending.
        let mut conflicts = Vec::new();
        let mut parser = Parser::default();
        let (mut line, mut line_len, mut max_line_len) = (0, 0, 0);
        let mut prefix: Vec<u8> = Vec::with_capacity(MARKER_LEN + 1);
        let mut newline = None;
        let mut last = 0u8;
        let mut end_line = |prefix: &mut Vec<u8>, line: &mut usize, line_len: &mut usize| {
            if matches!(prefix.first(), Some(b'<' | b'|' | b'=' | b'>'))
                && let Some(c) = parser.feed(*line, prefix)
            {
                conflicts.push(c);
            }
            max_line_len = max_line_len.max(*line_len);
            prefix.clear();
            *line += 1;
            *line_len = 0;
        };
        for chunk in rope.chunks() {
            let bytes = chunk.as_bytes();
            let mut pos = 0;
            for nl in memchr::memchr_iter(b'\n', bytes).chain([bytes.len()]) {
                let seg = &bytes[pos..nl];
                let take = (MARKER_LEN + 1 - prefix.len()).min(seg.len());
                prefix.extend_from_slice(&seg[..take]);
                line_len += seg.len();
                if let Some(&b) = seg.last() {
                    last = b;
                }
                if nl == bytes.len() {
                    break;
                }
                // `last` is the byte before this newline, even across chunk boundaries.
                newline.get_or_insert(if last == b'\r' { "\r\n" } else { "\n" });
                last = b'\n';
                end_line(&mut prefix, &mut line, &mut line_len);
                pos = nl + 1;
            }
        }
        end_line(&mut prefix, &mut line, &mut line_len);
        Doc {
            path,
            rope,
            total: conflicts.len(),
            conflicts,
            newline: newline.unwrap_or("\n"),
            lossy,
            max_line_len,
            version: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            shifts: Vec::new(),
        }
    }

    pub fn lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Line text without its line ending.
    pub fn line_text(&self, line: usize) -> String {
        let mut s = self.rope.line(line).to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        if s.ends_with('\r') {
            s.pop();
        }
        s
    }

    /// Chars in a line, excluding its line ending.
    pub fn line_len(&self, line: usize) -> usize {
        let l = self.rope.line(line);
        let mut n = l.len_chars();
        if n > 0 && l.char(n - 1) == '\n' {
            n -= 1;
            if n > 0 && l.char(n - 1) == '\r' {
                n -= 1;
            }
        }
        n
    }

    /// Text of lines `[a, b)` including their line endings.
    pub fn lines_str(&self, (a, b): (usize, usize)) -> String {
        self.rope.slice(self.rope.line_to_char(a)..self.rope.line_to_char(b)).to_string()
    }

    /// Index of the conflict containing `line`, if any.
    pub fn conflict_at(&self, line: usize) -> Option<usize> {
        let ci = self.conflicts.partition_point(|c| c.end < line);
        (ci < self.conflicts.len() && self.conflicts[ci].start <= line).then_some(ci)
    }

    pub fn kind(&self, line: usize) -> (Kind, Option<usize>) {
        let Some(ci) = self.conflict_at(line) else { return (Kind::Plain, None) };
        let c = &self.conflicts[ci];
        let kind = if line == c.start {
            Kind::Start
        } else if line == c.sep {
            Kind::Sep
        } else if line == c.end {
            Kind::End
        } else if Some(line) == c.base {
            Kind::BaseMarker
        } else if line > c.sep {
            Kind::Incoming
        } else if c.base.is_some_and(|b| line > b) {
            Kind::Base
        } else {
            Kind::Current
        };
        (kind, Some(ci))
    }

    /// Line-number shifts since the last call (for remapping things keyed by line).
    pub fn take_shifts(&mut self) -> Vec<Shift> {
        std::mem::take(&mut self.shifts)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Replace chars `range` with `text` as one undo step. Consecutive `typing`
    /// edits that continue from the previous cursor merge into a single step.
    pub fn edit(&mut self, range: Range<usize>, text: &str, before: Selection, after: Selection, typing: bool) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        let op = self.apply(range.start, range.len(), text);
        self.redo.clear();
        let breaks = text.contains('\n') || op.removed.contains('\n');
        match self.undo.last_mut() {
            Some(g) if typing && g.typing && !breaks && g.after == before => {
                g.ops.push(op);
                g.after = after;
            }
            _ => self.undo.push(Group { ops: vec![op], before, after, typing: typing && !breaks }),
        }
    }

    /// Undo the last step; returns the selection to restore.
    pub fn undo(&mut self) -> Option<Selection> {
        let g = self.undo.pop()?;
        for op in g.ops.iter().rev() {
            self.apply(op.at, op.inserted.chars().count(), &op.removed);
        }
        let sel = g.before;
        self.redo.push(Group { typing: false, ..g });
        Some(sel)
    }

    pub fn redo(&mut self) -> Option<Selection> {
        let g = self.redo.pop()?;
        for op in &g.ops {
            self.apply(op.at, op.removed.chars().count(), &op.inserted);
        }
        let sel = g.after;
        self.undo.push(g);
        Some(sel)
    }

    /// Resolve conflict `ci` by keeping a side; returns where the result starts.
    pub fn resolve(&mut self, ci: usize, side: Side, sel: Selection) -> usize {
        let c = self.conflicts[ci];
        let text = match side {
            Side::Current => self.lines_str(c.current()),
            Side::Incoming => self.lines_str(c.incoming()),
            Side::Both => self.lines_str(c.current()) + &self.lines_str(c.incoming()),
        };
        self.replace_conflict(c, &text, sel)
    }

    /// Resolve conflict `ci` keeping only `lines` (absolute line numbers, ascending).
    pub fn resolve_picked(&mut self, ci: usize, lines: &[usize], sel: Selection) -> usize {
        let c = self.conflicts[ci];
        let text: String = lines.iter().map(|&l| self.lines_str((l, l + 1))).collect();
        self.replace_conflict(c, &text, sel)
    }

    fn replace_conflict(&mut self, c: Conflict, text: &str, sel: Selection) -> usize {
        let a = self.rope.line_to_char(c.start);
        let b = self.rope.line_to_char(c.end + 1);
        self.edit(a..b, text, sel, (a, a), false);
        a
    }

    /// Apply one replacement and update the conflict index around it.
    fn apply(&mut self, at: usize, remove: usize, insert: &str) -> Op {
        let first = self.rope.char_to_line(at);
        let old_last = self.rope.char_to_line(at + remove);
        let removed = self.rope.slice(at..at + remove).to_string();
        self.rope.remove(at..at + remove);
        self.rope.insert(at, insert);
        let new_last = self.rope.char_to_line(at + insert.chars().count());
        self.reindex(first, old_last, new_last);
        self.version += 1;
        Op { at, removed, inserted: insert.to_owned() }
    }

    /// Lines `first..=old_last` became `first..=new_last`: keep conflicts outside,
    /// shift the ones after, and re-scan the region in between.
    fn reindex(&mut self, first: usize, old_last: usize, new_last: usize) {
        let delta = new_last as isize - old_last as isize;
        self.shifts.push(Shift { first, old_last, delta });
        let shift = |l: usize| (l as isize + delta) as usize;

        let keep = self.conflicts.partition_point(|c| c.end < first);
        let after_i = self.conflicts.partition_point(|c| c.start <= old_last);
        let touched = &self.conflicts[keep..after_i];
        let touched_start = touched.iter().map(|c| c.start).min().unwrap_or(first);
        let touched_end = touched.iter().map(|c| shift(c.end)).max().unwrap_or(new_last);
        let after: Vec<Conflict> = self.conflicts[after_i..]
            .iter()
            .map(|c| Conflict { start: shift(c.start), base: c.base.map(shift), sep: shift(c.sep), end: shift(c.end) })
            .collect();

        // Look back a bit for a start marker the edit may have completed, and forward
        // until the parser is idle again past the edit (never into the next conflict).
        let lo = if keep > 0 { self.conflicts[keep - 1].end + 1 } else { 0 };
        let from = first.min(touched_start).saturating_sub(RESCAN).max(lo);
        let hi = after.first().map_or(self.rope.len_lines(), |c| c.start);
        let limit = (new_last + 1 + RESCAN).max(touched_end + 1).min(hi);

        let mut found = Vec::new();
        let mut parser = Parser::default();
        let mut prefix = String::new();
        for (i, line) in (from..limit).zip(self.rope.lines_at(from)) {
            if i > new_last && parser.idle() {
                break;
            }
            if matches!(line.chars().next(), Some('<' | '|' | '=' | '>')) {
                prefix.clear();
                prefix.extend(line.chars().take(MARKER_LEN + 1));
                if let Some(c) = parser.feed(i, prefix.as_bytes()) {
                    found.push(c);
                }
            }
        }
        self.conflicts.truncate(keep);
        self.conflicts.extend(found);
        self.conflicts.extend(after);
    }
}

/// Atomically replace `path` with `rope`'s text (temp file + rename).
pub fn save(rope: &Rope, path: &Path) -> io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.mergefix.tmp"));
    let result = (|| {
        let f = fs::File::create(&tmp)?;
        if let Ok(meta) = fs::metadata(path) {
            f.set_permissions(meta.permissions())?;
        }
        let mut w = BufWriter::with_capacity(1 << 20, f);
        for chunk in rope.chunks() {
            w.write_all(chunk.as_bytes())?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> br\nb\n<<<<<<< HEAD\nx\n||||||| base\no\n=======\ny\n>>>>>>> br\n";
    const S: Selection = (0, 0);

    fn doc(text: &str) -> Doc {
        Doc::from_bytes(PathBuf::new(), text.as_bytes().to_vec())
    }

    #[test]
    fn parses_conflicts() {
        let d = doc(TWO);
        assert_eq!(d.conflicts.len(), 2);
        assert_eq!(d.conflicts[1], Conflict { start: 7, base: Some(9), sep: 11, end: 13 });
        assert_eq!(d.kind(2), (Kind::Current, Some(0)));
        assert_eq!(d.kind(10), (Kind::Base, Some(1)));
        assert_eq!(d.kind(6), (Kind::Plain, None));
    }

    #[test]
    fn resolutions() {
        for (side, expect) in [
            (Side::Current, "a\nmine\nb\nx\n"),
            (Side::Incoming, "a\ntheirs\nb\ny\n"),
            (Side::Both, "a\nmine\ntheirs\nb\nx\ny\n"),
        ] {
            let mut d = doc(TWO);
            d.resolve(1, side, S);
            d.resolve(0, side, S);
            assert_eq!(d.rope.to_string(), expect);
            assert!(d.conflicts.is_empty());
        }
        let mut d = doc(TWO);
        d.resolve_picked(0, &[2, 4], S);
        assert_eq!(d.conflicts.len(), 1);
        assert_eq!(d.conflicts[0].start, 4, "later conflict shifted up");
        d.resolve_picked(0, &[], S);
        assert_eq!(d.rope.to_string(), "a\nmine\ntheirs\nb\n");
    }

    #[test]
    fn undo_redo() {
        let mut d = doc(TWO);
        d.resolve(0, Side::Both, (5, 5));
        d.resolve(0, Side::Incoming, S);
        assert_eq!(d.undo(), Some(S));
        assert_eq!(d.undo(), Some((5, 5)));
        assert_eq!(d.rope.to_string(), TWO);
        assert_eq!(d.conflicts, doc(TWO).conflicts);
        d.redo();
        d.redo();
        assert_eq!(d.rope.to_string(), "a\nmine\ntheirs\nb\ny\n");
        assert!(!d.can_redo());
    }

    #[test]
    fn typing_keeps_index_in_sync() {
        let mut d = doc(TWO);
        // Type inside conflict 0's current side: still a conflict, the later one shifts.
        let at = d.rope.line_to_char(2);
        d.edit(at..at, "my ", S, S, true);
        d.edit(at + 3..at + 3, "new\n", S, S, true);
        assert_eq!(d.conflicts.len(), 2);
        assert_eq!(d.conflicts[0], Conflict { start: 1, base: None, sep: 4, end: 6 });
        assert_eq!(d.conflicts[1].start, 8);
        // Break the end marker: conflict 0 is gone; fix it: back.
        let end = d.rope.line_to_char(6);
        d.edit(end..end + 1, "", S, S, true);
        assert_eq!(d.conflicts.len(), 1);
        d.edit(end..end, ">", S, S, true);
        assert_eq!(d.conflicts.len(), 2);
        // Delete its three marker lines by hand = resolved.
        for line in [6, 4, 1] {
            let (a, b) = (d.rope.line_to_char(line), d.rope.line_to_char(line + 1));
            d.edit(a..b, "", S, S, false);
        }
        assert_eq!(d.conflicts.len(), 1);
        assert_eq!(d.conflicts[0].start, 5);
        assert!(d.rope.to_string().starts_with("a\nmy new\nmine\ntheirs\nb\n<<<<<<< HEAD\n"));
    }

    #[test]
    fn typed_conflict_is_detected() {
        let mut d = doc("a\nb\n");
        let end = d.rope.len_chars();
        let text = "<<<<<<< mine\nx\n=======\ny\n>>>>>>> theirs\n";
        for (i, ch) in text.chars().enumerate() {
            d.edit(end + i..end + i, &ch.to_string(), S, S, true);
        }
        assert_eq!(d.conflicts, vec![Conflict { start: 2, base: None, sep: 4, end: 6 }]);
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let mut d = doc("<<<<<<< HEAD\r\nm\r\n=======\r\nt\r\n>>>>>>> br");
        assert_eq!(d.newline, "\r\n");
        assert_eq!(d.line_text(1), "m");
        assert_eq!(d.line_len(1), 1);
        d.resolve(0, Side::Incoming, S);
        assert_eq!(d.rope.to_string(), "t\r\n");
    }

    #[test]
    fn ignores_lookalikes_and_malformed() {
        let d = doc("<<<<<<<< not\n=======x\n<<<<<<< HEAD\nonly start\n");
        assert!(d.conflicts.is_empty());
    }

    #[test]
    fn undo_groups_typing() {
        let mut d = doc("ab\n");
        d.edit(1..1, "x", (1, 1), (2, 2), true);
        d.edit(2..2, "y", (2, 2), (3, 3), true);
        d.edit(3..3, "\n", (3, 3), (4, 4), true); // a newline starts a new step
        assert_eq!(d.rope.to_string(), "axy\nb\n");
        d.undo();
        assert_eq!(d.rope.to_string(), "axyb\n");
        d.undo();
        assert_eq!(d.rope.to_string(), "ab\n");
    }

    #[test]
    fn load_streams_and_detects() {
        let dir = std::env::temp_dir().join(format!("mergefix-load-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crlf.txt");
        fs::write(&path, "x\r\n<<<<<<< a\r\n1\r\n=======\r\n2\r\n>>>>>>> b\r\n").unwrap();
        let d = Doc::load(path.clone()).unwrap();
        assert_eq!((d.newline, d.lossy, d.conflicts.len()), ("\r\n", false, 1));
        assert_eq!(d.conflicts[0], Conflict { start: 1, base: None, sep: 3, end: 5 });
        fs::write(&path, b"ok\n\xff\n").unwrap();
        let d = Doc::load(path).unwrap();
        assert!(d.lossy);
        assert_eq!(d.line_text(1), "\u{fffd}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_writes_text() {
        let dir = std::env::temp_dir().join(format!("mergefix-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        fs::write(&path, TWO).unwrap();
        let mut d = Doc::load(path.clone()).unwrap();
        d.resolve(0, Side::Current, S);
        save(&d.rope, &path).unwrap();
        let expect = "a\nmine\nb\n<<<<<<< HEAD\nx\n||||||| base\no\n=======\ny\n>>>>>>> br\n";
        assert_eq!(fs::read_to_string(&path).unwrap(), expect);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// `cargo test --release bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn large_pbxproj_like() {
        let mut s = String::new();
        for i in 0..400_000 {
            s.push_str(&format!("\t\t{i:024X} /* File{i}.swift in Sources */ = {{isa = PBXBuildFile; fileRef = {i:024X}; }};\n"));
            if i % 200 == 0 {
                s.push_str(&format!("<<<<<<< HEAD\n\t\tA{i} /* mine */,\n=======\n\t\tB{i} /* theirs */,\n>>>>>>> feature/x\n"));
            }
        }
        let bytes = s.into_bytes();
        let size = bytes.len();
        let t = Instant::now();
        let mut d = Doc::from_bytes(PathBuf::new(), bytes);
        let load = t.elapsed();
        let n = d.conflicts.len();

        // Type 1000 chars in the middle of the file.
        let mid = d.rope.line_to_char(d.lines() / 2);
        let t = Instant::now();
        for i in 0..1000 {
            d.edit(mid + i..mid + i, "x", (0, 0), (0, 0), true);
        }
        let typing = t.elapsed() / 1000;

        let t = Instant::now();
        for _ in 0..n {
            d.resolve(0, Side::Both, (0, 0));
        }
        let resolve = t.elapsed() / n as u32;

        let t = Instant::now();
        let mut out = Vec::with_capacity(size);
        for chunk in d.rope.chunks() {
            out.extend_from_slice(chunk.as_bytes());
        }
        let write = t.elapsed();
        println!(
            "{:.1} MB, {} lines, {n} conflicts | load {load:?} | keystroke {typing:?} | resolve {resolve:?} each | serialize {write:?}",
            size as f64 / 1e6,
            d.lines(),
        );
    }
}
