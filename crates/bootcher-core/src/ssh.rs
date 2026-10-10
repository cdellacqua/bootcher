//! `ssh` to a remote host — the one place that knows the connection details every
//! ssh call needs, which the scattered bare `ssh <remote>` sites kept getting
//! wrong. Used by both the deploy path (`upgrade`/`deploy`/`rotate`, reaching a
//! `[deploy] remotes` device) and the remote image builder, which drives a build
//! host the same way (a real box or a throwaway local VM).
//!
//! It bundles three things:
//!
//! 1. **The host + extra args.** A target may need ssh options the user's
//!    `~/.ssh/config` doesn't carry — a non-standard port, an identity, a host-key
//!    policy. The host is whatever ssh itself accepts as a destination: a plain
//!    `[user@]host`, or an `ssh://[user@]host[:port]` URL when a non-default port is
//!    needed. Caller-supplied opts (a remote's `ssh_opts`, or the VM builder's
//!    `-p`/`-i`) are prepended to every call.
//! 2. **Non-interactive, trust-on-first-use defaults.** Every call gets
//!    `-o BatchMode=yes` (a swallowed password prompt would just hang under the
//!    piped progress output) and `-o StrictHostKeyChecking=accept-new` (a freshly
//!    provisioned device's host key isn't in `known_hosts` yet).
//! 3. **A POSIX shell, on demand.** sshd runs a command string through the host's
//!    *login* shell, and the scaffold gives the admin user `fish` — which
//!    can't parse the `set -eu; tmp=$(mktemp); …` scripts rotation/upgrade send. So
//!    `run_sh`/`read_sh` pipe the script to `sh` on stdin, never handing it to
//!    the login shell. The builder, whose hosts run a POSIX login shell, uses
//!    [`argv`](Ssh::argv)/`read` (command as the ssh arg).

use crate::exec::run_command;
use crate::progress::Scope;
use anyhow::{Context, Result, bail};
use std::borrow::Cow;
use std::ffi::OsString;
use std::iter::empty;
use std::time::{Duration, Instant};

/// A remote host: the ssh destination (`[user@]host` or `ssh://[user@]host[:port]`)
/// plus the args prepended to every call (any caller-supplied opts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
	host: String,
	opts: Vec<String>,
}

impl Ssh {
	/// Build from a host spec (`[user@]host` or `ssh://[user@]host[:port]`) and extra
	/// opts (a remote's `ssh_opts`, or the VM builder's `-p`/`-i`), prepended to every
	/// call after the non-interactive base.
	pub(crate) fn new<Remote: Into<String>, Opt: Into<String>, Opts: IntoIterator<Item = Opt>>(
		remote: Remote,
		opts: Opts,
	) -> Self {
		Self { host: remote.into(), opts: opts.into_iter().map(Opt::into).collect() }
	}

	/// A copy that records and checks host keys *only* against `file` — the user's
	/// and the global `known_hosts` are ignored. Placed ahead of the configured opts,
	/// since ssh keeps the first value it sees for an option (a remote's own
	/// `UserKnownHostsFile` would otherwise win). Under the base `accept-new` policy
	/// a connect writes the host's current key to `file`, leaving the real
	/// `known_hosts` untouched.
	pub(crate) fn with_known_hosts_file(&self, file: &std::path::Path) -> Self {
		let mut opts = vec![
			"-o".to_owned(),
			format!("UserKnownHostsFile={}", file.display()),
			"-o".to_owned(),
			"GlobalKnownHostsFile=/dev/null".to_owned(),
		];
		opts.extend(self.opts.iter().cloned());
		Self { host: self.host.clone(), opts }
	}

	#[must_use]
	pub fn host(&self) -> &str {
		&self.host
	}

	/// ssh args after the program: the non-interactive + trust-on-first-use base
	/// (`-o BatchMode=yes -o StrictHostKeyChecking=accept-new`), the configured opts,
	/// then call-specific `pre` flags (e.g. `-R <forward>`, `-i <key>`, a probe
	/// timeout), the host, and `remote_cmd`.
	fn args(&self, pre: &[&str], remote_cmd: &str) -> Vec<OsString> {
		let mut v: Vec<OsString> = vec![
			"-o".into(),
			"BatchMode=yes".into(),
			"-o".into(),
			"StrictHostKeyChecking=accept-new".into(),
		];
		v.extend(self.opts.iter().map(OsString::from));
		v.extend(pre.iter().map(OsString::from));
		v.push((&self.host).into());
		v.push(remote_cmd.into());
		v
	}

	/// Full `ssh …` argv (program included), for the sites that drive a raw
	/// [`std::process::Command`] (the silent reachability probe, the reboot session).
	#[must_use]
	pub fn argv(&self, pre: &[&str], remote_cmd: &str) -> Vec<OsString> {
		let mut v = vec![OsString::from("ssh")];
		v.extend(self.args(pre, remote_cmd));
		v
	}

	fn expr(&self, pre: &[&str], remote_cmd: &str) -> duct::Expression {
		duct::cmd("ssh", self.args(pre, remote_cmd))
	}

	/// The `ssh … sh` [`duct::Expression`] with `script` on stdin, *unstarted* — for
	/// the few callers that need to chain duct combinators ([`unchecked`], a custom
	/// `read`) that [`run_sh`](Self::run_sh)/[`read_sh`](Self::read_sh) don't expose.
	///
	/// [`unchecked`]: duct::Expression::unchecked
	pub(crate) fn sh_expr(&self, script: &str) -> duct::Expression {
		self.expr(&[], "sh").stdin_bytes(script.to_owned())
	}

