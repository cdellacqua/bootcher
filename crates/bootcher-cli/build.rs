//! Stamp `bootcher --version` with the commit it was built from.
//!
//! `Cargo.toml` carries the last release's version until a release stamps the next
//! one, so a build of an unreleased commit (`cargo install --git … --branch …`)
//! would report the previous release. When the build runs inside a git work tree,
//! this appends `git describe --tags --always` (`0.2.7 (v0.2.7-5-gcbc5700)`, or
//! `0.2.7 (git cbc5700)` in a checkout without tags), unless it names exactly that
//! version's tag. Outside git (a source tarball, a container
//! build without `.git`) the version is the bare `Cargo.toml` one.

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
	let out = Command::new("git").args(args).output().ok()?;
	let s = String::from_utf8(out.stdout).ok()?;
	(out.status.success() && !s.trim().is_empty()).then(|| s.trim().to_owned())
}

fn main() {
	let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
	let describe = git(&["describe", "--tags", "--always"]);

	// Re-run when HEAD moves: a checkout (HEAD), a commit on the current branch (its
	// ref, loose or packed), or a new tag (packed-refs, refs/tags).
	if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) {
		for rel in ["HEAD", "packed-refs", "refs/tags"] {
			println!("cargo:rerun-if-changed={}", dir.join(rel).display());
		}
		if let Some(head) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
			println!("cargo:rerun-if-changed={}", dir.join(head).display());
		}
	} else {
		println!("cargo:rerun-if-changed=build.rs");
	}

	// A checkout without tags (cargo's own git checkouts fetch none) describes as a
	// bare short hash: label it.
	let full = match describe {
		Some(d) if d == format!("v{version}") => version,
		Some(d) if d.starts_with('v') => format!("{version} ({d})"),
		Some(d) => format!("{version} (git {d})"),
		None => version,
	};
	println!("cargo:rustc-env=BOOTCHER_VERSION={full}");
}
