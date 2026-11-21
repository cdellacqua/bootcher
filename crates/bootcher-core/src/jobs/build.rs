use crate::context::{Arch, GitProvenance, ImageRef, LATEST, Manifest};
use crate::exec::run;
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use anyhow::{Result, bail};
use duct::cmd;

/// Check the external tools the build fan-out needs: a local `podman` (every build
/// loads its result into local rootless storage, even from a remote/VM builder),
/// plus, per arch's `[builder] build`, its transport (ssh for a remote, qemu +
/// firmware for a `vm`) and a qemu-user binfmt handler for a cross-arch in-process
/// build. `target` scopes the checks to the run's arch(es) — the same selection
/// [`run()`] builds (`None` ⇒ every arch). Co-located with [`run()`] so the two stay
/// in sync; the CLI / pipeline calls it before any build starts.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(manifest: &Manifest, target: Option<Arch>) -> Result<()> {
	let arches: Vec<Arch> = manifest.images_for(target).iter().map(|i| i.arch).collect();
	preflight_arches(manifest, &arches)
}

/// [`preflight()`] for an explicit arch set (a refresh's stale arches).
pub(crate) fn preflight_arches(manifest: &Manifest, arches: &[Arch]) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	for &arch in arches {
		let spec = manifest.build_builder(arch);
		preflight::builder_transport(spec, arch, &mut checks);
		// A cross-arch in-process build runs the foreign userspace under qemu-user.
		if crate::builder::is_local(spec) && Arch::host() != Some(arch) {
			checks.qemu_user();
		}
	}
	checks.finish()
}

/// What one build run produces: which arches, under which member tag prefix, and
/// whether to assemble the members into a local manifest list afterwards.
#[derive(Clone, Debug)]
pub(crate) struct BuildOpts {
	/// The arches to build (each must be a `[targets]` arch).
	pub(crate) arches: Vec<Arch>,
	/// The member tag prefix (`localhost/<name>:<prefix>-<arch>`); `latest` for a
	/// plain build, `refresh-<sha12>` for a refresh build in a worktree.
	pub(crate) tag_prefix: String,
	/// `--no-cache` (a forced refresh).
	pub(crate) no_cache: bool,
	/// Assemble the built members into this local list ref afterwards (`None` stops
	/// at the members, e.g. `build --target`).
	pub(crate) list: Option<String>,
}

impl BuildOpts {
	/// A plain build of the current checkout: every arch (or just `target`) under
	/// `latest-<arch>`, assembling `localhost/<name>:latest` when unscoped.
	pub(crate) fn plain(manifest: &Manifest, target: Option<Arch>) -> Self {
		Self {
			arches: manifest.images_for(target).iter().map(|i| i.arch).collect(),
			tag_prefix: LATEST.to_owned(),
			no_cache: false,
			list: target.is_none().then(|| manifest.local_list_ref()),
		}
	}
}

/// Build the run's target arch(es)' bootc container, in parallel, into local
/// rootless storage under `localhost/<name>:latest-<arch>`, then — for an unscoped
/// run — assemble them into the local suffix-free manifest list
/// (`localhost/<name>:latest`). This is `bootcher build`; every other building
/// command shares the same build, parameterised by arch set, tag prefix and cache use.
///
/// # Errors
///
/// Returns an error if any arch's build fails, a hook fails, or a signal interrupts.
pub fn run(manifest: &Manifest, target: Option<Arch>, job: &mut Scope) -> Result<()> {
	run_opts(manifest, &BuildOpts::plain(manifest, target), job).map(drop)
}

/// Build `opts.arches` of `manifest`'s project (its [`Manifest::root`] is the build
/// context, so a refresh points it at a worktree) in parallel, each on its own
/// builder, under the `opts.tag_prefix` member tags, then assemble `opts.list` if
/// set. Returns the git provenance the members were stamped with (so a caller can
/// tell whether they record a source).
///
/// Each arch runs on its own worker (the crate's fleet fan-out): it selects
/// *its* builder (the `[builder.<arch>] build` spec — `local`, a `vm`, or a
/// remote, so a cross-arch arch boots its own VM) and renders its `podman build`
/// and export under its own concurrent scope.
///
/// The `[hooks.build]` `pre`/`post` lifecycle hooks bracket the *whole* fan-out
/// once — not once per arch. A `build.pre` that prepares a shared input therefore
/// runs a single time, before any arch builds. The hooks run from the project root
/// and own the terminal, so running them outside the parallel region also keeps
/// their stdio off the concurrent bars.
///
/// The container is secret-free: the admin SSH key and registry pull secret are
/// injected into the device's `/etc` at provision (see `jobs::secrets` /
/// `jobs::disk`), not baked here.
///
/// # Errors
///
/// Returns an error if any arch's build fails, a hook fails, or a signal interrupts.
pub(crate) fn run_opts(
	manifest: &Manifest,
	opts: &BuildOpts,
	job: &mut Scope,
) -> Result<GitProvenance> {
	let mut images: Vec<ImageRef> =
		opts.arches.iter().map(|&a| manifest.image_tagged(a, &opts.tag_prefix)).collect();
	let hooks = manifest.hooks();
	// Detect git provenance once (the build context is the project root, shared by
	// every arch) and stamp it as OCI labels and manifest annotations on each arch's
	// `podman build`, plus surface it to the hooks below. Best-effort: absent outside a
	// git work tree, and warned about (never fatal) when git is there but can't be read.
	let provenance = GitProvenance::detect(manifest.root(), job);
	let provenance_args = provenance.provenance_args();
	for image in &mut images {
		image.provenance_args.clone_from(&provenance_args);
		image.no_cache = opts.no_cache;
	}
	// The hook's `image_ref` names what the build leaves behind: the assembled list,
	// or the lone member tag for a list-less build, so a `build.post` hook always
	// points at a ref that exists.
	let image_ref = match (&opts.list, images.as_slice()) {
		(Some(list), _) => list.clone(),
		(None, [one]) => one.tag(),
		(None, _) => manifest.local_list_ref_tagged(&opts.tag_prefix),
	};
	let mut meta = crate::hooks::HookMetadata::new(manifest, crate::hooks::Phase::Build, image_ref);
	meta.arches = images.iter().map(|i| i.arch).collect();
	meta.revision.clone_from(&provenance.revision);
	meta.version.clone_from(&provenance.version);
	crate::hooks::run(&meta, hooks.build.pre.as_deref(), job)?;
	crate::fleet::for_each(
		&images,
		"arch",
		"build",
		|i| i.arch.to_string(),
		manifest.concurrency().build,
		job,
		|image, work| {
			// Each worker selects (and, for a `vm` spec, boots) its own builder on its
			// concurrent scope, so cross-arch VMs come up in parallel.
			let builder = crate::builder::select(image, manifest.build_builder(image.arch), work)?;
			builder.build_image(image, work)?;
			builder.export_home(image, work)
		},
	)?;
	if let Some(list) = &opts.list {
		manifest_list(&images, list, &mut job.child("manifest list"))?;
	}
	// `build.post` runs as the build's very last step — after the manifest list is
	// assembled — so `image_ref` names a ref that actually exists by then.
	meta.stage = crate::hooks::Stage::Post;
	crate::hooks::run(&meta, hooks.build.post.as_deref(), job)?;
	Ok(provenance)
}

