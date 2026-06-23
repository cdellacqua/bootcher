//! A single persistent privileged process, authenticated once, that runs every
//! local root command on our behalf.
//!
//! `LocalBuilder`'s image-builder step needs root on *this* host (loop mounts,
//! `mount(2)`, the image load into root storage). Done the obvious way — a `sudo`
//! per command — each invocation is subject to sudo's credential cache, so a
//! hardened host with `timestamp_timeout=0` re-prompts every time, and even on a
//! normal host a prompt can land minutes into a build (the post-build cleanup
//! `sudo`), under the
//! concurrent per-arch progress bars of a multi-arch [`crate::jobs::disk`] run.
//!
//! Instead we spawn one `sudo sh` up front, on the quiet terminal, and feed it
//! commands for the rest of the phase. It authenticates exactly once — so there's
//! exactly one password prompt regardless of `timestamp_timeout`, and it's clean
//! because it happens before any worker opens a bar. Every later privileged
//! command runs inside that already-root shell, no further `sudo`, no further
//! prompt.
//!
//! The shell is driven over its stdin (one command per call) and read back over
//! its stdout — with the shell's own stderr folded in (`exec 2>&1`) so a child's
//! diagnostics (image-builder's verbose progress) are captured too. Each command is followed
//! by a unique sentinel carrying its `$?`, so [`run_as_root`] knows when the
//! command finished and whether it succeeded while streaming its output to the
//! progress UI in between. Commands serialise on a global lock; in practice only
//! the one native-arch worker is local (cross-arch builds run on a VM/remote), so
//! there's no contention.

use crate::progress::Scope;
use crate::signals;
use anyhow::{Context, Result, bail};
use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, PoisonError};

/// The process-wide privileged session. `None` until [`ensure_session`] (or the
/// first [`run_as_root`]) brings it up; cleared — and the shell killed — when the
/// [`SessionGuard`] drops at the end of the phase.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Bring the privileged session up now, authenticating on `job`'s suspended
/// (quiet) terminal so the one password prompt doesn't land under later bars.
/// Idempotent. Call once before a parallel region that will run local root work,
/// and hold the returned [`SessionGuard`] for the region's lifetime — its drop
/// tears the shell down.
///
/// # Errors
///
/// Returns an error if the `sudo sh` session fails to start (e.g. wrong password).
pub(crate) fn ensure_session(job: &Scope) -> Result<SessionGuard> {
	let mut slot = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
	if slot.is_none() {
		*slot = Some(Session::start(job)?);
	}
	Ok(SessionGuard)
}

/// Run `command` as root in the persistent session, forwarding its output to
/// scroll-back under a `label`led spinner, exactly like [`crate::exec::run_command`]
/// does for an ordinary child. A non-zero exit (or the session dying) is an error.
///
/// Brings the session up on first use if [`ensure_session`] wasn't called, but
/// then the auth prompt lands wherever this is called (possibly under bars) — so
/// callers in a parallel region should `ensure_session` up front.
///
/// # Panics
///
/// Panics if the session mutex is poisoned (another thread panicked while holding it).
///
/// # Errors
///
/// Returns an error if the session fails to start or the command exits non-zero.
pub(crate) fn run_as_root(
	job: &Scope,
	label: impl Into<Cow<'static, str>>,
	command: &str,
) -> Result<()> {
	let mut slot = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
	if slot.is_none() {
		*slot = Some(Session::start(job)?);
	}
	let session = slot.as_mut().expect("session just ensured");
	let spinner = job.spinner(label);
	let result = session.exec(command, &mut |line| job.println(line));
	spinner.finish();
	result
}

/// RAII handle to the process-wide privileged session. Dropping it clears and kills the shell, so the
/// privileged process never outlives the region it was opened for.
#[must_use = "dropping the guard tears down the privileged session"]
pub(crate) struct SessionGuard;

impl Drop for SessionGuard {
	fn drop(&mut self) {
		// Take the session out and let it drop (killing the shell). After a phase
		// this is quiescent — no command is mid-flight — so there's nothing to race.
		let _ = SESSION.lock().unwrap_or_else(PoisonError::into_inner).take();
	}
}

/// A live `sudo sh` and the pipes that drive it. The `_kill` guard SIGKILLs the
/// shell on an interrupt that arrives while we're blocked reading its output;
/// [`Drop`] kills it on the way out of the phase.
struct Session {
	child: Child,
	stdin: ChildStdin,
	stdout: BufReader<ChildStdout>,
	/// Per-session marker base + a per-command counter, so each command's sentinel
	/// can't collide with another's (or with the command's own output).
	nonce: u32,
	counter: u64,
	_kill: signals::KillGuard,
}

