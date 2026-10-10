//! The user's `known_hosts`, as `takeover` needs to edit it: a reinstall gives the
//! host fresh SSH host keys, so the entry ssh recorded for the stock OS no longer
//! matches. Everything here defers to OpenSSH's own view of the world rather than
//! re-implementing its lookup rules:
//!
//! - **Which entry, in which file.** `ssh -G` with the remote's full argv (its
//!   `ssh_opts` included) reports the effective `hostkeyalias`, `hostname`, `port`
//!   and `userknownhostsfile` — so a `Host` alias, a `HostKeyAlias`, a non-default
//!   port (`[host]:port`) or a per-remote `UserKnownHostsFile` all resolve to the
//!   name ssh itself looks up.
//! - **Reading and removing entries.** `ssh-keygen -F` / `-R`, which understand
//!   hashed (`HashKnownHosts`) lines.
//! - **The new key's line.** Captured by connecting once with a throwaway
//!   `UserKnownHostsFile` under `accept-new`: ssh writes the line exactly as it
//!   would have written it to the real file (same name, same hashing), and that
//!   line is what gets appended.

use crate::ssh::Ssh;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Where ssh records a remote's host key: the entry name (`host`, `[host]:port`, or
/// the `HostKeyAlias`) and the first `UserKnownHostsFile`, the one ssh writes to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
	pub(crate) name: String,
	pub(crate) file: PathBuf,
}

/// One `known_hosts` line for an entry: the raw line (kept verbatim, so a hashed
/// name stays hashed when re-appended) and the key it carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Line {
	pub(crate) raw: String,
	pub(crate) key: HostKey,
}

/// A host public key: its type (`ssh-ed25519`, …) and base64 blob.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HostKey {
	kind: String,
	blob: String,
}

impl HostKey {
	/// The `SHA256:…` fingerprint `ssh` and `ssh-keygen -l` print, prefixed with the
	/// key type. A blob that isn't valid base64 falls back to showing the type alone.
	pub(crate) fn fingerprint(&self) -> String {
		match STANDARD.decode(&self.blob) {
			Ok(bytes) => {
				format!("{} SHA256:{}", self.kind, STANDARD_NO_PAD.encode(Sha256::digest(bytes)))
			}
			Err(_) => format!("{} (unparseable key)", self.kind),
		}
	}
}

/// Resolve the `known_hosts` [`Entry`] ssh uses for `ssh`, via `ssh -G` over the
/// same argv every real call gets.
///
/// # Errors
///
/// Returns an error if `ssh -G` fails or its output lacks the expected keys.
pub(crate) fn resolve(ssh: &Ssh) -> Result<Entry> {
	let argv = ssh.argv(&["-G"], "true");
	let (program, rest) = argv.split_first().expect("non-empty argv");
	let out = duct::cmd(program, rest)
		.stdin_null()
		.read()
		.with_context(|| format!("resolving the ssh config for {}", ssh.host()))?;
	parse_ssh_g(&out).with_context(|| format!("parsing `ssh -G` output for {}", ssh.host()))
}

fn parse_ssh_g(out: &str) -> Result<Entry> {
	let get =
		|key: &str| out.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ').map(str::trim));
	let name = match get("hostkeyalias") {
		Some(alias) if !alias.is_empty() && alias != "none" => alias.to_owned(),
		_ => {
			let host = get("hostname").context("no `hostname`")?;
			match get("port").context("no `port`")? {
				"22" => host.to_owned(),
				port => format!("[{host}]:{port}"),
			}
		}
	};
	let file = get("userknownhostsfile")
		.and_then(|v| v.split_whitespace().next())
		.context("no `userknownhostsfile`")?;
	Ok(Entry { name, file: PathBuf::from(file) })
}

/// Every line in `file` for entry `name` (`ssh-keygen -F`); a missing file has none.
///
/// # Errors
///
/// Returns an error if `ssh-keygen` fails for a reason other than "not found".
pub(crate) fn lookup(name: &str, file: &Path) -> Result<Vec<Line>> {
	if !file.exists() {
		return Ok(Vec::new());
	}
	let out = duct::cmd!("ssh-keygen", "-F", name, "-f", file)
		.stdin_null()
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
		.with_context(|| format!("looking up {name} in {}", file.display()))?;
	match out.status.code() {
		Some(0) => Ok(parse_lookup(&String::from_utf8_lossy(&out.stdout))),
		// Exit 1: no entry for the name.
		Some(1) => Ok(Vec::new()),
		_ => bail!(
			"ssh-keygen -F {name} -f {} failed: {}",
			file.display(),
			String::from_utf8_lossy(&out.stderr).trim()
		),
	}
}