/// For a direct `--skip-build`: check every member the run needs is already in
/// local storage under `localhost/<name>:latest-<arch>` (however it got there — a
/// prior `bootcher build`, or a CI job that `podman load`ed it), failing with a
/// pointer at `bootcher build` otherwise. There's no registry fallback: a direct
/// device runs what's built here.
///
/// # Errors
///
/// Returns an error naming the missing arches.
pub(crate) fn require_local(manifest: &Manifest, target: Option<Arch>) -> Result<()> {
	let missing: Vec<String> = manifest
		.images_for(target)
		.into_iter()
		.filter(|i| !image_present(&i.tag()))
		.map(|i| i.arch.to_string())
		.collect();
	if !missing.is_empty() {
		bail!(
			"`--skip-build` uses the images already in local storage, but \
			 localhost/{}:latest-<arch> is missing for {} — run `bootcher build` first",
			manifest.general.name,
			missing.join(", ")
		);
	}
	Ok(())
}

/// Whether `tag` resolves to an image already in local rootless storage
/// (`podman image exists`, exit 0 when present).
pub(crate) fn image_present(tag: &str) -> bool {
	cmd!("podman", "image", "exists", tag)
		.stdout_null()
		.stderr_null()
		.unchecked()
		.run()
		.is_ok_and(|o| o.status.success())
}

/// Assemble the per-arch member images into a local OCI **manifest list** named
/// after the project's suffix-free [`Manifest::local_list_ref`] (`localhost/<name>:latest`),
/// so a multi-arch project has one local ref that resolves to any built arch — the
/// same object a publish then `podman manifest push`es to a registry. Every `image`
/// must already be in local rootless storage under its [`ImageRef::tag`] (the
/// postcondition of [`run()`]).
///
/// Idempotent: a stale list of this name from a previous run is removed first,
/// then recreated from the members. A single-arch project gets a one-member list,
/// so the suffix-free ref exists either way.
///
/// `local_list_ref` is the suffix-free name to assemble under (the project's
/// [`Manifest::local_list_ref`]); `images` are its per-arch members.
///
/// # Errors
///
/// Returns an error if `images` is empty or a `podman manifest` command fails.
pub(crate) fn manifest_list(
	images: &[ImageRef],
	local_list_ref: &str,
	job: &mut Scope,
) -> Result<()> {
	anyhow::ensure!(!images.is_empty(), "no arches to assemble into a manifest list");
	job.set_total(1 + images.len() as u64);

	// Drop any prior list of this name so `create` starts clean (it errors if the
	// name already exists). Best-effort — absent is the common case.
	job.step("create manifest");
	cmd!("podman", "manifest", "rm", local_list_ref)
		.stdout_null()
		.stderr_null()
		.unchecked()
		.run()
		.ok();
	run!(job, "podman", "manifest", "create", local_list_ref)?;
	for image in images {
		job.step(format!("add {}", image.arch));
		// Reference the member via the explicit `containers-storage:` transport so
		// podman resolves it from the local store and never falls back to a
		// registry. Without the prefix, older podman (4.9 on CI) fails to resolve
		// the just-built `localhost/<name>:latest-<arch>` image and tries to pull
		// it from `localhost` over HTTPS, dialing `localhost:443` and erroring.
		//
		// Stamp the entry's platform from the known build arch rather than trusting
		// podman to infer it from the image config: some podman versions mislabel a
		// `FROM scratch` image's architecture, which silently collapses two members
		// to one entry. `manifest_list` already knows each member's arch, so make it
		// authoritative.
		run!(
			job,
			"podman",
			"manifest",
			"add",
			"--os",
			"linux",
			"--arch",
			image.arch.oci_arch(),
			local_list_ref,
			&format!("containers-storage:{}", image.tag())
		)?;
	}
	Ok(())
}
