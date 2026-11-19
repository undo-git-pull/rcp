//! Results of a content search, grouped by file.

use std::collections::HashMap;
use std::ops::Range;

pub const MAX_HITS: usize = 10_000;

#[derive(Clone, Debug)]
pub struct Hit {
    pub line_no: u64,
    pub line: String,
    pub ranges: Vec<Range<usize>>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub path: String,
    pub hits: Vec<Hit>,
}

pub enum Row<'a> {
    File(&'a str, usize),
    Hit(&'a Hit),
}

/// Matches from parallel searchers arrive interleaved; grouping keeps each
/// file's hits together under one header.
#[derive(Default)]
pub struct Results {
    pub groups: Vec<Group>,
    index: HashMap<String, usize>,
    pub count: usize,
}

impl Results {
    pub fn clear(&mut self) {
        self.groups.clear();
        self.index.clear();
        self.count = 0;
    }

    pub fn push(&mut self, path: String, hit: Hit) {
        let gi = *self.index.entry(path.clone()).or_insert_with(|| {
            self.groups.push(Group { path, hits: vec![] });
            self.groups.len() - 1
        });
        let hits = &mut self.groups[gi].hits;
        // Keep line order within a file even if a batch arrives late.
        let at = hits.partition_point(|h| h.line_no <= hit.line_no);
        hits.insert(at, hit);
        self.count += 1;
    }

    /// The `n`th hit overall.
    pub fn get(&self, mut n: usize) -> Option<(&str, &Hit)> {
        for g in &self.groups {
            if n < g.hits.len() {
                return Some((&g.path, &g.hits[n]));
            }
            n -= g.hits.len();
        }
        None
    }

    /// Display row of the `n`th hit (file headers take a row each).
    pub fn row_of(&self, mut n: usize) -> usize {
        let mut row = 0;
        for g in &self.groups {
            if n < g.hits.len() {
                return row + 1 + n;
            }
            n -= g.hits.len();
            row += 1 + g.hits.len();
        }
        row
    }

    #[cfg(test)]
    pub fn row_count(&self) -> usize {
        self.groups.len() + self.count
    }

    pub fn rows(&self, start: usize, len: usize) -> Vec<Row<'_>> {
        self.groups
            .iter()
            .flat_map(|g| {
                std::iter::once(Row::File(&g.path, g.hits.len())).chain(g.hits.iter().map(Row::Hit))
            })
            .skip(start)
            .take(len)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(n: u64) -> Hit {
        Hit {
            line_no: n,
            line: format!("l{n}"),
            ranges: vec![],
        }
    }

    #[test]
    fn groups_interleaved_hits() {
        let mut r = Results::default();
        r.push("a".into(), hit(5));
        r.push("b".into(), hit(1));
        r.push("a".into(), hit(2));
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.get(0).unwrap().1.line_no, 2);
        assert_eq!(r.get(2).unwrap().0, "b");
        assert_eq!(r.row_of(0), 1);
        assert_eq!(r.row_of(2), 4);
        assert_eq!(r.row_count(), 5);
        assert!(matches!(r.rows(3, 1)[0], Row::File("b", 1)));
    }
}
