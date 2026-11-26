//! A SPARQL evaluator for the subset one stack of tools actually speaks,
//! over any quad source.
//!
//! The subset is not a guess: it is the construct set of every query the
//! ravel stack issues (daemon internals, its own CLI as an outside issuer,
//! and the dialect its `query help` documents), enumerated in
//! `SPARQL-SUBSET.md` (task 34.6.8/7.7.11/9.5.8). Constructs proven by an
//! issued query are CORE. A small FRINGE of adjacent constructs is
//! implemented on prediction — each marked FRINGE where it is defined —
//! headed by CONSTRUCT and DESCRIBE, which ravel's routes advertise but no
//! code ever issued. Everything else is refused at parse time, by name:
//! an out-of-subset construct fails at the construct, with the construct
//! named in the error, never half-parsed and never silently approximated.
//!
//! The evaluator knows nothing about any project's ontology or storage.
//! It reads quads through [`QuadSource`]; the two implementations shipped
//! here are an in-memory vector and an N-Quads reader. A caller whose
//! names live under more than one spelling (ravel's `subtexture:` base
//! names vs its stored `ravel:` names, rewritten by calamus in the
//! original) passes those maps in as data ([`Prep`]): equivalent
//! properties and class subsumption apply in pattern positions, names
//! inside expressions are compared as written and reported back
//! ([`Outcome::left_as_written`]), and bindings to abstract names are
//! dropped from answers — the original pipeline's three behaviours,
//! without the substrate learning any project's vocabulary.
//!
//! Dataset rule (the original's, named-graph spec section 12): a query
//! that names no dataset sees the default graph as the union of every
//! graph in the source, the source's own default graph included; a
//! caller-written FROM / FROM NAMED is kept as written.

use std::collections::{HashMap, HashSet};
use std::fmt;

pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
pub const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
pub const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
pub const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
pub const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";

/// An RDF 1.2 term: an IRI, a literal, or a triple term.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Term {
    Iri(String),
    Literal(Literal),
    Triple(Box<Triple>),
}

/// A literal. `datatype: None` is a simple literal (RDF 1.2: the same
/// term as `^^xsd:string`, and normalized to it here).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Literal {
    pub value: String,
    pub datatype: Option<String>,
    pub lang: Option<String>,
}

impl Literal {
    pub fn simple(value: &str) -> Self {
        Literal { value: value.to_string(), datatype: None, lang: None }
    }
    pub fn typed(value: &str, datatype: &str) -> Self {
        let dt = if datatype == XSD_STRING { None } else { Some(datatype.to_string()) };
        Literal { value: value.to_string(), datatype: dt, lang: None }
    }
    pub fn datatype_str(&self) -> &str {
        if self.lang.is_some() {
            return "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
        }
        self.datatype.as_deref().unwrap_or(XSD_STRING)
    }
    pub fn is_numeric(&self) -> bool {
        matches!(self.datatype.as_deref(), Some(XSD_INTEGER) | Some(XSD_DECIMAL))
    }
}

/// A triple (term): subject, predicate, object. Also the shape of one
/// asserted statement when a graph name rides beside it ([`Quad`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Triple {
    pub s: Term,
    pub p: Term,
    pub o: Term,
}

/// One statement in a source: a triple plus its graph (`None` = the
/// source's own default graph).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Quad {
    pub graph: Option<String>,
    pub s: Term,
    pub p: Term,
    pub o: Term,
}

impl Term {
    pub fn iri(s: &str) -> Term {
        Term::Iri(s.to_string())
    }
    /// The N-Triples form — also the display form the stack's own tools
    /// print (oxigraph's `Term::to_string` shape).
    pub fn to_nt(&self) -> String {
        match self {
            Term::Iri(i) => format!("<{i}>"),
            Term::Literal(l) => {
                let mut out = format!("\"{}\"", escape_nt(&l.value));
                if let Some(lang) = &l.lang {
                    out.push('@');
                    out.push_str(lang);
                } else if let Some(dt) = &l.datatype {
                    out.push_str(&format!("^^<{dt}>"));
                }
                out
            }
            Term::Triple(t) => format!("<<( {} {} {} )>>", t.s.to_nt(), t.p.to_nt(), t.o.to_nt()),
        }
    }
    /// The plain value: an IRI's string, a literal's lexical form. What
    /// row-shaping callers (the original's `query_rows`) hand on.
    pub fn value(&self) -> String {
        match self {
            Term::Iri(i) => i.clone(),
            Term::Literal(l) => l.value.clone(),
            Term::Triple(_) => self.to_nt(),
        }
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_nt())
    }
}

fn escape_nt(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// SPARQL's ordering of terms, as far as this subset needs it: IRIs
/// before literals before triple terms; numbers by value, dateTimes by
/// instant, other literals lexically by (value, datatype, language).
pub fn term_cmp(a: &Term, b: &Term) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let rank = |t: &Term| match t {
        Term::Iri(_) => 1,
        Term::Literal(_) => 2,
        Term::Triple(_) => 3,
    };
    match rank(a).cmp(&rank(b)) {
        Equal => {}
        o => return o,
    }
    match (a, b) {
        (Term::Iri(x), Term::Iri(y)) => x.cmp(y),
        (Term::Literal(x), Term::Literal(y)) => literal_cmp(x, y),
        (Term::Triple(x), Term::Triple(y)) => term_cmp(&x.s, &y.s)
            .then_with(|| term_cmp(&x.p, &y.p))
            .then_with(|| term_cmp(&x.o, &y.o)),
        _ => Equal,
    }
}

fn literal_cmp(a: &Literal, b: &Literal) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    if a.is_numeric() && b.is_numeric() {
        if let (Ok(x), Ok(y)) = (a.value.parse::<i64>(), b.value.parse::<i64>()) {
            return x.cmp(&y);
        }
        if let (Ok(x), Ok(y)) = (a.value.parse::<f64>(), b.value.parse::<f64>()) {
            return x.partial_cmp(&y).unwrap_or(Equal);
        }
    }
    if a.datatype.as_deref() == Some(XSD_DATETIME) && b.datatype.as_deref() == Some(XSD_DATETIME) {
        if let (Some(x), Some(y)) = (parse_instant(&a.value), parse_instant(&b.value)) {
            return x.cmp(&y);
        }
    }
    a.value
        .cmp(&b.value)
        .then_with(|| a.datatype_str().cmp(b.datatype_str()))
        .then_with(|| a.lang.cmp(&b.lang))
}

/// An RFC 3339 time as epoch milliseconds, for ordering and for the
/// stack's own newest-first sorts. `None` when the form is not read.
pub fn parse_instant(ts: &str) -> Option<i64> {
    let (date, time) = ts.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    let (hms, off) = if let Some(i) = time.find(['Z', 'z']) {
        (&time[..i], 0i64)
    } else if let Some(i) = time.rfind(['+', '-']) {
        if i == 0 {
            return None;
        }
        let sign = if time.as_bytes()[i] == b'+' { 1 } else { -1 };
        let mut op = time[i + 1..].split(':');
        let oh: i64 = op.next()?.parse().ok()?;
        let om: i64 = op.next().unwrap_or("0").parse().ok()?;
        (&time[..i], sign * (oh * 3600 + om * 60))
    } else {
        (time, 0)
    };
    let mut tp = hms.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: f64 = tp.next().unwrap_or("0").parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 - off) * 1000) + (sec * 1000.0) as i64)
}

/// Where an evaluator reads its quads. The seam of this module: an
/// implementation walks whatever it walks (scrolls, a file, memory) and
/// yields statements; the evaluator never learns which.
pub trait QuadSource {
    /// The named graphs in the source (the default graph is not listed).
    fn graph_names(&self) -> Vec<String>;
    /// Every quad in one graph; `None` names the default graph.
    fn quads(&self, graph: Option<&str>) -> Vec<Quad>;
}

/// An in-memory [`QuadSource`]: the test fixture and the small-store
/// implementation.
#[derive(Debug, Default, Clone)]
pub struct VecSource {
    pub quads: Vec<Quad>,
}

impl VecSource {
    pub fn new() -> Self {
        VecSource { quads: Vec::new() }
    }
    pub fn push(&mut self, graph: Option<&str>, s: Term, p: Term, o: Term) {
        self.quads.push(Quad { graph: graph.map(str::to_string), s, p, o });
    }
    /// Read N-Quads / N-Triples text (a quad per line; triple terms and
    /// typed literals included). Errors name the line.
    pub fn from_nquads(text: &str) -> Result<Self, String> {
        let mut src = VecSource::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let q = parse_nt_line(line).map_err(|e| format!("line {}: {e}", i + 1))?;
            src.quads.push(q);
        }
        Ok(src)
    }
    /// Parse and append ONE N-Triples / N-Quads line (the per-line seam
    /// of [`VecSource::from_nquads`]). Blank lines and comments are an
    /// error here — a caller feeding lines one at a time means each one
    /// to count. The internal representation is a plain `Vec<Quad>`
    /// with no precomputed indexes, so appending is always sound.
    pub fn push_nt_line(&mut self, line: &str) -> Result<(), String> {
        let q = parse_nt_line(line.trim())?;
        self.quads.push(q);
        Ok(())
    }
    /// Remove every quad equal to the one a single N-Triples / N-Quads
    /// line parses to; returns whether anything was removed. Sound for
    /// the same reason as [`VecSource::push_nt_line`]: no indexes to
    /// keep in step, `quads()` filters the Vec on every call.
    pub fn remove_nt_line(&mut self, line: &str) -> Result<bool, String> {
        let q = parse_nt_line(line.trim())?;
        let before = self.quads.len();
        self.quads.retain(|x| x != &q);
        Ok(self.quads.len() != before)
    }
}

/// The per-line parser both bulk and line-at-a-time ingestion share.
fn parse_nt_line(line: &str) -> Result<Quad, String> {
    if line.is_empty() || line.starts_with('#') {
        return Err("not a quad line".into());
    }
    parse_nquad_line(line)
}

impl QuadSource for VecSource {
    fn graph_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .quads
            .iter()
            .filter_map(|q| q.graph.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        names
    }
    fn quads(&self, graph: Option<&str>) -> Vec<Quad> {
        self.quads
            .iter()
            .filter(|q| q.graph.as_deref() == graph)
            .cloned()
            .collect()
    }
}

fn parse_nquad_line(line: &str) -> Result<Quad, String> {
    let mut cur = NtCursor { chars: line.chars().collect(), at: 0 };
    let s = cur.term()?;
    let p = cur.term()?;
    let o = cur.term()?;
    let graph = if cur.peek() == Some('.') {
        None
    } else {
        match cur.term()? {
            Term::Iri(g) => Some(g),
            _ => return Err("graph name is not an IRI".into()),
        }
    };
    cur.expect('.')?;
    Ok(Quad { graph, s, p, o })
}

struct NtCursor {
    chars: Vec<char>,
    at: usize,
}

