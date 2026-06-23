//! Cooperative cancellation on SIGINT / SIGTERM, with prompt killing of the
//! heavyweight children.
//!
//! [`install`] replaces the default "terminate immediately" disposition with
//! two layers that share one flag:
//!
//! - **Cooperative:** the long-running loops poll `check` and `bail!` on the
//!   first signal, so the stack unwinds and every RAII guard runs exactly as on
//!   a normal error — the qemu kill in `builder::vm`, the cleared progress bars
//!   in `progress`. Killing the process outright (the default) would skip those
//!   and leak an orphaned TCG qemu.
//!
//! - **Active kill:** a poll only fires *between* loop iterations, so a main
//!   thread blocked reading a quiet child (a slow `podman build` step, a `ssh`
//!   image-builder run) wouldn't notice for a long time — and a `kill`-delivered SIGTERM
//!   never reaches the child at all (only a terminal Ctrl-C hits the whole
//!   group). So `kill_on_signal` lets those children register a kill action —
//!   a graceful SIGTERM (see `signal_pid`) — that the signal-listener thread
//!   runs immediately, unblocking the read once the child exits and closes its
//!   pipe.
//!
//! A *second* signal escalates: every registered child is sent SIGKILL — the
//! escape hatch for one that ignored SIGTERM (a wedged qemu left orphaned would
//! keep eating RAM) — and then the process hard-exits (130). The hard-exit also
//! covers the rare blocking call that can neither poll nor be unblocked by a
//! child dying (an in-flight `zip` extract, `fs.unmount`).

