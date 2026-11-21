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

use crate::{cargo, changelog, workspace_root};

/// Absolute path to the workspace manifest. Anchored to the workspace root
/// rather than the process CWD so it resolves the same from a `cargo xtask` run
/// (which inherits the caller's directory) and from `cargo test` (which uses the
/// package root).
fn manifest() -> PathBuf {
	workspace_root().join("Cargo.toml")
}

/// Cut a release: bump the workspace version, commit, tag `vX.Y.Z` (its
/// message carrying the changelog since the previous final release), and push —
/// which triggers the release/image jobs in `.github/workflows/release.yml`
/// (they're gated on `refs/tags/v*`, whatever branch the tagged commit is on).
///
/// `bump` is `patch`/`minor`/`major`/`rc`, an explicit semver version (`X.Y.Z`,
/// or a pre-release such as `X.Y.Z-rc.1`), or `snap`. All but `snap` release
/// from a clean `main` and push the bump commit with the tag; see [`bumped`] for
/// how the keywords treat a current pre-release version.
///
/// `snap` publishes a throwaway pre-release of whatever HEAD is, on any branch:
/// see [`snap_version`] for the version and [`snapshot`] for how it stays off
/// the branch. `level` (`patch` by default) picks the base it's a snapshot of,
/// and is rejected for every other bump.
pub(crate) fn release(sh: &Shell, bump: &str, level: Option<&str>) -> Result<()> {
	if !cmd!(sh, "git status --porcelain").read()?.trim().is_empty() {
		bail!("working tree is dirty — commit or stash first");
	}
	let snap = bump == "snap";
	if !snap && let Some(level) = level {
		bail!("`{level}` only applies to `snap` (got bump `{bump}`)");
	}
	if !snap {
		let branch = cmd!(sh, "git rev-parse --abbrev-ref HEAD").read()?;
		if branch.trim() != "main" {
			bail!(
				"not on main (HEAD is `{}`) — only `snap` releases from other branches",
				branch.trim()
			);
		}
	}

	// Resolve the target version *before* touching the manifest: an explicit
	// `X.Y.Z` is parsed as-is, a keyword is applied to the current version. Both
	// are pure, so the tag collision below is caught while the tree is still
	// clean — a failure there leaves nothing to `git checkout`.
	let next = match bump {
		"snap" => {
			let sha = cmd!(sh, "git rev-parse --short HEAD").read()?;
			snap_version(current()?, level.unwrap_or("patch"), sha.trim())?
		}
		"patch" | "minor" | "major" | "rc" => bumped(current()?, bump)?,
		raw => raw.parse().with_context(|| {
			format!(
				"`{raw}` is neither a bump (patch|minor|major|rc|snap) nor a valid semver version"
			)
		})?,
	};
	let tag = format!("v{next}");
	let reference = format!("refs/tags/{tag}");
	// --verify --quiet: exits non-zero, silently, when the tag doesn't exist.
	let probe = cmd!(sh, "git rev-parse --verify --quiet {reference}");
	if probe.ignore_stdout().quiet().run().is_ok() {
		bail!("tag {tag} already exists");
	}

	// A snapshot is a test build of a branch, not what `bootcher init` users get,
	// so a stale scaffold pin shouldn't stop one.
	if !snap {
		scaffold_release_is_current()?;
	}

	// Rendered before the bump commit exists; it would be skipped anyway.
	let notes = changelog::unreleased(sh)?;
	// The tag's subject line is a title; its body is the changelog, which the
	// release workflow publishes as the GitHub release description.
	let tag_message = if notes.is_empty() {
		format!("Release {tag}\n")
	} else {
		format!("Release {tag}\n\n{notes}")
	};

	if snap {
		snapshot(sh, &next, &tag, &tag_message)?;
		cmd!(sh, "git push origin {reference}").run()?;
	} else {
		commit_and_tag(sh, &next, &tag, &tag_message)?;
		cmd!(sh, "git push origin main {reference}").run()?;
	}
	println!("pushed {tag} — watch the release/image jobs in CI");
	Ok(())
}

