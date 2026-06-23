//! Progress reporting as a small event surface over a swappable rendering
//! backend.
//!
//! The design separates *what is happening* from *how it's drawn*:
//!
//! - The *sink* is the dumb backend — one implementation for the live TTY UI,
//!   one for non-TTY runs, a future JSON sink for machine-readable
//!   output. It is written carefully once per backend; nothing else calls it
//!   directly. It holds no node table: each `open_*` hands back a *self-owned*
//!   section- or leaf-node handle that the caller keeps and that
//!   clears itself on drop — so there is no shared id→node map to lock on the
//!   hot path.
//! - [`Scope`], [`Bar`] and [`Spinner`] are the misuse-resistant surface every
//!   operation uses. The lifecycle is encoded structurally rather than by
//!   convention: a node ends when its handle drops (RAII), a leaf indicator is
//!   only reachable through the [`Scope`] that opened it, an indeterminate
//!   [`Spinner`] has no `inc`, and a child [`Scope`] borrows its parent so it
//!   cannot outlive it. The result is one arbitrarily-nestable kind of section
//!   (no `Pipeline`-vs-`Job` split) plus typed leaf indicators.
//!
//! A [`Scope`] *enumerates its children* ("[i/N] active-child-label") — that is
//! a distinct mechanism from a [`Bar`] count leaf, which tracks a bare number
//! with no children (e.g. podman's `STEP n/m`). They share no implementation.
//!
//! Layout: a [`Scope`] renders a header line "[i/N] active-child-label" at its
//! nesting depth; leaf bars render one level deeper. Headers are static (no
//! steady tick) so they don't fight subprocesses that own the terminal; the
//! backend mirrors transitions and completed-step "✓" lines to scroll-back so a
//! non-TTY run (CI, journal) and the persistent log stay informative.
//!
//! The completed-step mark (`✓`/`✗`) is decided *here*, in the handle layer
//! ([`Scope`]'s drop consults [`crate::signals`]), and passed down to the
//! backend, which only renders it, never judging *why* a section closed.

use std::io::IsTerminal;

mod indicatif_sink;
mod plain;

pub(crate) use indicatif_sink::IndicatifSink;
pub(crate) use plain::PlainSink;

use std::borrow::Cow;

/// What a leaf indicator measures — the typed vocabulary that picks both the
/// rendered shape and the methods available on the returned handle.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LeafKind {
	/// Indeterminate work: a spinner with a message, no measurable progress.
	Spinner,
	/// A byte transfer (download, raw disk write); total may be unknown. The
	/// sink applies the GiB/MiB/KiB prefix.
	Bytes(Option<u64>),
	/// An integer counter "n/N"; total may start unknown and be set later.
	Count(Option<u64>),
	/// Like [`LeafKind::Count`] but the counter text comes from the bar's
	/// *message* rather than its raw position/length, so the fill can advance on
	/// a finer scale than the printed counter (podman's multi-stage builds).
	CountLabeled(Option<u64>),
}

/// How a section finished — chosen by the handle layer at drop time and handed
/// to the backend, which only renders the corresponding glyph.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Mark {
	/// The section completed normally.
	Done,
	/// The section was torn down mid-flight (interrupt).
	Cancelled,
}

impl Mark {
	pub(crate) fn glyph(self) -> char {
		match self {
			Mark::Done => '✓',
			Mark::Cancelled => '✗',
		}
	}
}

/// The rendering backend. Deliberately dumb: backends are few and written once,
/// so the lifecycle/ordering safety lives in the [`Scope`] / [`Bar`] /
/// [`Spinner`] handles above, not here. Every method takes `&self` so a shared
/// `&dyn Sink` can open nodes from several threads (fan-out); the returned node
/// handles carry whatever per-node state the backend needs, so the trait itself
/// keeps no map.
pub(crate) trait Sink: Send + Sync {
	/// Open a section (header) node at nesting `depth`, showing `label` (which
	/// may be empty — a child section whose label is already carried by its
	/// parent's enumerator line).
	fn open_section(&self, depth: usize, label: &str) -> Box<dyn SectionNode>;

	/// Open a leaf indicator at nesting `depth` with an initial `label`
	/// (the message, for a spinner; empty for a self-describing bar) and a
	/// `prefix` rendered to the *right* of the bar — the owning worker's identity
	/// in a fan-out (empty for an ordinary bar). See [`Scope::concurrent_child`].
	fn open_leaf(
		&self,
		depth: usize,
		kind: LeafKind,
		label: &str,
		prefix: &str,
	) -> Box<dyn LeafNode>;

