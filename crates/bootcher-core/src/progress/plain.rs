//! A plain-text [`Sink`] for non-interactive output: no live bars, just the
//! forwarded subprocess lines and step transitions written straight to stderr.
//!
//! It exists because the live [`IndicatifSink`](super::IndicatifSink) goes *silent*
//! off-TTY — `MultiProgress::println` is a no-op when its draw target is hidden, so
//! a piped or CI run (or a test capturing the child) would otherwise see none of a
//! build's output, including the error that explains a failure. This sink realises
//! what the indicatif path only intends off-TTY: mirror every line to scroll-back.
//! Live-bar updates (`inc`/`set_position`/…) are dropped — there's no terminal to
//! animate — but `log` (command output, the fleet `✓`/`✗` lines) and section
//! transitions (the `[i/N] step` lines) are printed, which is the whole log.

use super::{LeafKind, LeafNode, Mark, SectionNode, Sink};

#[derive(Default)]
pub(crate) struct PlainSink;

impl PlainSink {
	#[must_use]
	pub(crate) fn new() -> Self {
		Self
	}
}

impl Sink for PlainSink {
	fn open_section(&self, _depth: usize, _label: &str) -> Box<dyn SectionNode> {
		Box::new(PlainSection)
	}

	fn open_leaf(
		&self,
		_depth: usize,
		_kind: LeafKind,
		label: &str,
		prefix: &str,
	) -> Box<dyn LeafNode> {
		// Announce the bar's purpose once (named by its fan-out worker, if any); its
		// live fill is then dropped.
		match (label.is_empty(), prefix.is_empty()) {
			(false, false) => eprintln!("{prefix}: {label}"),
			(false, true) => eprintln!("{label}"),
			(true, false) => eprintln!("{prefix}"),
			(true, true) => {}
		}
		Box::new(PlainLeaf)
	}

	fn log(&self, text: &str) {
		eprintln!("{text}");
	}

	fn suspend(&self, f: &mut dyn FnMut()) {
		// No bars to clear; just run it.
		f();
	}
}

/// A section with no live bar: it just prints each `[i/N] step` transition.
struct PlainSection;

impl SectionNode for PlainSection {
	fn advance(&mut self, current: u64, total: Option<u64>, label: &str) {
		// The "[i/N] step" transition — no indentation (a flat log needs none).
		let counter = total.map_or_else(|| format!("[{current}]"), |t| format!("[{current}/{t}]"));
		if label.is_empty() {
			eprintln!("{counter}");
		} else {
			eprintln!("{counter} {label}");
		}
	}

	fn finish(&mut self, _mark: Mark) {
		// The transitions already carry the log; no separate done-line off-TTY.
	}
}

/// A leaf with no live bar: every update is dropped.
struct PlainLeaf;

impl LeafNode for PlainLeaf {
	fn inc(&self, _delta: u64) {}
	fn set_position(&self, _pos: u64) {}
	fn set_length(&self, _len: u64) {}
	fn set_message(&self, _msg: &str) {}
}
