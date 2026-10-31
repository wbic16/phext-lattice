/// Term Index — ranked (BM25) full-text search over lattice scrolls.
///
/// `search.rs` answers "where does this exact text occur?" by scanning every byte.
/// The term index answers "which scrolls are most about these words?" from an
/// inverted index built once, so a query touches only the postings of its terms.
///
/// On wikibench (3,820 Wikipedia articles, 4 MB) a BM25 index ranks the right article
/// first for 34% of vague 3-word queries versus 17% for grep-style ranking, reads 4×
/// fewer bytes to reach it, and answers in ~100 µs instead of ~24 ms.
///
/// Documents are scrolls by default. `build_grouped` folds several scrolls into one
/// document (e.g. a title scroll and a body scroll that share a section), keyed by
/// whatever coordinate the grouping function returns.

use std::collections::HashMap;
use libphext::phext::Coordinate;
use rayon::prelude::*;
use crate::mmap::MappedLattice;

/// BM25 tuning. The defaults are the usual Robertson/Lucene values.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    /// Term-frequency saturation.
    pub k1: f64,
    /// Length normalization (0 = none, 1 = full).
    pub b: f64,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75 }
    }
}

/// A ranked hit: the document coordinate and its BM25 score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TermHit {
    pub coordinate: Coordinate,
    pub score: f64,
}

#[derive(Debug, Clone, Copy)]
struct Posting {
    doc: u32,
    tf: u32,
}

/// Inverted index over lattice documents with BM25 ranking.
#[derive(Debug, Clone)]
pub struct TermIndex {
    vocab: HashMap<String, u32>,
    /// Postings per term id, sorted by document id.
    postings: Vec<Vec<Posting>>,
    /// Document id → coordinate.
    docs: Vec<Coordinate>,
    /// Document coordinate → current document id.
    doc_ids: HashMap<Coordinate, u32>,
    /// Token count per document; 0 for removed documents.
    doc_len: Vec<u32>,
    live: Vec<bool>,
    total_len: u64,
    live_docs: usize,
    params: Bm25Params,
}

/// Split text into index terms: lowercased runs of letters/digits, at least 2 chars.
pub fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().nth(1).is_some())
        .map(|t| t.to_lowercase())
}

fn term_frequencies(text: &str) -> (HashMap<String, u32>, u32) {
    let mut tf: HashMap<String, u32> = HashMap::new();
    let mut len = 0;
    for t in tokenize(text) {
        *tf.entry(t).or_default() += 1;
        len += 1;
    }
    (tf, len)
}

impl Default for TermIndex {
    fn default() -> Self {
        TermIndex::new(Bm25Params::default())
    }
}

impl TermIndex {
    /// An empty index.
    pub fn new(params: Bm25Params) -> TermIndex {
        TermIndex {
            vocab: HashMap::new(),
            postings: Vec::new(),
            docs: Vec::new(),
            doc_ids: HashMap::new(),
            doc_len: Vec::new(),
            live: Vec::new(),
            total_len: 0,
            live_docs: 0,
            params,
        }
    }

    /// Index every populated scroll as its own document (overlay edits included).
    pub fn build(lattice: &MappedLattice) -> TermIndex {
        Self::build_grouped(lattice, |c| Some(*c))
    }

    /// Index scrolls grouped into documents: `group` maps a scroll coordinate to the
    /// coordinate of the document it belongs to, or `None` to leave it out.
    /// Tokenization runs in parallel; documents keep document order.
    pub fn build_grouped<F>(lattice: &MappedLattice, group: F) -> TermIndex
    where
        F: Fn(&Coordinate) -> Option<Coordinate> + Sync,
    {
        let mut order: Vec<Coordinate> = Vec::new();
        let mut members: HashMap<Coordinate, Vec<Coordinate>> = HashMap::new();
        for c in lattice.populated_coordinates() {
            if let Some(doc) = group(&c) {
                members.entry(doc).or_insert_with(|| {
                    order.push(doc);
                    Vec::new()
                }).push(c);
            }
        }

        let analyzed: Vec<(HashMap<String, u32>, u32)> = order
            .par_iter()
            .map(|doc| {
                let mut tf: HashMap<String, u32> = HashMap::new();
                let mut len = 0;
                for c in &members[doc] {
                    if let Some(text) = lattice.read_scroll(c) {
                        let (t, l) = term_frequencies(&text);
                        for (k, v) in t {
                            *tf.entry(k).or_default() += v;
                        }
                        len += l;
                    }
                }
                (tf, len)
            })
            .collect();

        let mut index = TermIndex::default();
        for (doc, (tf, len)) in order.into_iter().zip(analyzed) {
            index.push_document(doc, tf, len);
        }
        index
    }

