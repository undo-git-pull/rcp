//! fzf-style fuzzy file finder backed by nucleo.

use std::sync::Arc;

use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Matcher, Nucleo, Utf32Str};

pub const MAX_PATHS: u32 = 1_000_000;

pub struct Finder {
    nucleo: Nucleo<String>,
    matcher: Matcher,
    last_query: String,
}

impl Finder {
    pub fn new() -> Finder {
        Finder {
            nucleo: Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            last_query: String::new(),
        }
    }

    /// Clears all items; pushes from older injectors are ignored afterwards.
    pub fn restart(&mut self) -> Injector<String> {
        self.nucleo.restart(true);
        self.nucleo.injector()
    }

    pub fn set_query(&mut self, q: &str) {
        let append = q.starts_with(&self.last_query);
        self.nucleo.pattern.reparse(0, q, CaseMatching::Smart, Normalization::Smart, append);
        self.last_query = q.to_string();
    }

    pub fn tick(&mut self) {
        self.nucleo.tick(10);
    }

    pub fn total(&self) -> u32 {
        self.nucleo.snapshot().item_count()
    }

    pub fn matched(&self) -> u32 {
        self.nucleo.snapshot().matched_item_count()
    }

    pub fn get(&self, idx: u32) -> Option<String> {
        self.nucleo.snapshot().get_matched_item(idx).map(|i| i.data.clone())
    }

    /// Matched paths in `start..end` with the char indices that matched.
    pub fn visible(&mut self, start: u32, end: u32) -> Vec<(String, Vec<u32>)> {
        let snap = self.nucleo.snapshot();
        let end = end.min(snap.matched_item_count());
        if start >= end {
            return vec![];
        }
        let pattern = snap.pattern().column_pattern(0);
        snap.matched_items(start..end)
            .map(|item| {
                let mut idx = Vec::new();
                let hay: Utf32Str = item.matcher_columns[0].slice(..);
                pattern.indices(hay, &mut self.matcher, &mut idx);
                idx.sort_unstable();
                idx.dedup();
                (item.data.clone(), idx)
            })
            .collect()
    }
}

pub fn push(injector: &Injector<String>, path: String) {
    injector.push(path, |s, cols| cols[0] = s.as_str().into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_and_highlights() {
        let mut f = Finder::new();
        let inj = f.restart();
        for p in ["src/main.rs", "docs/maintainers.md", "src/app.rs", "README.md"] {
            push(&inj, p.into());
        }
        f.set_query("srcmain");
        for _ in 0..100 {
            f.tick();
        }
        assert_eq!(f.total(), 4);
        assert_eq!(f.matched(), 1);
        let v = f.visible(0, 10);
        assert_eq!(v[0].0, "src/main.rs");
        assert_eq!(v[0].1, vec![0, 1, 2, 4, 5, 6, 7]);

        f.set_query("");
        for _ in 0..100 {
            f.tick();
        }
        assert_eq!(f.matched(), 4);
    }
}
