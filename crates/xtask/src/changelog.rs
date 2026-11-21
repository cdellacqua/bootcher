//! Release notes from conventional-commit subjects.
//!
//! `cargo xtask release` renders the notes for the commits since the previous
//! final-release `vX.Y.Z` tag and stores them as the body of the annotated release tag; the release
//! workflow reads the tag message back and publishes it as the GitHub release
//! description. Keeping the notes in the tag means CI needs no Rust toolchain to
//! produce them, and `git show vX.Y.Z` carries the same changelog offline.
//!
//! Grouping follows the subject line, `type(scope)!: description`. A `!` or a
//! `BREAKING CHANGE:`/`BREAKING-CHANGE:` footer files the commit under breaking
//! changes regardless of its type (the footer even for a non-conventional
//! subject). Merge commits and the `release vX.Y.Z` bump commits are left out;
//! other subjects that aren't conventional land under "Other changes" rather
//! than being dropped.

use anyhow::Result;
use xshell::{Shell, cmd};

/// A commit as read from `git log`: abbreviated hash, subject, and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Commit {
	pub(crate) hash: String,
	pub(crate) subject: String,
	pub(crate) body: String,
}

/// The parts of a conventional subject the notes are grouped and rendered by.
#[derive(Debug, PartialEq, Eq)]
struct Parsed<'a> {
	kind: &'a str,
	scope: Option<&'a str>,
	breaking: bool,
	description: &'a str,
}

/// Parse `type(scope)!: description`. `None` when the subject isn't
/// conventional: no `: ` separator, an empty or non-alphanumeric type, or an
/// unclosed scope.
fn parse(subject: &str) -> Option<Parsed<'_>> {
	let (head, description) = subject.split_once(": ")?;
	let description = description.trim();
	let (head, breaking) = match head.strip_suffix('!') {
		Some(head) => (head, true),
		None => (head, false),
	};
	let (kind, scope) = match head.split_once('(') {
		Some((kind, rest)) => (kind, Some(rest.strip_suffix(')')?)),
		None => (head, None),
	};
	let well_formed = |s: &str| {
		!s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
	};
	if !well_formed(kind) || scope.is_some_and(str::is_empty) || description.is_empty() {
		return None;
	}
	Some(Parsed { kind, scope, breaking, description })
}

/// Whether the body carries a breaking-change footer.
fn has_breaking_footer(body: &str) -> bool {
	body.lines().any(|l| l.starts_with("BREAKING CHANGE:") || l.starts_with("BREAKING-CHANGE:"))
}

/// The commits `xtask release` itself creates, which say nothing a reader of
/// the notes needs.
fn is_release_bump(subject: &str) -> bool {
	subject.strip_prefix("release v").is_some_and(|v| v.parse::<semver::Version>().is_ok())
}

/// Section headings, in rendering order. Breaking changes always come first;
/// the internal kinds are folded into a collapsed "Other changes" block so the
/// user-facing sections stay short.
const BREAKING: &str = "⚠️ Breaking changes";
const SECTIONS: &[(&str, &[&str])] = &[
	("Features", &["feat"]),
	("Bug fixes", &["fix"]),
	("Performance", &["perf"]),
	("Documentation", &["docs"]),
];
const OTHER: &str = "Other changes";

/// Render the notes for `commits` (newest first, as `git log` lists them) as
/// Markdown. Empty when there's nothing worth listing.
pub(crate) fn render(commits: &[Commit]) -> String {
	let mut breaking = Vec::new();
	let mut sections: Vec<Vec<String>> = vec![Vec::new(); SECTIONS.len()];
	let mut other = Vec::new();

	for c in commits {
		if is_release_bump(&c.subject) {
			continue;
		}
		let footer = has_breaking_footer(&c.body);
		let Some(p) = parse(&c.subject) else {
			// A breaking footer still files it as breaking, subject as written.
			let line = format!("- {} ({})", c.subject.trim(), c.hash);
			if footer {
				breaking.push(line);
			} else {
				other.push(line);
			}
			continue;
		};
		let scope = p.scope.map(|s| format!("**{s}:** ")).unwrap_or_default();
		if p.breaking || footer {
			// The type is kept here since this section mixes them.
			breaking.push(format!("- {}: {scope}{} ({})", p.kind, p.description, c.hash));
			continue;
		}
		let line = format!("- {scope}{} ({})", p.description, c.hash);
		match SECTIONS.iter().position(|(_, kinds)| kinds.contains(&p.kind)) {
			Some(i) => sections[i].push(line),
			None => other.push(format!("- {}: {scope}{} ({})", p.kind, p.description, c.hash)),
		}
	}

	let headed = std::iter::once((BREAKING, &breaking))
		.chain(SECTIONS.iter().map(|(title, _)| *title).zip(&sections))
		.filter(|(_, lines)| !lines.is_empty())
		.map(|(title, lines)| format!("## {title}\n\n{}\n", lines.join("\n")));
	// GitHub renders <details> collapsed; the blank lines around the list are
	// what make it parse as Markdown inside the HTML block.
	let collapsed = (!other.is_empty()).then(|| {
		format!("<details>\n<summary>{OTHER}</summary>\n\n{}\n\n</details>\n", other.join("\n"))
	});
	headed.chain(collapsed).collect::<Vec<_>>().join("\n")
}