    fn push_document(&mut self, coord: Coordinate, tf: HashMap<String, u32>, len: u32) {
        let id = self.docs.len() as u32;
        self.docs.push(coord);
        self.doc_len.push(len);
        self.live.push(true);
        self.doc_ids.insert(coord, id);
        self.total_len += len as u64;
        self.live_docs += 1;
        for (term, count) in tf {
            let next = self.postings.len() as u32;
            let tid = *self.vocab.entry(term).or_insert(next);
            if tid == next {
                self.postings.push(Vec::new());
            }
            self.postings[tid as usize].push(Posting { doc: id, tf: count });
        }
    }

    /// Add or replace the document at `coord` with `text`.
    ///
    /// Replacement retires the old document id and appends a new one, so postings
    /// stay sorted without rewriting them. Retired postings still occupy memory and
    /// still count toward document frequency until the index is rebuilt; check
    /// `retired()` to decide when.
    pub fn upsert(&mut self, coord: Coordinate, text: &str) {
        self.remove(&coord);
        let (tf, len) = term_frequencies(text);
        self.push_document(coord, tf, len);
    }

    /// Remove the document at `coord`. Returns whether it was indexed.
    pub fn remove(&mut self, coord: &Coordinate) -> bool {
        match self.doc_ids.remove(coord) {
            Some(id) => {
                let id = id as usize;
                self.live[id] = false;
                self.total_len -= self.doc_len[id] as u64;
                self.doc_len[id] = 0;
                self.live_docs -= 1;
                true
            }
            None => false,
        }
    }

    /// Rank documents against a free-text query (terms are OR'ed). Ties break in
    /// document order, so results are deterministic.
    pub fn search(&self, query: &str, limit: usize) -> Vec<TermHit> {
        if self.live_docs == 0 || limit == 0 {
            return Vec::new();
        }
        let n = self.live_docs as f64;
        let avg = (self.total_len as f64 / n).max(1.0);
        let Bm25Params { k1, b } = self.params;

        let mut terms: Vec<u32> = tokenize(query).filter_map(|t| self.vocab.get(&t).copied()).collect();
        terms.sort_unstable();
        terms.dedup();

        let mut scores: HashMap<u32, f64> = HashMap::new();
        for tid in terms {
            let list = &self.postings[tid as usize];
            let df = list.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for p in list {
                if !self.live[p.doc as usize] {
                    continue;
                }
                let tf = p.tf as f64;
                let norm = 1.0 - b + b * self.doc_len[p.doc as usize] as f64 / avg;
                *scores.entry(p.doc).or_default() += idf * tf * (k1 + 1.0) / (tf + k1 * norm);
            }
        }

        let mut ranked: Vec<(u32, f64)> = scores.into_iter().collect();
        let by_score = |a: &(u32, f64), b: &(u32, f64)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        if ranked.len() > limit {
            ranked.select_nth_unstable_by(limit - 1, by_score);
            ranked.truncate(limit);
        }
        ranked.sort_by(by_score);
        ranked.into_iter()
            .map(|(d, score)| TermHit { coordinate: self.docs[d as usize], score })
            .collect()
    }

    /// Number of live documents.
    pub fn doc_count(&self) -> usize {
        self.live_docs
    }

    /// Number of distinct terms.
    pub fn term_count(&self) -> usize {
        self.vocab.len()
    }

    /// Documents replaced or removed since the last build.
    pub fn retired(&self) -> usize {
        self.docs.len() - self.live_docs
    }

    /// How many documents (including retired ones) contain `term`.
    pub fn document_frequency(&self, term: &str) -> usize {
        self.vocab.get(&term.to_lowercase()).map(|&t| self.postings[t as usize].len()).unwrap_or(0)
    }