impl NtCursor {
    fn peek(&mut self) -> Option<char> {
        while self.chars.get(self.at) == Some(&' ') || self.chars.get(self.at) == Some(&'\t') {
            self.at += 1;
        }
        self.chars.get(self.at).copied()
    }
    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected {c:?} at {}", self.at))
        }
    }
    fn term(&mut self) -> Result<Term, String> {
        match self.peek() {
            Some('<') => {
                if self.chars.get(self.at + 1) == Some(&'<') {
                    self.at += 2;
                    self.expect('(')?;
                    let s = self.term()?;
                    let p = self.term()?;
                    let o = self.term()?;
                    self.expect(')')?;
                    self.expect('>')?;
                    self.expect('>')?;
                    Ok(Term::Triple(Box::new(Triple { s, p, o })))
                } else {
                    self.at += 1;
                    let start = self.at;
                    while self.chars.get(self.at) != Some(&'>') {
                        if self.at >= self.chars.len() {
                            return Err("unterminated IRI".into());
                        }
                        self.at += 1;
                    }
                    let iri: String = self.chars[start..self.at].iter().collect();
                    self.at += 1;
                    Ok(Term::Iri(iri))
                }
            }
            Some('"') => {
                self.at += 1;
                let mut value = String::new();
                loop {
                    match self.chars.get(self.at) {
                        None => return Err("unterminated literal".into()),
                        Some('"') => {
                            self.at += 1;
                            break;
                        }
                        Some('\\') => {
                            self.at += 1;
                            let e = *self.chars.get(self.at).ok_or("bad escape")?;
                            self.at += 1;
                            value.push(match e {
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                'b' => '\u{8}',
                                'f' => '\u{c}',
                                '"' => '"',
                                '\'' => '\'',
                                '\\' => '\\',
                                'u' => {
                                    let hex: String =
                                        self.chars[self.at..self.at + 4].iter().collect();
                                    self.at += 4;
                                    char::from_u32(
                                        u32::from_str_radix(&hex, 16).map_err(|_| "bad \\u")?,
                                    )
                                    .ok_or("bad \\u")?
                                }
                                _ => return Err(format!("bad escape \\{e}")),
                            });
                        }
                        Some(c) => {
                            value.push(*c);
                            self.at += 1;
                        }
                    }
                }
                if self.peek() == Some('@') {
                    self.at += 1;
                    let start = self.at;
                    while self.chars.get(self.at).is_some_and(|c| c.is_alphanumeric() || *c == '-') {
                        self.at += 1;
                    }
                    let lang: String = self.chars[start..self.at].iter().collect();
                    return Ok(Term::Literal(Literal {
                        value,
                        datatype: None,
                        lang: Some(lang.to_lowercase()),
                    }));
                }
                if self.peek() == Some('^') {
                    self.at += 1;
                    self.expect('^')?;
                    return match self.term()? {
                        Term::Iri(dt) => Ok(Term::Literal(Literal::typed(&value, &dt))),
                        _ => Err("datatype is not an IRI".into()),
                    };
                }
                Ok(Term::Literal(Literal::simple(&value)))
            }
            other => Err(format!("expected a term, found {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// The query algebra: exactly the subset's constructs. FRINGE marks the
// predicted additions; everything not representable here is refused by
// the parser, by name.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub kind: QueryKind,
    /// SELECT projection (empty for other forms; `star` is FRINGE).
    pub projection: Vec<ProjItem>,
    pub star: bool,
    /// FRINGE: SELECT DISTINCT.
    pub distinct: bool,
    /// FRINGE (CONSTRUCT): the template, as patterns.
    pub template: Vec<TriplePattern>,
    /// FRINGE (DESCRIBE): the resources asked about.
    pub describe: Vec<PatternTerm>,
    /// A caller-written dataset, kept as written; `None` = the union rule.
    pub dataset: Option<Dataset>,
    pub group: Group,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
    /// FRINGE: OFFSET (LIMIT's pagination pair).
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Select,
    Ask,
    /// FRINGE: advertised by the original's routes, never issued.
    Construct,
    /// FRINGE: advertised by the original's routes, never issued.
    Describe,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjItem {
    Var(String),
    Alias(Expr, String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    pub default: Vec<String>,
    pub named: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Bgp(Vec<TriplePattern>),
    Graph(GraphTerm, Group),
    Optional(Group),
    Filter(Expr),
    Values(String, Vec<Term>),
    /// FRINGE: BIND(expr AS ?var).
    Bind(Expr, String),
    Group(Group),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphTerm {
    Var(String),
    Iri(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TriplePattern {
    pub s: PatternTerm,
    pub p: PatternTerm,
    pub o: PatternTerm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternTerm {
    Var(String),
    Term(Term),
    /// A triple-term pattern: `<<( s p o )>>` with variables inside —
    /// CORE, the claim layer's core query (query_emojikeys).
    Triple(Box<TriplePattern>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Var(String),
    Term(Term),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Func(String, Vec<Expr>),
    Exists(Group, bool),
    Agg(Agg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    /// FRINGE below: the comparison family's other half (`=` and `>`
    /// are the proven pair; these are the same match arm).
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Agg {
    CountStar,
    Count(Box<Expr>, bool),
    Min(Box<Expr>),
    Max(Box<Expr>),
}

/// A parse failure. When the query used a construct outside the subset,
/// `construct` names it — refusal by name is the subset's contract, so a
/// caller can tell its user exactly which construct to rewrite.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub construct: Option<String>,
    pub message: String,
}

impl ParseError {
    fn refused(construct: &str) -> Self {
        ParseError {
            construct: Some(construct.to_string()),
            message: format!("{construct} is outside the supported SPARQL subset"),
        }
    }
    fn plain(message: String) -> Self {
        ParseError { construct: None, message }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Iri(String),
    PName(String, String),
    Str(String),
    Lit(Literal),
    Var(String),
    Word(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    Dot,
    Semi,
    Comma,
    Star,
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    And,
    Or,
    Not,
    TripleOpen,
    TripleClose,
    Lang(String),
    Caret,
    Slash,
    Pipe,
    Quest,
    Plus,
    Minus,
}

fn lex(text: &str) -> Result<Vec<Tok>, ParseError> {
    let cs: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    macro_rules! err { ($m:expr) => { ParseError::plain(format!("{} at character {}", $m, i)) }; }
    while i < cs.len() {
        let c = cs[i];
        match c {
            _ if c.is_whitespace() => i += 1,
            '#' => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
            }
            '{' => {
                toks.push(Tok::LBrace);
                i += 1;
            }
            '}' => {
                toks.push(Tok::RBrace);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            ';' => {
                toks.push(Tok::Semi);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '^' => {
                toks.push(Tok::Caret);
                i += 1;
            }
            '|' => {
                if cs.get(i + 1) == Some(&'|') {
                    toks.push(Tok::Or);
                    i += 2;
                } else {
                    toks.push(Tok::Pipe);
                    i += 1;
                }
            }
            '?' => {
                if cs.get(i + 1).is_some_and(|n| n.is_alphanumeric() || *n == '_') {
                    let start = i + 1;
                    i = start;
                    while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                        i += 1;
                    }
                    toks.push(Tok::Var(cs[start..i].iter().collect()));
                } else {
                    toks.push(Tok::Quest);
                    i += 1;
                }
            }
            '$' => {
                let start = i + 1;
                i = start;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                if i == start {
                    return Err(err!("lone $"));
                }
                toks.push(Tok::Var(cs[start..i].iter().collect()));
            }
            '&' => {
                if cs.get(i + 1) == Some(&'&') {
                    toks.push(Tok::And);
                    i += 2;
                } else {
                    return Err(err!("lone &"));
                }
            }
            '!' => {
                if cs.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    toks.push(Tok::Not);
                    i += 1;
                }
            }
            '=' => {
                toks.push(Tok::Eq);
                i += 1;
            }
            '>' => {
                if cs.get(i + 1) == Some(&'>') {
                    toks.push(Tok::TripleClose);
                    i += 2;
                } else if cs.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            '<' => {
                if cs.get(i + 1) == Some(&'<') && cs.get(i + 2) == Some(&'(') {
                    toks.push(Tok::TripleOpen);
                    i += 3;
                } else if cs.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Le);
                    i += 2;
                } else {
                    // An IRI if a clean `>` closes it before any blank.
                    let mut j = i + 1;
                    let mut iri = true;
                    while j < cs.len() && cs[j] != '>' {
                        if cs[j].is_whitespace() || "<>\"{}|^`\\".contains(cs[j]) {
                            iri = false;
                            break;
                        }
                        j += 1;
                    }
                    if iri && j < cs.len() && j > i + 1 {
                        toks.push(Tok::Iri(cs[i + 1..j].iter().collect()));
                        i = j + 1;
                    } else {
                        toks.push(Tok::Lt);
                        i += 1;
                    }
                }
            }
            '.' => {
                if cs.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                    // .5 — a decimal; read it as one.
                    let start = i;
                    i += 1;
                    while i < cs.len() && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                    let raw: String = cs[start..i].iter().collect();
                    toks.push(Tok::Lit(Literal::typed(&format!("0{raw}"), XSD_DECIMAL)));
                } else {
                    toks.push(Tok::Dot);
                    i += 1;
                }
            }
            '"' | '\'' => {
                let quote = c;
                i += 1;
                let mut value = String::new();
                loop {
                    match cs.get(i) {
                        None => return Err(err!("unterminated string")),
                        Some(&q) if q == quote => {
                            i += 1;
                            break;
                        }
                        Some('\\') => {
                            i += 1;
                            let e = *cs.get(i).ok_or_else(|| err!("bad escape"))?;
                            i += 1;
                            value.push(match e {
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                'b' => '\u{8}',
                                'f' => '\u{c}',
                                '"' => '"',
                                '\'' => '\'',
                                '\\' => '\\',
                                'u' => {
                                    if i + 4 > cs.len() {
                                        return Err(err!("bad \\u escape"));
                                    }
                                    let hex: String = cs[i..i + 4].iter().collect();
                                    i += 4;
                                    char::from_u32(
                                        u32::from_str_radix(&hex, 16)
                                            .map_err(|_| err!("bad \\u escape"))?,
                                    )
                                    .ok_or_else(|| err!("bad \\u escape"))?
                                }
                                _ => return Err(err!("bad escape")),
                            });
                        }
                        Some(&ch) => {
                            value.push(ch);
                            i += 1;
                        }
                    }
                }
                // Suffixes: @lang or ^^datatype.
                if cs.get(i) == Some(&'@') {
                    i += 1;
                    let start = i;
                    while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '-') {
                        i += 1;
                    }
                    toks.push(Tok::Str(value));
                    toks.push(Tok::Lang(cs[start..i].iter().collect::<String>().to_lowercase()));
                } else {
                    toks.push(Tok::Str(value));
                }
            }
            '@' => {
                i += 1;
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '-') {
                    i += 1;
                }
                toks.push(Tok::Lang(cs[start..i].iter().collect()));
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    i += 1;
                }
                let mut decimal = false;
                if cs.get(i) == Some(&'.')
                    && cs.get(i + 1).is_some_and(|n| n.is_ascii_digit())
                {
                    decimal = true;
                    i += 1;
                    while i < cs.len() && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let raw: String = cs[start..i].iter().collect();
                toks.push(Tok::Lit(Literal::typed(
                    &raw,
                    if decimal { XSD_DECIMAL } else { XSD_INTEGER },
                )));
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                let word: String = cs[start..i].iter().collect();
                if cs.get(i) == Some(&':') {
                    // A prefixed name (the prefix may be the whole word).
                    i += 1;
                    let lstart = i;
                    while i < cs.len() {
                        let ch = cs[i];
                        if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '~' | '%') {
                            i += 1;
                        } else if ch == '.'
                            && cs.get(i + 1).is_some_and(|n| {
                                n.is_alphanumeric() || matches!(n, '_' | '-' | '~')
                            })
                        {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    toks.push(Tok::PName(word, cs[lstart..i].iter().collect()));
                } else {
                    toks.push(Tok::Word(word));
                }
            }
            ':' => {
                // An empty-prefix name, `:local`.
                i += 1;
                let lstart = i;
                while i < cs.len()
                    && (cs[i].is_alphanumeric() || matches!(cs[i], '_' | '-' | '~' | '%'))
                {
                    i += 1;
                }
                toks.push(Tok::PName(String::new(), cs[lstart..i].iter().collect()));
            }
            '[' => return Err(ParseError::refused("blank nodes")),
            _ => return Err(err!("unexpected character")),
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------------------
// Parser: subset-native. An out-of-subset construct fails here, named —
// there is no full-SPARQL parse behind this to audit afterwards.
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    base: String,
    prefixes: HashMap<String, String>,
}

/// Parse a query against a base IRI (relative addresses resolve against
/// it). The base is the caller's — for ravel, the soul's root.
pub fn parse_query(text: &str, base: &str) -> Result<Query, ParseError> {
    let mut p = Parser { toks: lex(text)?, at: 0, base: base.to_string(), prefixes: HashMap::new() };
    p.prologue()?;
    p.query()
}

const FUNCTIONS: &[&str] = &[
    "STR", "STRSTARTS", "STRENDS", "CONTAINS", "LCASE", "DATATYPE", "REGEX", "SUBSTR",
];
const AGGREGATES: &[&str] = &["COUNT", "MIN", "MAX"];

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        if t.is_some() {
            self.at += 1;
        }
        t
    }
    fn word(&self) -> Option<String> {
        match self.peek() {
            Some(Tok::Word(w)) => Some(w.to_uppercase()),
            _ => None,
        }
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.word().as_deref() == Some(w) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, w: &str) -> Result<(), ParseError> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {w}")))
        }
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok) -> Result<(), ParseError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {t:?}")))
        }
    }
    fn err(&self, m: &str) -> ParseError {
        ParseError::plain(format!("{m} (token {})", self.at))
    }
    fn resolve(&self, raw: &str) -> String {
        if raw.contains("://") || raw.starts_with("urn:") {
            return raw.to_string();
        }
        if let Some(rest) = raw.strip_prefix('/') {
            if let Some(i) = self.base.find("://") {
                let host_end = self.base[i + 3..].find('/').map(|j| i + 3 + j).unwrap_or(self.base.len());
                return format!("{}/{rest}", &self.base[..host_end]);
            }
            return raw.to_string();
        }
        if self.base.ends_with('/') {
            format!("{}{raw}", self.base)
        } else {
            match self.base.rfind('/') {
                Some(i) => format!("{}{raw}", &self.base[..=i]),
                None => raw.to_string(),
            }
        }
    }
    fn pname(&self, prefix: &str, local: &str) -> Result<String, ParseError> {
        let ns = self
            .prefixes
            .get(prefix)
            .ok_or_else(|| self.err(&format!("prefix {prefix}: is not declared")))?;
        Ok(format!("{ns}{local}"))
    }
    /// An IRI in term position: `<...>` or a prefixed name, resolved.
    fn iri_term(&mut self) -> Result<Option<String>, ParseError> {
        match self.next() {
            Some(Tok::Iri(raw)) => Ok(Some(self.resolve(&raw))),
            Some(Tok::PName(p, l)) => self.pname(&p, &l).map(Some),
            other => {
                if other.is_some() {
                    self.at -= 1;
                }
                Ok(None)
            }
        }
    }

    fn prologue(&mut self) -> Result<(), ParseError> {
        loop {
            if self.eat_word("PREFIX") {
                let (prefix, local) = match self.next() {
                    Some(Tok::PName(p, l)) => (p, l),
                    _ => return Err(self.err("PREFIX wants a prefix name")),
                };
                if !local.is_empty() {
                    return Err(self.err("PREFIX name must end at the colon"));
                }
                let iri = match self.next() {
                    Some(Tok::Iri(raw)) => self.resolve(&raw),
                    _ => return Err(self.err("PREFIX wants an <iri>")),
                };
                self.prefixes.insert(prefix, iri);
            } else if self.eat_word("BASE") {
                match self.next() {
                    Some(Tok::Iri(raw)) => self.base = raw,
                    _ => return Err(self.err("BASE wants an <iri>")),
                }
            } else {
                return Ok(());
            }
        }
    }

    fn query(&mut self) -> Result<Query, ParseError> {
        let mut q = Query {
            kind: QueryKind::Select,
            projection: Vec::new(),
            star: false,
            distinct: false,
            template: Vec::new(),
            describe: Vec::new(),
            dataset: None,
            group: Group { elements: Vec::new() },
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: 0,
        };
        match self.word() {
            Some(ref w) if w == "SELECT" => {
                self.at += 1;
                self.select_tail(&mut q)?;
            }
            Some(ref w) if w == "ASK" => {
                self.at += 1;
                q.kind = QueryKind::Ask;
                self.dataset(&mut q)?;
                self.where_group(&mut q)?;
            }
            // FRINGE: the routes advertised both forms; no code issued one.
            Some(ref w) if w == "CONSTRUCT" => {
                self.at += 1;
                q.kind = QueryKind::Construct;
                self.expect(&Tok::LBrace)?;
                q.template = self.triples_block()?;
                self.expect(&Tok::RBrace)?;
                self.dataset(&mut q)?;
                self.where_group(&mut q)?;
                self.modifiers(&mut q)?;
            }
            Some(ref w) if w == "DESCRIBE" => {
                self.at += 1;
                q.kind = QueryKind::Describe;
                if self.eat(&Tok::Star) {
                    q.star = true;
                } else {
                    loop {
                        if let Some(t) = self.pattern_term()? {
                            q.describe.push(t);
                        } else {
                            break;
                        }
                    }
                    if q.describe.is_empty() {
                        return Err(self.err("DESCRIBE wants a resource"));
                    }
                }
                self.dataset(&mut q)?;
                if self.peek() == Some(&Tok::LBrace) || self.word().as_deref() == Some("WHERE") {
                    self.where_group(&mut q)?;
                }
                self.modifiers(&mut q)?;
            }
            Some(ref w)
                if ["INSERT", "DELETE", "LOAD", "CLEAR", "DROP", "CREATE", "COPY", "MOVE", "ADD"]
                    .contains(&w.as_str()) =>
            {
                return Err(ParseError::refused("SPARQL Update"));
            }
            _ => return Err(self.err("expected SELECT, ASK, CONSTRUCT or DESCRIBE")),
        }
        if self.at != self.toks.len() {
            // A UNION trailing the group is still a UNION: refuse it by
            // name rather than as anonymous trailing tokens.
            if self.word().as_deref() == Some("UNION") {
                return Err(ParseError::refused("UNION"));
            }
            return Err(self.err("trailing tokens after the query"));
        }
        Ok(q)
    }

    fn select_tail(&mut self, q: &mut Query) -> Result<(), ParseError> {
        if self.eat_word("DISTINCT") {
            q.distinct = true; // FRINGE
        } else if self.word().as_deref() == Some("REDUCED") {
            return Err(ParseError::refused("SELECT REDUCED"));
        }
        if self.eat(&Tok::Star) {
            q.star = true; // FRINGE
        } else {
            loop {
                match self.peek() {
                    Some(Tok::Var(v)) => {
                        let v = v.clone();
                        self.at += 1;
                        q.projection.push(ProjItem::Var(v));
                    }
                    Some(Tok::LParen) => {
                        self.at += 1;
                        let e = self.expr()?;
                        self.expect_word("AS")?;
                        let v = match self.next() {
                            Some(Tok::Var(v)) => v,
                            _ => return Err(self.err("AS wants a variable")),
                        };
                        self.expect(&Tok::RParen)?;
                        q.projection.push(ProjItem::Alias(e, v));
                    }
                    _ => break,
                }
            }
            if q.projection.is_empty() {
                return Err(self.err("SELECT wants at least one variable"));
            }
        }
        self.dataset(q)?;
        self.where_group(q)?;
        // Solution modifiers.
        if self.eat_word("GROUP") {
            self.expect_word("BY")?;
            while let Some(Tok::Var(v)) = self.peek().cloned() {
                self.at += 1;
                q.group_by.push(v);
            }
            if q.group_by.is_empty() {
                return Err(self.err("GROUP BY wants a variable"));
            }
        }
        if self.word().as_deref() == Some("HAVING") {
            return Err(ParseError::refused("HAVING"));
        }
        self.modifiers(q)
    }

    fn modifiers(&mut self, q: &mut Query) -> Result<(), ParseError> {
        if self.eat_word("ORDER") {
            self.expect_word("BY")?;
            loop {
                if matches!(
                    self.word().map(|w| w.to_uppercase()).as_deref(),
                    Some("LIMIT") | Some("OFFSET")
                ) {
                    break;
                }
                let key = if self.eat_word("ASC") {
                    self.expect(&Tok::LParen)?;
                    let e = self.expr()?;
                    self.expect(&Tok::RParen)?;
                    OrderKey { expr: e, desc: false }
                } else if self.eat_word("DESC") {
                    self.expect(&Tok::LParen)?;
                    let e = self.expr()?;
                    self.expect(&Tok::RParen)?;
                    OrderKey { expr: e, desc: true }
                } else if let Some(Tok::Var(v)) = self.peek().cloned() {
                    self.at += 1;
                    OrderKey { expr: Expr::Var(v), desc: false }
                } else if self.peek() == Some(&Tok::LParen) {
                    self.at += 1;
                    let e = self.expr()?;
                    self.expect(&Tok::RParen)?;
                    OrderKey { expr: e, desc: false }
                } else if matches!(self.peek(), Some(Tok::Word(_))) {
                    OrderKey { expr: self.expr()?, desc: false }
                } else {
                    break;
                };
                q.order_by.push(key);
            }
            if q.order_by.is_empty() {
                return Err(self.err("ORDER BY wants a key"));
            }
        }
        for _ in 0..2 {
            if self.eat_word("LIMIT") {
                q.limit = Some(self.count()?);
            } else if self.eat_word("OFFSET") {
                q.offset = self.count()?; // FRINGE
            } else {
                break;
            }
        }
        Ok(())
    }

    fn count(&mut self) -> Result<usize, ParseError> {
        match self.next() {
            Some(Tok::Lit(l)) if l.datatype.as_deref() == Some(XSD_INTEGER) => {
                l.value.parse().map_err(|_| self.err("bad count"))
            }
            _ => Err(self.err("wants an integer")),
        }
    }

    fn dataset(&mut self, q: &mut Query) -> Result<(), ParseError> {
        let mut ds = Dataset { default: Vec::new(), named: Vec::new() };
        let mut any = false;
        loop {
            if self.eat_word("FROM") {
                any = true;
                if self.eat_word("NAMED") {
                    let iri = self.iri_term()?.ok_or_else(|| self.err("FROM NAMED wants an IRI"))?;
                    ds.named.push(iri);
                } else {
                    let iri = self.iri_term()?.ok_or_else(|| self.err("FROM wants an IRI"))?;
                    ds.default.push(iri);
                }
            } else {
                break;
            }
        }
        if any {
            q.dataset = Some(ds);
        }
        Ok(())
    }

    fn where_group(&mut self, q: &mut Query) -> Result<(), ParseError> {
        self.eat_word("WHERE");
        q.group = self.group()?;
        Ok(())
    }

    fn group(&mut self) -> Result<Group, ParseError> {
        self.expect(&Tok::LBrace)?;
        let mut elements = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::RBrace) => {
                    self.at += 1;
                    return Ok(Group { elements });
                }
                None => return Err(self.err("unterminated group")),
                Some(Tok::LBrace) => {
                    // A nested group, or a subquery — the subset refuses
                    // the second by name and joins the first.
                    let save = self.at;
                    self.at += 1;
                    if self.word().as_deref() == Some("SELECT") {
                        return Err(ParseError::refused("subqueries"));
                    }
                    self.at = save;
                    elements.push(Element::Group(self.group()?));
                }
                Some(Tok::Word(_)) => match self.word().as_deref() {
                    Some("GRAPH") => {
                        self.at += 1;
                        let g = match self.next() {
                            Some(Tok::Var(v)) => GraphTerm::Var(v),
                            Some(Tok::Iri(raw)) => GraphTerm::Iri(self.resolve(&raw)),
                            Some(Tok::PName(p, l)) => GraphTerm::Iri(self.pname(&p, &l)?),
                            _ => return Err(self.err("GRAPH wants a variable or an IRI")),
                        };
                        elements.push(Element::Graph(g, self.group()?));
                    }
                    Some("OPTIONAL") => {
                        self.at += 1;
                        elements.push(Element::Optional(self.group()?));
                    }
                    Some("FILTER") => {
                        self.at += 1;
                        elements.push(Element::Filter(self.filter()?));
                    }
                    Some("VALUES") => {
                        self.at += 1;
                        let var = match self.next() {
                            Some(Tok::Var(v)) => v,
                            _ => return Err(self.err("VALUES wants a variable")),
                        };
                        self.expect(&Tok::LBrace)?;
                        let mut terms = Vec::new();
                        while self.peek() != Some(&Tok::RBrace) {
                            if self.word().as_deref() == Some("UNDEF") {
                                return Err(ParseError::refused("VALUES (UNDEF)"));
                            }
                            match self.pattern_term()? {
                                Some(PatternTerm::Term(t)) => terms.push(t),
                                _ => return Err(self.err("VALUES wants ground terms")),
                            }
                        }
                        self.at += 1;
                        elements.push(Element::Values(var, terms));
                    }
                    Some("BIND") => {
                        // FRINGE: BIND(expr AS ?var).
                        self.at += 1;
                        self.expect(&Tok::LParen)?;
                        let e = self.expr()?;
                        self.expect_word("AS")?;
                        let v = match self.next() {
                            Some(Tok::Var(v)) => v,
                            _ => return Err(self.err("BIND wants AS ?var")),
                        };
                        self.expect(&Tok::RParen)?;
                        elements.push(Element::Bind(e, v));
                        self.eat(&Tok::Dot);
                    }
                    Some("UNION") => return Err(ParseError::refused("UNION")),
                    Some("MINUS") => return Err(ParseError::refused("MINUS")),
                    Some("SERVICE") => return Err(ParseError::refused("SERVICE")),
                    Some("SELECT") => return Err(ParseError::refused("subqueries")),
                    _ => {
                        let before = self.at;
                        let pats = self.triples_block()?;
                        if self.at == before {
                            return Err(self.err("unexpected token in a group"));
                        }
                        if !pats.is_empty() {
                            elements.push(Element::Bgp(pats));
                        }
                    }
                },
                _ => {
                    let before = self.at;
                    let pats = self.triples_block()?;
                    if self.at == before {
                        return Err(self.err("unexpected token in a group"));
                    }
                    if !pats.is_empty() {
                        elements.push(Element::Bgp(pats));
                    }
                }
            }
        }
    }

    fn filter(&mut self) -> Result<Expr, ParseError> {
        if self.eat_word("NOT") {
            self.expect_word("EXISTS")?;
            return Ok(Expr::Exists(self.group()?, false));
        }
        if self.eat_word("EXISTS") {
            return Ok(Expr::Exists(self.group()?, true));
        }
        if self.eat(&Tok::LParen) {
            let e = self.expr()?;
            self.expect(&Tok::RParen)?;
            return Ok(e);
        }
        // A bare function call is a constraint too.
        self.expr()
    }

    /// One or more same-subject triple patterns, `.`-separated.
    fn triples_block(&mut self) -> Result<Vec<TriplePattern>, ParseError> {
        let mut out = Vec::new();
        loop {
            let Some(s) = self.pattern_term()? else {
                return Ok(out);
            };
            loop {
                // Predicate: a variable, `a`, an IRI — or a path, refused.
                let p = match self.peek() {
                    Some(Tok::Var(v)) => {
                        let v = v.clone();
                        self.at += 1;
                        PatternTerm::Var(v)
                    }
                    Some(Tok::Word(w)) if w.eq_ignore_ascii_case("a") => {
                        self.at += 1;
                        PatternTerm::Term(Term::iri(RDF_TYPE))
                    }
                    Some(Tok::Caret) | Some(Tok::Slash) | Some(Tok::LParen) | Some(Tok::Not) => {
                        return Err(ParseError::refused("property paths"));
                    }
                    _ => match self.pattern_term()? {
                        Some(t) => t,
                        None => return Err(self.err("wants a predicate")),
                    },
                };
                // A path operator after a predicate term is a path.
                if matches!(
                    self.peek(),
                    Some(Tok::Slash) | Some(Tok::Caret) | Some(Tok::Star) | Some(Tok::Plus)
                        | Some(Tok::Quest) | Some(Tok::Pipe)
                ) {
                    return Err(ParseError::refused("property paths"));
                }
                loop {
                    let Some(o) = self.pattern_term()? else {
                        return Err(self.err("wants an object"));
                    };
                    out.push(TriplePattern { s: s.clone(), p: p.clone(), o });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                if self.eat(&Tok::Semi) {
                    if matches!(self.peek(), Some(Tok::Dot) | Some(Tok::RBrace) | None) {
                        break;
                    }
                    continue;
                }
                break;
            }
            if self.eat(&Tok::Dot) {
                if matches!(self.peek(), Some(Tok::RBrace) | None) {
                    return Ok(out);
                }
                continue;
            }
            return Ok(out);
        }
    }

    /// A term in pattern position, if the next tokens form one.
    fn pattern_term(&mut self) -> Result<Option<PatternTerm>, ParseError> {
        match self.next() {
            Some(Tok::Var(v)) => Ok(Some(PatternTerm::Var(v))),
            Some(Tok::Iri(raw)) => Ok(Some(PatternTerm::Term(Term::Iri(self.resolve(&raw))))),
            Some(Tok::PName(p, l)) => Ok(Some(PatternTerm::Term(Term::Iri(self.pname(&p, &l)?)))),
            Some(Tok::Lit(l)) => Ok(Some(PatternTerm::Term(Term::Literal(l)))),
            Some(Tok::Str(s)) => Ok(Some(PatternTerm::Term(Term::Literal(self.literal_tail(s)?)))),
            Some(Tok::TripleOpen) => {
                let s = self.pattern_term()?.ok_or_else(|| self.err("triple term wants a subject"))?;
                let p = self.pattern_term()?.ok_or_else(|| self.err("triple term wants a predicate"))?;
                let o = self.pattern_term()?.ok_or_else(|| self.err("triple term wants an object"))?;
                self.expect(&Tok::RParen)?;
                if !self.eat(&Tok::TripleClose) {
                    return Err(self.err("triple term wants )>>"));
                }
                Ok(Some(PatternTerm::Triple(Box::new(TriplePattern { s, p, o }))))
            }
            other => {
                if other.is_some() {
                    self.at -= 1;
                }
                Ok(None)
            }
        }
    }

    /// After a string token: an optional @lang (already a token) or
    /// ^^datatype.
    fn literal_tail(&mut self, value: String) -> Result<Literal, ParseError> {
        match self.next() {
            Some(Tok::Lang(lang)) => Ok(Literal { value, datatype: None, lang: Some(lang) }),
            Some(Tok::Caret) => {
                self.expect(&Tok::Caret)?;
                let dt = self.iri_term()?.ok_or_else(|| self.err("^^ wants a datatype IRI"))?;
                Ok(Literal::typed(&value, &dt))
            }
            other => {
                if other.is_some() {
                    self.at -= 1;
                }
                Ok(Literal::simple(&value))
            }
        }
    }

    // Expressions, weakest binding first: ||, &&, comparison, unary,
    // primary. Arithmetic is refused by name (no issued query computes).
    fn expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.expr_and()?;
        while self.eat(&Tok::Or) {
            let r = self.expr_and()?;
            e = Expr::Or(Box::new(e), Box::new(r));
        }
        Ok(e)
    }
    fn expr_and(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.expr_cmp()?;
        while self.eat(&Tok::And) {
            let r = self.expr_cmp()?;
            e = Expr::And(Box::new(e), Box::new(r));
        }
        Ok(e)
    }
    fn expr_cmp(&mut self) -> Result<Expr, ParseError> {
        let e = self.expr_unary()?;
        let op = match self.peek() {
            Some(Tok::Eq) => Some(CmpOp::Eq),
            Some(Tok::Ne) => Some(CmpOp::Ne),
            Some(Tok::Gt) => Some(CmpOp::Gt),
            Some(Tok::Lt) => Some(CmpOp::Lt),
            Some(Tok::Ge) => Some(CmpOp::Ge),
            Some(Tok::Le) => Some(CmpOp::Le),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("IN") => {
                return Err(ParseError::refused("IN"));
            }
            _ => None,
        };
        match op {
            Some(op) => {
                self.at += 1;
                let r = self.expr_unary()?;
                Ok(Expr::Cmp(op, Box::new(e), Box::new(r)))
            }
            None => Ok(e),
        }
    }
    fn expr_unary(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Tok::Not) {
            return Ok(Expr::Not(Box::new(self.expr_unary()?)));
        }
        if self.eat(&Tok::Minus) {
            // A negative number folds; anything else is arithmetic.
            return match self.next() {
                Some(Tok::Lit(l)) if l.is_numeric() => {
                    let neg = if l.value.starts_with('-') {
                        l.value[1..].to_string()
                    } else {
                        format!("-{}", l.value)
                    };
                    Ok(Expr::Term(Term::Literal(Literal { value: neg, ..l })))
                }
                _ => Err(ParseError::refused("arithmetic expressions")),
            };
        }
        if self.eat(&Tok::Plus) {
            return Err(ParseError::refused("arithmetic expressions"));
        }
        let e = self.expr_primary()?;
        if matches!(self.peek(), Some(Tok::Plus) | Some(Tok::Minus) | Some(Tok::Star) | Some(Tok::Slash)) {
            return Err(ParseError::refused("arithmetic expressions"));
        }
        Ok(e)
    }
    fn expr_primary(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Tok::Var(v)) => Ok(Expr::Var(v)),
            Some(Tok::Iri(raw)) => Ok(Expr::Term(Term::Iri(self.resolve(&raw)))),
            Some(Tok::PName(p, l)) => Ok(Expr::Term(Term::Iri(self.pname(&p, &l)?))),
            Some(Tok::Lit(l)) => Ok(Expr::Term(Term::Literal(l))),
            Some(Tok::Str(s)) => Ok(Expr::Term(Term::Literal(self.literal_tail(s)?))),
            Some(Tok::LParen) => {
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::TripleOpen) => {
                // A triple term as a value (bound, never matched inside).
                let s = self.ground_term()?;
                let p = self.ground_term()?;
                let o = self.ground_term()?;
                self.expect(&Tok::RParen)?;
                if !self.eat(&Tok::TripleClose) {
                    return Err(self.err("triple term wants )>>"));
                }
                Ok(Expr::Term(Term::Triple(Box::new(Triple { s, p, o }))))
            }
            Some(Tok::Word(w)) => {
                let name = w.to_uppercase();
                if name == "EXISTS" || name == "NOT" {
                    if name == "NOT" {
                        self.expect_word("EXISTS")?;
                        return Ok(Expr::Exists(self.group()?, false));
                    }
                    return Ok(Expr::Exists(self.group()?, true));
                }
                if self.peek() != Some(&Tok::LParen) {
                    return Err(self.err(&format!("unexpected word {w} in an expression")));
                }
                self.at += 1;
                if AGGREGATES.contains(&name.as_str()) {
                    return self.aggregate(&name);
                }
                if !FUNCTIONS.contains(&name.as_str()) {
                    return Err(ParseError::refused(&format!("function {name}")));
                }
                let mut args = Vec::new();
                if self.peek() != Some(&Tok::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                Ok(Expr::Func(name, args))
            }
            other => {
                if other.is_some() {
                    self.at -= 1;
                }
                Err(self.err("wants an expression"))
            }
        }
    }
    fn ground_term(&mut self) -> Result<Term, ParseError> {
        match self.pattern_term()? {
            Some(PatternTerm::Term(t)) => Ok(t),
            _ => Err(self.err("a triple-term value wants ground terms")),
        }
    }
    fn aggregate(&mut self, name: &str) -> Result<Expr, ParseError> {
        // The opening paren is already eaten.
        let agg = match name {
            "COUNT" => {
                if self.eat(&Tok::Star) {
                    Agg::CountStar
                } else {
                    let distinct = self.eat_word("DISTINCT");
                    let e = self.expr()?;
                    Agg::Count(Box::new(e), distinct)
                }
            }
            "MIN" => Agg::Min(Box::new(self.expr()?)),
            "MAX" => Agg::Max(Box::new(self.expr()?)),
            _ => unreachable!(),
        };
        self.expect(&Tok::RParen)?;
        Ok(Expr::Agg(agg))
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// The caller's name maps, as data: names the caller asks with, and the
/// stored spellings they mean. (For ravel these come from its two
/// ontologies: `owl:equivalentProperty` pairs and the `rdfs:subClassOf`
/// closure — five properties and the class tree, nothing else.)
#[derive(Debug, Clone, Default)]
pub struct Prep {
    /// Asked name → stored names it matches in predicate position.
    pub prop_equiv: HashMap<String, Vec<String>>,
    /// Asked class → stored classes it matches as an `rdf:type` object
    /// (the subclass closure, the asked class itself included by the
    /// evaluator if stored).
    pub class_subs: HashMap<String, Vec<String>>,
    /// Prefixes no store holds; a solution binding one is dropped from
    /// the answer (calamus drops abstract names from answers).
    pub abstract_prefixes: Vec<String>,
}

/// What a query produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Results {
    Solutions { vars: Vec<String>, rows: Vec<Binding> },
    Boolean(bool),
    /// CONSTRUCT / DESCRIBE output (FRINGE): ground triples, deduplicated.
    Triples(Vec<Triple>),
}

/// One solution row: variable → term (absent = unbound).
pub type Binding = HashMap<String, Term>;

#[derive(Debug)]
pub struct Outcome {
    pub results: Results,
    /// Names met inside expressions that have other stored spellings and
    /// were compared as written — the original's `left_as_written`.
    pub left_as_written: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalError(pub String);

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for EvalError {}

/// Evaluate a parsed query over a source.
pub fn evaluate<S: QuadSource>(q: &Query, src: &S, prep: &Prep) -> Result<Outcome, EvalError> {
    let ev = Ev { src, prep, ds: DatasetView::resolve(q, src) };
    let mut left: Vec<String> = Vec::new();
    collect_left(q, prep, &mut left);
    let sols = ev.eval_group(&q.group, &Ctx::Default, Binding::new())?;
    let results = match q.kind {
        QueryKind::Ask => Results::Boolean(!sols.is_empty()),
        QueryKind::Select => {
            let (vars, rows) = ev.project(q, sols)?;
            Results::Solutions { vars, rows }
        }
        QueryKind::Construct => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for sol in &sols {
                for pat in &q.template {
                    if let Some(t) = instantiate(pat, sol) {
                        if seen.insert(t.clone()) {
                            out.push(t);
                        }
                    }
                }
            }
            Results::Triples(out)
        }
        QueryKind::Describe => {
            let mut targets: Vec<String> = Vec::new();
            if q.star {
                for sol in &sols {
                    for t in sol.values() {
                        if let Term::Iri(i) = t {
                            targets.push(i.clone());
                        }
                    }
                }
            } else {
                for dt in &q.describe {
                    match dt {
                        PatternTerm::Term(Term::Iri(i)) => targets.push(i.clone()),
                        PatternTerm::Var(v) => {
                            for sol in &sols {
                                if let Some(Term::Iri(i)) = sol.get(v) {
                                    targets.push(i.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            targets.sort();
            targets.dedup();
            // Subject closure: every statement the source makes about
            // each resource, in any graph (the concise-description
            // shape; this stack's data has no blank nodes to chase).
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            let mut graphs: Vec<Option<String>> = vec![None];
            graphs.extend(ev.ds.named.iter().map(|g| Some(g.clone())));
            for g in &graphs {
                for quad in src.quads(g.as_deref()) {
                    if let Term::Iri(s) = &quad.s {
                        if targets.binary_search(s).is_ok() {
                            let t = Triple { s: quad.s.clone(), p: quad.p.clone(), o: quad.o.clone() };
                            if seen.insert(t.clone()) {
                                out.push(t);
                            }
                        }
                    }
                }
            }
            Results::Triples(out)
        }
    };
    // Abstract names are dropped from answers.
    let results = match results {
        Results::Solutions { vars, rows } => Results::Solutions {
            vars,
            rows: rows
                .into_iter()
                .filter(|r| {
                    !r.values().any(|t| match t {
                        Term::Iri(i) => prep.abstract_prefixes.iter().any(|p| i.starts_with(p)),
                        _ => false,
                    })
                })
                .collect(),
        },
        other => other,
    };
    Ok(Outcome { results, left_as_written: left })
}

fn instantiate(pat: &TriplePattern, sol: &Binding) -> Option<Triple> {
    Some(Triple { s: inst_term(&pat.s, sol)?, p: inst_term(&pat.p, sol)?, o: inst_term(&pat.o, sol)? })
}

fn inst_term(pt: &PatternTerm, sol: &Binding) -> Option<Term> {
    match pt {
        PatternTerm::Var(v) => sol.get(v).cloned(),
        PatternTerm::Term(t) => Some(t.clone()),
        PatternTerm::Triple(inner) => Some(Term::Triple(Box::new(instantiate(inner, sol)?))),
    }
}

/// Names inside expressions that have other stored spellings.
fn collect_left(q: &Query, prep: &Prep, out: &mut Vec<String>) {
    fn from_expr(e: &Expr, prep: &Prep, out: &mut Vec<String>) {
        match e {
            Expr::Term(Term::Iri(i)) => {
                if (prep.prop_equiv.contains_key(i) || prep.class_subs.contains_key(i))
                    && !out.contains(i)
                {
                    out.push(i.clone());
                }
            }
            Expr::Term(Term::Triple(t)) => {
                for part in [&t.s, &t.p, &t.o] {
                    if let Term::Iri(i) = part {
                        if (prep.prop_equiv.contains_key(i) || prep.class_subs.contains_key(i))
                            && !out.contains(i)
                        {
                            out.push(i.clone());
                        }
                    }
                }
            }
            Expr::Not(a) => from_expr(a, prep, out),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Cmp(_, a, b) => {
                from_expr(a, prep, out);
                from_expr(b, prep, out);
            }
            Expr::Func(_, args) => args.iter().for_each(|a| from_expr(a, prep, out)),
            Expr::Agg(a) => match a {
                Agg::Count(e, _) | Agg::Min(e) | Agg::Max(e) => from_expr(e, prep, out),
                Agg::CountStar => {}
            },
            _ => {}
        }
    }
    fn from_group(g: &Group, prep: &Prep, out: &mut Vec<String>) {
        for el in &g.elements {
            match el {
                Element::Filter(e) => from_expr(e, prep, out),
                Element::Bind(e, _) => from_expr(e, prep, out),
                Element::Optional(inner) | Element::Group(inner) => from_group(inner, prep, out),
                Element::Graph(_, inner) => from_group(inner, prep, out),
                _ => {}
            }
        }
    }
    from_group(&q.group, prep, out);
    for item in &q.projection {
        if let ProjItem::Alias(e, _) = item {
            from_expr(e, prep, out);
        }
    }
    for k in &q.order_by {
        from_expr(&k.expr, prep, out);
    }
}

/// The dataset a query runs against, resolved once.
struct DatasetView {
    /// Triples visible to a pattern outside GRAPH (deduplicated set).
    default_triples: Vec<Triple>,
    /// Named graphs a GRAPH pattern may address.
    named: Vec<String>,
}

impl DatasetView {
    fn resolve<S: QuadSource>(q: &Query, src: &S) -> Self {
        let all_named = src.graph_names();
        match &q.dataset {
            None => {
                // The union rule: default = own default ∪ every named.
                let mut seen = HashSet::new();
                let mut triples = Vec::new();
                let mut graphs: Vec<Option<String>> = vec![None];
                graphs.extend(all_named.iter().map(|g| Some(g.clone())));
                for g in &graphs {
                    for quad in src.quads(g.as_deref()) {
                        let t = Triple { s: quad.s, p: quad.p, o: quad.o };
                        if seen.insert(t.clone()) {
                            triples.push(t);
                        }
                    }
                }
                DatasetView { default_triples: triples, named: all_named }
            }
            Some(ds) => {
                let named = if !ds.named.is_empty() {
                    ds.named.clone()
                } else if !ds.default.is_empty() {
                    Vec::new()
                } else {
                    all_named.clone()
                };
                let default_triples = if ds.default.is_empty() && !ds.named.is_empty() {
                    src.quads(None)
                        .into_iter()
                        .map(|quad| Triple { s: quad.s, p: quad.p, o: quad.o })
                        .collect()
                } else {
                    let mut seen = HashSet::new();
                    let mut triples = Vec::new();
                    for g in &ds.default {
                        for quad in src.quads(Some(g)) {
                            let t = Triple { s: quad.s, p: quad.p, o: quad.o };
                            if seen.insert(t.clone()) {
                                triples.push(t);
                            }
                        }
                    }
                    triples
                };
                DatasetView { default_triples, named }
            }
        }
    }
}

enum Ctx {
    Default,
    Named(String),
}

struct Ev<'a, S: QuadSource> {
    src: &'a S,
    prep: &'a Prep,
    ds: DatasetView,
}

type EvalResult<T> = Result<T, EvalError>;

impl<'a, S: QuadSource> Ev<'a, S> {
    fn triples_in(&self, ctx: &Ctx) -> Vec<Triple> {
        match ctx {
            Ctx::Default => self.ds.default_triples.clone(),
            Ctx::Named(g) => self
                .src
                .quads(Some(g))
                .into_iter()
                .map(|q| Triple { s: q.s, p: q.p, o: q.o })
                .collect(),
        }
    }

    fn eval_group(&self, g: &Group, ctx: &Ctx, init: Binding) -> EvalResult<Vec<Binding>> {
        let mut sols = vec![init];
        // SPARQL's rule: a FILTER speaks about its whole group, wherever
        // it is written in it. The other elements run in order; the
        // group's filters run last, in written order.
        let mut filters: Vec<&Expr> = Vec::new();
        for el in &g.elements {
            if let Element::Filter(e) = el {
                filters.push(e);
            }
        }
        for el in &g.elements {
            if matches!(el, Element::Filter(_)) {
                continue;
            }
            let mut next = Vec::new();
            match el {
                Element::Bgp(pats) => {
                    let triples = self.triples_in(ctx);
                    for sol in sols {
                        self.match_bgp(pats, &triples, &sol, &mut next);
                    }
                }
                Element::Graph(gt, inner) => {
                    for sol in sols {
                        match gt {
                            GraphTerm::Iri(name) => {
                                if self.ds.named.iter().any(|n| n == name) {
                                    next.extend(self.eval_group(inner, &Ctx::Named(name.clone()), sol)?);
                                }
                            }
                            GraphTerm::Var(v) => {
                                for name in &self.ds.named {
                                    let mut s2 = sol.clone();
                                    let bound = match s2.get(v) {
                                        Some(Term::Iri(i)) => i == name,
                                        Some(_) => false,
                                        None => {
                                            s2.insert(v.clone(), Term::Iri(name.clone()));
                                            true
                                        }
                                    };
                                    if bound {
                                        next.extend(self.eval_group(inner, &Ctx::Named(name.clone()), s2)?);
                                    }
                                }
                            }
                        }
                    }
                }
                Element::Optional(inner) => {
                    for sol in sols {
                        let found = self.eval_group(inner, ctx, sol.clone())?;
                        if found.is_empty() {
                            next.push(sol);
                        } else {
                            next.extend(found);
                        }
                    }
                }
                Element::Group(inner) => {
                    for sol in sols {
                        next.extend(self.eval_group(inner, ctx, sol)?);
                    }
                }
                Element::Values(var, terms) => {
                    for sol in sols {
                        for t in terms {
                            match sol.get(var) {
                                Some(have) if have == t => next.push(sol.clone()),
                                Some(_) => {}
                                None => {
                                    let mut s2 = sol.clone();
                                    s2.insert(var.clone(), t.clone());
                                    next.push(s2);
                                }
                            }
                        }
                    }
                }
                Element::Bind(e, var) => {
                    for sol in sols {
                        let mut s2 = sol.clone();
                        if let Ok(v) = self.eval_expr(e, &sol, ctx) {
                            s2.insert(var.clone(), v);
                        }
                        next.push(s2);
                    }
                }
                Element::Filter(_) => {} // applied after the loop, below
            }
            sols = next;
        }
        for e in filters {
            sols = sols
                .into_iter()
                .filter(|sol| self.filter_true(e, sol, ctx).unwrap_or(false))
                .collect();
        }
        Ok(sols)
    }

    fn match_bgp(&self, pats: &[TriplePattern], triples: &[Triple], sol: &Binding, out: &mut Vec<Binding>) {
        if pats.is_empty() {
            out.push(sol.clone());
            return;
        }
        let (pat, rest) = (&pats[0], &pats[1..]);
        // Fan-out deduplication (defect #104, V2-REPORT §5.3): calamus
        // fans one pattern out as { SELECT DISTINCT <pattern vars>
        // WHERE { VALUES … } }, so solutions from ONE triple pattern
        // that are identical as binding maps collapse to one BEFORE
        // joining. The prep maps above fan a pattern out at match time
        // instead (one asked class matching two stored spellings of a
        // subject typed both ways yields the same extension twice), so
        // the same discipline is applied here: collect this pattern's
        // extensions of the incoming solution, drop exact duplicates
        // keeping first-occurrence order, and only then join with the
        // remaining patterns. This is semantics-safe because two
        // identical binding maps are interchangeable in every later
        // join, filter and projection — deduplicating them changes
        // only multiplicity, never the set of distinct results, which
        // is exactly the DISTINCT the fan-out prescribes. Nothing is
        // deduplicated across patterns or joins.
        let mut first: Vec<Binding> = Vec::new();
        for t in triples {
            if let Some(extended) = self.match_pattern(pat, t, sol) {
                if !first.contains(&extended) {
                    first.push(extended);
                }
            }
        }
        for extended in first {
            self.match_bgp(rest, triples, &extended, out);
        }
    }

    fn match_pattern(&self, pat: &TriplePattern, t: &Triple, sol: &Binding) -> Option<Binding> {
        let mut b = sol.clone();
        if !self.match_term(&pat.s, &t.s, &mut b, false, false) {
            return None;
        }
        if !self.match_term(&pat.p, &t.p, &mut b, true, false) {
            return None;
        }
        let is_type = matches!(&t.p, Term::Iri(i) if i == RDF_TYPE);
        if !self.match_term(&pat.o, &t.o, &mut b, false, is_type) {
            return None;
        }
        Some(b)
    }

    /// Match one pattern position against a stored term. `predicate`
    /// enables the equivalence map; `type_object` the subclass map.
    fn match_term(
        &self,
        pat: &PatternTerm,
        stored: &Term,
        b: &mut Binding,
        predicate: bool,
        type_object: bool,
    ) -> bool {
        match pat {
            PatternTerm::Var(v) => match b.get(v) {
                Some(have) => have == stored,
                None => {
                    b.insert(v.clone(), stored.clone());
                    true
                }
            },
            PatternTerm::Term(want) => {
                if predicate {
                    if let (Term::Iri(w), Term::Iri(s)) = (want, stored) {
                        if let Some(set) = self.prep.prop_equiv.get(w) {
                            return s == w || set.iter().any(|x| x == s);
                        }
                    }
                }
                if type_object {
                    if let (Term::Iri(w), Term::Iri(s)) = (want, stored) {
                        if let Some(set) = self.prep.class_subs.get(w) {
                            return s == w || set.iter().any(|x| x == s);
                        }
                    }
                }
                want == stored
            }
            PatternTerm::Triple(inner) => match stored {
                Term::Triple(st) => {
                    self.match_term(&inner.s, &st.s, b, false, false)
                        && self.match_term(&inner.p, &st.p, b, true, false)
                        && self.match_term(&inner.o, &st.o, b, false, false)
                }
                _ => false,
            },
        }
    }

    fn filter_true(&self, e: &Expr, sol: &Binding, ctx: &Ctx) -> EvalResult<bool> {
        match self.eval_expr(e, sol, ctx) {
            Ok(t) => Ok(ebv(&t)),
            Err(_) => Ok(false),
        }
    }

    fn eval_expr(&self, e: &Expr, sol: &Binding, ctx: &Ctx) -> EvalResult<Term> {
        match e {
            Expr::Var(v) => sol.get(v).cloned().ok_or_else(|| EvalError(format!("unbound variable ?{v}"))),
            Expr::Term(t) => Ok(t.clone()),
            Expr::Not(a) => {
                let v = self.eval_expr(a, sol, ctx)?;
                Ok(bool_term(!ebv(&v)))
            }
            Expr::And(a, b) => {
                let (x, y) = (self.eval_expr(a, sol, ctx), self.eval_expr(b, sol, ctx));
                match (x, y) {
                    (Ok(t1), Ok(t2)) => Ok(bool_term(ebv(&t1) && ebv(&t2))),
                    (Ok(t), Err(_)) | (Err(_), Ok(t)) if !ebv(&t) => Ok(bool_term(false)),
                    _ => Err(EvalError("&& over an error".into())),
                }
            }
            Expr::Or(a, b) => {
                let (x, y) = (self.eval_expr(a, sol, ctx), self.eval_expr(b, sol, ctx));
                match (x, y) {
                    (Ok(t1), Ok(t2)) => Ok(bool_term(ebv(&t1) || ebv(&t2))),
                    (Ok(t), Err(_)) | (Err(_), Ok(t)) if ebv(&t) => Ok(bool_term(true)),
                    _ => Err(EvalError("|| over an error".into())),
                }
            }
            Expr::Cmp(op, a, b) => {
                let x = self.eval_expr(a, sol, ctx)?;
                let y = self.eval_expr(b, sol, ctx)?;
                Ok(bool_term(compare(*op, &x, &y)?))
            }
            Expr::Func(name, args) => self.eval_func(name, args, sol, ctx),
            Expr::Exists(inner, want) => {
                let found = self.eval_group(inner, ctx, sol.clone())?;
                Ok(bool_term(found.is_empty() != *want))
            }
            Expr::Agg(_) => Err(EvalError("an aggregate outside a projection".into())),
        }
    }

    fn eval_func(&self, name: &str, args: &[Expr], sol: &Binding, ctx: &Ctx) -> EvalResult<Term> {
        let arg = |i: usize| self.eval_expr(&args[i], sol, ctx);
        let str_of = |t: &Term| -> EvalResult<String> {
            match t {
                Term::Literal(l) if l.lang.is_none() => Ok(l.value.clone()),
                Term::Iri(i) => Ok(i.clone()),
                _ => Err(EvalError("not a string argument".into())),
            }
        };
        match name {
            "STR" => {
                let t = arg(0)?;
                Ok(Term::Literal(Literal::simple(&t.value())))
            }
            "STRSTARTS" | "STRENDS" | "CONTAINS" => {
                let (a, b) = (str_of(&arg(0)?)?, str_of(&arg(1)?)?);
                Ok(bool_term(match name {
                    "STRSTARTS" => a.starts_with(&b),
                    "STRENDS" => a.ends_with(&b),
                    _ => a.contains(&b),
                }))
            }
            "LCASE" => Ok(Term::Literal(Literal::simple(&str_of(&arg(0)?)?.to_lowercase()))),
            "DATATYPE" => {
                let t = arg(0)?;
                match t {
                    Term::Literal(l) => Ok(Term::Iri(l.datatype_str().to_string())),
                    _ => Err(EvalError("DATATYPE of a non-literal".into())),
                }
            }
            "SUBSTR" => {
                let s = str_of(&arg(0)?)?;
                let start: i64 = num_of(&arg(1)?)? as i64;
                let chars: Vec<char> = s.chars().collect();
                let from = (start.max(1) - 1) as usize;
                let out: String = if args.len() > 2 {
                    let len: i64 = num_of(&arg(2)?)? as i64;
                    chars.iter().skip(from).take(len.max(0) as usize).collect()
                } else {
                    chars.iter().skip(from).collect()
                };
                Ok(Term::Literal(Literal::simple(&out)))
            }
            "REGEX" => {
                let text = str_of(&arg(0)?)?;
                let pattern = str_of(&arg(1)?)?;
                let flags = if args.len() > 2 { str_of(&arg(2)?)? } else { String::new() };
                let found = regex_search(&pattern, &text, flags.contains('i'))
                    .map_err(EvalError)?;
                Ok(bool_term(found))
            }
            _ => Err(EvalError(format!("function {name} is not evaluated"))),
        }
    }

    /// Projection, grouping, ordering, slicing — the solution modifiers.
    fn project(&self, q: &Query, sols: Vec<Binding>) -> EvalResult<(Vec<String>, Vec<Binding>)> {
        let has_agg = q.projection.iter().any(|p| matches!(p, ProjItem::Alias(e, _) if has_agg(e)))
            || q.order_by.iter().any(|k| has_agg(&k.expr));
        let grouped = !q.group_by.is_empty() || has_agg;
        // Each unit: the binding order keys and aliases read from, and the
        // row the projection emits.
        let mut units: Vec<(Binding, Binding)> = Vec::new();
        if grouped {
            let mut groups: Vec<(Vec<Option<Term>>, Vec<Binding>)> = Vec::new();
            for sol in sols {
                let key: Vec<Option<Term>> =
                    q.group_by.iter().map(|v| sol.get(v).cloned()).collect();
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, members)) => members.push(sol),
                    None => groups.push((key, vec![sol])),
                }
            }
            if groups.is_empty() && q.group_by.is_empty() {
                groups.push((Vec::new(), Vec::new()));
            }
            for (key, members) in groups {
                let mut base = Binding::new();
                for (i, v) in q.group_by.iter().enumerate() {
                    if let Some(Some(t)) = key.get(i) {
                        base.insert(v.clone(), t.clone());
                    }
                }
                let mut row = base.clone();
                for item in &q.projection {
                    match item {
                        ProjItem::Var(v) => {
                            if let Some(t) = base.get(v) {
                                row.insert(v.clone(), t.clone());
                            }
                        }
                        ProjItem::Alias(e, v) => {
                            let val = self.eval_group_expr(e, &members, &base)?;
                            row.insert(v.clone(), val);
                        }
                    }
                }
                let mut full = base;
                for (k, v) in &row {
                    full.insert(k.clone(), v.clone());
                }
                units.push((full, row));
            }
        } else {
            for sol in sols {
                let mut row = Binding::new();
                if q.star {
                    row = sol.clone();
                } else {
                    for item in &q.projection {
                        match item {
                            ProjItem::Var(v) => {
                                if let Some(t) = sol.get(v) {
                                    row.insert(v.clone(), t.clone());
                                }
                            }
                            ProjItem::Alias(e, v) => {
                                let val = self.eval_expr(e, &sol, &Ctx::Default)?;
                                row.insert(v.clone(), val);
                            }
                        }
                    }
                }
                let mut full = sol.clone();
                for (k, v) in &row {
                    full.insert(k.clone(), v.clone());
                }
                units.push((full, row));
            }
        }
        // ORDER BY over the full binding (aliases visible).
        if !q.order_by.is_empty() {
            let mut keyed: Vec<(Vec<Option<Term>>, (Binding, Binding))> = Vec::new();
            for u in units {
                let mut keys = Vec::new();
                for k in &q.order_by {
                    keys.push(self.order_key(&k.expr, &u.0, grouped).ok());
                }
                keyed.push((keys, u));
            }
            keyed.sort_by(|a, b| {
                for (i, k) in q.order_by.iter().enumerate() {
                    let ord = cmp_opt(&a.0[i], &b.0[i]);
                    let ord = if k.desc { ord.reverse() } else { ord };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
            units = keyed.into_iter().map(|(_, u)| u).collect();
        }
        let mut rows: Vec<Binding> = units.into_iter().map(|(_, row)| row).collect();
        if q.distinct {
            let mut seen = HashSet::new();
            rows.retain(|r| {
                let mut items: Vec<(&String, &Term)> = r.iter().collect();
                items.sort_by(|a, b| a.0.cmp(b.0));
                seen.insert(format!("{items:?}"))
            });
        }
        if q.offset > 0 {
            rows = rows.into_iter().skip(q.offset).collect();
        }
        if let Some(n) = q.limit {
            rows.truncate(n);
        }
        let vars = if q.star {
            let mut all: Vec<String> = Vec::new();
            for r in &rows {
                for k in r.keys() {
                    if !all.contains(k) {
                        all.push(k.clone());
                    }
                }
            }
            all
        } else {
            q.projection
                .iter()
                .map(|p| match p {
                    ProjItem::Var(v) => v.clone(),
                    ProjItem::Alias(_, v) => v.clone(),
                })
                .collect()
        };
        Ok((vars, rows))
    }

    fn order_key(&self, e: &Expr, full: &Binding, grouped: bool) -> EvalResult<Term> {
        if grouped && has_agg(e) {
            // An aggregate in ORDER BY over a grouped query reads the
            // group's members, which `full` no longer holds; the alias
            // path covers the issued queries (ORDER BY DESC(?n)).
            if let Expr::Var(v) = e {
                return full.get(v).cloned().ok_or_else(|| EvalError("order key unbound".into()));
            }
            if let Expr::Agg(_) = e {
                return Err(EvalError("a bare aggregate ORDER BY key is unsupported".into()));
            }
        }
        self.eval_expr(e, full, &Ctx::Default)
    }

    /// An expression over a group: aggregates compute over the members,
    /// everything else reads the group-key binding.
    fn eval_group_expr(&self, e: &Expr, members: &[Binding], base: &Binding) -> EvalResult<Term> {
        match e {
            Expr::Agg(a) => self.eval_agg(a, members),
            Expr::Func(name, args) => {
                let vals: Vec<Term> = args
                    .iter()
                    .map(|a| self.eval_group_expr(a, members, base))
                    .collect::<EvalResult<Vec<_>>>()?;
                let mut fake = Binding::new();
                let mut names = Vec::new();
                for (i, v) in vals.iter().enumerate() {
                    let n = format!("__a{i}");
                    fake.insert(n.clone(), v.clone());
                    names.push(Expr::Var(n));
                }
                self.eval_func(name, &names, &fake, &Ctx::Default)
            }
            Expr::Cmp(op, a, b) => {
                let x = self.eval_group_expr(a, members, base)?;
                let y = self.eval_group_expr(b, members, base)?;
                Ok(bool_term(compare(*op, &x, &y)?))
            }
            Expr::Not(a) => Ok(bool_term(!ebv(&self.eval_group_expr(a, members, base)?))),
            Expr::And(a, b) => Ok(bool_term(
                ebv(&self.eval_group_expr(a, members, base)?)
                    && ebv(&self.eval_group_expr(b, members, base)?),
            )),
            Expr::Or(a, b) => Ok(bool_term(
                ebv(&self.eval_group_expr(a, members, base)?)
                    || ebv(&self.eval_group_expr(b, members, base)?),
            )),
            _ => self.eval_expr(e, base, &Ctx::Default),
        }
    }

    fn eval_agg(&self, a: &Agg, members: &[Binding]) -> EvalResult<Term> {
        match a {
            Agg::CountStar => Ok(int_term(members.len() as i64)),
            Agg::Count(e, distinct) => {
                let mut n = 0i64;
                let mut seen = HashSet::new();
                for m in members {
                    if let Ok(t) = self.eval_expr(e, m, &Ctx::Default) {
                        if *distinct {
                            if seen.insert(t.to_nt()) {
                                n += 1;
                            }
                        } else {
                            n += 1;
                        }
                    }
                }
                Ok(int_term(n))
            }
            Agg::Min(e) | Agg::Max(e) => {
                let mut best: Option<Term> = None;
                for m in members {
                    if let Ok(t) = self.eval_expr(e, m, &Ctx::Default) {
                        best = Some(match best {
                            None => t,
                            Some(b) => {
                                let ord = term_cmp(&t, &b);
                                if (matches!(a, Agg::Min(_)) && ord == std::cmp::Ordering::Less)
                                    || (matches!(a, Agg::Max(_)) && ord == std::cmp::Ordering::Greater)
                                {
                                    t
                                } else {
                                    b
                                }
                            }
                        });
                    }
                }
                best.ok_or_else(|| EvalError("MIN/MAX over no values".into()))
            }
        }
    }
}

fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg(_) => true,
        Expr::Not(a) => has_agg(a),
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Cmp(_, a, b) => has_agg(a) || has_agg(b),
        Expr::Func(_, args) => args.iter().any(has_agg),
        _ => false,
    }
}

pub fn bool_term(b: bool) -> Term {
    Term::Literal(Literal::typed(if b { "true" } else { "false" }, XSD_BOOLEAN))
}

pub fn int_term(n: i64) -> Term {
    Term::Literal(Literal::typed(&n.to_string(), XSD_INTEGER))
}

fn num_of(t: &Term) -> EvalResult<f64> {
    match t {
        Term::Literal(l) if l.is_numeric() => {
            l.value.parse().map_err(|_| EvalError("not a number".into()))
        }
        _ => Err(EvalError("not a number".into())),
    }
}

/// Effective boolean value (SPARQL's rules, the cases this subset meets).
pub fn ebv(t: &Term) -> bool {
    match t {
        Term::Literal(l) => match l.datatype.as_deref() {
            Some(XSD_BOOLEAN) => l.value == "true" || l.value == "1",
            Some(dt) if l.is_numeric() => l.value.parse::<f64>().map(|n| n != 0.0).unwrap_or(false),
            _ => !l.value.is_empty(),
        },
        _ => true,
    }
}

fn compare(op: CmpOp, x: &Term, y: &Term) -> EvalResult<bool> {
    // RDFterm-equal first: `=` on any two terms is identity (with the
    // literal normalization in Literal's PartialEq doing the RDF 1.2
    // simple-string work).
    if op == CmpOp::Eq {
        return Ok(x == y || value_eq(x, y));
    }
    if op == CmpOp::Ne {
        return Ok(!(x == y || value_eq(x, y)));
    }
    // Ordered comparison: numbers by value, dateTimes by instant, plain
    // strings lexically; anything else falls back to the term order.
    let ord = match (x, y) {
        (Term::Literal(a), Term::Literal(b)) if a.is_numeric() && b.is_numeric() => {
            let (fa, fb) = (num_of(x)?, num_of(y)?);
            fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Term::Literal(a), Term::Literal(b))
            if a.datatype.as_deref() == Some(XSD_DATETIME)
                && b.datatype.as_deref() == Some(XSD_DATETIME) =>
        {
            match (parse_instant(&a.value), parse_instant(&b.value)) {
                (Some(i1), Some(i2)) => i1.cmp(&i2),
                _ => a.value.cmp(&b.value),
            }
        }
        (Term::Literal(a), Term::Literal(b))
            if a.datatype.is_none() && b.datatype.is_none() && a.lang.is_none() && b.lang.is_none() =>
        {
            a.value.cmp(&b.value)
        }
        _ => term_cmp(x, y),
    };
    Ok(match op {
        CmpOp::Gt => ord == std::cmp::Ordering::Greater,
        CmpOp::Lt => ord == std::cmp::Ordering::Less,
        CmpOp::Ge => ord != std::cmp::Ordering::Less,
        CmpOp::Le => ord != std::cmp::Ordering::Greater,
        _ => unreachable!(),
    })
}

/// Value equality for `=`: numeric literals compare by value across
/// integer/decimal; everything else is term identity.
fn value_eq(x: &Term, y: &Term) -> bool {
    match (x, y) {
        (Term::Literal(a), Term::Literal(b)) if a.is_numeric() && b.is_numeric() => {
            match (a.value.parse::<f64>(), b.value.parse::<f64>()) {
                (Ok(fa), Ok(fb)) => fa == fb,
                _ => false,
            }
        }
        _ => false,
    }
}

fn cmp_opt(a: &Option<Term>, b: &Option<Term>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => term_cmp(x, y),
    }
}

// ---------------------------------------------------------------------------
// A small regular-expression engine for SPARQL's REGEX (XPath fn:matches
// semantics: the pattern matches if it matches any substring; `^`/`$`
// anchor). Set-based matching — every node yields the set of end
// positions it can reach — so nested quantifiers cannot blow up the way
// naive backtracking does. Supports literals, `.`, classes with ranges
// and negation, \d \w \s (and negations), groups, alternation, and the
// `* + ? {n,m}` quantifiers.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Re {
    Empty,
    Char(char),
    Any,
    Class { neg: bool, items: Vec<ClassItem> },
    Start,
    End,
    Concat(Vec<Re>),
    Alt(Vec<Re>),
    Repeat { node: Box<Re>, min: u32, max: Option<u32> },
}

#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

pub fn regex_search(pattern: &str, text: &str, ci: bool) -> Result<bool, String> {
    let pat: Vec<char> = if ci {
        pattern.chars().flat_map(|c| c.to_lowercase()).collect()
    } else {
        pattern.chars().collect()
    };
    let txt: Vec<char> = if ci {
        text.chars().flat_map(|c| c.to_lowercase()).collect()
    } else {
        text.chars().collect()
    };
    let mut p = ReParser { cs: pat, at: 0 };
    let re = p.parse_alt()?;
    if p.at != p.cs.len() {
        return Err(format!("regex: unexpected {:?} at {}", p.cs.get(p.at), p.at));
    }
    let mut budget = 400_000usize;
    for start in 0..=txt.len() {
        if !re_match(&re, &txt, start, &mut budget)?.is_empty() {
            return Ok(true);
        }
        if budget == 0 {
            return Err("regex: step budget spent".into());
    }
    }
    Ok(false)
}

struct ReParser {
    cs: Vec<char>,
    at: usize,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.cs.get(self.at).copied()
    }
    fn parse_alt(&mut self) -> Result<Re, String> {
        let mut arms = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.at += 1;
            arms.push(self.parse_concat()?);
        }
        if arms.len() == 1 {
            Ok(arms.pop().unwrap())
        } else {
            Ok(Re::Alt(arms))
        }
    }
    fn parse_concat(&mut self) -> Result<Re, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        Ok(Re::Concat(items))
    }
    fn parse_repeat(&mut self) -> Result<Re, String> {
        let mut node = self.parse_atom()?;
        loop {
            match self.peek() {
                Some('*') => {
                    self.at += 1;
                    node = Re::Repeat { node: Box::new(node), min: 0, max: None };
                }
                Some('+') => {
                    self.at += 1;
                    node = Re::Repeat { node: Box::new(node), min: 1, max: None };
                }
                Some('?') => {
                    self.at += 1;
                    node = Re::Repeat { node: Box::new(node), min: 0, max: Some(1) };
                }
                Some('{') => {
                    let save = self.at;
                    self.at += 1;
                    let mut min = String::new();
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        min.push(self.cs[self.at]);
                        self.at += 1;
                    }
                    let mut max = None;
                    let mut ok = !min.is_empty();
                    if self.peek() == Some(',') {
                        self.at += 1;
                        let mut m = String::new();
                        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            m.push(self.cs[self.at]);
                            self.at += 1;
                        }
                        if !m.is_empty() {
                            max = m.parse().ok();
                        }
                    } else {
                        max = min.parse().ok();
                    }
                    if ok && self.peek() == Some('}') {
                        self.at += 1;
                        let min: u32 = min.parse().map_err(|_| "regex: bad {n,m}")?;
                        node = Re::Repeat { node: Box::new(node), min, max };
                    } else {
                        self.at = save;
                        break;
                    }
                }
                _ => break,
            }
            // A lazy marker changes match order, never existence.
            if self.peek() == Some('?') {
                self.at += 1;
            }
        }
        Ok(node)
    }
    fn parse_atom(&mut self) -> Result<Re, String> {
        match self.peek() {
            Some('(') => {
                self.at += 1;
                if self.peek() == Some('?') {
                    self.at += 1;
                    if self.peek() == Some(':') {
                        self.at += 1;
                    } else {
                        return Err("regex: only (?:...) groups are supported".into());
                    }
                }
                let inner = self.parse_alt()?;
                if self.peek() != Some(')') {
                    return Err("regex: unterminated group".into());
                }
                self.at += 1;
                Ok(inner)
            }
            Some('[') => {
                self.at += 1;
                let neg = if self.peek() == Some('^') {
                    self.at += 1;
                    true
                } else {
                    false
                };
                let mut items = Vec::new();
                let mut first = true;
                loop {
                    match self.peek() {
                        None => return Err("regex: unterminated class".into()),
                        Some(']') if !first => {
                            self.at += 1;
                            break;
                        }
                        _ => {}
                    }
                    first = false;
                    let lo = self.class_char()?;
                    if let ClassItem::Ch(a) = lo {
                        if self.peek() == Some('-')
                            && self.cs.get(self.at + 1).is_some_and(|&c| c != ']')
                        {
                            self.at += 1;
                            let hi = self.class_char()?;
                            if let ClassItem::Ch(b) = hi {
                                items.push(ClassItem::Range(a, b));
                                continue;
                            }
                            return Err("regex: bad class range".into());
                        }
                    }
                    items.push(lo);
                }
                Ok(Re::Class { neg, items })
            }
            Some('.') => {
                self.at += 1;
                Ok(Re::Any)
            }
            Some('^') => {
                self.at += 1;
                Ok(Re::Start)
            }
            Some('$') => {
                self.at += 1;
                Ok(Re::End)
            }
            Some('\\') => {
                self.at += 1;
                Ok(self.escape()?.0)
            }
            Some(c) => {
                self.at += 1;
                Ok(Re::Char(c))
            }
            None => Ok(Re::Empty),
        }
    }
    fn class_char(&mut self) -> Result<ClassItem, String> {
        match self.peek() {
            Some('\\') => {
                self.at += 1;
                Ok(self.escape()?.1)
            }
            Some(c) => {
                self.at += 1;
                Ok(ClassItem::Ch(c))
            }
            None => Err("regex: unterminated class".into()),
        }
    }
    /// An escape: (as an atom, as a class item).
    fn escape(&mut self) -> Result<(Re, ClassItem), String> {
        let c = self.peek().ok_or("regex: trailing backslash")?;
        self.at += 1;
        let lit = |ch: char| (Re::Char(ch), ClassItem::Ch(ch));
        Ok(match c {
            'd' => (Re::Class { neg: false, items: vec![ClassItem::Digit(false)] }, ClassItem::Digit(false)),
            'D' => (Re::Class { neg: false, items: vec![ClassItem::Digit(true)] }, ClassItem::Digit(true)),
            'w' => (Re::Class { neg: false, items: vec![ClassItem::Word(false)] }, ClassItem::Word(false)),
            'W' => (Re::Class { neg: false, items: vec![ClassItem::Word(true)] }, ClassItem::Word(true)),
            's' => (Re::Class { neg: false, items: vec![ClassItem::Space(false)] }, ClassItem::Space(false)),
            'S' => (Re::Class { neg: false, items: vec![ClassItem::Space(true)] }, ClassItem::Space(true)),
            'n' => lit('\n'),
            'r' => lit('\r'),
            't' => lit('\t'),
            other => lit(other),
        })
    }
}

