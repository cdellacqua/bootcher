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
/// earlier `build` produced or iterating on the rollout step.
///
/// # Errors
///
/// Returns an error if the build or upgrade phase fails.
pub fn run(manifest: &Manifest, skip_bootc_upgrade: bool, skip_build: bool) -> Result<()> {
	if skip_build {
		return jobs::upgrade::run(
			manifest,
			skip_bootc_upgrade,
			&mut progress::Scope::standalone(),
		);
	}
	let mut b = progress::Scope::root("deploy", Some(2));
	jobs::build::run(manifest, &mut b.child("build"))?;
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
		jobs::build::preflight(manifest)?;
	}
	jobs::upgrade::preflight(manifest, skip_bootc_upgrade)
}