	/// Forward a raw line to scroll-back (subprocess output, free-form messages),
	/// the same channel the live bars use so they aren't clobbered.
	fn log(&self, text: &str);

	/// Clear all bars for the duration of `f`, then redraw them below whatever
	/// `f` wrote. Used around shell-outs that own the terminal.
	fn suspend(&self, f: &mut dyn FnMut());
}

/// A live section header, owned by the [`Scope`] that opened it. Clears its bar
/// on drop; [`SectionNode::finish`] flushes the final completed/cancelled line.
///
/// `Sync` because a parent [`Scope`] is shared by `&` across worker threads
/// during fan-out (see [`Scope::concurrent_child`]); the header itself is only
/// mutated through the owning scope's `&mut self`, never across that share.
pub(crate) trait SectionNode: Send + Sync {
	/// Move to the next active child: render `[current/total] label` (or
	/// `[current] label` when `total` is `None`), flushing the previous child's
	/// `✓` line to scroll-back first.
	fn advance(&mut self, current: u64, total: Option<u64>, label: &str);

	/// Flush the active child's final line marked `mark`. Called once, from the
	/// owning [`Scope`]'s drop.
	fn finish(&mut self, mark: Mark);
}

/// A live leaf indicator (bytes / count / spinner), owned by the [`Bar`] or
/// [`Spinner`] that opened it. Clears its bar on drop. `Sync` so a [`Scope`]
/// holding bars stays shareable across threads alongside its header.
pub(crate) trait LeafNode: Send + Sync {
	fn inc(&self, delta: u64);
	fn set_position(&self, pos: u64);
	fn set_length(&self, len: u64);
	fn set_message(&self, msg: &str);
}

/// Either an owned backend (the root scope) or a borrowed reference to one
/// (every nested scope). The root keeps the backend alive; children borrow it,
/// which is what ties a child's lifetime to its parent.
enum SinkRef<'s> {
	Owned(Box<dyn Sink>),
	Borrowed(&'s dyn Sink),
}

impl SinkRef<'_> {
	fn get(&self) -> &dyn Sink {
		match self {
			SinkRef::Owned(b) => b.as_ref(),
			SinkRef::Borrowed(r) => *r,
		}
	}
}

/// A nestable section of work. Renders a header line showing its currently
/// active child ("[i/N] label"); child scopes and leaf bars render beneath it.
///
/// Obtain the root with [`Scope::root`] / [`Scope::standalone`] (which owns the
/// backend), then [`Scope::child`] to nest. The header advances on every
/// [`Scope::step`] / [`Scope::child`]; the completed line is flushed to
/// scroll-back when the next child starts or the scope drops.
///
/// The header line is *lazy*: it only appears once the scope steps (or, for the
/// root, when given a non-empty label). A child scope's label is already shown
/// by its parent's enumerator line, so it stays headerless until it grows
/// sub-steps of its own — otherwise the same label would render twice. A scope
/// that only ever creates leaf bars (e.g. a builder that streams a download
/// without declaring steps) likewise has no header, so its bars sit at the
/// scope's own depth rather than indenting under a phantom blank line.
pub struct Scope<'s> {
	sink: SinkRef<'s>,
	/// The header node, opened lazily on the first [`Scope::step`] / [`Scope::child`]
	/// (or eagerly for a labeled root). `None` means no header line is shown, and
	/// leaf bars render at `depth` instead of `depth + 1`.
	header: Option<Box<dyn SectionNode>>,
	depth: usize,
	total: Option<u64>,
	current: u64,
	/// Label stamped to the right of every leaf bar opened on this scope — a
	/// fan-out worker's identity (the arch, the device), set by
	/// [`Scope::concurrent_child`] and inherited by [`Scope::child`]. Empty for an
	/// ordinary scope, so its bars render unadorned.
	prefix: Cow<'static, str>,
}

