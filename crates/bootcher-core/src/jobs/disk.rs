use crate::builder::Builder;
use crate::context::{Arch, DiskTarget, DiskType, Manifest};
use crate::hooks::{DiskTargetMeta, HookMetadata, Phase, Stage};
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

/// Check the external tools the `image-builder` fan-out needs: a local
/// `podman`, plus, per arch's `[builder] image`, either a privileged local
/// image-builder run (`sudo`, and qemu-user for a cross-arch in-process build) or a
/// remote/VM transport. Co-located with [`run`]; the CLI / pipeline calls it before
/// any disk build starts.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub(crate) fn preflight(manifest: &Manifest, target: Option<Arch>) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	for image in manifest.images_for(target) {
		let arch = image.arch;
		let spec = manifest.image_builder(arch);
		if crate::builder::is_local(spec) {
			// The local image-builder step runs privileged, opened through one `sudo sh`.
			checks.bin("sudo", "run the privileged image-builder step (`sudo sh`)");
			if Arch::host() != Some(arch) {
				checks.qemu_user();
			}
		} else {
			preflight::builder_transport(spec, arch, &mut checks);
		}
	}
	checks.finish()
}

/// Build the run's disk image(s) with `image-builder`, in parallel — for
/// `provision` (build then disk). `target`/`disks` are the run's [`Manifest`]
/// selection (threaded from the CLI, the manifest itself untouched): `target` `None`
/// fans out over every arch in `[targets]`, `Some(arch)` over just that one
/// ([`Manifest::images_for`]); an empty `disks` builds each arch's full type list,
/// else only the listed types ([`Manifest::disks_for`]). So `provision --target
/// aarch64 --disk qcow2` builds exactly one disk.
///
/// Each arch runs on its own worker (see [`crate::fleet::for_each`]): it selects
/// (and, for a `vm` spec, boots) *its* image builder — the `[builder.<arch>]
/// image` spec — and renders its image-builder run under its own concurrent scope,
/// so a multi-arch run boots cross-arch VMs in parallel and shows per-arch progress
/// side by side. Each disk target writes to its own [`DiskTarget::output_dir`]
/// (`output/<arch>/<disk_type>/`), so neither arches nor an arch's multiple disk
/// types clobber each other.
///
/// `config` is an optional blueprint (TOML) injecting provision-time `/etc`
/// files (admin SSH key, registry pull secret); see `jobs::secrets`. It's shared
/// by every arch (the secrets are arch-independent).
///
/// The `[hooks.disk]` `pre`/`post` lifecycle hooks bracket the *whole* fan-out
/// once — not once per arch — mirroring [`crate::jobs::build::run`]. A
/// `disk.post` that post-processes the built disks (e.g. embedding board
/// firmware) therefore runs a single time, after every target's artifacts are in
/// their `output/<arch>/<disk_type>/` dir, and must walk those per-target dirs
/// itself for a multi-arch or multi-type project. Running the hooks outside the
/// parallel region also keeps their stdio off the concurrent bars.
///
/// # Errors
///
/// Returns an error if any arch's disk build fails, a hook fails, or a signal interrupts.
pub(crate) fn run(
	manifest: &Manifest,
	target: Option<Arch>,
	disks: &[DiskType],
	config: Option<&str>,
	channel: &str,
	job: &mut Scope,
) -> Result<()> {
	let images = manifest.images_for(target);
	let hooks = manifest.hooks();
	// Project-level (arch-independent) refs the image-builder source ref each arch
	// records as its bootc origin derives from (see `run_one`): the registry list ref
	// for this run's channel (`Some` only in registry mode) takes precedence over the
	// local list ref.
	let registry_ref = manifest.registry_list_ref(channel);
	let local_list_ref = manifest.local_list_ref();
	warn_if_signing_unenforced(manifest, job);

	// The whole (arch × disk_type) build matrix, the unit `BOOTCHER_METADATA.targets`
	// exposes to the disk hooks (and the same units the fan-out below builds). At
	// `disk.pre` the dirs are still empty, so each target's resolved `file` is filled
	// in only for `disk.post`, after the artifacts exist.
	let targets: Vec<DiskTarget> = images
		.iter()
		.flat_map(|image| {
			manifest
				.disks_for(image.arch, disks)
				.into_iter()
				.map(|disk_type| DiskTarget { image: image.clone(), disk_type })
		})
		.collect();
	let provenance = crate::context::GitProvenance::detect(std::path::Path::new("."));
	let mut meta = HookMetadata {
		phase: Phase::Disk,
		stage: Stage::Pre,
		image_name: manifest.general.name.clone(),
		arches: images.iter().map(|i| i.arch).collect(),
		image_ref: registry_ref.clone().unwrap_or_else(|| local_list_ref.clone()),
		revision: provenance.revision,
		version: provenance.version,
		output_dir: Some(PathBuf::from("output")),
		targets: Some(target_meta(&targets, false)),
		remotes: None,
		credential: None,
	};
	crate::hooks::run(&meta, hooks.disk.pre.as_deref(), job)?;

	// If any arch's disk is built in-process, that arch's image-builder does privileged work
	// on this host. Open one authenticated `sudo sh` session now — on the quiet
	// terminal, before the fan-out opens its concurrent bars — so every local root
	// command runs through it (see [`crate::sudo`]): a single password prompt that
	// holds even on a `timestamp_timeout=0` host, never re-prompting under another
	// arch's live progress. Gated so an all-remote/VM build never prompts for a
	// credential it won't use; the guard's drop tears the session down.
	let _sudo = images
		.iter()
		.any(|i| crate::builder::runs_local_root(manifest.image_builder(i.arch)))
		.then(|| crate::sudo::ensure_session(job))
		.transpose()?;

	crate::fleet::for_each(
		&images,
		"arch",
		"disk",
		|i| i.arch.to_string(),
		manifest.concurrency().disk,
		job,
		|image, work| {
			// Each worker selects (and, for a `vm` spec, boots) its own image builder on
			// its concurrent scope, so cross-arch VMs come up in parallel.
			let builder = crate::builder::select(image, manifest.image_builder(image.arch), work)?;
			// One container build per arch, but one image-builder run per disk type listed
			// for it ([targets]): each type reuses this arch's container and the
			// builder selected above, so a `vm`/remote builder boots or connects just once
			// for the whole list. `build_disk` renders its phases as leaf bars/spinners on
			// `work` (never header steps), so the worker's arch header stays put above them.
			for disk_type in manifest.disks_for(image.arch, disks) {
				let target = DiskTarget { image: image.clone(), disk_type };
				run_one(
					&target,
					registry_ref.as_deref(),
					&local_list_ref,
					builder.as_ref(),
					config,
					work,
				)?;
			}
			Ok(())
		},
	)?;
	// `disk.post` runs once, after every target's artifacts are in their output dir,
	// so now resolve each target's concrete `disk.<ext>` for the metadata.
	meta.stage = Stage::Post;
	meta.targets = Some(target_meta(&targets, true));
	crate::hooks::run(&meta, hooks.disk.post.as_deref(), job)
}

