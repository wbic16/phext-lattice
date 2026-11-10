//! Bake this build's identity into the binary as `PHEXT_LATTICE_BUILD`.
//!
//! Every served page footer carries the stamp, so a bug report can name
//! the exact build it was seen on: `git describe` output — the nearest
//! release tag, how many commits past it, and the commit itself (e.g.
//! `v1.0.0-5-g9458cb10`, or exactly `v1.0.0` on the tag). A hand-kept
//! version string would drift from the build actually running; this one
//! cannot, because it is read from the tree being compiled, at compile
//! time.
//!
//! Sources, in order:
//! 1. `PHEXT_LATTICE_BUILD` in the build environment — the container
//!    build passes it as a build arg, because its context has no `.git`.
//! 2. `git describe --tags --always --dirty` in this checkout — the
//!    host deploy (deploy/deploy.sh) builds inside one, so it needs no
//!    configuration at all.
//! 3. `v<CARGO_PKG_VERSION>+unknown` — no tag and no commit: the stamp
//!    says so rather than inventing one.

use std::process::Command;

/// A stamp is one short line of tag/hash characters. Anything else is
/// rejected: the stamp is handed to rustc as an env value and is later
/// served into page footers unescaped.
fn acceptable(stamp: &str) -> bool {
    !stamp.is_empty()
        && stamp.len() <= 128
        && stamp
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | '~'))
}

fn from_env() -> Option<String> {
    let stamp = std::env::var("PHEXT_LATTICE_BUILD").ok()?.trim().to_string();
    acceptable(&stamp).then_some(stamp)
}

fn from_git() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stamp = String::from_utf8(out.stdout).ok()?.trim().to_string();
    acceptable(&stamp).then_some(stamp)
}

fn main() {
    println!("cargo:rerun-if-env-changed=PHEXT_LATTICE_BUILD");
    // A commit moves the branch ref, not HEAD itself, and a pull can
    // land through packed refs — watch all of them, or the stamp goes
    // stale in exactly the rebuild-after-pull flow the deploy uses.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    let stamp = from_env().or_else(from_git).unwrap_or_else(|| {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0".to_string());
        format!("v{version}+unknown")
    });
    println!("cargo:rustc-env=PHEXT_LATTICE_BUILD={stamp}");
}