impl Scope<'static> {
	/// Root scope owning a live `IndicatifSink` on a TTY, or a plain-text
	/// `PlainSink` off one (CI, a pipe, a test capturing the child) — the live
	/// bars are pointless without a terminal, and indicatif goes *silent* there, so
	/// off-TTY we fall back to printing each line to stderr instead of nothing.
	/// `label` shows on the top line until the first [`Scope::step`] /
	/// [`Scope::child`] replaces it; `total` is `Some` for an `[i/N]` counter, `None`
	/// for bare `[i]`.
	pub fn root(label: impl Into<Cow<'static, str>>, total: Option<u64>) -> Self {
		let sink: Box<dyn Sink> = if std::io::stderr().is_terminal() {
			Box::new(IndicatifSink::new())
		} else {
			Box::new(PlainSink::new())
		};
		Self::with_sink(sink, label, total)
	}

	/// An unlabeled, uncounted root — a direct CLI invocation with no enclosing
	/// pipeline (or the privileged side of a sudo split).
	#[must_use]
	pub fn standalone() -> Self {
		Self::root("", None)
	}

	/// Root scope over an explicit backend — the seam a test or alternate
	/// frontend uses to drive a custom [`Sink`] instead of the live UI.
	pub(crate) fn with_sink(
		sink: Box<dyn Sink>,
		label: impl Into<Cow<'static, str>>,
		total: Option<u64>,
	) -> Self {
		let label = label.into();
		// A labeled root (a pipeline) shows its name on the top line right away,
		// bare — no "[0/N]" counter until the first child replaces it. An unlabeled
		// root (a standalone job) starts headerless and only grows a header if it
		// steps.
		let header = (!label.is_empty()).then(|| sink.open_section(0, &label));
		Self {
			sink: SinkRef::Owned(sink),
			header,
			depth: 0,
			total,
			current: 0,
			prefix: Cow::Borrowed(""),
		}
	}
}

impl Scope<'_> {
	/// Declare the child count for `[i/N]` rendering. Optional — without it,
	/// steps render `[i]`. For callers that learn the total after construction
	/// (e.g. a standalone job).
	pub fn set_total(&mut self, n: u64) {
		self.total = Some(n);
	}

	/// Advance to the next child activity: bump the counter and update this
	/// scope's header to "[i/N] label". Use for a leaf step whose work is just
	/// bars created on `self`; use [`Scope::child`] when the step has its own
	/// nested structure.
	pub fn step(&mut self, label: impl Into<Cow<'static, str>>) {
		self.advance(&label.into());
	}

	/// Like [`Scope::step`], but returns a nested child scope (one level deeper)
	/// for the step's own sub-steps and bars. The child borrows `self`, so it
	/// must drop before the next `step`/`child` on this scope — the live "one
	/// active child" invariant, enforced by the borrow checker.
	pub fn child(&mut self, label: impl Into<Cow<'static, str>>) -> Scope<'_> {
		self.advance(&label.into());
		// The child starts headerless: it grows one only if it steps. Its bars
		// otherwise anchor to this scope's header (one indent up). It inherits this
		// scope's prefix, so a worker's sub-scopes keep labelling their bars.
		Scope {
			sink: SinkRef::Borrowed(self.sink.get()),
			header: None,
			depth: self.depth + 1,
			total: None,
			current: 0,
			prefix: self.prefix.clone(),
		}
	}

	/// Open a **concurrent** child scope for fan-out work. Unlike [`Scope::child`]
	/// it borrows only the shared, thread-safe sink (a `&dyn Sink`) rather than
	/// `&mut self`, so several may be live at once — one per worker thread under a
	/// [`std::thread::scope`] — and it does *not* advance this scope's header
	/// counter, since concurrent children have no single "active" one.
	///
	/// `name` (e.g. the arch, the device) is *not* given its own header row;
	/// instead it is stamped to the right of every leaf bar the worker opens, so a
	/// worker shows as a single labelled row (`[==> ] 2/2  aarch64`) rather than a
	/// name line plus a bar line. The underlying `MultiProgress` stacks every
	/// sibling's bar and updates them independently. Pair it with a
	/// [`Scope::count`] bar on the parent for overall progress, and drive the
	/// fan-out with the crate's fleet worker pool.
	///
	/// The returned scope carries only a borrowed sink reference, so it is `Send`
	/// — created and used inside the worker it belongs to. Open leaf bars on it;
	/// the worker conventionally renders its phases as bars/spinners, not header
	/// steps, so the `name` label rides along on each.
	pub fn concurrent_child(&self, name: impl AsRef<str>) -> Scope<'_> {
		Scope {
			sink: SinkRef::Borrowed(self.sink.get()),
			header: None,
			depth: self.depth + 1,
			total: None,
			current: 0,
			prefix: Cow::Owned(name.as_ref().to_owned()),
		}
	}

	fn advance(&mut self, label: &str) {
		self.current += 1;
		let (current, total) = (self.current, self.total);
		self.ensure_header().advance(current, total, label);
	}

	/// The header node, opening it on first use. The one place a header line
	/// comes into existence (besides a labeled root). A lazily-opened header
	/// carries no label of its own — the parent's enumerator already shows it.
	fn ensure_header(&mut self) -> &mut dyn SectionNode {
		if self.header.is_none() {
			self.header = Some(self.sink.get().open_section(self.depth, ""));
		}
		self.header.as_deref_mut().expect("header just set")
	}

	/// Indent depth for this scope's leaf bars: one below its header when it has
	/// one, else at its own depth (nothing to nest under).
	fn leaf_depth(&self) -> usize {
		self.depth + usize::from(self.header.is_some())
	}

	/// Determinate byte bar (download, raw disk write); `total` may be unknown.
	#[must_use]
	pub fn bytes(&self, total: Option<u64>) -> Bar {
		self.leaf(LeafKind::Bytes(total), "")
	}

	/// Integer-counter bar showing "n/N"; `total` may start unknown.
	#[must_use]
	pub fn count(&self, total: Option<u64>) -> Bar {
		self.leaf(LeafKind::Count(total), "")
	}

	/// Counter bar whose printed text is its message rather than `pos`/`len`, so
	/// the fill can interpolate on a finer scale than the displayed counter.
	#[must_use]
	pub fn count_labeled(&self, total: Option<u64>) -> Bar {
		self.leaf(LeafKind::CountLabeled(total), "")
	}

	/// Indeterminate spinner labeled `msg`.
	pub fn spinner(&self, msg: impl Into<Cow<'static, str>>) -> Spinner {
		let node = self.sink.get().open_leaf(
			self.leaf_depth(),
			LeafKind::Spinner,
			&msg.into(),
			&self.prefix,
		);
		Spinner { node }
	}

	fn leaf(&self, kind: LeafKind, label: &str) -> Bar {
		let node = self.sink.get().open_leaf(self.leaf_depth(), kind, label, &self.prefix);
		Bar { node }
	}

	/// Forward `line` to scroll-back without clobbering the live bars.
	pub fn println(&self, line: impl AsRef<str>) {
		self.sink.get().log(line.as_ref());
	}

	/// Clear all bars for the duration of `f`, then redraw them. Use around
	/// shell-outs that own the terminal (sudo'd children, interactive prompts).
	///
	/// # Panics
	///
	/// Panics if the sink calls the suspend closure more than once (internal invariant).
	pub fn suspend<F, R>(&self, f: F) -> R
	where
		F: FnOnce() -> R,
	{
		let mut f = Some(f);
		let mut out = None;
		self.sink.get().suspend(&mut || {
			out = Some((f.take().expect("suspend closure runs once"))());
		});
		out.expect("suspend ran the closure")
	}
}

