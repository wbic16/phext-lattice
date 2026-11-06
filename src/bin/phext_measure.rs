//! Print a funnel report from the phext-edit measurement log.
//!
//! Usage: phext-measure [directory]

use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
phext-measure — funnel report for phext-edit ad measurement

Usage: phext-measure [directory]

The directory holds measure.jsonl (PHEXT_DATA_DIR, or /var/lib/phext-edit
when omitted). Rotated measure.jsonl.1 through measure.jsonl.3 are included.

Counts are unique sessions, filed under the utm_campaign and utm_source on
each session's earliest note: visits (landing), demo starts, sign-in clicks,
sign-ins, and first file.
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let dir = match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some(path) => PathBuf::from(path),
        None => phext_lattice::measure::configured_dir(),
    };
    if args.next().is_some() {
        eprintln!("phext-measure: expected one directory");
        return ExitCode::from(2);
    }
    match phext_lattice::measure::summarize(&dir) {
        Ok(report) => {
            print!("{}", phext_lattice::measure::render(&dir, &report));
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("phext-measure: {err}");
            ExitCode::from(1)
        }
    }
}