/// Map the build matrix to [`DiskTargetMeta`] for `BOOTCHER_METADATA`. When
/// `resolve_files` (i.e. at `disk.post`, after the build), each target's concrete
/// `disk.<ext>` is looked up via [`resolve_disk_file`]; at `disk.pre` (`false`) the
/// artifacts don't exist yet so `file` is left `None`.
fn target_meta(targets: &[DiskTarget], resolve_files: bool) -> Vec<DiskTargetMeta> {
	targets
		.iter()
		.map(|t| {
			let dir = t.output_dir();
			let file = resolve_files.then(|| resolve_disk_file(&dir)).flatten();
			DiskTargetMeta { arch: t.image.arch, disk_type: t.disk_type, dir, file }
		})
		.collect()
}

/// The single `disk.*` artifact `image-builder` wrote in `dir`, if it resolves
/// unambiguously — what `disk.post`'s metadata hands the hook so a recipe needn't
/// know the type→extension mapping (`ami`→`raw`, `gce`→`tar.gz`, …). `None` (recipe
/// falls back to `dir`) when the dir is unreadable or doesn't hold exactly one
/// `disk.*` file.
fn resolve_disk_file(dir: &Path) -> Option<PathBuf> {
	let mut matches =
		fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| {
			p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("disk."))
		});
	let first = matches.next()?;
	matches.next().is_none().then_some(first)
}