impl Drop for Scope<'_> {
	fn drop(&mut self) {
		if let Some(header) = self.header.as_mut() {
			// A section torn down with an active child is either a normal end (the
			// child finished) or an interrupt mid-flight: mark `✗` only in the
			// latter, so a cancelled step doesn't masquerade as done. The choice is
			// made here, in the handle layer, and handed to the backend to render.
			let mark = if crate::signals::interrupted() { Mark::Cancelled } else { Mark::Done };
			header.finish(mark);
		}
	}
}

/// A determinate leaf indicator (bytes / count). Created from a [`Scope`];
/// clears itself on drop. Call [`Bar::finish`] to clear it early.
pub struct Bar {
	node: Box<dyn LeafNode>,
}

impl Bar {
	pub fn inc(&self, delta: u64) {
		self.node.inc(delta);
	}
	pub fn set_position(&self, pos: u64) {
		self.node.set_position(pos);
	}
	pub fn set_length(&self, len: u64) {
		self.node.set_length(len);
	}
	pub fn set_message(&self, msg: impl AsRef<str>) {
		self.node.set_message(msg.as_ref());
	}
	/// Clear the bar now rather than at end of scope.
	pub fn finish(self) {}
}

/// An indeterminate leaf indicator: a spinner with no measurable progress.
/// Clears itself on drop; [`Spinner::finish`] clears it early.
pub struct Spinner {
	node: Box<dyn LeafNode>,
}

impl Spinner {
	pub fn set_message(&self, msg: impl AsRef<str>) {
		self.node.set_message(msg.as_ref());
	}
	/// Clear the spinner now rather than at end of scope.
	pub fn finish(self) {}
}

/// Width of a single nesting level — the one source of truth for indentation,
/// shared by the live templates and the scroll-back lines so both nest alike.
const INDENT: &str = "  ";

/// Indent string for nesting depth `level`.
pub(crate) fn indent(level: usize) -> String {
	INDENT.repeat(level)
}
