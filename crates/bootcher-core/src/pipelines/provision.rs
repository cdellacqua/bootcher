use anyhow::Result;

use crate::context::{Arch, DiskType, Manifest};
use crate::{jobs, progress};

/// Full first-time provisioning: build every target arch's container, then build
/// each one's disk image(s) — leaving them under `output/<arch>/<disk_type>/` for
/// the user to write or upload to the target. The
/// scaffolded image grows its own root filesystem on first boot (the scaffold
/// ships a drop-in that lets fedora-bootc's `bootc-generic-growpart.service` run
/// on bare metal too).
///
/// Two phases, each fanning out across the project's arches in parallel on their
/// own builders (see [`jobs::build::run`] and the disk job): the
/// build phase brings every arch's container into local rootless storage, the
/// disk phase runs image-builder for each from there. A cross-arch arch whose `[builder]`
/// routes a step to a `vm` boots that VM only in the phase that needs it (e.g. the
/// recommended `build = local`, `image = vm` boots the VM once, in the disk
/// phase). The `[hooks.build]` and `[hooks.disk]` lifecycle hooks bracket their
/// respective phase once (not per arch); any project-specific post-processing of
/// the built disks (embedding firmware or files outside the Containerfile's reach)
/// is a `[hooks.disk] post` hook that walks the per-target output dirs.
///
/// `skip_build` (`--skip-build`) builds the disk from the already-built container
/// instead of rebuilding it — the disk phase alone, for iterating on the disk step
/// or shipping an image an earlier `build` produced. The container is taken from
/// local storage when present, else pulled from the configured registry — so a
/// `provision --skip-build` in a fresh CI runner reuses the exact image the
/// automatically-run `deploy` already built and pushed, rather than rebuilding it.
///
/// `target`/`disks` are the run's [`Manifest`] selection (`provision
/// --target`/`--disk`), threaded straight through to the build and disk jobs rather
/// than narrowing the manifest: `None`/`&[]` is the whole matrix; a `target` scopes
/// every phase to that arch, and `disks` further restricts the disk fan-out to those
/// types. Validate it with [`Manifest::check_selection`] before calling.
///
/// # Errors
///
/// Returns an error if the build/fetch or disk phase fails.
pub fn run(
	manifest: &Manifest,
	target: Option<Arch>,
	disks: &[DiskType],
	config: Option<&str>,
	skip_build: bool,
) -> Result<()> {
	let mut b = progress::Scope::root("provision", Some(2));
	if skip_build {
		jobs::build::ensure_local(manifest, target, &mut b.child("fetch image"))?;
	} else {
		jobs::build::run(manifest, target, &mut b.child("build"))?;
	}
	jobs::disk::run(manifest, target, disks, config, &mut b.child("disk"))
}

/// Check the external tools `provision` needs — its two phases in order (`build`
/// then `disk`). The caller runs this up front, before collecting secrets or
/// starting either phase, so a missing tool the *disk* phase needs fails fast
/// rather than after the whole build. With `skip_build` only the disk phase's tools
/// are checked (the container build is skipped).
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(manifest: &Manifest, target: Option<Arch>, skip_build: bool) -> Result<()> {
	if !skip_build {
		jobs::build::preflight(manifest, target)?;
	}
	jobs::disk::preflight(manifest, target)
}