impl Session {
	/// Spawn `sudo sh`, fold its stderr into the stdout pipe, and authenticate —
	/// the auth handshake is confirmed by a no-op command round-tripping its
	/// sentinel back, which only happens once the shell is actually running (i.e.
	/// sudo accepted the password). Run under `job.suspend` by the caller path so
	/// sudo's `/dev/tty` prompt owns a quiet terminal.
	fn start(job: &Scope) -> Result<Session> {
		// stdin/stdout piped (our command + readback channels); stderr inherited so
		// any pre-shell sudo error is visible. sudo reads the password from
		// `/dev/tty`, not stdin, so the piped stdin doesn't interfere with auth.
		let mut child = Command::new("sudo")
			.arg("sh")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.spawn()
			.context("spawning `sudo sh` for the privileged session (is sudo installed?)")?;

		// SIGTERM the shell by pid on a signal so an interrupt while we're blocked
		// reading its output tears it down at once instead of waiting for the unwind.
		// No process-group kill: the shell inherits our foreground group, so an
		// interactive Ctrl-C already reaches its children (the image-builder `podman`) directly
		// via the tty.
		let pid = child.id();
		let kill = signals::kill_on_signal(move |sig| signals::signal_pid(pid, sig));

		let mut stdin = child.stdin.take().expect("stdin piped");
		let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));

		// Fold the shell's (and its children's) stderr into the pipe we read, so image-builder
		// progress on stderr is captured the same as the `stderr_to_stdout` the rest
		// of `exec` uses.
		writeln!(stdin, "exec 2>&1").context("priming the privileged session")?;
		stdin.flush().ok();

		let mut session =
			Session { child, stdin, stdout, nonce: std::process::id(), counter: 0, _kill: kill };

		// The probe blocks through sudo's password prompt; suspend the (empty, but
		// possibly-leftover) bars so the prompt is clean. A failed auth makes the
		// shell exit, which `exec` sees as EOF-before-sentinel and reports.
		job.suspend(|| session.exec("true", &mut |_| {}))
			.context("authenticating sudo for the privileged image build")?;
		Ok(session)
	}

	/// Send `command` to the shell, stream its output through `on_line`, and block
	/// until its trailing sentinel reports completion. Returns an error on a
	/// non-zero exit, or if the shell ends before the sentinel (auth failure, kill).
	fn exec(&mut self, command: &str, on_line: &mut dyn FnMut(&str)) -> Result<()> {
		self.counter += 1;
		let marker = format!("__BOOTCHER_SUDO_{}_{}__", self.nonce, self.counter);

		// Run the command, then print the marker followed by its exit status. The
		// leading `\n` in the printf guarantees the marker starts its own line even
		// if the command's last output line had no trailing newline.
		writeln!(self.stdin, "{command}").context("writing command to privileged session")?;
		writeln!(self.stdin, "printf '\\n%s %d\\n' '{marker}' \"$?\"")
			.context("writing command sentinel to privileged session")?;
		self.stdin.flush().context("flushing privileged session")?;

		let mut line = String::new();
		loop {
			signals::check()?;
			line.clear();
			let n =
				self.stdout.read_line(&mut line).context("reading privileged session output")?;
			if n == 0 {
				// EOF before the sentinel: the shell exited under us (sudo auth
				// failed, or the kill guard fired). Surface a signal as the
				// interruption it was, else report the lost session.
				signals::check()?;
				bail!("privileged sudo session ended unexpectedly");
			}
			let line = line.trim_end_matches(['\n', '\r']);
			if let Some(code) = match_sentinel(line, &marker) {
				if code != 0 {
					bail!("privileged command failed (exit {code}): {command}");
				}
				return Ok(());
			}
			on_line(line);
		}
	}
}

/// If `line` is `command`'s sentinel (`<marker> <exit-code>`), return the exit
/// code; otherwise `None`, so the line is ordinary command output to forward. An
/// unparsable code reads as `-1` (a failure), since a mangled sentinel means we
/// can't trust success.
fn match_sentinel(line: &str, marker: &str) -> Option<i32> {
	let rest = line.strip_prefix(marker)?;
	Some(rest.trim().parse().unwrap_or(-1))
}

impl Drop for Session {
	fn drop(&mut self) {
		// Best-effort: kill the shell and reap it. At phase end nothing is in flight,
		// so there's no orphaned image-builder `podman` to chase — the last command already
		// returned before we got here.
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

#[cfg(test)]
mod tests {
	use super::match_sentinel;

	const M: &str = "__BOOTCHER_SUDO_42_7__";

	#[test]
	fn reads_exit_code_off_the_sentinel_line() {
		assert_eq!(match_sentinel(&format!("{M} 0"), M), Some(0));
		assert_eq!(match_sentinel(&format!("{M} 125"), M), Some(125));
		// Trailing/leading whitespace around the code is tolerated.
		assert_eq!(match_sentinel(&format!("{M}   3  "), M), Some(3));
	}

	#[test]
	fn ordinary_output_is_not_a_sentinel() {
		// Command output that doesn't carry the marker is forwarded, not consumed.
		assert_eq!(match_sentinel("STEP 1/5: FROM fedora-bootc", M), None);
		assert_eq!(match_sentinel("", M), None);
		// A different command's marker (wrong counter) must not match this one.
		assert_eq!(match_sentinel("__BOOTCHER_SUDO_42_6__ 0", M), None);
	}

	#[test]
	fn a_mangled_exit_code_reads_as_failure() {
		// A marker with no/garbled code can't be trusted as success, so it's -1.
		assert_eq!(match_sentinel(M, M), Some(-1));
		assert_eq!(match_sentinel(&format!("{M} notanumber"), M), Some(-1));
	}
}
