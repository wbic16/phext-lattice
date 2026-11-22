//! 6D UML diagram notation and rendering.
//!
//! The uml6d module maps phext coordinates to SDLC semantics (Collection =
//! phase, 5D artifact address invariant across phases). This module adds the
//! missing surface: a *diagram* — a named set of artifacts (nodes) and typed
//! relations (edges), declared in a line-oriented plain-text notation,
//! projected into one phase for drawing, and rendered to SVG or text.
//!
//! Notation (one statement per line; `#` starts a comment):
//!
//! ```text
//! diagram "Payments flow"
//! phase Design
//! word checkout 1.2.3/4.5
//! word ledger 1.2.3/4.6
//! node checkout "Checkout component"
//! node ledger "Ledger component" @ Source
//! edge checkout -> ledger : depends "posts entries"
//! ```
//!
//! - `diagram "<name>"` — optional; defaults to "diagram".
//! - `phase <PhaseName>` — the projection phase (default Design). Every
//!   node's artifact address is phase-invariant; the phase only chooses the
//!   collection the diagram is read in and the lane a node is drawn in when
//!   it carries no explicit `@ <PhaseName>` lane of its own.
//! - `word <name> <addr>` — bind a design word to an artifact address (the
//!   diagram's lexicon). `node`/`edge` may then reference the artifact by
//!   word instead of by raw address. Bindings are collected in a first pass
//!   over the whole text, so a word may be bound after the statements that
//!   use it — a diagram is a declaration set, and its canonical form is
//!   order-free. Binding one word to two different addresses is an error
//!   naming the word and both addresses; rebinding it to the same address
//!   is harmless. Several words may name one address; the lexicographically
//!   first is canonical (it is the one canonical emission uses) and the
//!   rest are aliases — resolvable, never emitted. A reference to an
//!   unbound word is a parse error naming the word and its line. A word
//!   never names another word: bindings are one step, word to address.
//! - `node <addr-or-word> "<label>" [@ <PhaseName>]` — the artifact's 5D
//!   address in ArtifactAddr display form (volume.book.chapter/section.scroll),
//!   or a word bound to that address.
//! - `edge <addr-or-word> -> <addr-or-word> : <relation> ["<label>"]` —
//!   relation is a bare token (depends, traces, refines, tests, documents,
//!   proves, ...).
//!
//! Canonical form: `to_notation` sorts word bindings, nodes, and edges, and
//! writes each reference by its canonical word where one is bound (raw
//! address otherwise), so parse(to_notation(d)) == d and to_notation is a
//! fixed point.

use crate::uml6d::{ArtifactAddr, Phase};

/// A node in a 6D UML diagram: one artifact, drawn in one phase lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DNode {
    pub addr: ArtifactAddr,
    pub label: String,
    /// The phase lane this node is drawn in. The artifact address itself
    /// does not change with the lane — that is the phase-invariance property.
    pub lane: Phase,
}

/// A typed relation between two artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DEdge {
    pub from: ArtifactAddr,
    pub to: ArtifactAddr,
    pub relation: String,
    pub label: String,
}

/// A 6D UML diagram: nodes + edges under one projection phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagram {
    pub name: String,
    pub phase: Phase,
    pub nodes: Vec<DNode>,
    pub edges: Vec<DEdge>,
    /// The diagram's lexicon: design words bound to artifact addresses,
    /// sorted by word. The word is the human surface; the address is the
    /// machine truth underneath.
    pub words: Vec<(String, ArtifactAddr)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}
impl std::error::Error for ParseError {}

fn addr_key(a: &ArtifactAddr) -> (usize, usize, usize, usize, usize) {
    (a.volume, a.book, a.chapter, a.section, a.scroll)
}

fn phase_by_name(name: &str) -> Option<Phase> {
    Phase::all().iter().find(|p| p.name() == name).copied()
}