use anyhow::{Context, Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Set true by the listener on the first SIGINT/SIGTERM. A `OnceLock` so code
/// paths that never `install` (the test binaries, library callers) read as
/// "not interrupted" instead of tripping over an uninitialised flag.
static INTERRUPTED: OnceLock<&'static AtomicBool> = OnceLock::new();

/// Monotonic id source for kill-registry entries.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A registered kill action paired with the id that lets its [`KillGuard`]
/// remove it again. The action takes a [`Sig`] so the same registration handles
/// both escalation steps: SIGTERM on the first signal, SIGKILL on the second.
type Killer = (u64, Box<dyn FnMut(Sig) + Send>);

/// Heavyweight children to kill on the first signal. A plain `Mutex<Vec<…>>`
/// rather than anything fancier: registrations are rare (a handful of live
/// children at once) and only contend with the one-shot listener.
static KILLERS: Mutex<Vec<Killer>> = Mutex::new(Vec::new());

/// Install the signal handling for SIGINT and SIGTERM. Call once, early in
/// `main`, before any cancellable work. The single call covers the sudo'd
/// `__…` children too, since they re-enter the same `main`.
///
/// # Errors
///
/// Returns an error if registering the signal handlers or spawning the listener thread fails.
pub fn install() -> Result<()> {
	// Leaked so the listener thread can hold a `'static` reference without an
	// `Arc`; there's exactly one, for the life of the process.
	let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
	// Set before the listener starts so `interrupted()` is live immediately.
	let _ = INTERRUPTED.set(flag);

	let mut signals = Signals::new([SIGINT, SIGTERM]).context("registering signal handlers")?;
	std::thread::Builder::new()
		.name("bootcher-signals".to_owned())
		.spawn(move || {
			for _sig in &mut signals {
				// First signal: arm cancellation, then SIGTERM the heavyweight
				// children so any blocked read returns at once and the main thread
				// can unwind through its RAII guards. A second signal means the user
				// is done waiting — SIGKILL anything that ignored the SIGTERM (a
				// wedged qemu we'd otherwise orphan), then hard-exit.
				if flag.swap(true, Ordering::SeqCst) {
					kill_registered(Sig::Kill);
					signal_hook::low_level::exit(130);
				}
				kill_registered(Sig::Term);
			}
		})
		.context("spawning signal-listener thread")?;
	Ok(())
}

/// Whether a shutdown signal has arrived. Cheap (one relaxed atomic load) —
/// safe to poll in tight loops.
pub fn interrupted() -> bool {
	INTERRUPTED.get().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// `Err("interrupted")` once a shutdown signal has arrived, else `Ok(())`. Drop
/// `signals::check()?` into long loops: the error unwinds through the RAII
/// guards, leaving no loop device, mount, or qemu behind.
///
/// # Errors
///
/// Returns an error once a shutdown signal (SIGINT or SIGTERM) has been received.
pub(crate) fn check() -> Result<()> {
	if interrupted() {
		bail!("interrupted");
	}
	Ok(())
}

/// How hard a [`kill_on_signal`] action signals its child: gracefully on the
/// first shutdown signal, forcefully on the second.
#[derive(Clone, Copy)]
pub(crate) enum Sig {
	/// SIGTERM — let the child exit cleanly and reap its *own* descendants.
	Term,
	/// SIGKILL — unconditional, for a child that ignored SIGTERM.
	Kill,
}

/// Send `sig` to `pid`, best-effort (a process that has already exited is
/// ignored). Registrants take [`Sig::Term`] on the first shutdown signal —
/// graceful, so a child can tear its own descendants down (podman's conmon, the
/// guest under qemu) and flush — and [`Sig::Kill`] on the second, the escape
/// hatch for one that ignored SIGTERM.
pub(crate) fn signal_pid(pid: u32, sig: Sig) {
	let signal = match sig {
		Sig::Term => rustix::process::Signal::TERM,
		Sig::Kill => rustix::process::Signal::KILL,
	};
	if let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) {
		let _ = rustix::process::kill_process(pid, signal);
	}
}

/// Register `kill` to run on shutdown, off the listener thread — called with
/// [`Sig::Term`] on the first SIGINT/SIGTERM and, if that one doesn't take,
/// [`Sig::Kill`] on the second. For the long-running children (qemu, podman,
/// ssh): unlike the cooperative flag, which a loop only notices between
/// iterations, this interrupts a main thread *blocked* reading the child's
/// output. The returned [`KillGuard`] deregisters on drop, so a child that exits
/// normally is never killed by a later signal (whose pid may by then have been
/// recycled).
#[must_use = "dropping the guard immediately deregisters the child"]
pub(crate) fn kill_on_signal(kill: impl FnMut(Sig) + Send + 'static) -> KillGuard {
	let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
	if let Ok(mut killers) = KILLERS.lock() {
		killers.push((id, Box::new(kill)));
	}
	KillGuard(id)
}

/// Drops a [`kill_on_signal`] registration. Held in the owner's scope alongside
/// the child it kills, so the registration's lifetime matches the child's.
pub(crate) struct KillGuard(u64);

impl Drop for KillGuard {
	fn drop(&mut self) {
		if let Ok(mut killers) = KILLERS.lock() {
			killers.retain(|(id, _)| *id != self.0);
		}
	}
}

/// Run every registered kill action at escalation level `sig`. Invoked from the
/// listener thread — never from handler context, so taking the lock is safe.
fn kill_registered(sig: Sig) {
	if let Ok(mut killers) = KILLERS.lock() {
		for (_, kill) in killers.iter_mut() {
			kill(sig);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{Sig, signal_pid};
	use std::io::Read;
	use std::process::{Child, Command, Stdio};

	/// Spawn an `sh` with `trap` installed for SIGTERM, blocked in the `read`
	/// builtin (no external child to orphan, and `read` is interruptible by a
	/// trapped signal where a foreground `sleep` would defer it). Returns once the
	/// child has printed `ready` — trap set, about to block. The returned `Child`
	/// owns the still-open stdin pipe, so `read` keeps blocking until we signal it.
	fn blocked_sh(trap: &str) -> Child {
		let mut child = Command::new("sh")
			.args(["-c", &format!("trap '{trap}' TERM; echo ready; read _")])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.spawn()
			.expect("spawn sh");
		let mut ready = [0u8; 6];
		child.stdout.as_mut().expect("piped stdout").read_exact(&mut ready).expect("ready marker");
		child
	}

	/// [`Sig::Term`] must deliver a *catchable* SIGTERM: a child that traps it runs
	/// its handler and exits on its own terms (status 7). A SIGKILL would instead
	/// kill it by signal with no exit code — so `code() == Some(7)` proves graceful.
	#[test]
	fn term_is_catchable() {
		let mut child = blocked_sh("exit 7");
		signal_pid(child.id(), Sig::Term);
		let status = child.wait().expect("wait child");
		assert_eq!(status.code(), Some(7), "expected catchable SIGTERM, got {status:?}");
	}

	/// [`Sig::Kill`] must be unstoppable — the second-signal escape hatch: a child
	/// that *ignores* SIGTERM still dies, killed by signal 9 with no exit code.
	#[test]
	fn kill_is_unstoppable() {
		use std::os::unix::process::ExitStatusExt;
		let mut child = blocked_sh(""); // ignore SIGTERM
		signal_pid(child.id(), Sig::Term); // no-op against the ignore
		signal_pid(child.id(), Sig::Kill);
		let status = child.wait().expect("wait child");
		assert_eq!(status.code(), None, "SIGKILL leaves no exit code, got {status:?}");
		assert_eq!(status.signal(), Some(9), "expected death by SIGKILL");
	}
}
