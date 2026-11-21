//! The rules every interactive prompt follows, and the helpers that implement them.
//!
//! - A prompt only fills in what a flag could have supplied. A non-TTY run never
//!   prompts: a missing value is an error naming the flag. "TTY" means stdin is a
//!   terminal ([`is_tty`]).
//! - Options come from live state and are annotated (`stable: v1.3, base current`,
//!   `kiosk-2: on :next`, `vps3: unreachable`); see [`Choice`].
//! - The pre-selection equals the non-TTY default, so pressing Enter does what a
//!   scripted run would.
//! - After any prompt, the equivalent command is printed (`→ bootcher channel
//!   promote v1.4 stable`); see [`Equivalent`].
//! - Esc and Ctrl-C inside a prompt both mean "interrupted": the run exits 130
//!   through the same path as a signal ([`Prompted`]).
//! - Option building and pre-selection are plain functions, unit-tested without
//!   `inquire`; only the thin wrappers here touch the terminal.
//!
//! `-y/--yes` means "proceed without asking": on a TTY it skips the confirmation,
//! on a non-TTY it is required ([`gate`]).

use anyhow::{Result, bail};
use inquire::InquireError;
use std::fmt;
use std::io::IsTerminal;

/// Whether stdin is a terminal — the one test for "can we prompt".
pub(crate) fn is_tty() -> bool {
	std::io::stdin().is_terminal()
}

/// The three outcomes of a confirmation gate.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Gate {
	/// `-y` given — go ahead with no prompt.
	Proceed,
	/// Interactive TTY — ask.
	Prompt,
	/// Non-TTY without `-y` — fail closed.
	Bail,
}

/// Decide a confirmation from `-y` and whether stdin is a TTY.
pub(crate) fn gate(yes: bool, is_tty: bool) -> Gate {
	if yes {
		Gate::Proceed
	} else if is_tty {
		Gate::Prompt
	} else {
		Gate::Bail
	}
}

/// Map an `inquire` result: Esc (`OperationCanceled`) and Ctrl-C
/// (`OperationInterrupted`) mark the run interrupted (so it exits 130 like a
/// signal); anything else is an ordinary error.
pub(crate) trait Prompted<T> {
	/// See the trait docs.
	///
	/// # Errors
	///
	/// Returns an error for a cancelled or failed prompt.
	fn prompted(self) -> Result<T>;
}

impl<T> Prompted<T> for Result<T, InquireError> {
	fn prompted(self) -> Result<T> {
		match self {
			Ok(v) => Ok(v),
			Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
				crate::signals::mark_interrupted();
				bail!("interrupted")
			}
			Err(e) => Err(e.into()),
		}
	}
}

/// Gate an action on `-y` or an interactive yes/no. `lines` describe what will
/// happen (printed before the question); `default` is what Enter answers. A
/// non-TTY run without `-y` fails naming `what`.
///
/// # Errors
///
/// Returns an error when declined, cancelled, or on a non-TTY without `-y`.
pub(crate) fn confirm(
	yes: bool,
	what: &str,
	lines: &[String],
	question: &str,
	default: bool,
) -> Result<()> {
	match gate(yes, is_tty()) {
		Gate::Proceed => Ok(()),
		Gate::Bail => {
			bail!("{what}, and stdin is not a TTY to confirm — re-run with -y/--yes to proceed")
		}
		Gate::Prompt => {
			for l in lines {
				eprintln!("{l}");
			}
			if inquire::Confirm::new(question).with_default(default).prompt().prompted()? {
				Ok(())
			} else {
				bail!("aborted: not confirmed")
			}
		}
	}
}

/// One option in an annotated select or checklist: what's shown, the value it
/// stands for, whether it can be picked at all (an unselectable option is shown,
/// with its reason, but rejected), and whether it's pre-selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice<T> {
	pub(crate) label: String,
	pub(crate) value: T,
	/// `None` when selectable; otherwise why not (shown after the label).
	pub(crate) disabled: Option<String>,
	pub(crate) checked: bool,
}

impl<T> Choice<T> {
	/// A selectable option.
	pub(crate) fn new(label: impl Into<String>, value: T, checked: bool) -> Self {
		Self { label: label.into(), value, disabled: None, checked }
	}

	/// An option shown but not selectable, with the reason.
	pub(crate) fn disabled(label: impl Into<String>, value: T, why: impl Into<String>) -> Self {
		Self { label: label.into(), value, disabled: Some(why.into()), checked: false }
	}
}

impl<T> fmt::Display for Choice<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.disabled {
			None => f.write_str(&self.label),
			Some(why) => write!(f, "{} — not selectable: {why}", self.label),
		}
	}
}

/// A checklist (`inquire::MultiSelect`, default help line: → all, ← none, type to
/// filter) over `choices`, pre-checking the checked ones. Picking a disabled option
/// is rejected with its reason. Returns the picked values in order.
///
/// # Errors
///
/// Returns an error when cancelled or nothing selectable exists.
pub(crate) fn checklist<T: Clone>(question: &str, choices: Vec<Choice<T>>) -> Result<Vec<T>> {
	if choices.iter().all(|c| c.disabled.is_some()) {
		bail!("nothing to select for: {question}");
	}
	let defaults: Vec<usize> =
		choices.iter().enumerate().filter(|(_, c)| c.checked).map(|(i, _)| i).collect();
	let picked = inquire::MultiSelect::new(question, choices)
		.with_default(&defaults)
		.with_validator(|picked: &[inquire::list_option::ListOption<&Choice<T>>]| {
			Ok(match picked.iter().find_map(|o| o.value.disabled.as_ref().map(|w| (o, w))) {
				Some((o, why)) => inquire::validator::Validation::Invalid(
					format!("{} can't be selected: {why}", o.value.label).into(),
				),
				None => inquire::validator::Validation::Valid,
			})
		})
		.prompt()
		.prompted()?;
	Ok(picked.into_iter().map(|c| c.value).collect())
}

