//! phext-sparql — the lattice SPARQL subset evaluator as a process.
//!
//! The line protocol exists so callers in any language (the ravel
//! backend's QuadSource glue is Python) can use the evaluator without
//! linking the library or depending on a serialization stack:
//!
//!   phext-sparql <quads.nq> <query.rq> <base-iri> [prep.txt]
//!
//! prep.txt lines (tab-separated): `equiv <asked-iri> <stored-iri>`,
//! `subclass <asked-iri> <stored-iri>`, `abstract <iri-prefix>` — the
//! caller's ontology preparation, as data.
//!
//! Output, on stdout:
//!   NOTE\t<iri>                      (zero or more: names used as written)
//!   SOLUTIONS\t<var>\t<var>...        then one tab-separated row per
//!                                     solution, terms in N-Triples form,
//!                                     an empty field for an unbound var
//!   BOOLEAN\t<true|false>
//!   TRIPLES                           then one N-Triples line per triple
//! A named refusal exits 3 printing `REFUSED\t<construct>\t<message>`;
//! any other failure exits 2 printing `ERROR\t<message>`.

use std::process::ExitCode;

use phext_lattice::sparql::{evaluate, parse_query, Prep, Results, VecSource};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args.len() > 5 {
        eprintln!("usage: phext-sparql <quads.nq> <query.rq> <base-iri> [prep.txt]");
        return ExitCode::from(2);
    }
    let run = || -> Result<String, (u8, String)> {
        let nq = std::fs::read_to_string(&args[1]).map_err(|e| (2u8, format!("quads: {e}")))?;
        let rq = std::fs::read_to_string(&args[2]).map_err(|e| (2u8, format!("query: {e}")))?;
        let src = VecSource::from_nquads(&nq).map_err(|e| (2u8, e))?;
        let mut prep = Prep::default();
        if args.len() == 5 {
            let text = std::fs::read_to_string(&args[4]).map_err(|e| (2u8, format!("prep: {e}")))?;
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                match f.as_slice() {
                    ["equiv", asked, stored] => prep
                        .prop_equiv
                        .entry(asked.to_string())
                        .or_default()
                        .push(stored.to_string()),
                    ["subclass", asked, stored] => prep
                        .class_subs
                        .entry(asked.to_string())
                        .or_default()
                        .push(stored.to_string()),
                    ["abstract", prefix] => prep.abstract_prefixes.push(prefix.to_string()),
                    _ => return Err((2u8, format!("prep: bad line {line:?}"))),
                }
            }
        }
        let query = parse_query(&rq, &args[3]).map_err(|e| match e.construct {
            Some(c) => (3u8, format!("REFUSED\t{c}\t{}", e.message)),
            None => (2u8, format!("ERROR\t{}", e.message)),
        })?;
        let outcome = evaluate(&query, &src, &prep).map_err(|e| (2u8, format!("ERROR\t{e}")))?;
        let mut out = String::new();
        for name in &outcome.left_as_written {
            out.push_str(&format!("NOTE\t{name}\n"));
        }
        match outcome.results {
            Results::Solutions { vars, rows } => {
                out.push_str("SOLUTIONS");
                for v in &vars {
                    out.push('\t');
                    out.push_str(v);
                }
                out.push('\n');
                for row in rows {
                    let fields: Vec<String> = vars
                        .iter()
                        .map(|v| row.get(v).map(|t| t.to_nt()).unwrap_or_default())
                        .collect();
                    out.push_str(&fields.join("\t"));
                    out.push('\n');
                }
            }
            Results::Boolean(b) => out.push_str(&format!("BOOLEAN\t{b}\n")),
            Results::Triples(triples) => {
                out.push_str("TRIPLES\n");
                for t in triples {
                    out.push_str(&format!("{} {} {} .\n", t.s.to_nt(), t.p.to_nt(), t.o.to_nt()));
                }
            }
        }
        Ok(out)
    };
    match run() {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err((code, msg)) => {
            println!("{msg}");
            ExitCode::from(code)
        }
    }
}
