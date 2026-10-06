//! Plain-text search over the rope (SIMD substring search, optional ASCII case folding).

use memchr::memmem::Finder;
use ropey::Rope;

/// Bytes gathered from rope chunks before each search pass.
const BUF: usize = 256 * 1024;

/// A compiled query. Without `case`, ASCII letters match either case.
pub struct Needle {
    finder: Finder<'static>,
    case: bool,
}

impl Needle {
    pub fn new(query: &str, case: bool) -> Option<Needle> {
        if query.is_empty() {
            return None;
        }
        let bytes: Vec<u8> = if case { query.as_bytes().to_vec() } else { query.bytes().map(|b| b.to_ascii_lowercase()).collect() };
        Some(Needle { finder: Finder::new(&bytes).into_owned(), case })
    }

    /// Match length in bytes (case folding never changes it).
    pub fn len(&self) -> usize {
        self.finder.needle().len()
    }

    fn fold_into(&self, bytes: &[u8], out: &mut Vec<u8>) {
        if self.case {
            out.extend_from_slice(bytes);
        } else {
            out.extend(bytes.iter().map(u8::to_ascii_lowercase));
        }
    }

    /// Byte offsets of the non-overlapping matches in `text`.
    pub fn find_in(&self, text: &str, buf: &mut Vec<u8>) -> Vec<usize> {
        buf.clear();
        self.fold_into(text.as_bytes(), buf);
        self.finder.find_iter(buf).collect()
    }

    /// Calls `f` with the byte offset of each non-overlapping match starting at
    /// or after byte `from`, in order, until `f` returns false.
    pub fn scan(&self, rope: &Rope, from: usize, mut f: impl FnMut(usize) -> bool) {
        let keep = self.len() - 1;
        let (mut chunks, chunk_start, _, _) = rope.chunks_at_byte(from.min(rope.len_bytes()));
        let mut skip = from - chunk_start;
        let mut buf = Vec::with_capacity(BUF + 4096);
        // Rope offset of buf[0], and the end of the last reported match.
        let (mut base, mut last_end) = (from, from);
        loop {
            let chunk = chunks.next();
            if let Some(c) = chunk {
                self.fold_into(&c.as_bytes()[skip.min(c.len())..], &mut buf);
                skip = 0;
            }
            if buf.len() >= BUF || chunk.is_none() {
                for m in self.finder.find_iter(&buf) {
                    if base + m < last_end {
                        continue;
                    }
                    last_end = base + m + self.len();
                    if !f(base + m) {
                        return;
                    }
                }
                // Keep a tail so matches straddling the boundary are found next pass.
                let cut = buf.len().saturating_sub(keep);
                buf.drain(..cut);
                base += cut;
            }
            if chunk.is_none() {
                return;
            }
        }
    }
}

/// Every match in the document (offsets of at most `cap` are kept) and the total count.
pub fn find_all(rope: &Rope, needle: &Needle, cap: usize, mut cancelled: impl FnMut() -> bool) -> Option<(Vec<usize>, usize)> {
    let (mut starts, mut total, mut stop) = (Vec::new(), 0, false);
    needle.scan(rope, 0, |m| {
        if starts.len() < cap {
            starts.push(m);
        }
        total += 1;
        stop = total % 4096 == 0 && cancelled();
        !stop
    });
    (!stop).then_some((starts, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str, q: &str, case: bool) -> Vec<usize> {
        // Build from many small pieces so the rope has many chunks.
        let mut rope = Rope::new();
        for piece in text.as_bytes().chunks(7) {
            let s = std::str::from_utf8(piece).unwrap();
            rope.insert(rope.len_chars(), s);
        }
        let n = Needle::new(q, case).unwrap();
        let (v, total) = find_all(&rope, &n, usize::MAX, || false).unwrap();
        assert_eq!(v.len(), total);
        v
    }

    #[test]
    fn finds_across_chunks() {
        let text = "abc ".repeat(200_000);
        let v = all(&text, "c ab", true);
        assert_eq!(v.len(), 199_999);
        assert!(v.iter().enumerate().all(|(i, &m)| m == i * 4 + 2));
    }

    #[test]
    fn case_and_overlap() {
        assert_eq!(all("Foo foo FOO", "foo", false), vec![0, 4, 8]);
        assert_eq!(all("Foo foo FOO", "foo", true), vec![4]);
        assert_eq!(all("aaaa", "aa", true), vec![0, 2], "non-overlapping");
        assert_eq!(all("héllo HÉLLO", "héllo", false), vec![0], "only ASCII folds");
    }

    #[test]
    fn scan_from_offset() {
        let rope = Rope::from_str("x.x.x.x");
        let n = Needle::new("x", true).unwrap();
        let mut v = Vec::new();
        n.scan(&rope, 3, |m| {
            v.push(m);
            true
        });
        assert_eq!(v, vec![4, 6]);
        let mut first = None;
        n.scan(&rope, 7, |m| {
            first = Some(m);
            false
        });
        assert_eq!(first, None);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release find::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn search_large() {
        let text = "\t\tA1B2C3D4E5F6 /* Foo.swift in Sources */ = {isa = PBXBuildFile; fileRef = 0A1B2C3D; };\n".repeat(500_000);
        let rope = Rope::from_str(&text);
        for q in ["pbxbuildfile", "zzz", "e"] {
            let n = Needle::new(q, false).unwrap();
            let t = std::time::Instant::now();
            let (_, total) = find_all(&rope, &n, 1_000_000, || false).unwrap();
            println!("{} MB, {q:?}: {total} matches in {:?}", text.len() >> 20, t.elapsed());
        }
    }
}