/// Resolve a reference token — a raw address, or a design word bound in
/// `words` — to an artifact address. A token containing '.' or '/' is
/// always read as an address (and errors as one, so a malformed address is
/// never silently re-read as a word); anything else is a word, and an
/// unbound word is an error naming the word.
fn resolve_ref(tok: &str, words: &[(String, ArtifactAddr)]) -> Result<ArtifactAddr, String> {
    if tok.contains('.') || tok.contains('/') {
        return parse_addr(tok);
    }
    match words.iter().find(|(w, _)| w == tok) {
        Some((_, a)) => Ok(*a),
        None => Err(format!(
            "unknown word '{}': no `word {} <address>` binding in this diagram",
            tok, tok
        )),
    }
}

/// The canonical word for an address in this lexicon: the lexicographically
/// first word bound to it, if any. Aliases resolve but are never emitted.
fn canonical_word(words: &[(String, ArtifactAddr)], addr: &ArtifactAddr) -> Option<String> {
    words
        .iter()
        .filter(|(_, a)| a == addr)
        .map(|(w, _)| w.clone())
        .min()
}

/// Parse a 5D artifact address in display form: volume.book.chapter/section.scroll
pub fn parse_addr(s: &str) -> Result<ArtifactAddr, String> {
    let (left, right) = s
        .split_once('/')
        .ok_or_else(|| format!("address '{}' is missing '/'", s))?;
    let l: Vec<&str> = left.split('.').collect();
    let r: Vec<&str> = right.split('.').collect();
    if l.len() != 3 || r.len() != 2 {
        return Err(format!(
            "address '{}' must be volume.book.chapter/section.scroll",
            s
        ));
    }
    let num = |t: &str| -> Result<usize, String> {
        t.parse::<usize>()
            .map_err(|_| format!("address '{}' has a non-numeric component '{}'", s, t))
    };
    Ok(ArtifactAddr {
        volume: num(l[0])?,
        book: num(l[1])?,
        chapter: num(l[2])?,
        section: num(r[0])?,
        scroll: num(r[1])?,
    })
}

/// Split a statement tail into (bare tokens, optional quoted string).
/// The quoted string, if present, must be the final element.
fn split_quoted(s: &str) -> Result<(Vec<String>, Option<String>), String> {
    let s = s.trim();
    if let Some(start) = s.find('"') {
        let rest = &s[start + 1..];
        let end = rest
            .find('"')
            .ok_or_else(|| "unterminated quoted string".to_string())?;
        let quoted = rest[..end].to_string();
        let after = rest[end + 1..].trim();
        if !after.is_empty() {
            return Err(format!("unexpected text after quoted string: '{}'", after));
        }
        let tokens = s[..start]
            .split_whitespace()
            .map(|t| t.to_string())
            .collect();
        Ok((tokens, Some(quoted)))
    } else {
        Ok((
            s.split_whitespace().map(|t| t.to_string()).collect(),
            None,
        ))
    }
}

/// Parse the tail of a `node` statement: `<addr-or-word> "<label>" [@ <PhaseName>]`.
/// The lane annotation follows the quoted label, so this statement form
/// cannot share the "quoted string comes last" split used by the other
/// statements. `default_lane` is the diagram's current projection phase.
fn parse_node_tail(
    s: &str,
    default_lane: Phase,
    words: &[(String, ArtifactAddr)],
) -> Result<(ArtifactAddr, String, Phase), String> {
    const FORM: &str = "node form: node <addr-or-word> \"<label>\" [@ <PhaseName>]";
    let (addr_tok, rest) = s
        .split_once(char::is_whitespace)
        .ok_or_else(|| FORM.to_string())?;
    let addr = resolve_ref(addr_tok, words)?;
    let rest = rest.trim_start();
    if !rest.starts_with('"') {
        return Err("node requires a quoted label".into());
    }
    let inner = &rest[1..];
    let end = inner
        .find('"')
        .ok_or_else(|| "unterminated quoted string".to_string())?;
    let label = inner[..end].to_string();
    let tail = inner[end + 1..].trim();
    if tail.is_empty() {
        return Ok((addr, label, default_lane));
    }
    let parts: Vec<&str> = tail.split_whitespace().collect();
    if parts.len() == 2 && parts[0] == "@" {
        let lane = phase_by_name(parts[1])
            .ok_or_else(|| format!("unknown phase '{}'", parts[1]))?;
        Ok((addr, label, lane))
    } else {
        Err(FORM.into())
    }
}

