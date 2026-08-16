//! Cutting a release: bump the workspace version, commit, tag, push.
//!
//! The version lives in exactly one place — `[workspace.package].version` in the
//! workspace `Cargo.toml` — and is edited in place with `toml_edit` so the
//! manifest's formatting and comments survive. Everything else is plain git, so
//! this depends on no `cargo-edit`/`cargo-release` tooling.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{fs, path::PathBuf};
use xshell::{Shell, cmd};

use crate::{cargo, workspace_root};

/// Absolute path to the workspace manifest. Anchored to the workspace root
/// rather than the process CWD so it resolves the same from a `cargo xtask` run
/// (which inherits the caller's directory) and from `cargo test` (which uses the
/// package root).
fn manifest() -> PathBuf {
	workspace_root().join("Cargo.toml")
}

/// Cut a release from a clean `main`: bump the workspace version, commit, tag
/// `vX.Y.Z`, and push both — which triggers the release/image jobs in
/// `.github/workflows/release.yml` (they're gated on `refs/tags/v*`).
///
/// `bump` is `patch`/`minor`/`major` or an explicit `X.Y.Z`.
pub(crate) fn release(sh: &Shell, bump: &str) -> Result<()> {
	if !cmd!(sh, "git status --porcelain").read()?.trim().is_empty() {
		bail!("working tree is dirty — commit or stash first");
	}
	let branch = cmd!(sh, "git rev-parse --abbrev-ref HEAD").read()?;
	if branch.trim() != "main" {
		bail!("not on main (HEAD is `{}`)", branch.trim());
	}

	// Resolve the target version *before* touching the manifest: an explicit
	// `X.Y.Z` is parsed as-is, a keyword is applied to the current version. Both
	// are pure, so the tag collision below is caught while the tree is still
	// clean — a failure there leaves nothing to `git checkout`.
	let next = match bump {
		"patch" | "minor" | "major" => bumped(current()?, bump)?,
		raw => raw.parse().with_context(|| {
			format!("`{raw}` is neither a bump (patch|minor|major) nor a valid semver version")
		})?,
	};
	let tag = format!("v{next}");
	let reference = format!("refs/tags/{tag}");
	// --verify --quiet: exits non-zero, silently, when the tag doesn't exist.
	let probe = cmd!(sh, "git rev-parse --verify --quiet {reference}");
	if probe.ignore_stdout().quiet().run().is_ok() {
		bail!("tag {tag} already exists");
	}

	write(&next)?;
	// Restore Cargo.lock too: bumping the version dirties the workspace members'
	// lock entries. Offline and best-effort — a failure here only means the lock
	// keeps the old version strings, which the commit below picks up anyway.
	let cargo = cargo();
	let _ = cmd!(sh, "{cargo} update --workspace --offline").ignore_stdout().ignore_stderr().run();

	let commit_message = format!("release {tag}");
	let tag_message = format!("Release {tag}");
	cmd!(sh, "git commit -am {commit_message}").run()?;
	cmd!(sh, "git tag -a {tag} -m {tag_message}").run()?;
	cmd!(sh, "git push origin main {tag}").run()?;
	println!("pushed {tag} — watch the release/image jobs in CI");
	Ok(())
}

/// Apply a `patch`/`minor`/`major` increment, zeroing the lower-precedence
/// fields per semver semantics (a minor bump resets patch, a major resets both),
/// and clearing any pre-release/build metadata.
fn bumped(mut v: Version, kind: &str) -> Result<Version> {
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
	fs::write(manifest(), doc.to_string()).context("writing Cargo.toml")
}

fn read() -> Result<toml_edit::DocumentMut> {
	let path = manifest();
	fs::read_to_string(&path)
		.with_context(|| format!("reading {}", path.display()))?
		.parse()
		.with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn v(s: &str) -> Version {
		s.parse().unwrap()
	}

	#[test]
	fn bumps_zero_lower_precedence_fields() {
		assert_eq!(bumped(v("1.2.3"), "patch").unwrap(), v("1.2.4"));
		assert_eq!(bumped(v("1.2.3"), "minor").unwrap(), v("1.3.0"));
		assert_eq!(bumped(v("1.2.3"), "major").unwrap(), v("2.0.0"));
	}

	#[test]
	fn bumps_clear_pre_release_and_build_metadata() {
		assert_eq!(bumped(v("1.2.3-rc.1+build.5"), "patch").unwrap(), v("1.2.4"));
	}

	#[test]
	fn unknown_bump_is_rejected() {
		assert!(bumped(v("1.2.3"), "nightly").is_err());
	}

	/// Guards the workspace-root anchoring *and* the manifest shape: a moved
	/// xtask crate or a restructured `[workspace.package]` would otherwise only
	/// surface halfway through a release.
	#[test]
	fn workspace_version_is_readable() {
		current().expect("`[workspace.package].version` should be readable");
	}
}
