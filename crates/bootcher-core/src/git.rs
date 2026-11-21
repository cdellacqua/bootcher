//! The git operations the release commands need: where HEAD is and whether the
//! tree is clean, release tags, and temporary worktrees of a release's commit for
//! a refresh build. (Build provenance has its own best-effort reader in
//! [`crate::context::GitProvenance`]; everything here is strict.)

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Run `git -C <dir> <args>`, returning trimmed stdout, or `Ok(None)` on a non-zero
/// exit (the caller decides what that means).
fn try_git(dir: &Path, args: &[&str]) -> Result<Option<String>> {
	let mut full: Vec<OsString> = vec!["-C".into(), dir.into()];
	full.extend(args.iter().map(OsString::from));
	let out = duct::cmd("git", full)
		.env("LC_ALL", "C")
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
		.context("running git")?;
	Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// Run `git -C <dir> <args>`, failing with git's own message.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
	let mut full: Vec<OsString> = vec!["-C".into(), dir.into()];
	full.extend(args.iter().map(OsString::from));
	let out = duct::cmd("git", full)
		.env("LC_ALL", "C")
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
		.context("running git")?;
	if !out.status.success() {
		let stderr = String::from_utf8_lossy(&out.stderr);
		let line = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no message");
		bail!("`git {}` failed: {line}", args.join(" "));
	}
	Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The commit HEAD is at, or `None` when `dir` isn't in a git work tree (or the
/// repo has no commit yet).
///
/// # Errors
///
/// Returns an error if git can't be run.
pub(crate) fn head(dir: &Path) -> Result<Option<String>> {
	if try_git(dir, &["rev-parse", "--is-inside-work-tree"])?.as_deref() != Some("true") {
		return Ok(None);
	}
	try_git(dir, &["rev-parse", "--verify", "-q", "HEAD"])
}

/// Whether the work tree at `dir` has no uncommitted changes (untracked files
/// included, as in the build provenance).
///
/// # Errors
///
/// Returns an error if `git status` fails.
pub(crate) fn is_clean(dir: &Path) -> Result<bool> {
	Ok(git(dir, &["status", "--porcelain"])?.is_empty())
}

/// The commit tag `tag` points at, or `None` when there's no such tag.
///
/// # Errors
///
/// Returns an error if git can't be run.
pub(crate) fn tag_commit(dir: &Path, tag: &str) -> Result<Option<String>> {
	try_git(dir, &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}^{{commit}}")])
}

/// The tags pointing at HEAD.
///
/// # Errors
///
/// Returns an error if git can't be run.
pub(crate) fn tags_at_head(dir: &Path) -> Result<Vec<String>> {
	Ok(try_git(dir, &["tag", "--points-at", "HEAD"])?
		.unwrap_or_default()
		.lines()
		.map(str::to_owned)
		.filter(|l| !l.is_empty())
		.collect())
}

/// Whether `tag` is a valid git tag name.
///
/// # Errors
///
/// Returns an error if git can't be run.
pub(crate) fn valid_tag_name(dir: &Path, tag: &str) -> Result<bool> {
	Ok(try_git(dir, &["check-ref-format", &format!("refs/tags/{tag}")])?.is_some())
}

/// Create the annotated tag `tag` at HEAD.
///
/// # Errors
///
/// Returns an error if `git tag` fails.
pub(crate) fn create_tag(dir: &Path, tag: &str, message: &str) -> Result<()> {
	git(dir, &["tag", "-a", tag, "-m", message]).map(drop)
}

/// The remote to push a tag to: the current branch's upstream remote, else
/// `origin`, else `None`.
///
/// # Errors
///
/// Returns an error if git can't be run.
pub(crate) fn push_remote(dir: &Path) -> Result<Option<String>> {
	if let Some(branch) = current_branch(dir)?
		&& let Some(remote) = try_git(dir, &["config", &format!("branch.{branch}.remote")])?
		&& !remote.is_empty()
		&& remote != "."
	{
		return Ok(Some(remote));
	}
	let remotes = try_git(dir, &["remote"])?.unwrap_or_default();
	Ok(remotes.lines().any(|r| r == "origin").then(|| "origin".to_owned()))
}

/// The current branch, or `None` on a detached HEAD.
fn current_branch(dir: &Path) -> Result<Option<String>> {
	Ok(try_git(dir, &["symbolic-ref", "--short", "-q", "HEAD"])?.filter(|b| !b.is_empty()))
}

/// Push `tag` to `remote`.
///
/// # Errors
///
/// Returns an error if the push fails.
pub(crate) fn push_tag(dir: &Path, remote: &str, tag: &str) -> Result<()> {
	git(dir, &["push", remote, &format!("refs/tags/{tag}")]).map(drop)
}

/// A temporary `git worktree` of one commit, removed (and pruned) on drop. A
/// refresh builds a release's recorded commit in one, under that commit's own
/// config, without touching the checkout it was started from. Gitignored or
/// generated build inputs don't exist in it.
pub(crate) struct Worktree {
	repo: PathBuf,
	dir: tempfile::TempDir,
}

impl Worktree {
	/// Check `commit` out (detached) into a fresh temporary worktree of the
	/// repository at `repo`, fetching it by SHA from the push remote when it isn't
	/// local, and initialise its submodules.
	///
	/// # Errors
	///
	/// Returns an error if the commit can't be found or fetched, or the worktree
	/// can't be created.
	pub(crate) fn add(repo: &Path, commit: &str) -> Result<Self> {
		let have = |c: &str| try_git(repo, &["cat-file", "-e", &format!("{c}^{{commit}}")]);
		if have(commit)?.is_none() {
			let remote = push_remote(repo)?.with_context(|| {
				format!(
					"commit {commit} isn't in this clone and there's no remote to fetch it from"
				)
			})?;
			git(repo, &["fetch", "--quiet", &remote, commit]).with_context(|| {
				format!("commit {commit} isn't in this clone; fetching it from {remote} failed")
			})?;
		}
		let dir = tempfile::Builder::new()
			.prefix(&format!("bootcher-{}-", &commit[..12.min(commit.len())]))
			.tempdir()
			.context("creating a temp dir for the worktree")?;
		let path = dir.path().to_str().context("temp dir path is not UTF-8")?;
		git(repo, &["worktree", "add", "--quiet", "--detach", path, commit])?;
		let wt = Self { repo: repo.to_path_buf(), dir };
		if wt.path().join(".gitmodules").exists() {
			git(wt.path(), &["submodule", "update", "--init", "--recursive", "--quiet"])?;
		}
		Ok(wt)
	}

	/// The worktree's directory.
	pub(crate) fn path(&self) -> &Path {
		self.dir.path()
	}
}

impl Drop for Worktree {
	fn drop(&mut self) {
		if let Some(path) = self.dir.path().to_str() {
			let _ = try_git(&self.repo, &["worktree", "remove", "--force", path]);
		}
		let _ = try_git(&self.repo, &["worktree", "prune"]);
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	/// Run git in a test, asserting success.
	pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
		git(dir, args).unwrap_or_else(|e| panic!("{e:#}"))
	}

	/// A fresh repo with a local identity and one commit.
	pub(crate) fn repo() -> tempfile::TempDir {
		let d = tempfile::tempdir().unwrap();
		run(d.path(), &["init", "-q", "-b", "main"]);
		run(d.path(), &["config", "user.email", "t@example.com"]);
		run(d.path(), &["config", "user.name", "Test"]);
		run(d.path(), &["config", "commit.gpgsign", "false"]);
		run(d.path(), &["config", "tag.gpgsign", "false"]);
		std::fs::write(d.path().join("f"), "1\n").unwrap();
		run(d.path(), &["add", "-A"]);
		run(d.path(), &["commit", "-qm", "one"]);
		d
	}

	#[test]
	fn head_clean_and_tags() {
		let r = repo();
		let head1 = head(r.path()).unwrap().unwrap();
		assert!(is_clean(r.path()).unwrap());
		std::fs::write(r.path().join("g"), "untracked\n").unwrap();
		assert!(!is_clean(r.path()).unwrap(), "untracked files make the tree dirty");
		std::fs::remove_file(r.path().join("g")).unwrap();
		assert_eq!(tag_commit(r.path(), "v1").unwrap(), None);
		create_tag(r.path(), "v1", "release v1").unwrap();
		assert_eq!(tag_commit(r.path(), "v1").unwrap().as_deref(), Some(head1.as_str()));
		assert_eq!(tags_at_head(r.path()).unwrap(), ["v1"]);
		assert!(valid_tag_name(r.path(), "v1.4").unwrap());
		assert!(!valid_tag_name(r.path(), "a..b").unwrap());
		let outside = tempfile::tempdir().unwrap();
		assert_eq!(head(outside.path()).unwrap(), None);
	}

	#[test]
	fn push_remote_prefers_upstream_then_origin() {
		let bare = tempfile::tempdir().unwrap();
		run(bare.path(), &["init", "-q", "--bare"]);
		let r = repo();
		assert_eq!(push_remote(r.path()).unwrap(), None);
		run(r.path(), &["remote", "add", "origin", bare.path().to_str().unwrap()]);
		assert_eq!(push_remote(r.path()).unwrap().as_deref(), Some("origin"));
		run(r.path(), &["remote", "add", "up", bare.path().to_str().unwrap()]);
		run(r.path(), &["push", "-q", "-u", "up", "main"]);
		assert_eq!(push_remote(r.path()).unwrap().as_deref(), Some("up"));
		create_tag(r.path(), "v2", "v2").unwrap();
		push_tag(r.path(), "up", "v2").unwrap();
		assert!(run(bare.path(), &["tag"]).contains("v2"));
	}

	#[test]
	fn worktree_checks_out_a_commit_and_cleans_up() {
		let r = repo();
		let first = head(r.path()).unwrap().unwrap();
		std::fs::write(r.path().join("f"), "2\n").unwrap();
		run(r.path(), &["commit", "-qam", "two"]);
		let path = {
			let wt = Worktree::add(r.path(), &first).unwrap();
			assert_eq!(std::fs::read_to_string(wt.path().join("f")).unwrap(), "1\n");
			wt.path().to_path_buf()
		};
		assert!(!path.exists());
		assert!(!run(r.path(), &["worktree", "list"]).contains(path.to_str().unwrap()));
		assert!(Worktree::add(r.path(), "0123456789abcdef0123456789abcdef01234567").is_err());
	}
}