/// Parse the diagram notation.
pub fn parse_diagram(text: &str) -> Result<Diagram, ParseError> {
    // First pass: collect `word` bindings from the whole text, so a word
    // may be bound after the statements that use it. Binding one word to
    // two different addresses is an error naming the word and both
    // addresses; rebinding it to the same address is harmless.
    let mut words: Vec<(String, ArtifactAddr)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("word ") {
            let err = |m: String| ParseError {
                line: line_no,
                message: m,
            };
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() != 2 {
                return Err(err("word form: word <name> <address>".into()));
            }
            let (w, atok) = (toks[0], toks[1]);
            if w.contains('.') || w.contains('/') {
                return Err(err(format!(
                    "'{}' cannot be a design word: words contain no '.' or '/' (those make an address)",
                    w
                )));
            }
            if !(atok.contains('.') && atok.contains('/')) {
                return Err(err(format!(
                    "word '{}' must be bound to a raw artifact address, not '{}'",
                    w, atok
                )));
            }
            let addr = parse_addr(atok).map_err(err)?;
            match words.iter().find(|(existing, _)| existing == w) {
                Some((_, existing_addr)) if *existing_addr != addr => {
                    return Err(err(format!(
                        "word '{}' is already bound to {}; cannot rebind to {}",
                        w,
                        existing_addr.display(),
                        addr.display()
                    )));
                }
                Some(_) => {}
                None => words.push((w.to_string(), addr)),
            }
        }
    }
    words.sort_by(|a, b| a.0.cmp(&b.0));

    let mut name = "diagram".to_string();
    let mut phase = Phase::Design;
    let mut nodes: Vec<DNode> = Vec::new();
    let mut edges: Vec<DEdge> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |m: String| ParseError {
            line: line_no,
            message: m,
        };

        if let Some(rest) = line.strip_prefix("diagram ") {
            let (tokens, quoted) = split_quoted(rest).map_err(err)?;
            if !tokens.is_empty() {
                return Err(err("diagram takes only a quoted name".into()));
            }
            name = quoted.ok_or_else(|| err("diagram requires a quoted name".into()))?;
        } else if let Some(rest) = line.strip_prefix("phase ") {
            let p = rest.trim();
            phase = phase_by_name(p)
                .ok_or_else(|| err(format!("unknown phase '{}'", p)))?;
        } else if line.starts_with("word ") {
            // Bindings were collected in the first pass; nothing to do.
            continue;
        } else if let Some(rest) = line.strip_prefix("node ") {
            // node <addr-or-word> "<label>" [@ <PhaseName>]
            let (addr, label, lane) =
                parse_node_tail(rest.trim(), phase, &words).map_err(err)?;
            nodes.push(DNode { addr, label, lane });
        } else if let Some(rest) = line.strip_prefix("edge ") {
            // edge <addr-or-word> -> <addr-or-word> : <relation> ["<label>"]
            let (tokens, quoted) = split_quoted(rest).map_err(err)?;
            if tokens.len() != 5 || tokens[1] != "->" || tokens[3] != ":" {
                return Err(err(
                    "edge form: edge <addr-or-word> -> <addr-or-word> : <relation> [\"<label>\"]".into(),
                ));
            }
            let from = resolve_ref(&tokens[0], &words).map_err(err)?;
            let to = resolve_ref(&tokens[2], &words).map_err(err)?;
            edges.push(DEdge {
                from,
                to,
                relation: tokens[4].clone(),
                label: quoted.unwrap_or_default(),
            });
        } else {
            return Err(err(format!("unknown statement: '{}'", line)));
        }
    }

    Ok(Diagram {
        name,
        phase,
        nodes,
        edges,
        words,
    })
}

impl Diagram {
    /// Nodes in canonical order.
    fn sorted_nodes(&self) -> Vec<&DNode> {
        let mut v: Vec<&DNode> = self.nodes.iter().collect();
        v.sort_by(|a, b| {
            addr_key(&a.addr)
                .cmp(&addr_key(&b.addr))
                .then((a.lane as usize).cmp(&(b.lane as usize)))
                .then(a.label.cmp(&b.label))
        });
        v
    }