/// Bump the manifest to `version`, commit it on the current HEAD, and tag that
/// commit `tag` with `tag_message`.
fn commit_and_tag(sh: &Shell, version: &Version, tag: &str, tag_message: &str) -> Result<()> {
	write(version)?;
	// Restore Cargo.lock too: bumping the version dirties the workspace members'
	// lock entries. Offline and best-effort — a failure here only means the lock
	// keeps the old version strings, which the commit below picks up anyway.
	let cargo = cargo();
	let _ = cmd!(sh, "{cargo} update --workspace --offline").ignore_stdout().ignore_stderr().run();

	let commit_message = format!("release {tag}");
	cmd!(sh, "git commit -am {commit_message}").run()?;
	// --cleanup=verbatim: the default `strip` mode treats lines starting with `#`
	// as comments, which would delete every Markdown heading in the notes. Passed
	// on stdin (`-F -`) so the message reaches git byte for byte.
	cmd!(sh, "git tag -a --cleanup=verbatim -F - {tag}").stdin(tag_message).run()?;
	println!("{tag_message}");
	Ok(())
}

/// [`commit_and_tag`] on a detached HEAD, then return to where we started. The
/// bump commit is reachable only through its tag, so the branch never carries a
/// snapshot version into `main`, and parallel branches' snapshots can't conflict
/// on the version line. The way back is taken whether or not the commit
/// succeeds; `-f` only discards the bump's own edits, since the tree was clean.
fn snapshot(sh: &Shell, version: &Version, tag: &str, tag_message: &str) -> Result<()> {
	let origin = match cmd!(sh, "git symbolic-ref --quiet --short HEAD").ignore_stderr().read() {
		Ok(branch) => branch,
		Err(_) => cmd!(sh, "git rev-parse HEAD").read()?,
	};
	cmd!(sh, "git checkout --quiet --detach").run()?;
	let made = commit_and_tag(sh, version, tag, tag_message);
	let back = cmd!(sh, "git checkout --quiet --force {origin}").run();
	made?;
	back.with_context(|| format!("returning to `{origin}` after tagging {tag}"))?;
	Ok(())
}

/// The version of a `snap` release of commit `sha`: `level` applied to `current`
/// as by [`bumped`], with a `snap.g<sha>` pre-release. The hash makes it unique
/// per commit with no counter to coordinate, so branches never collide. The `g`
/// (as in `git describe`) keeps the identifier alphanumeric: an all-digit hash
/// would be a numeric identifier, invalid with a leading zero.
fn snap_version(current: Version, level: &str, sha: &str) -> Result<Version> {
	if !matches!(level, "patch" | "minor" | "major") {
		bail!("unknown snap level `{level}` (expected patch, minor, or major)");
	}
	let mut v = bumped(current, level)?;
	v.pre = semver::Prerelease::new(&format!("snap.g{sha}"))
		.with_context(|| format!("`{sha}` doesn't make a valid pre-release identifier"))?;
	Ok(v)
}

/// Refuse to release while the scaffold pins an older Fedora release than the current
/// stable one. `bootcher init` falls back to the scaffold's release when it can't
/// reach the registry, so a stale pin there would scaffold offline projects onto an
/// old release.
fn scaffold_release_is_current() -> Result<()> {
	use bootcher_core::fedora;
	let scaffold = fedora::scaffold_release()?;
	let current = fedora::current_release().context("looking up the current Fedora release")?;
	if scaffold < current {
		bail!(
			"the scaffold Containerfile pins Fedora {scaffold}, but Fedora {current} is the \
			 current release: bump `ARG {}` in crates/bootcher-core/scaffold/Containerfile first",
			fedora::RELEASE_ARG,
		);
	}
	Ok(())
}

