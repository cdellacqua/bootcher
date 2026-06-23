//! Shared best-effort podman-store teardown primitives.
//!
//! Every RAII cleanup guard in the codebase — the build scratch guards, the
//! interrupt-time working-container sweep, and the
//! throwaway test registries — ends up issuing the same handful of `podman`
//! removals. This module gives each its own named, podman-shaped helper so the
//! command (and its rationale, e.g. why container teardown needs `-v`) lives in
//! one place. They all run on a teardown/unwind path, so each defers to
//! [`crate::exec::best_effort`] for the silent, never-raise contract those paths
//! require — an ad-hoc podman command not covered here calls that directly.

use crate::exec::best_effort;
use std::ffi::{OsStr, OsString};

/// Force-remove a container *and its anonymous volumes* (`rm -f -v`).
///
/// The `-v` matters: images like `registry:2` declare a `VOLUME`, so each run
/// gets an anonymous volume (for a registry, holding the pushed blobs — multi-GB
/// for a bootc image). A plain `rm -f` orphans that volume even when the
/// container ran under `--rm`, leaking one per run; `-v` reaps it with the
/// container. Best-effort: a prior container of this name may not exist.
pub fn reap_container(name: &str) {
	best_effort(&duct::cmd!("podman", "rm", "-f", "-v", name));
}

/// Best-effort untag of local images by reference (`rmi -f --ignore`). `--ignore`
/// makes an absent ref a no-op (so this is safe on any exit path); `-f` removes
/// even if some container still references it. A no-op when `refs` is empty.
pub(crate) fn rmi<I, S>(refs: I)
where
	I: IntoIterator<Item = S>,
	S: AsRef<OsStr>,
{
	let mut argv: Vec<OsString> = ["rmi", "-f", "--ignore"].iter().map(Into::into).collect();
	argv.extend(refs.into_iter().map(|r| r.as_ref().to_owned()));
	if argv.len() == 3 {
		return; // nothing beyond the flags
	}
	best_effort(&duct::cmd("podman", argv));
}

/// Untag every local image whose `repository:tag` starts with `prefix`, by
/// reference (not by id). Used by tests that push under `localhost:<port>/…` into
/// the *default* store (no `XDG_DATA_HOME` override) and so must surgically remove
/// their own refs rather than wipe the store.
///
/// Matched in Rust rather than via podman's `reference=` filter: that filter's
/// glob doesn't cross `/`, and when it matches it lists *every* tag of the image
/// id — which would sweep a shared cached base (e.g. busybox) sharing the layer.
/// Removing by the matched reference leaves such bases, under their own names, be.
#[cfg(test)]
pub(crate) fn untag_prefixed(prefix: &str) {
	list_filter_remove(
		&["images", "--format", "{{.Repository}}:{{.Tag}}"],
		|r| r.starts_with(prefix),
		&["rmi", "-f"],
	);
}

/// Best-effort reclaim of the buildah working containers that a daemonless
/// `podman build` strands when it's interrupted.
///
/// On SIGINT/SIGTERM `podman build` `os.Exit`s without running its own cleanup,
/// leaving the in-flight step's working container behind — overlay rootfs still
/// *mounted*, so a later plain `podman rm --storage` refuses it with "container
/// state improper". Nothing else reaps these: a clean build *failure* removes its
/// own container, so only interruption leaks. Since podman never self-cleans on a
/// signal (verified across SIGINT/SIGTERM, group delivery, and `--force-rm`),
/// bootcher sweeps them itself on the way out rather than relying on a gentler
/// kill that wouldn't help.
///
/// `--storage -f` is required (the `-f` unmounts the stranded rootfs first); the
/// `working-container` name match keeps the sweep to build leftovers — including
/// an unrelated build's, the accepted cost of the simplest reliable approach —
/// and off the user's real `podman` containers. Rootless only: `LocalBuilder`'s
/// image build runs unprivileged, and only its image-builder *step* uses `sudo podman`,
/// which is a `podman run`, not a build, so it leaves no working container.
pub fn sweep_working_containers() {
	list_filter_remove(
		&["ps", "-a", "--storage", "--format", "{{.Names}}"],
		|n| n.contains("working-container"),
		&["rm", "--storage", "-f"],
	);
}

/// Shared shape behind [`sweep_working_containers`] and `untag_prefixed`: list
/// names/refs with a `--format` query, keep the ones matching `keep`, and remove
/// the survivors with one `podman <remove…> <names…>` call. Best-effort and
/// silent throughout; a no-op when nothing matches.
fn list_filter_remove(list_args: &[&str], keep: impl Fn(&str) -> bool, remove_args: &[&str]) {
	let Ok(listed) = duct::cmd("podman", list_args).stderr_null().unchecked().read() else {
		return;
	};
	let matched: Vec<&str> = listed.lines().map(str::trim).filter(|l| keep(l)).collect();
	if matched.is_empty() {
		return;
	}
	let mut argv: Vec<&str> = remove_args.to_vec();
	argv.extend_from_slice(&matched);
	best_effort(&duct::cmd("podman", argv));
}