/// A single-choice select over `choices`, starting on the first checked one (else
/// the first selectable one). Picking a disabled option re-prompts.
///
/// # Errors
///
/// Returns an error when cancelled or nothing is selectable.
pub(crate) fn select<T: Clone>(question: &str, choices: &[Choice<T>]) -> Result<T> {
	let start = start_cursor(choices)
		.ok_or_else(|| anyhow::anyhow!("nothing to select for: {question}"))?;
	loop {
		let picked = inquire::Select::new(question, choices.to_vec())
			.with_starting_cursor(start)
			.prompt()
			.prompted()?;
		match &picked.disabled {
			None => return Ok(picked.value),
			Some(why) => eprintln!("{} can't be selected: {why}", picked.label),
		}
	}
}

/// Where a select starts: the first checked selectable option, else the first
/// selectable one.
pub(crate) fn start_cursor<T>(choices: &[Choice<T>]) -> Option<usize> {
	choices
		.iter()
		.position(|c| c.checked && c.disabled.is_none())
		.or_else(|| choices.iter().position(|c| c.disabled.is_none()))
}

/// The command line a prompted run is equivalent to, printed after the prompts
/// (`→ bootcher device deploy -y --remote kiosk-1`) so it can be scripted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Equivalent(Vec<String>);

impl Equivalent {
	/// `bootcher <words…>` (the subcommand path, e.g. `["device", "deploy"]`).
	pub(crate) fn new(words: &[&str]) -> Self {
		Self(words.iter().map(|w| (*w).to_owned()).collect())
	}

	/// Append a positional argument or bare flag.
	#[must_use]
	pub(crate) fn arg(mut self, a: impl Into<String>) -> Self {
		self.0.push(a.into());
		self
	}

	/// Append several positional arguments.
	#[must_use]
	pub(crate) fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
		self.0.extend(args.into_iter().map(Into::into));
		self
	}

	/// Append `flag` when `on`.
	#[must_use]
	pub(crate) fn flag_if(self, on: bool, flag: &str) -> Self {
		if on { self.arg(flag) } else { self }
	}

	/// Append `--opt value` once per value.
	#[must_use]
	pub(crate) fn opts<S: AsRef<str>>(
		mut self,
		opt: &str,
		values: impl IntoIterator<Item = S>,
	) -> Self {
		for v in values {
			self.0.push(opt.to_owned());
			self.0.push(v.as_ref().to_owned());
		}
		self
	}

	/// The rendered, shell-quoted command line.
	pub(crate) fn render(&self) -> String {
		let mut out = String::from("bootcher");
		for w in &self.0 {
			out.push(' ');
			out.push_str(&crate::exec::sh_quote(w));
		}
		out
	}

	/// Print it (`→ bootcher …`) to stderr.
	pub(crate) fn print(&self) {
		eprintln!("→ {}", self.render());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gate_fails_closed_off_a_tty() {
		assert_eq!(gate(true, false), Gate::Proceed);
		assert_eq!(gate(true, true), Gate::Proceed);
		assert_eq!(gate(false, true), Gate::Prompt);
		assert_eq!(gate(false, false), Gate::Bail);
	}

	#[test]
	fn cancelled_prompts_map_to_interrupted() {
		let esc: Result<(), InquireError> = Err(InquireError::OperationCanceled);
		assert_eq!(esc.prompted().unwrap_err().to_string(), "interrupted");
		let ctrl_c: Result<(), InquireError> = Err(InquireError::OperationInterrupted);
		assert_eq!(ctrl_c.prompted().unwrap_err().to_string(), "interrupted");
		let other: Result<(), InquireError> = Err(InquireError::NotTTY);
		assert_ne!(other.prompted().unwrap_err().to_string(), "interrupted");
	}

	#[test]
	fn start_cursor_prefers_a_checked_selectable_option() {
		let c = vec![
			Choice::disabled("a", 0, "no"),
			Choice::new("b", 1, false),
			Choice::new("c", 2, true),
		];
		assert_eq!(start_cursor(&c), Some(2));
		let c = vec![Choice::disabled("a", 0, "no"), Choice::new("b", 1, false)];
		assert_eq!(start_cursor(&c), Some(1));
		let c = vec![Choice::disabled("a", 0, "no")];
		assert_eq!(start_cursor(&c), None);
	}

	#[test]
	fn disabled_choices_show_their_reason() {
		assert_eq!(
			Choice::disabled("kiosk-2", (), "tracks :next").to_string(),
			"kiosk-2 — not selectable: tracks :next"
		);
		assert_eq!(Choice::new("kiosk-1", (), true).to_string(), "kiosk-1");
	}

	#[test]
	fn equivalent_command_quotes_its_words() {
		let e =
			Equivalent::new(&["channel", "promote"]).arg("v1.4").arg("stable").flag_if(true, "-y");
		assert_eq!(e.render(), "bootcher channel promote v1.4 stable -y");
		let e = Equivalent::new(&["device", "deploy"])
			.opts("--remote", ["a b", "c"])
			.flag_if(false, "-y");
		assert_eq!(e.render(), "bootcher device deploy --remote 'a b' --remote c");
	}
}
