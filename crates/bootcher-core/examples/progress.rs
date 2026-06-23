//! Visual diagnostic for [`bootcher_core::progress`]. Drives fake nested scopes,
//! steps and bars — no real work — so the indentation, the `✓` done-lines and
//! the bar nesting can be eyeballed across every layout case.
//!
//! Run with:  `cargo run -p bootcher --example progress`
//!
//! Expected nesting:
//! - pipeline:   child `[i/N]` at L0 → step `[i/N]` at L1 → bar at L2
//! - standalone: step `[i/N]` (or `[i]`) at L0 → bar at L1

use std::thread::sleep;
use std::time::Duration;

use bootcher_core::progress::Scope;

const TICK: Duration = Duration::from_millis(120);

fn main() {
	section("pipeline — nested children / steps / bars");
	pipeline_case();

	section("standalone scope — counted steps [i/N]");
	standalone_counted();

	section("standalone scope — uncounted steps [i]");
	standalone_uncounted();

	section("concurrent fan-out — worker name stamped on each bar");
	concurrent_case();
}

/// A fan-out: a parent count bar over several `concurrent_child` workers running
/// at once, each rendering as a single row whose label sits to the *right* of its
/// bar (no separate name line). Mirrors how `fleet::for_each` drives a multi-arch
/// build or a device fleet.
fn concurrent_case() {
	let job = Scope::standalone();
	let bar = job.count(Some(3));
	let archs = ["x86_64", "aarch64", "s390x"];
	std::thread::scope(|s| {
		for (i, arch) in archs.iter().enumerate() {
			let bar = &bar;
			let job = &job;
			s.spawn(move || {
				let work = job.concurrent_child(arch);
				// Each worker shows phases as leaf bars/spinners; the arch label rides
				// on the right of each, never on its own row.
				let steps = 6 + i * 4;
				let pb = work.count(Some(steps as u64));
				for _ in 0..steps {
					sleep(TICK);
					pb.inc(1);
				}
				pb.finish();
				bar.inc(1);
			});
		}
	});
}

/// A root of three child scopes, each exercising a different bar type, so the
/// L0 child-line / L1 step-line / L2 bar nesting is visible along with the
/// `✓` child- and step-completion lines left behind in scroll-back.
fn pipeline_case() {
	let mut pipe = Scope::root("provision (demo)", Some(3));

	{
		let mut job = pipe.child("build");
		job.set_total(2);

		job.step("layers");
		let bar = job.count(Some(6));
		for _ in 0..6 {
			sleep(TICK);
			bar.inc(1);
		}
		bar.finish();

		job.step("export");
		let sp = job.spinner("writing manifest");
		sleep(8 * TICK);
		sp.finish();
	}

	{
		let mut job = pipe.child("image");
		job.set_total(1);
		job.step("bootc image builder");
		fake_download(&job, 8 << 20);
	}

	{
		let mut job = pipe.child("write");
		job.set_total(2);

		job.step("partition");
		// Forwarded subprocess output lands flush-left (as `run_command` does).
		job.println("sgdisk: creating new GPT on /dev/fake");
		let bar = job.count(Some(3));
		for _ in 0..3 {
			sleep(TICK);
			bar.inc(1);
		}
		bar.finish();

		job.step("copy rootfs");
		fake_download(&job, 4 << 20);
	}
}

/// A standalone scope with a declared total: steps render `[i/N]` at L0 (no
/// parent above), bars one level deeper at L1.
fn standalone_counted() {
	let mut job = Scope::standalone();
	job.set_total(3);

	job.step("build container");
	let sp = job.spinner("cargo build");
	sleep(6 * TICK);
	sp.finish();

	job.step("bootc image builder");
	let bar = job.count(Some(4));
	for _ in 0..4 {
		sleep(TICK);
		bar.inc(1);
	}
	bar.finish();

	job.step("copy rootfs");
	fake_download(&job, 6 << 20);
}

/// A standalone scope with no declared total: steps render `[i]` instead of
/// `[i/N]`.
fn standalone_uncounted() {
	let mut job = Scope::standalone();

	job.step("probe devices");
	let sp = job.spinner("scanning /dev");
	sleep(5 * TICK);
	sp.finish();

	job.step("attach loop");
	let sp = job.spinner("losetup --find");
	sleep(5 * TICK);
	sp.finish();
}

/// Drive a bytes bar from 0 to `total` in twenty fake chunks. Sets a message so
/// the diagnostic exercises the `{msg}` slot in the bytes template (as
/// `fetch::pump` does for a real transfer).
fn fake_download(job: &Scope, total: u64) {
	let bar = job.bytes(Some(total));
	bar.set_message("downloading");
	let chunk = (total / 20).max(1);
	let mut sent = 0;
	while sent < total {
		sleep(TICK);
		let n = chunk.min(total - sent);
		bar.inc(n);
		sent += n;
	}
	bar.finish();
}

/// Blank line + label between cases. Nothing is live here (the prior
/// scope has dropped), so a plain `eprintln!` is safe.
fn section(title: &str) {
	eprintln!("\n=== {title} ===");
}
