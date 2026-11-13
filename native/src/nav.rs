//! Browser-style back / forward between documents. One entry per document switch, each
//! remembering where the reader was (selection and scroll), so going back lands on the same spot.

const MAX: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub struct Visit {
    pub path: String,
    /// Scintilla selection: anchor and caret byte positions.
    pub anchor: usize,
    pub caret: usize,
    /// First visible display line in the editor.
    pub first_line: usize,
    /// Preview scroll offset in CSS pixels.
    pub preview_y: f32,
    /// Heading of the section that was at the top of the view; tells apart two visits of one file.
    pub section: Option<String>,
}

#[derive(Default)]
pub struct History {
    back: Vec<Visit>,
    fwd: Vec<Visit>,
}

impl History {
    /// Leaving `from` for another document by any route other than back / forward.
    pub fn push(&mut self, from: Visit) {
        if self.back.last().is_some_and(|v| v.path == from.path) {
            self.back.pop();
        }
        self.back.push(from);
        if self.back.len() > MAX {
            self.back.remove(0);
        }
        self.fwd.clear();
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_fwd(&self) -> bool {
        !self.fwd.is_empty()
    }

    /// Entries behind (`back`) or ahead, nearest first, for the arrow's drop-down.
    pub fn list(&self, back: bool) -> impl Iterator<Item = &Visit> {
        if back { self.back.iter().rev() } else { self.fwd.iter().rev() }
    }

    /// The drop-down's rows: (steps to go, visit), nearest first, one row per file and
    /// section. A repeat of a place already listed adds nothing a reader could tell apart.
    pub fn menu(&self, back: bool) -> Vec<(usize, &Visit)> {
        let mut out: Vec<(usize, &Visit)> = Vec::new();
        for (i, v) in self.list(back).enumerate() {
            if !out.iter().any(|(_, o)| o.path == v.path && o.section == v.section) {
                out.push((i + 1, v));
            }
        }
        out
    }

    /// Move `steps` entries back (or forward), leaving `here` on the other stack. None when
    /// there are not that many entries.
    pub fn go(&mut self, back: bool, steps: usize, here: Visit) -> Option<Visit> {
        let (from, to) = if back { (&mut self.back, &mut self.fwd) } else { (&mut self.fwd, &mut self.back) };
        if steps == 0 || steps > from.len() {
            return None;
        }
        let mut cur = here;
        for _ in 0..steps {
            to.push(cur);
            cur = from.pop().unwrap();
        }
        Some(cur)
    }

    /// Drop entries for files that no longer exist, and the repeats that leaves behind.
    pub fn retain(&mut self, exists: impl Fn(&str) -> bool) {
        for s in [&mut self.back, &mut self.fwd] {
            s.retain(|v| exists(&v.path));
            s.dedup_by(|a, b| a.path == b.path);
        }
    }

    pub fn rename(&mut self, from: &str, to: &str) {
        for v in self.back.iter_mut().chain(self.fwd.iter_mut()) {
            if v.path == from {
                v.path = to.to_string();
            }
        }
    }

    pub fn clear(&mut self) {
        self.back.clear();
        self.fwd.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(p: &str) -> Visit {
        Visit { path: p.into(), anchor: 0, caret: 0, first_line: 0, preview_y: 0.0, section: None }
    }

    fn at(p: &str, sec: &str) -> Visit {
        Visit { section: Some(sec.into()), ..v(p) }
    }

    #[test]
    fn menu_merges_same_place_keeps_different_sections() {
        let mut h = History::default();
        for x in [at("a", "Intro"), v("b"), at("a", "Setup"), v("b"), at("a", "Intro"), v("c")] {
            h.push(x);
        }
        // Nearest first: c(1) a/Intro(2) b(3) a/Setup(4) b(5, repeat) a/Intro(6, repeat).
        let rows: Vec<(usize, &str, Option<&str>)> = h.menu(true).iter().map(|(n, v)| (*n, v.path.as_str(), v.section.as_deref())).collect();
        assert_eq!(rows, [(1, "c", None), (2, "a", Some("Intro")), (3, "b", None), (4, "a", Some("Setup"))]);
    }

    #[test]
    fn back_and_forward_round_trip() {
        let mut h = History::default();
        h.push(v("a"));
        h.push(v("b")); // now on c
        assert_eq!(h.go(true, 1, v("c")).unwrap().path, "b");
        assert_eq!(h.go(true, 1, v("b")).unwrap().path, "a");
        assert!(h.go(true, 1, v("a")).is_none());
        assert_eq!(h.go(false, 2, v("a")).unwrap().path, "c");
        assert!(!h.can_fwd());
        assert_eq!(h.list(true).map(|v| v.path.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    }

    #[test]
    fn new_visit_clears_forward() {
        let mut h = History::default();
        h.push(v("a"));
        h.go(true, 1, v("b"));
        assert!(h.can_fwd());
        h.push(v("a"));
        assert!(!h.can_fwd());
    }

    #[test]
    fn missing_files_are_skipped() {
        let mut h = History::default();
        for p in ["a", "gone", "a", "b"] {
            h.push(v(p));
        }
        h.retain(|p| p != "gone");
        assert_eq!(h.list(true).map(|v| v.path.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        h.rename("a", "z");
        assert_eq!(h.go(true, 2, v("c")).unwrap().path, "z");
    }
}
