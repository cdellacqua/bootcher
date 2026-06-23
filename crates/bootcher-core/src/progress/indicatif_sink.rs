//! The live-terminal [`Sink`]: nested indicatif bars on a shared
//! `MultiProgress`.
//!
//! Each `open_*` returns a self-owned handle wrapping its own `ProgressBar`
//! (already an `Arc`, already thread-safe), so there is no central node table to
//! lock — `inc`/`set_*` call straight through, and a handle clears its bar on
//! drop.
//!
//! Section headers are static (no steady tick) so they don't fight subprocesses
//! that own the terminal beneath them; leaf bars steady-tick. Every transition
//! and completed-step "✓" line is mirrored to scroll-back *only when the live
//! header is hidden* (non-TTY: CI, journal) — in a TTY the live header already
//! carries it, so emitting both would duplicate at the moment of transition.

use super::{LeafKind, LeafNode, Mark, SectionNode, Sink, indent};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(120);

pub(crate) struct IndicatifSink {
	multi: MultiProgress,
}

impl IndicatifSink {
	#[must_use]
	pub(crate) fn new() -> Self {
		Self { multi: MultiProgress::new() }
	}
}

impl Default for IndicatifSink {
	fn default() -> Self {
		Self::new()
	}
}

impl Sink for IndicatifSink {
	fn open_section(&self, depth: usize, label: &str) -> Box<dyn SectionNode> {
		let pb = self.multi.add(level_header(depth));
		if !label.is_empty() {
			pb.set_message(label.to_owned());
		}
		Box::new(IndicatifSection { multi: self.multi.clone(), pb, depth, pending: None })
	}

	fn open_leaf(
		&self,
		depth: usize,
		kind: LeafKind,
		label: &str,
		prefix: &str,
	) -> Box<dyn LeafNode> {
		let pb = self.multi.add(build_bar(depth, kind));
		if !label.is_empty() {
			pb.set_message(label.to_owned());
		}
		if !prefix.is_empty() {
			// The template puts `{prefix}` flush against the body, so bake the
			// separating space into the value — an empty prefix then adds nothing.
			pb.set_prefix(format!("  {prefix}"));
		}
		pb.enable_steady_tick(TICK);
		Box::new(IndicatifLeaf { pb })
	}

	fn log(&self, text: &str) {
		let _ = self.multi.println(text);
	}

	fn suspend(&self, f: &mut dyn FnMut()) {
		self.multi.suspend(f);
	}
}

/// A section header: a static bar plus the pending "✓"/"✗" line of its
/// in-progress child, flushed to scroll-back when the next child starts or the
/// section finishes.
struct IndicatifSection {
	multi: MultiProgress,
	pb: ProgressBar,
	depth: usize,
	/// The currently-active child's `(counter, label)`, flushed as a done-line
	/// when the next child starts (`✓`) or the section finishes (the [`Mark`]
	/// chosen by the handle layer).
	pending: Option<(String, String)>,
}

impl IndicatifSection {
	fn flush(&self, counter: &str, mark: char, label: &str) {
		let _ = self.multi.println(done_line(&indent(self.depth), counter, mark, label));
	}
}

impl SectionNode for IndicatifSection {
	fn advance(&mut self, current: u64, total: Option<u64>, label: &str) {
		// A new child starting means the previous one genuinely completed: flush
		// its line marked `✓` before showing the new one.
		if let Some((counter, prev)) = self.pending.take() {
			self.flush(&counter, '✓', &prev);
		}

		let counter = total.map_or_else(|| format!("[{current}]"), |t| format!("[{current}/{t}]"));
		let line = format!("{counter} {label}");
		// Off-TTY the live header won't render, so mirror the transition to
		// scroll-back; on a TTY the header below carries it (no duplicate).
		if self.pb.is_hidden() {
			let _ = self.multi.println(format!("{}{line}", indent(self.depth)));
		}
		self.pb.set_message(line);
		self.pending = Some((counter, label.to_owned()));
	}

	fn finish(&mut self, mark: Mark) {
		if let Some((counter, label)) = self.pending.take() {
			self.flush(&counter, mark.glyph(), &label);
		}
	}
}

impl Drop for IndicatifSection {
	fn drop(&mut self) {
		self.pb.finish_and_clear();
	}
}

/// A leaf indicator: just its bar, cleared on drop.
struct IndicatifLeaf {
	pb: ProgressBar,
}

impl LeafNode for IndicatifLeaf {
	fn inc(&self, delta: u64) {
		self.pb.inc(delta);
	}
	fn set_position(&self, pos: u64) {
		self.pb.set_position(pos);
	}
	fn set_length(&self, len: u64) {
		self.pb.set_length(len);
	}
	fn set_message(&self, msg: &str) {
		self.pb.set_message(msg.to_owned());
	}
}

impl Drop for IndicatifLeaf {
	fn drop(&mut self) {
		self.pb.finish_and_clear();
	}
}

/// A completed/cancelled child line for scroll-back: "{ind}{counter} {mark} {label}",
/// where `mark` is `✓` for a finished child or `✗` for one cancelled by interrupt.
fn done_line(ind: &str, counter: &str, mark: char, label: &str) -> String {
	format!("{ind}{counter} {mark} {label}")
}

/// Static (no steady-tick) header bar for a section at `level`.
fn level_header(level: usize) -> ProgressBar {
	let template = format!("{}{{msg:.green/yellow}}", indent(level));
	ProgressBar::new_spinner().with_style(
		ProgressStyle::with_template(&template).expect("static template").progress_chars("=>-"),
	)
}

/// A leaf bar for `kind`, indented for `depth`.
fn build_bar(depth: usize, kind: LeafKind) -> ProgressBar {
	let ind = indent(depth);
	let (pb, body) = match kind {
		LeafKind::Spinner => {
			return ProgressBar::new_spinner().with_style(styled(&format!(
				"{ind}{{spinner:.green/yellow}} {{msg}}{{prefix:.dim}}"
			)));
		}
		LeafKind::Bytes(total) => (
			bar_for(total),
			"[{bar:40.green/yellow}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} eta {eta:>5} {msg}{prefix:.dim}",
		),
		LeafKind::Count(total) => {
			(bar_for(total), "[{bar:40.green/yellow}] {pos:>3}/{len:<3} {msg}{prefix:.dim}")
		}
		LeafKind::CountLabeled(total) => {
			(bar_for(total), "[{bar:40.green/yellow}] {msg}{prefix:.dim}")
		}
	};
	pb.with_style(styled(&format!("{ind}{body}")))
}

/// `ProgressBar::new(t)` when the total is known, else an unbounded bar.
fn bar_for(total: Option<u64>) -> ProgressBar {
	total.map_or_else(ProgressBar::no_length, ProgressBar::new)
}

fn styled(template: &str) -> ProgressStyle {
	ProgressStyle::with_template(template).expect("static template").progress_chars("=>-")
}