/// The most recent final-release tag (`vX.Y.Z`) reachable from `rev`, or `None`
/// before the first release.
///
/// Prerelease and build-metadata tags (`v1.0.0-rc.1`, `v1.0.0+x`) are skipped,
/// so a final release's notes span everything since the previous final release
/// rather than only what landed after its last rc. `describe` takes globs, not
/// regexes, hence excluding any `-`/`+` rather than matching the digits.
pub(crate) fn previous_tag(sh: &Shell, rev: &str) -> Option<String> {
	cmd!(sh, "git describe --tags --abbrev=0 --match v* --exclude v*-* --exclude v*+* {rev}")
		.quiet()
		.ignore_stderr()
		.read()
		.ok()
		.map(|t| t.trim().to_owned())
		.filter(|t| !t.is_empty())
}

/// The non-merge commits in `since..until` (all of `until`'s history when
/// `since` is `None`), newest first.
pub(crate) fn commits(sh: &Shell, since: Option<&str>, until: &str) -> Result<Vec<Commit>> {
	let range = match since {
		Some(since) => format!("{since}..{until}"),
		None => until.to_owned(),
	};
	// Unit/record separators can't appear in a commit message, so they split
	// fields and records unambiguously even across multi-line bodies.
	let format = "--format=%h%x1f%s%x1f%b%x1e";
	let log = cmd!(sh, "git log --no-merges {format} {range}").quiet().read()?;
	Ok(log
		.split('\x1e')
		.filter_map(|record| {
			let mut fields = record.trim_start_matches('\n').splitn(3, '\x1f');
			let hash = fields.next()?.trim();
			if hash.is_empty() {
				return None;
			}
			Some(Commit {
				hash: hash.to_owned(),
				subject: fields.next().unwrap_or_default().to_owned(),
				body: fields.next().unwrap_or_default().to_owned(),
			})
		})
		.collect())
}

/// Notes for everything after the latest `v*` tag reachable from `HEAD`.
pub(crate) fn unreleased(sh: &Shell) -> Result<String> {
	let since = previous_tag(sh, "HEAD");
	Ok(render(&commits(sh, since.as_deref(), "HEAD")?))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn c(subject: &str) -> Commit {
		Commit { hash: "abc1234".into(), subject: subject.into(), body: String::new() }
	}

	#[test]
	fn parses_type_scope_and_bang() {
		assert_eq!(
			parse("feat(cli)!: drop the old flag"),
			Some(Parsed {
				kind: "feat",
				scope: Some("cli"),
				breaking: true,
				description: "drop the old flag"
			})
		);
		assert_eq!(
			parse("fix: handle empty input"),
			Some(Parsed {
				kind: "fix",
				scope: None,
				breaking: false,
				description: "handle empty input"
			})
		);
	}

	#[test]
	fn rejects_non_conventional_subjects() {
		for s in [
			"Merge pull request #1 from x/y",
			"release v0.2.7",
			"feat(: x",
			"feat(): x",
			"fe at: x",
			"feat:",
		] {
			assert_eq!(parse(s), None, "{s}");
		}
	}

	#[test]
	fn renders_sections_in_order_and_skips_release_bumps() {
		let notes = render(&[
			c("test: cover the thing"),
			c("fix: second fix"),
			c("release v0.2.7"),
			c("feat(sign): add keys"),
			c("feat!: rename the command"),
			c("Tweak something"),
		]);
		assert_eq!(
			notes,
			"## ⚠️ Breaking changes\n\n- feat: rename the command (abc1234)\n\n\
			 ## Features\n\n- **sign:** add keys (abc1234)\n\n\
			 ## Bug fixes\n\n- second fix (abc1234)\n\n\
			 <details>\n<summary>Other changes</summary>\n\n\
			 - test: cover the thing (abc1234)\n- Tweak something (abc1234)\n\n</details>\n"
		);
	}

	#[test]
	fn breaking_footer_counts_as_breaking() {
		let mut commit = c("fix: change the default");
		commit.body = "Details.\n\nBREAKING CHANGE: the default flipped\n".into();
		assert!(
			render(&[commit]).starts_with("## ⚠️ Breaking changes\n\n- fix: change the default")
		);
	}

	#[test]
	fn breaking_footer_on_a_non_conventional_subject_counts_as_breaking() {
		let mut commit = c("Rework the config format");
		commit.body = "BREAKING-CHANGE: old configs must be migrated\n".into();
		assert_eq!(
			render(&[commit]),
			"## ⚠️ Breaking changes\n\n- Rework the config format (abc1234)\n"
		);
	}

	#[test]
	fn nothing_to_list_renders_empty() {
		assert_eq!(render(&[c("release v1.0.0")]), "");
		assert_eq!(render(&[]), "");
	}
}