/// Parse `ssh-keygen -F` output: `# Host … found` comments, then the matching
/// lines. Marker lines (`@cert-authority`, `@revoked`) aren't plain host keys and
/// are skipped.
fn parse_lookup(out: &str) -> Vec<Line> {
	out.lines()
		.filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('@'))
		.filter_map(|l| {
			let mut fields = l.split_whitespace().skip(1);
			let kind = fields.next()?.to_owned();
			let blob = fields.next()?.to_owned();
			Some(Line { raw: l.to_owned(), key: HostKey { kind, blob } })
		})
		.collect()
}

/// Drop every line for `entry.name` from `entry.file` (`ssh-keygen -R`, which keeps
/// a `.old` backup), if it has any.
///
/// # Errors
///
/// Returns an error if `ssh-keygen -R` fails.
pub(crate) fn remove(entry: &Entry) -> Result<()> {
	duct::cmd!("ssh-keygen", "-R", &entry.name, "-f", &entry.file)
		.stdin_null()
		.stdout_null()
		.stderr_capture()
		.run()
		.with_context(|| format!("removing {} from {}", entry.name, entry.file.display()))?;
	Ok(())
}

/// Append `lines` (verbatim) to `file`, creating it — and a missing parent, `0700`
/// like `~/.ssh` — if needed.
///
/// # Errors
///
/// Returns an error if the file can't be created or written.
pub(crate) fn append(file: &Path, lines: &[Line]) -> Result<()> {
	if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty() && !p.exists()) {
		use std::os::unix::fs::DirBuilderExt as _;
		fs::DirBuilder::new()
			.recursive(true)
			.mode(0o700)
			.create(parent)
			.with_context(|| format!("creating {}", parent.display()))?;
	}
	// Don't glue the first new line onto an unterminated last one.
	let needs_newline = fs::read(file).is_ok_and(|b| b.last().is_some_and(|&c| c != b'\n'));
	let mut text = String::from(if needs_newline { "\n" } else { "" });
	for l in lines {
		text.push_str(&l.raw);
		text.push('\n');
	}
	OpenOptions::new()
		.create(true)
		.append(true)
		.open(file)
		.and_then(|mut f| f.write_all(text.as_bytes()))
		.with_context(|| format!("appending to {}", file.display()))
}

#[cfg(test)]
mod tests {
	use super::*;

	const ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

	#[test]
	fn ssh_g_default_port_is_the_bare_hostname() {
		let out = "user admin\nhostname tsm.example.it\nport 22\n\
			userknownhostsfile /home/u/.ssh/known_hosts /home/u/.ssh/known_hosts2\n";
		let e = parse_ssh_g(out).unwrap();
		assert_eq!(e.name, "tsm.example.it");
		assert_eq!(e.file, PathBuf::from("/home/u/.ssh/known_hosts"));
	}

	#[test]
	fn ssh_g_non_default_port_is_bracketed() {
		let out = "hostname 10.0.0.5\nport 2222\nuserknownhostsfile /tmp/kh\n";
		assert_eq!(parse_ssh_g(out).unwrap().name, "[10.0.0.5]:2222");
	}

	#[test]
	fn ssh_g_host_key_alias_wins_verbatim() {
		let out = "hostname h\nport 2222\nhostkeyalias vps\nuserknownhostsfile /tmp/kh\n";
		assert_eq!(parse_ssh_g(out).unwrap().name, "vps");
		// `hostkeyaliases` must not be mistaken for `hostkeyalias`.
		let out = "hostname h\nport 22\nhostkeyaliases x\nuserknownhostsfile /tmp/kh\n";
		assert_eq!(parse_ssh_g(out).unwrap().name, "h");
	}

	#[test]
	fn lookup_output_skips_comments_and_markers() {
		let out = format!(
			"# Host [h]:2222 found: line 1 \n[h]:2222 ssh-ed25519 {ED25519} c\n\
			 # Host [h]:2222 found: line 3 \n@cert-authority [h]:2222 ssh-ed25519 {ED25519}\n\
			 |1|salt=|hash= ecdsa-sha2-nistp256 AAAAE2\n"
		);
		let lines = parse_lookup(&out);
		assert_eq!(lines.len(), 2);
		assert_eq!(lines[0].raw, format!("[h]:2222 ssh-ed25519 {ED25519} c"));
		assert_eq!(lines[0].key, HostKey { kind: "ssh-ed25519".into(), blob: ED25519.into() });
		assert_eq!(lines[1].key.kind, "ecdsa-sha2-nistp256");
	}

	#[test]
	fn fingerprint_matches_ssh_keygen() {
		// `ssh-keygen -lf` on this key prints SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.
		let key = HostKey { kind: "ssh-ed25519".into(), blob: ED25519.into() };
		assert_eq!(
			key.fingerprint(),
			"ssh-ed25519 SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"
		);
	}
}