fn class_has(items: &[ClassItem], neg: bool, c: char) -> bool {
    let inside = items.iter().any(|it| match it {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(a, b) => *a <= c && c <= *b,
        ClassItem::Digit(n) => c.is_ascii_digit() != *n,
        ClassItem::Word(n) => (c.is_alphanumeric() || c == '_') != *n,
        ClassItem::Space(n) => c.is_whitespace() != *n,
    });
    inside != neg
}

/// Every end position `re` can reach from `pos` in `text`.
fn re_match(re: &Re, text: &[char], pos: usize, budget: &mut usize) -> Result<HashSet<usize>, String> {
    if *budget == 0 {
        return Err("regex: step budget spent".into());
    }
    *budget -= 1;
    let one = |p: usize| {
        let mut s = HashSet::new();
        s.insert(p);
        s
    };
    Ok(match re {
        Re::Empty => one(pos),
        Re::Start => {
            if pos == 0 {
                one(pos)
            } else {
                HashSet::new()
            }
        }
        Re::End => {
            if pos == text.len() {
                one(pos)
            } else {
                HashSet::new()
            }
        }
        Re::Char(want) => {
            if text.get(pos) == Some(want) {
                one(pos + 1)
            } else {
                HashSet::new()
            }
        }
        Re::Any => {
            if text.get(pos).is_some_and(|c| *c != '\n') {
                one(pos + 1)
            } else {
                HashSet::new()
            }
        }
        Re::Class { neg, items } => {
            if text.get(pos).is_some_and(|c| class_has(items, *neg, *c)) {
                one(pos + 1)
            } else {
                HashSet::new()
            }
        }
        Re::Concat(items) => {
            let mut cur = one(pos);
            for it in items {
                let mut next = HashSet::new();
                for p in cur {
                    next.extend(re_match(it, text, p, budget)?);
                }
                cur = next;
                if cur.is_empty() {
                    break;
                }
            }
            cur
        }
        Re::Alt(arms) => {
            let mut out = HashSet::new();
            for a in arms {
                out.extend(re_match(a, text, pos, budget)?);
            }
            out
        }
        Re::Repeat { node, min, max } => {
            let mut out = HashSet::new();
            let mut cur = one(pos);
            let mut n = 0u32;
            loop {
                if n >= *min {
                    out.extend(cur.iter().copied());
                }
                if max.is_some_and(|m| n >= m) {
                    break;
                }
                let mut next = HashSet::new();
                for p in &cur {
                    next.extend(re_match(node, text, *p, budget)?);
                }
                // An iteration that reaches no new position ends the
                // loop (the empty-match guard).
                let fresh: HashSet<usize> = next.difference(&cur).copied().collect();
                cur = next;
                n += 1;
                if fresh.is_empty() && n >= *min {
                    break;
                }
                if n > 10_000 {
                    break;
                }
            }
            out
        }
    })
}

