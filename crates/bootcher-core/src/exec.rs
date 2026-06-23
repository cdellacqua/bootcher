//! Running subprocesses under a progress [`Scope`], with their output forwarded
//! to scroll-back and signal-driven killing wired in.
//!
//! Command-execution concerns are separated from the progress type; these
//! functions take the [`Scope`] they report under as a plain parameter. The lone
//! exception is [`best_effort`], the scope-free fire-and-forget runner the
//! teardown paths use.

use crate::progress::Scope;
use anyhow::Result;
use std::borrow::Cow;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

/// Run a duct command, showing a spinner labeled `label` for its duration and
/// merging stderr into stdout so each output line is forwarded to scroll-back
/// (live bars aren't clobbered). A non-zero exit surfaces as an `io::Error` on
/// the final read, per duct's `ReaderHandle` semantics.
///
/// For an ordinary single command, prefer the `run!` macro, which builds the
/// command *and* derives `label` from the same argv. Call this directly only for
/// shapes the macro can't express — e.g. a pipeline, where `label` is supplied
/// by hand.
///
/// # Errors
///
/// Returns an error if the command fails or a signal interrupts execution.
#[allow(clippy::needless_pass_by_value)] // `expr` is consumed into the reader chain.
pub(crate) fn run_command(
	scope: &Scope,
	expr: duct::Expression,
	label: impl Into<Cow<'static, str>>,
) -> Result<()> {
	let spinner = scope.spinner(label);
	let result = run_command_with(scope, expr, |_| {});
	spinner.finish();
	result
}

/// Run `argv` (program followed by its arguments) as a single command via
/// [`run_command`], using the joined command line as the spinner label. This is
/// the plumbing behind the `run!` macro — call that at use sites; reach for
/// this directly only when the argv is built dynamically.
///
/// # Errors
///
/// Returns an error if the command fails or a signal interrupts execution.
pub(crate) fn run_argv(scope: &Scope, argv: &[OsString]) -> Result<()> {
	let label = argv.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
	run_argv_labeled(scope, argv, label)
}

/// [`run_argv`] with an explicit spinner `label` instead of the joined command
/// line — for a builder phase where a friendly name ("bootc image builder", a
/// remote "podman build") reads better than a long, dynamically-built argv,
/// especially when it renders concurrently beneath a per-arch header.
///
/// # Panics
///
/// Panics if `argv` is empty.
///
/// # Errors
///
/// Returns an error if the command fails or a signal interrupts execution.
pub(crate) fn run_argv_labeled(
	scope: &Scope,
	argv: &[OsString],
	label: impl Into<Cow<'static, str>>,
) -> Result<()> {
	let (program, rest) = argv.split_first().expect("run_argv_labeled: argv must be non-empty");
	run_command(scope, duct::cmd(program, rest), label)
}

/// [`run_command`] with a per-line hook for callers that want to drive a side
/// bar from the subprocess output (e.g. parsing podman's `STEP N/M:` markers
/// into a count bar). The line is always also forwarded to scroll-back.
///
/// # Errors
///
/// Returns an error if the command fails or a signal interrupts execution.
#[allow(clippy::needless_pass_by_value)] // `expr` is consumed into the reader chain.
pub(crate) fn run_command_with(
	scope: &Scope,
	expr: duct::Expression,
	mut on_line: impl FnMut(&str),
) -> Result<()> {
	// Register the child for signal-driven killing: duct doesn't kill on drop
	// (it parks the child for later reaping), and a quiet stretch with no output
	// would otherwise leave us blocked in `read` past an interrupt. The guard
	// deregisters when this returns, so a finished command isn't killed by a
	// later signal. Shared via `Arc` so the listener thread can SIGTERM the child
	// while this thread reads.
	let reader = Arc::new(expr.stderr_to_stdout().reader()?);
	let _kill = crate::signals::kill_on_signal({
		let reader = Arc::clone(&reader);
		move |sig| {
			for pid in reader.pids() {
				crate::signals::signal_pid(pid, sig);
			}
		}
	});
	for line in BufReader::new(reader.as_ref()).lines() {
		crate::signals::check()?;
		let line = line?;
		on_line(&line);
		scope.println(&line);
	}
	// A signal-driven kill ends the stream with EOF, not a read error, so the
	// loop exits cleanly; surface it as the interruption it was.
	crate::signals::check()?;
	Ok(())
}

/// Run a command purely for effect, swallowing its output and any failure (a
/// missing binary, a non-zero exit, nothing to do). This is the never-raise
/// contract the RAII cleanup guards depend on — a panic out of a `Drop` during
/// an interrupt unwind aborts the process. Scope-free by design: teardown runs
/// with no progress bar to report under, and must stay silent so it can't
/// clobber one. Build the command (a `podman` removal, an `ssh` teardown) at the
/// call site and hand it here.
pub fn best_effort(expr: &duct::Expression) {
	let _ = expr.stdout_null().stderr_null().unchecked().run();
}

/// Build and run a single command with a spinner, mirroring duct's `cmd!`.
/// Each argument is converted with `Into<OsString>` exactly as `cmd!` does, and
/// the joined argv doubles as the spinner label — so there's no hand-written
/// description to drift from the command (or to merely echo the step title).
/// Pipelines and other non-`cmd!` shapes use [`run_command`] with an explicit
/// label instead.
///
/// ```ignore
/// run!(scope, "podman", "push", image.tag(), reg_ref)?;
/// ```
macro_rules! run {
	($scope:expr, $($arg:expr),+ $(,)?) => {
		$crate::exec::run_argv(
			$scope,
			&[$(::std::convert::Into::<::std::ffi::OsString>::into($arg)),+],
		)
	};
}
pub(crate) use run;

/// Single-quote `s` for safe interpolation into an `sh` command. Delegates to
/// [`shlex`]; panics only if `s` contains a nul byte, which is impossible in
/// valid Unix paths and image tags.
pub(crate) fn sh_quote(s: &str) -> String {
	shlex::try_quote(s).expect("shell argument contains nul byte").into_owned()
}
