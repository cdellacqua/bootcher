//! Release housekeeping for bootcher's own development.
//!
//! A dev-only CLI (never published, see `publish = false`) that the `release`
//! recipe in the Justfile calls to bump the single source-of-truth version in
//! the workspace `Cargo.toml`. It edits `[workspace.package].version` in place
//! with `toml_edit` (format- and comment-preserving) and prints the resulting
//! version to stdout so the recipe can capture it for the git tag.
//!
//! ```text
//! cargo run -p housekeeper -- bump patch|minor|major
//! cargo run -p housekeeper -- set 1.2.3
//! ```

use std::{env, fs, process::ExitCode};

use anyhow::{Context, Result, bail};
use semver::Version;

/// Path to the workspace manifest, relative to the directory the tool runs from.
/// The `release` recipe runs from the Justfile's directory (the repo root), so a
/// bare relative path resolves to the workspace `Cargo.toml`.
const MANIFEST: &str = "Cargo.toml";

fn main() -> ExitCode {
	match run() {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			eprintln!("housekeeper: {e:#}");
			ExitCode::FAILURE
		}
	}
}

fn run() -> Result<()> {
	let args: Vec<String> = env::args().skip(1).collect();
	let next = match args.as_slice() {
		[cmd, kind] if cmd == "bump" => bump(current()?, kind)?,
		[cmd, raw] if cmd == "set" => {
			raw.parse().with_context(|| format!("`{raw}` is not a valid semver version"))?
		}
		_ => bail!("usage: housekeeper (bump patch|minor|major | set X.Y.Z)"),
	};
	write(&next)?;
	// Stdout is the contract with the Justfile: just the new version, nothing
	// else, so the recipe can do `version=$(cargo run -p housekeeper -- ...)`.
	println!("{next}");
	Ok(())
}

/// Apply a `patch`/`minor`/`major` increment, zeroing the lower-precedence
/// fields per semver semantics (a minor bump resets patch, a major resets both),
/// and clearing any pre-release/build metadata.
fn bump(mut v: Version, kind: &str) -> Result<Version> {
	match kind {
		"major" => {
			v.major += 1;
			v.minor = 0;
			v.patch = 0;
		}
		"minor" => {
			v.minor += 1;
			v.patch = 0;
		}
		"patch" => v.patch += 1,
		other => bail!("unknown bump `{other}` (expected patch, minor, or major)"),
	}
	v.pre = semver::Prerelease::EMPTY;
	v.build = semver::BuildMetadata::EMPTY;
	Ok(v)
}

/// Read and parse `[workspace.package].version` from the manifest.
fn current() -> Result<Version> {
	let doc = read()?;
	let raw = doc["workspace"]["package"]["version"]
		.as_str()
		.context("`[workspace.package].version` is missing or not a string in Cargo.toml")?;
	raw.parse().with_context(|| format!("`{raw}` in Cargo.toml is not a valid semver version"))
}

/// Write `version` back into `[workspace.package].version`, preserving the rest
/// of the manifest's formatting and comments.
fn write(version: &Version) -> Result<()> {
	let mut doc = read()?;
	doc["workspace"]["package"]["version"] = toml_edit::value(version.to_string());
	fs::write(MANIFEST, doc.to_string()).context("writing Cargo.toml")
}

fn read() -> Result<toml_edit::DocumentMut> {
	fs::read_to_string(MANIFEST)
		.context("reading Cargo.toml (run from the repo root)")?
		.parse()
		.context("parsing Cargo.toml")
}