// ---------------------------------------------------------------------------
// Tests. Fixtures are the stack's own shapes: the outside.rs conformance
// data, an emojikey claim layer as annotate.rs emits it, a memory graph
// as memory_log.rs projects it. Every proving query from the subset
// determination runs here in its issued form.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = "https://repolex.ai/ontology/ravel/";
    const SUB: &str = "https://repolex.ai/ontology/subtexture/";
    const BASE: &str = "https://repolex.ai/";

    fn iri(s: &str) -> Term {
        Term::Iri(s.to_string())
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::simple(s))
    }
    fn dt(s: &str) -> Term {
        Term::Literal(Literal::typed(s, XSD_DATETIME))
    }
    fn int(n: i64) -> Term {
        int_term(n)
    }
    fn prep() -> Prep {
        let mut prop_equiv = HashMap::new();
        for name in ["relatedToId", "author", "owner", "visibility", "readableBy"] {
            prop_equiv.insert(format!("{SUB}{name}"), vec![format!("{R}{name}")]);
        }
        let mut class_subs = HashMap::new();
        class_subs.insert(
            format!("{SUB}Thing"),
            vec![format!("{R}Turn"), format!("{R}Memory")],
        );
        class_subs.insert(
            format!("{SUB}NamedGraph"),
            vec![
                format!("{R}NamedGraph"),
                format!("{R}TranscriptGraph"),
                format!("{R}MemoryGraph"),
            ],
        );
        class_subs.insert(
            format!("{SUB}DefaultGraph"),
            vec![format!("{R}TranscriptGraph"), format!("{R}MemoryGraph")],
        );
        Prep {
            prop_equiv,
            class_subs,
            abstract_prefixes: vec![SUB.to_string()],
        }
    }
    fn run(src: &VecSource, q: &str) -> Outcome {
        let parsed = parse_query(q, BASE).unwrap_or_else(|e| panic!("parse {q}: {e}"));
        evaluate(&parsed, src, &prep()).unwrap_or_else(|e| panic!("eval {q}: {e:?}"))
    }
    fn rows(src: &VecSource, q: &str) -> Vec<Binding> {
        match run(src, q).results {
            Results::Solutions { rows, .. } => rows,
            other => panic!("expected solutions for {q}, got {other:?}"),
        }
    }
    fn val(b: &Binding, v: &str) -> String {
        b.get(v).map(|t| t.value()).unwrap_or_default()
    }

    /// outside.rs's conformance DATA, verbatim in shape.
    fn outside_data() -> VecSource {
        let mut s = VecSource::new();
        let (a, b, m) = (
            Some("https://repolex.ai/ravel/NamedGraph/a"),
            Some("https://repolex.ai/ravel/NamedGraph/b"),
            Some("https://repolex.ai/ravel/NamedGraph/memory-v1"),
        );
        let t1 = iri("https://repolex.ai/ravel/Turn/t1");
        let t2 = iri("https://repolex.ai/ravel/Turn/t2");
        s.push(a, t1.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
        s.push(a, t1.clone(), iri(&format!("{R}role")), lit("user"));
        s.push(b, t2.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
        let mem = iri("https://repolex.ai/ravel/Memory/0-1");
        s.push(m, mem.clone(), iri(RDF_TYPE), iri(&format!("{R}Memory")));
        s.push(m, mem.clone(), iri(&format!("{R}relatedToId")), t1.clone());
        s
    }

    const P: &str = "PREFIX subtexture: <https://repolex.ai/ontology/subtexture/> \
                     PREFIX ravel: <https://repolex.ai/ontology/ravel/> ";

    #[test]
    fn a_query_with_no_graph_sees_every_graph() {
        let s = outside_data();
        assert_eq!(rows(&s, &format!("{P} SELECT ?t WHERE {{ ?t a ravel:Turn }}")).len(), 2);
    }

    #[test]
    fn a_base_class_finds_turns_and_memory_once_each() {
        let s = outside_data();
        let q = format!("{P} SELECT ?x WHERE {{ ?x a subtexture:Thing }}");
        assert_eq!(rows(&s, &q).len(), 3);
    }

    #[test]
    fn a_base_property_finds_the_stored_spelling() {
        let s = outside_data();
        let q = format!("{P} SELECT ?m ?t WHERE {{ ?m subtexture:relatedToId ?t }}");
        assert_eq!(rows(&s, &q).len(), 1);
    }

    #[test]
    fn a_short_address_resolves_against_the_base() {
        let s = outside_data();
        let q = format!("{P} SELECT ?r WHERE {{ <ravel/Turn/t1> ravel:role ?r }}");
        assert_eq!(rows(&s, &q).len(), 1);
        let q = format!("{P} SELECT ?t WHERE {{ GRAPH <ravel/NamedGraph/b> {{ ?t a ravel:Turn }} }}");
        assert_eq!(rows(&s, &q).len(), 1);
    }

    #[test]
    fn a_named_graph_still_narrows_the_question() {
        let s = outside_data();
        let q = format!("{P} SELECT ?t WHERE {{ GRAPH ?g {{ ?t a subtexture:Thing }} }}");
        assert_eq!(rows(&s, &q).len(), 3);
    }

    #[test]
    fn a_fanned_out_name_keeps_its_graph() {
        let s = outside_data();
        let q = format!("{P} SELECT ?g ?t WHERE {{ GRAPH ?g {{ ?t a subtexture:Thing }} }}");
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 3);
        assert!(rs.iter().all(|r| r.get("g").is_some()), "a row lost its graph");
    }

    /// The §5.3 crafted probe's data: ONE subject typed both stored
    /// spellings (ravel:Turn and ravel:Memory) of the abstract class
    /// subtexture:Thing, in ONE graph.
    fn fanout_data() -> VecSource {
        let mut s = VecSource::new();
        let g = Some("https://repolex.ai/ravel/NamedGraph/craft");
        let z9 = iri("https://repolex.ai/ravel/Turn/z9");
        s.push(g, z9.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
        s.push(g, z9, iri(RDF_TYPE), iri(&format!("{R}Memory")));
        s
    }

    #[test]
    fn fanout_duplicates_collapse_within_one_pattern() {
        // Defect #104: without per-pattern dedup this returns 2 rows —
        // one per fanned stored spelling — where calamus returns 1.
        let s = fanout_data();
        let q = format!("{P} SELECT ?s WHERE {{ ?s a subtexture:Thing }}");
        assert_eq!(rows(&s, &q).len(), 1);
    }

    #[test]
    fn fanout_duplicates_collapse_inside_graph() {
        let s = fanout_data();
        let q = format!("{P} SELECT ?g ?s WHERE {{ GRAPH ?g {{ ?s a subtexture:Thing }} }}");
        assert_eq!(rows(&s, &q).len(), 1);
    }

    #[test]
    fn fanout_with_distinct_subjects_per_spelling_keeps_all() {
        // Non-regression: dedup collapses identical maps only — a
        // second subject reachable through just one spelling survives.
        let mut s = fanout_data();
        let g = Some("https://repolex.ai/ravel/NamedGraph/craft");
        let z10 = iri("https://repolex.ai/ravel/Memory/z10");
        s.push(g, z10, iri(RDF_TYPE), iri(&format!("{R}Memory")));
        let q = format!("{P} SELECT ?s WHERE {{ ?s a subtexture:Thing }}");
        assert_eq!(rows(&s, &q).len(), 2);
        let q = format!("{P} SELECT ?g ?s WHERE {{ GRAPH ?g {{ ?s a subtexture:Thing }} }}");
        assert_eq!(rows(&s, &q).len(), 2);
    }

    #[test]
    fn nt_line_push_and_remove_round_trip() {
        let mut s = VecSource::new();
        let line = "<http://ex.org/a> <http://ex.org/name> \"Alice\" <http://ex.org/g> .";
        s.push_nt_line(line).unwrap();
        assert_eq!(s.quads.len(), 1);
        assert_eq!(s.graph_names(), vec!["http://ex.org/g".to_string()]);
        // A triple line (no graph) parses to the default graph.
        s.push_nt_line("<http://ex.org/a> <http://ex.org/name> \"Alice\" .").unwrap();
        assert_eq!(s.quads.len(), 2);
        assert!(s.remove_nt_line(line).unwrap());
        assert_eq!(s.quads.len(), 1);
        assert!(!s.remove_nt_line(line).unwrap());
        assert!(s.push_nt_line("not a quad").is_err());
        assert!(s.push_nt_line("").is_err());
    }

    #[test]
    fn a_name_inside_a_filter_is_reported() {
        let s = outside_data();
        let q = format!("{P} SELECT ?s WHERE {{ ?s ?p ?o FILTER(?p = subtexture:relatedToId) }}");
        let out = run(&s, &q);
        assert_eq!(out.left_as_written, vec![format!("{SUB}relatedToId")]);
        match out.results {
            Results::Solutions { rows, .. } => assert_eq!(rows.len(), 0),
            _ => panic!(),
        }
    }

    #[test]
    fn a_query_that_does_not_parse_is_refused() {
        assert!(parse_query("SELECT WHERE", BASE).is_err());
    }

    #[test]
    fn graph_descriptions_list_under_the_base_class() {
        // The help text's listing query, over described graphs.
        let mut s = VecSource::new();
        for (g, class) in [
            ("https://repolex.ai/ravel/NamedGraph/a", "TranscriptGraph"),
            ("https://repolex.ai/ravel/NamedGraph/optmem", "MemoryGraph"),
        ] {
            s.push(None, iri(g), iri(RDF_TYPE), iri(&format!("{R}{class}")));
            s.push(None, iri(g), iri(&format!("{R}visibility")), lit("private"));
        }
        let q = format!("{P} SELECT ?g WHERE {{ ?g a subtexture:NamedGraph }}");
        assert_eq!(rows(&s, &q).len(), 2);
        let q = format!("{P} SELECT ?g WHERE {{ ?g a subtexture:DefaultGraph }}");
        assert_eq!(rows(&s, &q).len(), 2);
    }

    /// One transcript's spine + one emojikey claim, as graph.rs ingests.
    fn emoji_data() -> VecSource {
        let mut s = VecSource::new();
        let g = Some("https://repolex.ai/ravel/NamedGraph/sess-A");
        let turn = iri("https://repolex.ai/ravel/Turn/e1");
        s.push(g, turn.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
        s.push(g, turn.clone(), iri(&format!("{R}turnId")), lit("e1"));
        s.push(g, turn.clone(), iri(&format!("{R}role")), lit("assistant"));
        s.push(g, turn.clone(), iri(&format!("{R}timestamp")), dt("2026-07-01T00:00:00Z"));
        s.push(g, turn.clone(), iri(&format!("{R}text")), lit("[ME|🧠]~[CONTENT|💻]~[YOU|🎓]"));
        let ann = iri("https://repolex.ai/ravel/Annotation/0123456789abcdef");
        let claim = iri("https://repolex.ai/ravel/Claim/0123456789abcdef");
        let body = iri("https://repolex.ai/ravel/Annotation/0123456789abcdef/body");
        let prop = Term::Triple(Box::new(Triple {
            s: turn.clone(),
            p: iri(&format!("{R}exhibits")),
            o: lit("emojikey/harvest"),
        }));
        s.push(g, claim.clone(), iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies"), prop);
        s.push(g, claim.clone(), iri("http://www.w3.org/ns/prov#wasDerivedFrom"), ann.clone());
        s.push(g, claim.clone(), iri(&format!("{R}detectorName")), lit("emojikey"));
        s.push(g, ann.clone(), iri("http://www.w3.org/ns/oa#hasBody"), body.clone());
        s.push(g, ann.clone(), iri(&format!("{R}textOrigin")), lit("authored"));
        s.push(g, ann.clone(), iri("http://www.w3.org/ns/prov#generatedAtTime"), dt("2026-07-01T00:00:00Z"));
        s.push(g, body.clone(), iri(&format!("{R}sig_me")), lit("🧠"));
        s.push(g, body.clone(), iri(&format!("{R}sig_content")), lit("💻"));
        s.push(g, body.clone(), iri(&format!("{R}sig_you")), lit("🎓"));
        s
    }

    /// I-2, in its issued form (graph.rs query_emojikeys).
    fn emojikey_query(filter: &str) -> String {
        format!(
            r#"
            PREFIX ravel: <{R}>
            PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
            PREFIX oa: <http://www.w3.org/ns/oa#>
            PREFIX prov: <http://www.w3.org/ns/prov#>
            SELECT ?g ?sk ?me ?content ?you ?ts WHERE {{
                GRAPH ?g {{
                    ?claim rdf:reifies <<( ?event ravel:exhibits "emojikey/harvest" )>> ;
                           prov:wasDerivedFrom ?ann .
                    ?ann oa:hasBody ?body .
                    OPTIONAL {{ ?ann ravel:textOrigin ?sk }}
                    OPTIONAL {{ ?ann prov:generatedAtTime ?ts }}
                    ?body ravel:sig_me ?me ; ravel:sig_content ?content ; ravel:sig_you ?you .
                    {filter}
                }}
            }}
            ORDER BY ?g ?ts
            "#
        )
    }

    #[test]
    fn the_emojikey_claim_query_reads_through_the_triple_term() {
        let s = emoji_data();
        let rs = rows(&s, &emojikey_query(""));
        assert_eq!(rs.len(), 1);
        assert_eq!(val(&rs[0], "me"), "🧠");
        assert_eq!(val(&rs[0], "content"), "💻");
        assert_eq!(val(&rs[0], "sk"), "authored");
        assert!(val(&rs[0], "g").ends_with("/NamedGraph/sess-A"));
        let rs = rows(&s, &emojikey_query(r#"FILTER(?sk = "tool_result")"#));
        assert_eq!(rs.len(), 0);
    }

    #[test]
    fn the_stats_battery_counts_what_it_should() {
        let s = outside_data();
        let one = |q: &str| {
            let rs = rows(&s, q);
            assert_eq!(rs.len(), 1, "{q}");
            val(&rs[0], "n")
        };
        assert_eq!(one("SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }"), "5");
        assert_eq!(one("SELECT (COUNT(DISTINCT ?g) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }"), "3");
        let turns = format!(
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(STRSTARTS(STR(?s), \"https://repolex.ai/ravel/Turn/\")) }}"
        );
        assert_eq!(one(&turns), "2");
        assert_eq!(
            one("SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?tt } }"),
            "0"
        );
        let e = emoji_data();
        assert_eq!(
            one_on(&e, "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?tt } }"),
            "1"
        );
        // Top predicates: GROUP BY + aggregate alias + DESC order + LIMIT.
        let rs = rows(
            &s,
            "SELECT ?p (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?p ORDER BY DESC(?n) LIMIT 20",
        );
        assert_eq!(val(&rs[0], "n"), "3", "rdf:type is the commonest predicate");
        assert!(val(&rs[0], "p").ends_with("#type"));
    }

    fn one_on(s: &VecSource, q: &str) -> String {
        let rs = rows(s, q);
        assert_eq!(rs.len(), 1, "{q}");
        val(&rs[0], "n")
    }

    #[test]
    fn dated_counts_and_min_max_read_datetimes() {
        let s = emoji_data();
        let dtf = "FILTER(DATATYPE(?t) = <http://www.w3.org/2001/XMLSchema#dateTime>)";
        let q = format!("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH ?g {{ ?s ?ts ?t }} {dtf} }}");
        assert_eq!(one_on(&s, &q), "2");
        let q = format!("SELECT (MIN(?t) AS ?n) WHERE {{ GRAPH ?g {{ ?s ?p ?t }} {dtf} }}");
        assert_eq!(one_on(&s, &q), "2026-07-01T00:00:00Z");
    }

    #[test]
    fn the_recent_bootstrap_orders_and_limits() {
        // I-1's shape: newest turns store-wide.
        let mut s = VecSource::new();
        for (g, id, ts) in [
            ("g1", "a", "2026-01-01T00:00:00Z"),
            ("g1", "b", "2026-01-03T00:00:00Z"),
            ("g2", "c", "2026-01-02T00:00:00Z"),
        ] {
            let t = iri(&format!("https://repolex.ai/ravel/Turn/{id}"));
            s.push(Some(&format!("https://repolex.ai/ravel/NamedGraph/{g}")), t.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
            s.push(Some(&format!("https://repolex.ai/ravel/NamedGraph/{g}")), t, iri(&format!("{R}timestamp")), dt(ts));
        }
        let q = format!(
            "SELECT ?t ?ts WHERE {{ GRAPH ?g {{ ?t a <{R}Turn> ; <{R}timestamp> ?ts }} }} ORDER BY DESC(?ts) LIMIT 2"
        );
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 2);
        assert!(val(&rs[0], "t").ends_with("/b"));
        assert!(val(&rs[1], "t").ends_with("/c"));
    }

    #[test]
    fn ask_and_exists_and_values_shapes() {
        let s = outside_data();
        // §2.2's ontology-consistency ASK: true when the ontology is in
        // the default graph (the class IRI a subject), false otherwise.
        let mut onto = VecSource::new();
        onto.push(None, iri(&format!("{R}Turn")), iri(RDF_TYPE), iri("http://www.w3.org/2002/07/owl#Class"));
        let q = format!("ASK {{ <{R}Turn> a ?k }}");
        match run(&onto, &q).results {
            Results::Boolean(b) => assert!(b),
            _ => panic!(),
        }
        match run(&s, &q).results {
            Results::Boolean(b) => assert!(!b),
            _ => panic!(),
        }
        // graph.rs test ASK with GRAPH + FILTER.
        let q = format!(
            "ASK {{ GRAPH ?g {{ ?t <{RDF_TYPE}> <{R}Turn> ; <{R}timestamp> ?ts . FILTER(STRSTARTS(STR(?t), \"https://repolex.ai/ravel/Turn/\")) }} }}"
        );
        match run(&emoji_data(), &q).results {
            Results::Boolean(b) => assert!(b),
            _ => panic!(),
        }
        // I-7: VALUES chunks of cited turns.
        let q = format!(
            "SELECT (COUNT(DISTINCT ?t) AS ?n) WHERE {{ VALUES ?t {{ <https://repolex.ai/ravel/Turn/t1> <https://repolex.ai/ravel/Turn/nope> }} GRAPH ?g {{ ?t a <{R}Turn> }} }}"
        );
        assert_eq!(one_on(&s, &q), "1");
        // I-6: NOT EXISTS over a dangling edge — a block pointing at a
        // Memory node that is not there counts; a sound one does not.
        let mut s6 = outside_data();
        s6.push(
            Some("https://repolex.ai/ravel/NamedGraph/memory-v1"),
            iri("https://repolex.ai/ravel/Memory/0-3"),
            iri(&format!("{R}relatedToId")),
            iri("https://repolex.ai/ravel/Memory/9-9"),
        );
        let q = format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ ?m <{R}relatedToId> ?c . FILTER(STRSTARTS(STR(?c), \"https://repolex.ai/ravel/Memory/\")) FILTER NOT EXISTS {{ ?c a <{R}Memory> }} }}"
        );
        assert_eq!(one_on(&s6, &q), "1");
        assert_eq!(one_on(&s, &q), "0");
        let q2 = format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ ?s <{R}role> ?c . FILTER EXISTS {{ ?s a <{R}Turn> }} }}"
        );
        assert_eq!(one_on(&s, &q2), "1");
    }

    #[test]
    fn numeric_legs_filter_as_numbers() {
        // annotate.rs's wave test: typed integers, `> 5` a real compare.
        let mut s = VecSource::new();
        for (id, mag) in [("b1", 3), ("b2", 8)] {
            s.push(
                None,
                iri(&format!("https://repolex.ai/ravel/Annotation/{id}/body")),
                iri(&format!("{R}sig_magnitude")),
                int(mag),
            );
        }
        let q = format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ ?body <{R}sig_magnitude> ?m . FILTER(?m > 5) }}"
        );
        assert_eq!(one_on(&s, &q), "1");
    }

    /// A memory graph as memory_log.rs projects it, plus one turn.
    fn memory_data() -> VecSource {
        let mut s = VecSource::new();
        let g = Some("https://repolex.ai/ravel/NamedGraph/optmem");
        let note = iri("https://repolex.ai/ravel/Memory/m0");
        s.push(g, note.clone(), iri(RDF_TYPE), iri(&format!("{R}Memory")));
        s.push(g, note.clone(), iri(&format!("{R}memoryLevel")), int(0));
        s.push(g, note.clone(), iri(&format!("{R}logStart")), int(0));
        s.push(g, note.clone(), iri(&format!("{R}windowStart")), dt("2026-07-01T00:00:00Z"));
        s.push(g, note.clone(), iri(&format!("{R}memoryText")), lit("the GOAT ate Cake at dawn"));
        s.push(g, note.clone(), iri(&format!("{R}relatedToId")), iri("https://repolex.ai/ravel/Turn/e1"));
        let tg = Some("https://repolex.ai/ravel/NamedGraph/sess-A");
        let turn = iri("https://repolex.ai/ravel/Turn/e1");
        s.push(tg, turn.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
        s.push(tg, turn.clone(), iri(&format!("{R}timestamp")), dt("2026-07-01T00:00:00Z"));
        s.push(tg, turn, iri(&format!("{R}text")), lit("the GOAT ate Cake at dawn, said the transcript"));
        s
    }

    #[test]
    fn the_recall_templates_run() {
        let s = memory_data();
        let hit = r#"FILTER(CONTAINS(LCASE(?x), "cake") && REGEX(?x, "(^|[^A-Za-z0-9_])cake($|[^A-Za-z0-9_])", "i"))"#;
        let q = format!(
            "{P} SELECT ?i ?ts ?x WHERE {{ GRAPH <ravel/NamedGraph/optmem> {{ ?m ravel:memoryLevel 0 ; ravel:logStart ?i ; ravel:windowStart ?ts ; ravel:memoryText ?x . {hit} }} }} ORDER BY ?i"
        );
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 1);
        assert_eq!(val(&rs[0], "i"), "0");
        // Turn counts with MIN/MAX over STR().
        let q = format!(
            "{P} SELECT (COUNT(DISTINCT ?u) AS ?n) (MIN(STR(?ts)) AS ?first) WHERE {{ GRAPH ?g {{ ?u a ravel:Turn ; ravel:text ?x ; ravel:timestamp ?ts . FILTER(CONTAINS(LCASE(?x), \"cake\")) }} }}"
        );
        let rs = rows(&s, &q);
        assert_eq!(val(&rs[0], "n"), "1");
        assert_eq!(val(&rs[0], "first"), "2026-07-01T00:00:00Z");
    }

    #[test]
    fn the_help_examples_run() {
        // Example 1: authors, counted (GROUP BY + DESC aggregate order).
        let mut s = VecSource::new();
        for (id, author) in [("a", "lux"), ("b", "lux"), ("c", "lux-on-fable")] {
            let t = iri(&format!("https://repolex.ai/ravel/Turn/{id}"));
            s.push(None, t.clone(), iri(RDF_TYPE), iri(&format!("{R}Turn")));
            s.push(None, t, iri(&format!("{R}author")), lit(author));
        }
        let q = format!(
            "{P} SELECT ?author (COUNT(?t) AS ?turns) WHERE {{ ?t a ravel:Turn ; ravel:author ?author }} GROUP BY ?author ORDER BY DESC(?turns)"
        );
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 2);
        assert_eq!(val(&rs[0], "author"), "lux");
        assert_eq!(val(&rs[0], "turns"), "2");
        // Example 3: the two-GRAPH join (optmem note × its turns).
        let s = memory_data();
        let q = format!(
            "{P} SELECT ?note ?ts (SUBSTR(?said, 1, 80) AS ?start) WHERE {{ GRAPH <ravel/NamedGraph/optmem> {{ ?m ravel:memoryLevel 0 ; ravel:logStart 0 ; ravel:memoryText ?note ; ravel:relatedToId ?t }} GRAPH ?g {{ ?t ravel:timestamp ?ts ; ravel:text ?said }} }} ORDER BY ?ts"
        );
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 1);
        assert_eq!(val(&rs[0], "note"), "the GOAT ate Cake at dawn");
    }

    #[test]
    fn out_of_subset_constructs_refuse_by_name() {
        let cases: &[(&str, &str)] = &[
            ("SELECT ?s WHERE { ?s ?p ?o . { ?s ?p ?o } UNION { ?s ?p ?o } }", "UNION"),
            ("SELECT ?s WHERE { { SELECT ?s WHERE { ?s ?p ?o } } }", "subqueries"),
            ("SELECT ?s WHERE { ?s ?p ?o MINUS { ?s ?p ?x } }", "MINUS"),
            ("SELECT ?s WHERE { ?s ?p ?o } GROUP BY ?s HAVING(?s > 1)", "HAVING"),
            ("SELECT REDUCED ?s WHERE { ?s ?p ?o }", "SELECT REDUCED"),
            ("SELECT ?s WHERE { SERVICE <https://x/> { ?s ?p ?o } }", "SERVICE"),
            ("SELECT ?s WHERE { ?s <https://x/p>/<https://x/q> ?o }", "property paths"),
            ("SELECT ?s WHERE { ?s ^<https://x/p> ?o }", "property paths"),
            ("SELECT ?s WHERE { ?s <https://x/p>+ ?o }", "property paths"),
            ("SELECT ?s WHERE { ?s ?p ?o FILTER(CONCAT(?s, \"x\") = \"y\") }", "function CONCAT"),
            ("SELECT ?s WHERE { ?s ?p ?o FILTER(?o + 1 > 2) }", "arithmetic expressions"),
            ("SELECT ?s WHERE { ?s ?p ?o FILTER(?p IN (<https://x/a>)) }", "IN"),
            ("INSERT DATA { <https://x/a> <https://x/b> <https://x/c> }", "SPARQL Update"),
            ("SELECT ?s WHERE { ?s ?p [ ?q ?o ] }", "blank nodes"),
        ];
        for (q, want) in cases {
            match parse_query(q, BASE) {
                Err(e) => {
                    assert_eq!(e.construct.as_deref(), Some(*want), "query {q}: {e}");
                }
                Ok(_) => panic!("query should refuse ({want}): {q}"),
            }
        }
    }

    #[test]
    fn fringe_constructs_run() {
        let s = outside_data();
        // CONSTRUCT: the claim layer's reifies triples, out as triples.
        let e = emoji_data();
        let q = format!(
            "{P} CONSTRUCT {{ ?claim <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?tt }} WHERE {{ GRAPH ?g {{ ?claim <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?tt }} }}"
        );
        match run(&e, &q).results {
            Results::Triples(ts) => assert_eq!(ts.len(), 1),
            other => panic!("{other:?}"),
        }
        // DESCRIBE: subject closure of t1 (type + role), nothing else.
        let q = "DESCRIBE <https://repolex.ai/ravel/Turn/t1>";
        match run(&s, q).results {
            Results::Triples(ts) => {
                assert_eq!(ts.len(), 2, "{ts:?}");
                assert!(ts.iter().all(|t| t.s == iri("https://repolex.ai/ravel/Turn/t1")));
            }
            other => panic!("{other:?}"),
        }
        // SELECT * + DISTINCT + OFFSET.
        let rs = rows(&s, &format!("{P} SELECT * WHERE {{ ?s ?p ?o }}"));
        assert_eq!(rs.len(), 5);
        let rs = rows(&s, &format!("{P} SELECT DISTINCT ?p WHERE {{ ?s ?p ?o }}"));
        assert_eq!(rs.len(), 3);
        let rs = rows(
            &s,
            &format!("{P} SELECT ?t WHERE {{ ?t a ravel:Turn }} ORDER BY ?t LIMIT 1 OFFSET 1"),
        );
        assert_eq!(rs.len(), 1);
        assert!(val(&rs[0], "t").ends_with("/t2"));
        // BIND + the comparison family's other half.
        let rs = rows(
            &s,
            &format!("{P} SELECT ?r ?short WHERE {{ <ravel/Turn/t1> ravel:role ?r . BIND(SUBSTR(?r, 1, 2) AS ?short) FILTER(?r != \"admin\") }}"),
        );
        assert_eq!(val(&rs[0], "short"), "us");
        let rs = rows(&emoji_data(), &format!("{P} SELECT ?b WHERE {{ ?b ravel:sig_me ?m FILTER(?m < \"🎓\") }}"));
        assert_eq!(rs.len(), 0, "lexical < on plain strings: 🧠 (U+1F9E0) is not < 🎓 (U+1F393)");
    }

    #[test]
    fn nquads_round_trip_with_a_triple_term() {
        let text = "<https://repolex.ai/ravel/Claim/h> <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( <https://repolex.ai/ravel/Turn/e1> <https://repolex.ai/ontology/ravel/exhibits> \"emojikey/harvest\" )>> <https://repolex.ai/ravel/NamedGraph/sess-A> .\n";
        let s = VecSource::from_nquads(text).unwrap();
        let q = format!(
            "SELECT ?event WHERE {{ GRAPH ?g {{ ?claim <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( ?event <{R}exhibits> \"emojikey/harvest\" )>> }} }}"
        );
        let rs = rows(&s, &q);
        assert_eq!(rs.len(), 1);
        assert!(val(&rs[0], "event").ends_with("/Turn/e1"));
        // And the term serializes back to the same N-Quads shape.
        let quad = &s.quads[0];
        assert_eq!(
            quad.o.to_nt(),
            "<<( <https://repolex.ai/ravel/Turn/e1> <https://repolex.ai/ontology/ravel/exhibits> \"emojikey/harvest\" )>>"
        );
    }

    #[test]
    fn regex_engine_basics() {
        assert!(regex_search("(^|[^A-Za-z0-9_])cake($|[^A-Za-z0-9_])", "the Cake, yes", true).unwrap());
        assert!(!regex_search("(^|[^A-Za-z0-9_])cake($|[^A-Za-z0-9_])", "cupcake", true).unwrap());
        assert!(regex_search("^a.c$", "abc", false).unwrap());
        assert!(!regex_search("^a.c$", "a\nc", false).unwrap());
        assert!(regex_search("x{2,3}y", "axxxy", false).unwrap());
        assert!(!regex_search("x{2,3}y", "axy", false).unwrap());
        assert!(regex_search("[0-9]+", "abc42", false).unwrap());
    }

    #[test]
    fn instants_order_across_zones() {
        assert!(parse_instant("2026-01-01T01:00:00+01:00") < parse_instant("2026-01-01T00:00:01Z").map(|x| x + 3_600_000));
        assert_eq!(
            parse_instant("2026-01-01T01:00:00+01:00"),
            parse_instant("2026-01-01T00:00:00Z")
        );
    }
}