/// Warn (don't fail) when signing is configured (via `[deploy] registry`) but the
/// build context ships no bootc install config enabling `enforce-container-sigpolicy`.
/// Without it a freshly provisioned device won't record a verifying origin and so won't
/// enforce signatures until its first `deploy` switches the scheme — a real gap worth
/// flagging loudly, but not fatal (the deploy path still enforces). `bootcher init`
/// with signing bakes the file; this catches a project that enabled signing afterward.
fn warn_if_signing_unenforced(manifest: &Manifest, job: &Scope) {
	if manifest.signing().is_none() {
		return;
	}
	let dir = std::path::Path::new("sysroot/usr/lib/bootc/install");
	let enforced = fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
		fs::read_to_string(e.path()).is_ok_and(|s| s.contains("enforce-container-sigpolicy"))
	});
	if !enforced {
		job.println(
			"warning: signing is configured in `[deploy] registry` but no bootc install config \
			 enables enforce-container-sigpolicy — a freshly provisioned device won't enforce \
			 signatures until its first `deploy`. Add sysroot/usr/lib/bootc/install/\
			 30-bootcher-signing.toml with `[install]\\nenforce-container-sigpolicy = true`.",
		);
	}
}

/// Run `image-builder` for one disk `target` (an arch + disk type) on an
/// already-selected `builder` (hook-free; the `[hooks.disk]` hooks bracket the
/// whole fan-out in [`run`]). On success `target.output_dir()` holds the
/// artifacts, with `source_ref` recorded as the bootc origin.
///
/// # Errors
///
/// Returns an error if the `image-builder` command fails.
pub(crate) fn run_one(
	target: &DiskTarget,
	registry_ref: Option<&str>,
	local_list_ref: &str,
	builder: &dyn Builder,
	config: Option<&str>,
	job: &mut Scope,
) -> Result<()> {
	let output = target.output_dir();
	fs::create_dir_all(&output)?;

	// The image ref image-builder builds from becomes the installed system's bootc
	// deployment origin — i.e. what `bootc upgrade` (and the auto-update timer)
	// pulls from. It's the **suffix-free** ref in both modes, so a single origin
	// resolves to the device's own arch out of the multi-arch image. In registry
	// mode that's the registry manifest-list ref, so a freshly provisioned device
	// fetches updates straight from the registry (authenticated by the baked
	// /etc/ostree/auth.json), no first deploy required. In LAN mode it's the
	// localhost list ref, which `deploy` re-pins to containers-storage on each
	// push. The builder tags the per-arch member as this ref in its (root) storage
	// just long enough for image-builder to record it — no manifest list is needed there.
	//
	// `registry_ref` is `Some(...)` in registry mode, `None` for LAN — the build
	// source ref is the registry list ref when set, else the project's localhost list
	// ref. Both are project-level (arch-independent), passed in from the manifest.
	let source_ref = registry_ref.unwrap_or(local_list_ref).to_owned();

	builder.build_disk(target, &source_ref, config, job)
}

#[cfg(test)]
mod tests {
	use super::resolve_disk_file;
	use std::fs;

	#[test]
	fn resolves_the_single_disk_artifact() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("disk.raw"), b"x").unwrap();
		// A non-`disk.*` sibling (e.g. a build log) doesn't confuse the match.
		fs::write(dir.path().join("manifest.json"), b"{}").unwrap();
		assert_eq!(resolve_disk_file(dir.path()), Some(dir.path().join("disk.raw")));
	}

	#[test]
	fn no_match_when_dir_is_empty_missing_or_ambiguous() {
		let empty = tempfile::tempdir().unwrap();
		assert_eq!(resolve_disk_file(empty.path()), None);
		assert_eq!(resolve_disk_file(&empty.path().join("does-not-exist")), None);

		let ambiguous = tempfile::tempdir().unwrap();
		fs::write(ambiguous.path().join("disk.raw"), b"x").unwrap();
		fs::write(ambiguous.path().join("disk.qcow2"), b"x").unwrap();
		assert_eq!(resolve_disk_file(ambiguous.path()), None);
	}
}