    /// Edges in canonical order.
    fn sorted_edges(&self) -> Vec<&DEdge> {
        let mut v: Vec<&DEdge> = self.edges.iter().collect();
        v.sort_by(|a, b| {
            addr_key(&a.from)
                .cmp(&addr_key(&b.from))
                .then(addr_key(&a.to).cmp(&addr_key(&b.to)))
                .then(a.relation.cmp(&b.relation))
                .then(a.label.cmp(&b.label))
        });
        v
    }

    /// Word bindings in canonical order (by word).
    fn sorted_words(&self) -> Vec<&(String, ArtifactAddr)> {
        let mut v: Vec<&(String, ArtifactAddr)> = self.words.iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// How an address is written in canonical notation: by its canonical
    /// word where one is bound in this diagram's lexicon, else by raw
    /// address. Words are the human surface; the address remains the truth
    /// underneath, one lookup away.
    fn reference(&self, addr: &ArtifactAddr) -> String {
        match canonical_word(&self.words, addr) {
            Some(w) => w,
            None => addr.display(),
        }
    }

    /// Emit the canonical notation for this diagram.
    pub fn to_notation(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("diagram \"{}\"\n", self.name));
        out.push_str(&format!("phase {}\n", self.phase.name()));
        for (w, a) in self.sorted_words() {
            out.push_str(&format!("word {} {}\n", w, a.display()));
        }
        for n in self.sorted_nodes() {
            if n.lane == self.phase {
                out.push_str(&format!("node {} \"{}\"\n", self.reference(&n.addr), n.label));
            } else {
                out.push_str(&format!(
                    "node {} \"{}\" @ {}\n",
                    self.reference(&n.addr),
                    n.label,
                    n.lane.name()
                ));
            }
        }
        for e in self.sorted_edges() {
            if e.label.is_empty() {
                out.push_str(&format!(
                    "edge {} -> {} : {}\n",
                    self.reference(&e.from),
                    self.reference(&e.to),
                    e.relation
                ));
            } else {
                out.push_str(&format!(
                    "edge {} -> {} : {} \"{}\"\n",
                    self.reference(&e.from),
                    self.reference(&e.to),
                    e.relation,
                    e.label
                ));
            }
        }
        out
    }

    /// Project the diagram into another phase: node lanes are re-read in the
    /// new phase, but every artifact address is unchanged. This is the
    /// phase-invariance property made operational.
    pub fn project(&self, phase: Phase) -> Diagram {
        Diagram {
            name: self.name.clone(),
            phase,
            nodes: self
                .nodes
                .iter()
                .map(|n| DNode {
                    addr: n.addr,
                    label: n.label.clone(),
                    lane: phase,
                })
                .collect(),
            edges: self.edges.clone(),
            words: self.words.clone(),
        }
    }

