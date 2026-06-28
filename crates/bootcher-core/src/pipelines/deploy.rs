use crate::context::Manifest;
use crate::{jobs, progress};
use anyhow::Result;

/// Subsequent deployment: rebuild every target arch's container, then ship the
/// result to the configured remotes. Each arch is (re)built in parallel on its
/// own arch's builder (the `[builder.<arch>]` override, else the flat default)
/// and brought into local storage, then `jobs::upgrade` publishes them — a
/// multi-arch manifest list to a registry, or the matching member to each LAN
/// device.
///
/// Two phases: the build fan-out honours the `[hooks.build]` hooks and the push
/// the `[hooks.upgrade]` ones; `upgrade` ships no disk image, so the disk hooks
/// don't apply here.
///
/// `skip_build` (`--skip-build`) pushes the already-built container, skipping the
/// container build — the push/upgrade phase alone — for shipping an image an
/// earlier `build` produced or iterating on the rollout step. It (re)assembles the
/// local manifest list from the per-arch members in local storage first, so it works
/// both same-runner (after `bootcher build`) and in a split CI pipeline where each
/// arch was built on its own runner by `build --target <arch>` and `podman load`ed
/// here from a job artifact — the deploy job being the only one that needs registry
/// push credentials and the signing key.
///
/// # Errors
///
/// Returns an error if the build or upgrade phase fails.
pub fn run(manifest: &Manifest, skip_bootc_upgrade: bool, skip_build: bool) -> Result<()> {
	if skip_build {
		let mut job = progress::Scope::standalone();
		// `upgrade` pushes the local manifest list but doesn't assemble it (the build
		// phase normally does). With `--skip-build` there was no build phase, so
		// (re)assemble it here from the per-arch members already in local storage —
		// whether a prior same-runner `bootcher build` left them, or the CI
		// `podman load`ed them from parallel `build --target <arch>` jobs. Idempotent
		// (rm + recreate), so the same-runner case is unaffected; this is what lets a
		// split build/deploy CI pipeline publish without this job building anything.
		jobs::build::manifest_list(
			&manifest.images(),
			&manifest.local_list_ref(),
			&mut job.child("manifest list"),
		)?;
		return jobs::upgrade::run(manifest, skip_bootc_upgrade, &mut job);
	}
	let mut b = progress::Scope::root("deploy", Some(2));
	// `deploy` publishes the whole multi-arch image, never a single-arch subset, so the
	// build is unscoped (`None`) — every arch, list assembled. Splitting the build per
	// arch is a separate-job concern (`build --target` + `deploy --skip-build`).
	jobs::build::run(manifest, None, &mut b.child("build"))?;
	jobs::upgrade::run(manifest, skip_bootc_upgrade, &mut b.child("upgrade"))
}

/// Check the external tools `deploy` needs — its two phases in order (`build` then
/// `upgrade`/push). Run up front so a missing `ssh` the push needs fails fast
/// rather than after the rebuild. With `skip_build` only the push phase's tools are
/// checked (the container build is skipped).
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(manifest: &Manifest, skip_bootc_upgrade: bool, skip_build: bool) -> Result<()> {
	if !skip_build {
		jobs::build::preflight(manifest, None)?;
	}
	jobs::upgrade::preflight(manifest, skip_bootc_upgrade)
}