    /// Approximate heap footprint in bytes.
    pub fn heap_bytes(&self) -> usize {
        let vocab: usize = self.vocab.keys().map(|k| k.capacity() + 56).sum();
        let postings: usize = self.postings.iter().map(|p| p.capacity() * 8 + 24).sum();
        let docs = self.docs.capacity() * std::mem::size_of::<Coordinate>()
            + self.doc_ids.capacity() * (std::mem::size_of::<Coordinate>() + 8)
            + self.doc_len.capacity() * 4
            + self.live.capacity();
        vocab + postings + docs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    fn lattice(scrolls: &[&str]) -> MappedLattice {
        MappedLattice::from_bytes(scrolls.join("\x17").as_bytes())
    }

    fn c(s: &str) -> Coordinate {
        to_coordinate(s)
    }

    #[test]
    fn tokenize_lowercases_and_drops_short_tokens() {
        let t: Vec<String> = tokenize("A Ferrel-cell İzmir, x 42!").collect();
        assert_eq!(t, vec!["ferrel", "cell", "i̇zmir", "42"]);
    }

    #[test]
    fn ranks_the_most_specific_scroll_first() {
        let lat = lattice(&[
            "the glacier melts and the sea rises",
            "thwaites glacier instability could raise the sea by meters",
            "the sea is wide",
        ]);
        let idx = TermIndex::build(&lat);
        assert_eq!(idx.doc_count(), 3);
        let hits = idx.search("glacier instability", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].coordinate, c("1.1.1/1.1.1/1.1.2"));
        assert!(hits[0].score > hits[1].score);
        assert!(idx.search("nothing here", 10).is_empty());
    }

    #[test]
    fn limit_keeps_the_best_hits_in_order() {
        let docs: Vec<String> = (1..=20).map(|i| "ice ".repeat(i) + "filler words here").collect();
        let refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
        let idx = TermIndex::build(&lattice(&refs));
        let all = idx.search("ice", 20);
        let top = idx.search("ice", 3);
        assert_eq!(top, all[..3].to_vec());
    }

    #[test]
    fn grouped_documents_combine_scrolls() {
        // two records (sections), each with a title scroll and a body scroll
        let lat = MappedLattice::from_bytes("Coal\x17burned for power\x18Wind\x17turbines make power".as_bytes());
        let idx = TermIndex::build_grouped(&lat, |s| {
            let mut doc = *s;
            doc.x.scroll = 1;
            Some(doc)
        });
        assert_eq!(idx.doc_count(), 2);
        let hits = idx.search("wind power", 10);
        assert_eq!(hits[0].coordinate, c("1.1.1/1.1.1/1.2.1"));
        assert_eq!(idx.document_frequency("POWER"), 2);
    }

    #[test]
    fn upsert_and_remove_update_results() {
        let lat = lattice(&["alpha beta", "gamma delta"]);
        let mut idx = TermIndex::build(&lat);
        idx.upsert(c("1.1.1/1.1.1/1.1.1"), "epsilon only");
        assert!(idx.search("alpha", 10).is_empty());
        assert_eq!(idx.search("epsilon", 10)[0].coordinate, c("1.1.1/1.1.1/1.1.1"));
        assert_eq!(idx.retired(), 1);
        assert!(idx.remove(&c("1.1.1/1.1.1/1.1.2")));
        assert!(!idx.remove(&c("1.1.1/1.1.1/1.1.2")));
        assert!(idx.search("gamma", 10).is_empty());
        assert_eq!(idx.doc_count(), 1);
        idx.upsert(c("1.1.1/1.1.1/1.1.9"), "brand new scroll");
        assert_eq!(idx.search("brand", 1)[0].coordinate, c("1.1.1/1.1.1/1.1.9"));
    }

    #[test]
    fn overlay_edits_are_indexed() {
        let mut lat = lattice(&["old text"]);
        lat.write_scroll(c("1.1.1/1.1.1/1.1.1"), "fresh words".to_string());
        let idx = TermIndex::build(&lat);
        assert!(idx.search("old", 10).is_empty());
        assert_eq!(idx.search("fresh", 10).len(), 1);
    }
}