    /// Plain-text rendering: lanes grouped in phase order, edges listed.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Diagram: {} (projection phase: {})\n",
            self.name,
            self.phase.name()
        ));
        for phase in Phase::all() {
            let lane_nodes: Vec<&DNode> =
                self.sorted_nodes().into_iter().filter(|n| n.lane == *phase).collect();
            if lane_nodes.is_empty() {
                continue;
            }
            out.push_str(&format!("[{}] {}\n", phase.short(), phase.name()));
            for n in lane_nodes {
                out.push_str(&format!("  {}  {}\n", n.addr.display(), n.label));
            }
        }
        if !self.edges.is_empty() {
            out.push_str("Edges:\n");
            for e in self.sorted_edges() {
                if e.label.is_empty() {
                    out.push_str(&format!(
                        "  {} -{}-> {}\n",
                        e.from.display(),
                        e.relation,
                        e.to.display()
                    ));
                } else {
                    out.push_str(&format!(
                        "  {} -{}-> {}  \"{}\"\n",
                        e.from.display(),
                        e.relation,
                        e.to.display(),
                        e.label
                    ));
                }
            }
        }
        out
    }

    /// SVG rendering. Layout is deterministic: one vertical lane per phase
    /// present among the nodes (in Phase::all() order), nodes stacked in
    /// canonical address order, edges as curved paths under the boxes.
    pub fn to_svg(&self) -> String {
        const LANE_W: usize = 230;
        const BOX_W: usize = 190;
        const BOX_H: usize = 44;
        const STEP: usize = 66;
        const TOP: usize = 46;
        const LEFT: usize = 20;

        // Lanes present, in canonical phase order.
        let mut lanes: Vec<Phase> = Vec::new();
        for p in Phase::all() {
            if self.nodes.iter().any(|n| n.lane == *p) {
                lanes.push(*p);
            }
        }
        if lanes.is_empty() {
            lanes.push(self.phase);
        }
        let lane_index = |p: Phase| lanes.iter().position(|x| *x == p).unwrap_or(0);

        // Position of each node (canonical order within its lane).
        let mut pos: Vec<(usize, usize)> = vec![(0, 0); self.nodes.len()];
        let mut per_lane: std::collections::HashMap<Phase, usize> = Default::default();
        let mut order: Vec<usize> = (0..self.nodes.len()).collect();
        order.sort_by(|&a, &b| {
            addr_key(&self.nodes[a].addr).cmp(&addr_key(&self.nodes[b].addr))
        });
        let mut max_rows = 1usize;
        for &i in &order {
            let n = &self.nodes[i];
            let row = *per_lane.get(&n.lane).unwrap_or(&0);
            per_lane.insert(n.lane, row + 1);
            max_rows = max_rows.max(row + 1);
            pos[i] = (
                LEFT + lane_index(n.lane) * LANE_W,
                TOP + row * STEP,
            );
        }
        let width = LEFT * 2 + lanes.len() * LANE_W - (LANE_W - BOX_W);
        let height = TOP + max_rows * STEP + 16;

        let find = |addr: &ArtifactAddr| -> Option<(usize, usize)> {
            self.nodes.iter().position(|n| &n.addr == addr).map(|i| pos[i])
        };

        let mut s = String::new();
        s.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
            width, height, width, height
        ));
        s.push_str("<defs><marker id=\"arr\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path d=\"M0,0 L10,5 L0,10 z\" fill=\"#666\"/></marker></defs>\n");
        s.push_str(&format!(
            "<text x=\"{}\" y=\"24\" font-family=\"monospace\" font-size=\"15\" fill=\"#222\">{}</text>\n",
            LEFT,
            xml_escape(&self.name)
        ));
        // Lane headers
        for (li, p) in lanes.iter().enumerate() {
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-size=\"12\" fill=\"{}\">[{}] {}</text>\n",
                LEFT + li * LANE_W,
                TOP - 8,
                p.color(),
                p.short(),
                p.name()
            ));
        }
        // Edges under boxes
        for e in self.sorted_edges() {
            if let (Some((x1, y1)), Some((x2, y2))) = (find(&e.from), find(&e.to)) {
                let (sx, sy) = (x1 + BOX_W, y1 + BOX_H / 2);
                let (tx, ty) = (x2, y2 + BOX_H / 2);
                let mx = (sx + tx) / 2;
                s.push_str(&format!(
                    "<path d=\"M{},{} C{},{} {},{} {},{}\" fill=\"none\" stroke=\"#666\" stroke-width=\"1.2\" marker-end=\"url(#arr)\"/>\n",
                    sx, sy, mx, sy, mx, ty, tx, ty
                ));
                let lx = (sx + tx) / 2;
                let ly = (sy + ty) / 2 - 4;
                s.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-size=\"10\" fill=\"#444\" text-anchor=\"middle\">{}</text>\n",
                    lx,
                    ly,
                    xml_escape(&e.relation)
                ));
            }
        }
        // Boxes
        for (i, n) in self.nodes.iter().enumerate() {
            let (x, y) = pos[i];
            s.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"5\" fill=\"{}\" fill-opacity=\"0.22\" stroke=\"{}\" stroke-width=\"1.4\"/>\n",
                x, y, BOX_W, BOX_H,
                n.lane.color(),
                n.lane.color()
            ));
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-size=\"11\" fill=\"#222\">{} {}</text>\n",
                x + 8,
                y + 19,
                n.addr.display(),
                xml_escape(&n.label)
            ));
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-size=\"9\" fill=\"#555\">{} @ {}</text>\n",
                x + 8,
                y + 35,
                n.addr.to_coordinate(n.lane),
                n.lane.name()
            ));
        }
        s.push_str("</svg>\n");
        s
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diagram \"Payments flow\"
phase Design
node 1.2.3/4.5 \"Checkout component\"
node 1.2.3/4.6 \"Ledger component\" @ Source
edge 1.2.3/4.5 -> 1.2.3/4.6 : depends \"posts entries\"
";

    const SAMPLE_WORDS: &str = "\
diagram \"Payments flow\"
phase Design
word checkout 1.2.3/4.5
word ledger 1.2.3/4.6
node checkout \"Checkout component\"
node ledger \"Ledger component\" @ Source
edge checkout -> ledger : depends \"posts entries\"
";

    #[test]
    fn parse_sample() {
        let d = parse_diagram(SAMPLE).unwrap();
        assert_eq!(d.name, "Payments flow");
        assert_eq!(d.phase, Phase::Design);
        assert_eq!(d.nodes.len(), 2);
        assert_eq!(d.nodes[0].lane, Phase::Design);
        assert_eq!(d.nodes[1].lane, Phase::Source);
        assert_eq!(d.edges.len(), 1);
        assert_eq!(d.edges[0].relation, "depends");
        assert_eq!(d.edges[0].label, "posts entries");
    }

    #[test]
    fn notation_round_trip() {
        let d = parse_diagram(SAMPLE).unwrap();
        let text = d.to_notation();
        let d2 = parse_diagram(&text).unwrap();
        assert_eq!(d, d2);
        // Canonical emission is a fixed point.
        assert_eq!(d2.to_notation(), text);
    }

    #[test]
    fn empty_diagram_round_trip() {
        let d = parse_diagram("diagram \"Empty\"\nphase Meta\n").unwrap();
        let d2 = parse_diagram(&d.to_notation()).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn parse_errors_carry_line_numbers() {
        let bad = "diagram \"X\"\nnode not-an-address \"L\"\n";
        let e = parse_diagram(bad).unwrap_err();
        assert_eq!(e.line, 2);
        let bad2 = "phase Hogwarts\n";
        let e2 = parse_diagram(bad2).unwrap_err();
        assert_eq!(e2.line, 1);
        let bad3 = "node 1.2.3/4.5 \"unterminated\n";
        assert!(parse_diagram(bad3).is_err());
    }

    #[test]
    fn phase_invariance_of_addresses() {
        // The core 6D UML property: an artifact's 5D address is the same in
        // every phase; only the collection changes.
        let d = parse_diagram(SAMPLE).unwrap();
        for node in &d.nodes {
            for phase in Phase::all() {
                let c = node.addr.to_coordinate(*phase);
                assert_eq!(c.y.collection, *phase as usize);
                assert_eq!(ArtifactAddr::from_coordinate(&c), node.addr);
            }
        }
        // Projection re-lanes the diagram without touching an address.
        let p = d.project(Phase::Tests);
        assert_eq!(p.phase, Phase::Tests);
        for (a, b) in d.nodes.iter().zip(p.nodes.iter()) {
            assert_eq!(a.addr, b.addr);
            assert_eq!(b.lane, Phase::Tests);
        }
        assert_eq!(d.edges, p.edges);
    }

    #[test]
    fn svg_render_is_deterministic_and_complete() {
        let d = parse_diagram(SAMPLE).unwrap();
        let svg = d.to_svg();
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("Checkout component"));
        assert!(svg.contains("Ledger component"));
        assert!(svg.contains("depends"));
        assert_eq!(svg, d.to_svg(), "rendering must be deterministic");
    }

    #[test]
    fn text_render_groups_by_lane() {
        let d = parse_diagram(SAMPLE).unwrap();
        let t = d.to_text();
        assert!(t.contains("[DES] Design"));
        assert!(t.contains("[SRC] Source"));
        assert!(t.contains("-depends->"));
    }

    #[test]
    fn addr_parse_matches_display() {
        let a = ArtifactAddr {
            volume: 12,
            book: 3,
            chapter: 4,
            section: 5,
            scroll: 678,
        };
        assert_eq!(parse_addr(&a.display()).unwrap(), a);
        assert!(parse_addr("1.2/3.4").is_err());
        assert!(parse_addr("1.2.3.4/5.6").is_err());
    }

    #[test]
    fn word_bindings_resolve() {
        let d = parse_diagram(SAMPLE_WORDS).unwrap();
        let plain = parse_diagram(SAMPLE).unwrap();
        assert_eq!(d.words.len(), 2);
        assert_eq!(d.words[0].0, "checkout");
        // Word references land on exactly the addresses they name.
        assert_eq!(d.nodes, plain.nodes);
        assert_eq!(d.edges, plain.edges);
        // Bindings are collected in a first pass: a word may be bound
        // after the statements that use it.
        let late = "node checkout \"Checkout component\"\nword checkout 1.2.3/4.5\n";
        let d2 = parse_diagram(late).unwrap();
        assert_eq!(d2.nodes[0].addr, plain.nodes[0].addr);
    }

    #[test]
    fn word_and_address_forms_render_identically() {
        let by_word = parse_diagram(SAMPLE_WORDS).unwrap();
        let by_addr = parse_diagram(SAMPLE).unwrap();
        assert_eq!(by_word.nodes, by_addr.nodes);
        assert_eq!(by_word.edges, by_addr.edges);
        assert_eq!(by_word.to_svg(), by_addr.to_svg());
        assert_eq!(by_word.to_text(), by_addr.to_text());
    }

    #[test]
    fn unknown_word_is_named_with_its_line() {
        let bad = "diagram \"X\"\nnode ghost \"G\"\n";
        let e = parse_diagram(bad).unwrap_err();
        assert_eq!(e.line, 2);
        assert!(
            e.message.contains("ghost"),
            "error must name the word: {}",
            e.message
        );
        let bad_edge = "word a 1.1.1/1.1\nedge a -> missing : depends\n";
        let e2 = parse_diagram(bad_edge).unwrap_err();
        assert_eq!(e2.line, 2);
        assert!(e2.message.contains("missing"));
    }

    #[test]
    fn conflicting_binding_is_an_error_same_address_is_harmless() {
        let conflict = "word a 1.1.1/1.1\nword a 1.2.1/1.1\n";
        let e = parse_diagram(conflict).unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("'a'"), "error must name the word: {}", e.message);
        assert!(e.message.contains("1.1.1/1.1"));
        assert!(e.message.contains("1.2.1/1.1"));
        let harmless = "word a 1.1.1/1.1\nword a 1.1.1/1.1\nnode a \"A\"\n";
        assert!(parse_diagram(harmless).is_ok());
    }

    #[test]
    fn word_round_trip_is_a_fixed_point() {
        let d = parse_diagram(SAMPLE_WORDS).unwrap();
        let text = d.to_notation();
        // Canonical emission keeps the lexicon and speaks words.
        assert!(text.contains("word checkout 1.2.3/4.5\n"));
        assert!(text.contains("node checkout \"Checkout component\"\n"));
        assert!(text.contains("edge checkout -> ledger : depends \"posts entries\"\n"));
        let d2 = parse_diagram(&text).unwrap();
        assert_eq!(d, d2);
        assert_eq!(d2.to_notation(), text);
    }

    #[test]
    fn phase_invariance_holds_with_words() {
        let d = parse_diagram(SAMPLE_WORDS).unwrap();
        let p = d.project(Phase::Proofs);
        // Projection carries the lexicon and touches no address.
        assert_eq!(p.words, d.words);
        for (a, b) in d.nodes.iter().zip(p.nodes.iter()) {
            assert_eq!(a.addr, b.addr);
            assert_eq!(b.lane, Phase::Proofs);
        }
        assert_eq!(d.edges, p.edges);
    }
}