	/// Run `script` on the device under `sh` — the script rides stdin, so the login
	/// shell (fish, per the scaffold) never parses it — under a `label`led spinner.
	/// `pre` carries call-specific ssh flags (usually none; `-R <forward>` for the
	/// LAN image transfer).
	///
	/// # Errors
	///
	/// Returns an error if the ssh connection fails or the script exits non-zero.
	pub(crate) fn run_sh(
		&self,
		scope: &Scope,
		label: impl Into<Cow<'static, str>>,
		pre: &[&str],
		script: String,
	) -> Result<()> {
		run_command(scope, self.expr(pre, "sh").stdin_bytes(script), label)
	}

	/// Run `script` under `sh` and return its trimmed stdout — for short control
	/// reads against the *device* (`uname -m`, a `podman image inspect`), whose
	/// login shell can't be trusted to parse the command.
	///
	/// # Errors
	///
	/// Returns an error if the ssh connection fails or the script exits non-zero.
	pub(crate) fn read_sh(&self, script: &str) -> Result<String> {
		self.expr(&[], "sh")
			.stdin_bytes(script.to_owned())
			.read()
			.with_context(|| format!("ssh {}: {script}", self.host))
			.map(|s| s.trim().to_owned())
	}

	/// Run `remote_cmd` (as the ssh command, via the host's login shell) and return
	/// its trimmed stdout — for the [builder](crate::builder::RemoteBuilder)'s short reads
	/// (`mktemp`, `du`) against a build host that runs a POSIX login shell. The
	/// device deploy path uses [`read_sh`](Self::read_sh) instead.
	///
	/// # Errors
	///
	/// Returns an error if the ssh connection fails or the command exits non-zero.
	pub(crate) fn read(&self, remote_cmd: &str) -> Result<String> {
		self.expr(&[], remote_cmd)
			.read()
			.with_context(|| format!("ssh {}: {remote_cmd}", self.host))
			.map(|s| s.trim().to_owned())
	}

	/// One throwaway `ssh … true`: did the host accept a key-based session? A silent
	/// probe — failure is the normal "not up yet" case while polling a booting guest —
	/// so all stdio is nulled and the run is `unchecked`; the bool is the whole answer.
	/// Relies on the opts carrying a `ConnectTimeout` (the VM probe does) so a
	/// black-holed host can't hang the call.
	#[must_use]
	pub(crate) fn reachable(&self) -> bool {
		self.expr(&[], "true")
			.stdin_null()
			.stdout_null()
			.stderr_null()
			.unchecked()
			.run()
			.is_ok_and(|o| o.status.success())
	}

	/// Poll until the host answers ssh, or `timeout`
	/// elapses — a freshly booted guest/device isn't reachable the instant it's
	/// spawned. Surfaces a signal-driven interrupt at once (so a caller's VM guard can
	/// drop and kill the guest instead of spinning).
	///
	/// # Errors
	///
	/// Returns an error if the timeout elapses or a signal interrupts the wait.
	pub fn wait_until_reachable(&self, timeout: Duration, job: &Scope) -> Result<()> {
		const POLL: Duration = Duration::from_secs(3);
		let pb = job.spinner("waiting for ssh");
		let start = Instant::now();
		while !self.reachable() {
			crate::signals::check()?;
			if start.elapsed() > timeout {
				pb.finish();
				bail!("host did not answer ssh within {}s", timeout.as_secs());
			}
			std::thread::sleep(POLL);
		}
		pb.finish();
		Ok(())
	}
}

impl<S: Into<String>> From<S> for Ssh {
	fn from(value: S) -> Self {
		Self::new(value, empty::<String>())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn strs(argv: &[OsString]) -> Vec<String> {
		argv.iter().map(|a| a.to_string_lossy().into_owned()).collect()
	}

	#[test]
	fn argv_carries_opts_then_pre_then_host_then_command() {
		let r = Ssh::new("ssh://admin@host:2222", ["-o", "StrictHostKeyChecking=no"]);
		let argv = strs(&r.argv(&["-i", "/k"], "sh"));
		assert_eq!(argv[0], "ssh");
		assert!(argv.windows(2).any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"));
		assert!(argv.windows(2).any(|w| w[0] == "-o" && w[1] == "StrictHostKeyChecking=no"));
		assert!(argv.windows(2).any(|w| w[0] == "-i" && w[1] == "/k"));
		// The host reaches ssh verbatim (port kept in the `ssh://` URL), second-to-last.
		assert_eq!(argv[argv.len() - 2], "ssh://admin@host:2222");
		assert_eq!(argv.last().unwrap(), "sh");
	}

	#[test]
	fn known_hosts_override_precedes_the_configured_opts() {
		let r = Ssh::new("admin@h", ["-o", "UserKnownHostsFile=/dev/null"]);
		let argv = strs(&r.with_known_hosts_file(std::path::Path::new("/t/kh")).argv(&[], "true"));
		let ours = argv.iter().position(|a| a == "UserKnownHostsFile=/t/kh").unwrap();
		let theirs = argv.iter().position(|a| a == "UserKnownHostsFile=/dev/null").unwrap();
		assert!(ours < theirs, "ssh keeps the first value, so the override must come first");
		assert!(argv.iter().any(|a| a == "GlobalKnownHostsFile=/dev/null"));
	}
}
