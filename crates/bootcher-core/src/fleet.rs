//! Run an independent operation across a set of items in parallel.
//!
//! Two embarrassingly-parallel fan-outs share this engine:
//! - a `deploy`/`upgrade`/`rotate` applies the same action to every `[deploy]
//!   remotes` device — each an isolated ssh round-trip ([`for_each_remote`]);
//! - a multi-arch `build`/`image`/`provision` builds every target arch on its
//!   own builder — independent `podman`/`image-builder` runs, possibly each on its own VM
//!   (`build::run` / `disk::run`).
//!
//! [`for_each`] runs the op on a bounded pool of worker threads, attempting
//! *every* item even when some fail (a fleet shouldn't be left half-applied
//! because one box was unreachable; a partial multi-arch build still tells you
//! which arch broke), and errors at the end naming the laggards.
//! [`for_each_remote`] instead hands back the per-device [`Report`], so a device
//! phase can run its `post` hook over a partial outcome before the failure is
//! raised (see [`crate::hooks::run_post`]).
//!
//! Each worker gets its own live [`Scope::concurrent_child`], so per-item
//! progress (transfer bars, `bootc`/`podman` steps, the reboot/online spinners)
//! renders concurrently rather than being flattened — the
//! [`progress`](crate::progress) sink is already thread-safe; this just fans
//! children out across threads.

use crate::progress::Scope;
use crate::signals;
use crate::ssh::Ssh;
use anyhow::{Result, bail};
use serde::Serialize;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

