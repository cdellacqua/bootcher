use crate::context::{Arch, ImageRef, Manifest};
use crate::exec::run;
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use anyhow::Result;
use duct::cmd;

/// Check the external tools the build fan-out needs: a local `podman` (every build
/// loads its result into local rootless storage, even from a remote/VM builder),
/// plus, per arch's `[builder] build`, its transport (ssh for a remote, qemu +
/// firmware for a `vm`) and a qemu-user binfmt handler for a cross-arch in-process
/// build. Co-located with [`run()`] so the two stay in sync; the CLI / pipeline calls
/// it before any build starts.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(manifest: &Manifest) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	for image in manifest.images() {
		let arch = image.arch;
		let spec = manifest.build_builder(arch);
		preflight::builder_transport(spec, arch, &mut checks);
		// A cross-arch in-process build runs the foreign userspace under qemu-user.
		if crate::builder::is_local(spec) && Arch::host() != Some(arch) {
			checks.qemu_user();
		}
	}
	checks.finish()
}

/// Build every target arch's bootc container, in parallel, into local rootless
/// storage — the postcondition `image`/`deploy`/`upgrade` rely on (each member
/// resident under [`ImageRef::tag`]) — then assemble them into the local suffix-free
/// manifest list (`localhost/<name>:latest`) as the final step, so every build
/// leaves one usable multi-arch ref. The shared front door for the standalone
/// `build`, for `provision` (build then disk), and for `deploy` (build then push).
///
/// Each arch runs on its own worker (the crate's fleet fan-out): it selects
/// *its* builder (the `[builder.<arch>] build` spec — `local`, a `vm`, or a
/// remote, so a cross-arch arch boots its own VM) and renders its `podman build`
/// and export under its own concurrent scope, so a multi-arch build exploits the
/// builder host's cores and shows per-arch progress side by side instead of one
/// serial stream.
///
/// The `[hooks.build]` `pre`/`post` lifecycle hooks bracket the *whole* fan-out
/// once — not once per arch — mirroring how the upgrade job runs the
/// upgrade hooks once around the fleet. A `build.pre` that prepares a shared input
/// (e.g. building an auxiliary image the `Containerfile` then `COPY`s) therefore
/// runs a single time, before any arch builds. The hooks own the terminal, so
/// running them outside the parallel region also keeps their stdio off the
/// concurrent bars.
///
/// A project is a single image whose `Containerfile` starts from a public base
/// (`fedora-bootc`), so there is no base image to build first. The container is
/// secret-free: the admin SSH key and registry pull secret are injected into the
/// device's `/etc` at provision (see `jobs::secrets` / `jobs::disk`), not baked
/// here.
///
/// # Errors
///
/// Returns an error if any arch's build fails, a hook fails, or a signal interrupts.
pub fn run(manifest: &Manifest, job: &mut Scope) -> Result<()> {
	let images = manifest.images();
	let hooks = manifest.hooks();
	let mut meta = crate::hooks::HookMetadata {
		phase: crate::hooks::Phase::Build,
		stage: crate::hooks::Stage::Pre,
		image_name: manifest.general.name.clone(),
		arches: images.iter().map(|i| i.arch).collect(),
		image_ref: manifest.local_list_ref(),
		output_dir: None,
		targets: None,
		remotes: None,
	};
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
			// `build_image`/`export_home` render their phases as leaf bars/spinners on
			// `work` (never header steps), so each shows as a single row with the arch
			// label stamped on its right (see `concurrent_child`) — no extra name line.
			builder.build_image(image, work)?;
			builder.export_home(image, work)
		},
	)?;
	// Assemble the freshly built members into the local suffix-free manifest list as
	// the build's final artifact, so a bare `build` (and every pipeline's build
	// phase) leaves one usable `localhost/<name>:latest` — what `deploy`/`takeover`
	// push or serve and a device resolves its own arch from. Local podman metadata
	// only (no layer copy), so it's negligible beside the container build; the push
	// paths still reassemble it at push time to stay self-contained.
	manifest_list(&images, &manifest.local_list_ref(), &mut job.child("manifest list"))?;
	// `build.post` runs as the build's very last step — after the manifest list is
	// assembled — so `image_ref` (the local list ref) names a ref that actually exists
	// by the time the hook can act on it.
	meta.stage = crate::hooks::Stage::Post;
	crate::hooks::run(&meta, hooks.build.post.as_deref(), job)?;
	Ok(())
}

/// Assemble the per-arch member images into a local OCI **manifest list** named
/// after the project's suffix-free [`Manifest::local_list_ref`] (`localhost/<name>:latest`),
/// so a multi-arch project has one local ref that resolves to any built arch — the
/// same object `deploy` then `podman manifest push`es to a registry. Every `image`
/// must already be in local rootless storage under its [`ImageRef::tag`] (the
/// postcondition of [`run()`]).
///
/// Idempotent: a stale list of this name from a previous run is removed first,
/// then recreated from the members. A single-arch project gets a one-member list,
/// so the suffix-free ref exists either way. The push/serve paths don't rebuild
/// this list — they just name it via [`Manifest::local_list_ref`].
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