/// Apply a bump keyword to `v`, following semver precedence.
///
/// From a final version, `patch`/`minor`/`major` increment that field and zero
/// the lower-precedence ones (a minor bump resets patch, a major resets both).
/// From a pre-release they *finish* it instead when it already has the shape
/// the bump would produce — `1.3.0-rc.2` + `patch` or `minor` is `1.3.0`, and
/// `2.0.0-rc.1` + `major` is `2.0.0` — since that release precedes its
/// pre-releases' successor, not the version after it. Otherwise they increment
/// as usual (`1.3.1-rc.1` + `minor` is `1.4.0`). Build metadata and any
/// pre-release tag are always cleared.
///
/// `rc` cuts the next release candidate: `X.Y.Z-rc.N` becomes `X.Y.Z-rc.N+1`,
/// and a final `X.Y.Z` starts `X.Y.(Z+1)-rc.1`. An rc for a minor or major
/// release starts with an explicit version (`X.Y.0-rc.1`), after which `rc`
/// continues it. Pre-releases not shaped `rc.N` are rejected rather than guessed
/// at.
fn bumped(mut v: Version, kind: &str) -> Result<Version> {
	let pre = !v.pre.is_empty();
	match kind {
		"major" if pre && v.minor == 0 && v.patch == 0 => {}
		"major" => {
			v.major += 1;
			v.minor = 0;
			v.patch = 0;
		}
		"minor" if pre && v.patch == 0 => {}
		"minor" => {
			v.minor += 1;
			v.patch = 0;
		}
		"patch" if pre => {}
		"patch" => v.patch += 1,
		"rc" => {
			let n = if pre {
				let n = v
					.pre
					.as_str()
					.strip_prefix("rc.")
					.and_then(|n| n.parse::<u64>().ok())
					.with_context(|| {
						format!(
							"current version {v} has a pre-release that isn't `rc.N`; \
							 pass the next version explicitly"
						)
					})?;
				n + 1
			} else {
				v.patch += 1;
				1
			};
			v.pre = semver::Prerelease::new(&format!("rc.{n}"))?;
			v.build = semver::BuildMetadata::EMPTY;
			return Ok(v);
		}
		other => bail!("unknown bump `{other}` (expected patch, minor, major, or rc)"),
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
	fn bumps_clear_build_metadata() {
		assert_eq!(bumped(v("1.2.3+build.5"), "patch").unwrap(), v("1.2.4"));
		assert_eq!(bumped(v("1.2.3-rc.1+build.5"), "patch").unwrap(), v("1.2.3"));
	}

	#[test]
	fn bumps_finish_a_matching_pre_release() {
		assert_eq!(bumped(v("1.3.0-rc.2"), "patch").unwrap(), v("1.3.0"));
		assert_eq!(bumped(v("1.3.0-rc.2"), "minor").unwrap(), v("1.3.0"));
		assert_eq!(bumped(v("2.0.0-rc.1"), "major").unwrap(), v("2.0.0"));
		assert_eq!(bumped(v("1.2.4-alpha"), "patch").unwrap(), v("1.2.4"));
	}

	#[test]
	fn bumps_past_a_pre_release_of_a_smaller_shape() {
		assert_eq!(bumped(v("1.3.1-rc.1"), "minor").unwrap(), v("1.4.0"));
		assert_eq!(bumped(v("1.3.0-rc.1"), "major").unwrap(), v("2.0.0"));
	}

	#[test]
	fn rc_continues_or_starts_a_candidate() {
		assert_eq!(bumped(v("1.3.0-rc.1"), "rc").unwrap(), v("1.3.0-rc.2"));
		assert_eq!(bumped(v("1.3.0-rc.9+b"), "rc").unwrap(), v("1.3.0-rc.10"));
		assert_eq!(bumped(v("1.2.3"), "rc").unwrap(), v("1.2.4-rc.1"));
	}

	#[test]
	fn snap_versions_carry_the_commit() {
		assert_eq!(snap_version(v("1.2.3"), "patch", "1a2b3c4").unwrap(), v("1.2.4-snap.g1a2b3c4"));
		assert_eq!(snap_version(v("1.2.3"), "minor", "1a2b3c4").unwrap(), v("1.3.0-snap.g1a2b3c4"));
		// Snapshots of an rc's base, not past it.
		assert_eq!(
			snap_version(v("1.3.0-rc.1"), "patch", "1a2b3c4").unwrap(),
			v("1.3.0-snap.g1a2b3c4")
		);
		// All digits with a leading zero: invalid as a bare numeric identifier.
		assert_eq!(snap_version(v("1.2.3"), "patch", "0123456").unwrap(), v("1.2.4-snap.g0123456"));
	}

	#[test]
	fn snap_rejects_other_levels() {
		assert!(snap_version(v("1.2.3"), "rc", "1a2b3c4").is_err());
	}

	#[test]
	fn rc_rejects_other_pre_releases() {
		assert!(bumped(v("1.3.0-beta.1"), "rc").is_err());
		assert!(bumped(v("1.3.0-rc1"), "rc").is_err());
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