/// One item's outcome in a [`Report`]: its label (the device, the arch) and, when
/// its op failed, the error (rendered with its context chain). Serializes as the
/// `post` hook's `results[]` entry.
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct Outcome {
	/// The item's label — for a device, its `user@host`.
	pub host: String,
	/// Whether the op succeeded on this item.
	pub ok: bool,
	/// The failure, with its context chain — present only when `ok` is false.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

/// Every item's [`Outcome`] from one fan-out, in input order, plus what's needed
/// to phrase the summary. Turned into the fan-out's verdict by [`Self::finish`].
#[derive(Debug)]
pub(crate) struct Report {
	task: String,
	unit: String,
	outcomes: Vec<Outcome>,
}

impl Report {
	/// An empty report: a phase whose per-device rollout had nothing to do (e.g. a
	/// registry push with no remotes to switch).
	pub(crate) fn empty(task: &str, unit: &str) -> Self {
		Self { task: task.to_owned(), unit: unit.to_owned(), outcomes: Vec::new() }
	}

	/// The per-item outcomes, in input order.
	pub(crate) fn outcomes(&self) -> &[Outcome] {
		&self.outcomes
	}

	/// The fan-out's verdict: an error naming every failed item, else `Ok` (printing
	/// the completion line). An empty report is `Ok` silently.
	///
	/// # Errors
	///
	/// Returns an error listing every item whose op failed.
	pub(crate) fn finish(&self, job: &Scope) -> Result<()> {
		let n = self.outcomes.len();
		if n == 0 {
			return Ok(());
		}
		let (task, unit) = (&self.task, &self.unit);
		let mut failed: Vec<&str> =
			self.outcomes.iter().filter(|o| !o.ok).map(|o| o.host.as_str()).collect();
		if !failed.is_empty() {
			failed.sort_unstable();
			bail!("{task} failed on {} of {n} {unit}(s): {}", failed.len(), failed.join(", "));
		}
		job.println(format!("{task} complete on {n} {unit}(s)"));
		Ok(())
	}
}

/// Apply `op` to every item in parallel on a pool of `min(items, available
/// cores)` worker threads pulling from a shared work-index (no idle workers, and
/// the slowest item never blocks a free one). `op` runs on a worker thread and is
/// handed that item's own concurrent [`Scope`] to render its live progress into;
/// it should return an error (with context) on failure, which this function
/// prints against the item.
///
/// `label` names an item (the device, the arch) for its concurrent bar label and the
/// ✓/✗ lines; `unit` is the noun for the banner/summary counts ("device",
/// "arch"); `task` names the operation. `limit` is the manifest's
/// `[concurrency]` cap for this activity (`None` = unbounded), which throttles the
/// worker pool below the host core count on a constrained machine. Results are
/// drained on the calling thread as they complete — ticking a count bar and
/// printing each outcome as it lands (out of order, each line named). On any
/// failure the call errors naming every item that didn't take it; otherwise it's
/// `Ok`.
///
/// # Errors
///
/// Returns an error listing every item whose `op` returned an error, or if a
/// shutdown signal cut the fan-out short.
pub(crate) fn for_each<T, L, F>(
	items: &[T],
	unit: &str,
	task: &str,
	label: L,
	limit: Option<NonZeroUsize>,
	job: &mut Scope,
	op: F,
) -> Result<()>
where
	T: Sync,
	L: Fn(&T) -> String + Sync,
	F: Fn(&T, &mut Scope) -> Result<()> + Sync,
{
	report(items, unit, task, label, limit, job, op)?.finish(job)
}

/// [`for_each`] without the verdict: run the fan-out and hand back every item's
/// [`Outcome`] as a [`Report`], failures included, for the caller to act on before
/// raising them with [`Report::finish`].
///
/// # Errors
///
/// Returns an error only if a shutdown signal cut the fan-out short — an
/// interrupted run reports nothing, since some items were never attempted.
fn report<T, L, F>(
	items: &[T],
	unit: &str,
	task: &str,
	label: L,
	limit: Option<NonZeroUsize>,
	job: &mut Scope,
	op: F,
) -> Result<Report>
where
	T: Sync,
	L: Fn(&T) -> String + Sync,
	F: Fn(&T, &mut Scope) -> Result<()> + Sync,
{
	let n = items.len();
	if n == 0 {
		return Ok(Report::empty(task, unit));
	}
	let workers = worker_count(n, limit);
	job.println(format!("{task}: {n} {unit}(s) across {workers} worker(s)"));

	let next = AtomicUsize::new(0);
	let (tx, rx) = mpsc::channel::<(usize, Result<()>)>();
	let mut outcomes: Vec<Option<Outcome>> = vec![None; n];

	let bar = job.count(Some(n as u64));
	// Shared, read-only handle to the parent for the workers (each opens its own
	// concurrent child off it) and the drain loop (println). The sink behind it is
	// thread-safe; `Scope` carries only a borrowed reference to it.
	let parent: &Scope = job;
	let label = &label;

	thread::scope(|scope| {
		for _ in 0..workers {
			let tx = tx.clone();
			let next = &next;
			let op = &op;
			scope.spawn(move || {
				// Pull the next item until the queue's drained (or a signal
				// arrives), each on its own live child scope.
				loop {
					if signals::interrupted() {
						break;
					}
					let i = next.fetch_add(1, Ordering::Relaxed);
					if i >= n {
						break;
					}
					let item = &items[i];
					let mut child = parent.concurrent_child(label(item));
					let result = op(item, &mut child);
					drop(child);
					if tx.send((i, result)).is_err() {
						break;
					}
				}
			});
		}
		// Drop the spare sender so `rx` ends once every worker has finished.
		drop(tx);

		// Drain results as they complete, on this (the only) progress thread.
		while let Ok((i, result)) = rx.recv() {
			bar.inc(1);
			let name = label(&items[i]);
			let error = match result {
				Ok(()) => {
					parent.println(format!("✓ {name}: {task} done"));
					None
				}
				Err(e) => {
					let error = format!("{e:#}");
					parent.println(format!("✗ {name}: {task} failed: {error}"));
					Some(error)
				}
			};
			outcomes[i] = Some(Outcome { host: name, ok: error.is_none(), error });
		}
	});

	// A signal stops the workers pulling new items, leaving some unattempted (`None`).
	signals::check()?;
	Ok(Report {
		task: task.to_owned(),
		unit: unit.to_owned(),
		outcomes: outcomes.into_iter().flatten().collect(),
	})
}

/// The fan-out specialised to the deploy fleet: the items are `[deploy] remotes`
/// destinations, each its own ssh round-trip. Returns the per-device [`Report`]
/// rather than a verdict, so the device phases (`upgrade`, `rotate`) run their
/// `post` hook over a partial outcome before raising it. `limit` is the relevant
/// `[concurrency]` cap (`upgrade` / `rotate`).
///
/// # Errors
///
/// Returns an error only if a shutdown signal cut the rollout short.
pub(crate) fn for_each_remote<F>(
	remotes: &[Ssh],
	task: &str,
	limit: Option<NonZeroUsize>,
	job: &mut Scope,
	op: F,
) -> Result<Report>
where
	F: Fn(&Ssh, &mut Scope) -> Result<()> + Sync,
{
	report(
		remotes,
		"device",
		task,
		|ssh| ssh.host().to_string(),
		limit,
		job,
		|remote, scope| op(remote, scope),
	)
}

/// [`for_each_remote`] over arbitrary per-host items (takeover's two-identity
/// targets), labelled by `label`; same [`Report`] contract.
///
/// # Errors
///
/// Returns an error only if a shutdown signal cut the rollout short.
pub(crate) fn for_each_host<T, L, F>(
	items: &[T],
	task: &str,
	label: L,
	limit: Option<NonZeroUsize>,
	job: &mut Scope,
	op: F,
) -> Result<Report>
where
	T: Sync,
	L: Fn(&T) -> String + Sync,
	F: Fn(&T, &mut Scope) -> Result<()> + Sync,
{
	report(items, "host", task, label, limit, job, op)
}

/// Worker-pool size: the host's available parallelism, capped by the work-item
/// count `n` (no point spawning idle workers) and, when set, the manifest's
/// `[concurrency]` `limit` for this activity (so a constrained machine can
/// throttle the heavy fan-outs). Always at least 1.
fn worker_count(n: usize, limit: Option<NonZeroUsize>) -> usize {
	let avail = thread::available_parallelism().map_or(1, NonZeroUsize::get);
	let mut workers = avail.min(n);
	if let Some(limit) = limit {
		workers = workers.min(limit.get());
	}
	workers
}

#[cfg(test)]
mod tests {
	use super::{for_each_host, worker_count};
	use crate::progress::Scope;
	use anyhow::bail;
	use std::num::NonZeroUsize;
	use std::thread;

	#[test]
	fn report_keeps_every_outcome_in_input_order_and_finish_names_failures() {
		// A partial failure is reported, not raised: every item is attempted and
		// lands in input order (whatever order the workers finished in), with the
		// error only on the failures. `finish` then raises it naming the laggards.
		let items = ["c", "a", "b", "d"];
		let mut job = Scope::standalone();
		let report = for_each_host(
			&items,
			"op",
			|s| (*s).to_owned(),
			None,
			&mut job,
			|s, _| {
				if matches!(*s, "a" | "d") {
					bail!("{s} broke");
				}
				Ok(())
			},
		)
		.unwrap();
		let got: Vec<_> =
			report.outcomes().iter().map(|o| (o.host.as_str(), o.ok, o.error.as_deref())).collect();
		assert_eq!(
			got,
			[
				("c", true, None),
				("a", false, Some("a broke")),
				("b", true, None),
				("d", false, Some("d broke"))
			]
		);
		let err = report.finish(&job).unwrap_err().to_string();
		assert_eq!(err, "op failed on 2 of 4 host(s): a, d");
	}

	#[test]
	fn an_all_ok_or_empty_report_finishes_ok() {
		let mut job = Scope::standalone();
		let ok =
			for_each_host(&["a", "b"], "op", |s| (*s).to_owned(), None, &mut job, |_, _| Ok(()))
				.unwrap();
		assert!(ok.outcomes().iter().all(|o| o.ok && o.error.is_none()));
		assert!(ok.finish(&job).is_ok());
		let none: [&str; 0] = [];
		let empty =
			for_each_host(&none, "op", |s| (*s).to_owned(), None, &mut job, |_, _| Ok(())).unwrap();
		assert!(empty.outcomes().is_empty());
		assert!(empty.finish(&job).is_ok());
	}

	fn nz(n: usize) -> Option<NonZeroUsize> {
		NonZeroUsize::new(n)
	}

	#[test]
	fn caps_at_item_count_when_uncapped() {
		// Never more workers than there is work, regardless of core count.
		let avail = thread::available_parallelism().map_or(1, NonZeroUsize::get);
		assert_eq!(worker_count(1, None), 1);
		assert_eq!(worker_count(3, None), 3.min(avail));
	}

	#[test]
	fn limit_only_lowers_the_pool() {
		// A limit caps below the item/core bound but never raises it: it's the
		// uncapped size capped by the limit. (Expressed against the uncapped value so
		// the test holds whatever the host core count is.)
		assert_eq!(worker_count(4, nz(2)), worker_count(4, None).min(2));
		// A limit far above the item count is a no-op — the item count still bounds it.
		assert_eq!(worker_count(2, nz(99)), worker_count(2, None));
		// A limit of 1 forces fully-serial execution however many items/cores.
		assert_eq!(worker_count(8, nz(1)), 1);
	}
}
