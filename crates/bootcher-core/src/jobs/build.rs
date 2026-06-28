use crate::context::{Arch, ImageRef, Manifest};
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
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	for image in manifest.images_for(target) {
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

/// Build the run's target arch(es)' bootc container, in parallel, into local
/// rootless storage — the postcondition `image`/`deploy`/`upgrade` rely on (each
/// member resident under [`ImageRef::tag`]) — then, for a *full* run, assemble them
/// into the local suffix-free manifest list (`localhost/<name>:latest`) as the final
/// step, so the build leaves one usable multi-arch ref. The shared front door for the
/// standalone `build`, for `provision` (build then disk), and for `deploy` (build
/// then push).
///
/// `target` is the run's [`Manifest`] selection (threaded from the CLI, not baked
/// into the manifest): `None` builds every arch in `[targets]` and assembles the
/// list; `Some(arch)` builds just that arch and stops at its
/// `localhost/<name>:latest-<arch>` member — **no list**, since a single-arch list
/// would masquerade as the whole image. The latter is the producer half of a split
/// CI pipeline: each arch built (and transferred) on its own runner, then a later
/// `deploy --skip-build` assembles the list from every arch's member. The pipelines
/// (`provision`/`deploy`/`takeover`) pass their own `target` (`None` for the
/// whole-image `deploy`/`takeover`).
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
pub fn run(manifest: &Manifest, target: Option<Arch>, job: &mut Scope) -> Result<()> {
	let images = manifest.images_for(target);
	let hooks = manifest.hooks();
	// The hook's `image_ref` names what the build leaves behind: the assembled list
	// for a full run, or the lone member tag for a targeted (list-less) build, so a
	// `build.post` hook always points at a ref that exists. Derived straight from the
	// matched `arch` — not `images.first()` — so there's no "can't happen" fallback to
	// silently mask a future bug.
	let image_ref = match target {
		None => manifest.local_list_ref(),
		Some(arch) => manifest.image(arch).tag(),
	};
	let mut meta = crate::hooks::HookMetadata {
		phase: crate::hooks::Phase::Build,
		stage: crate::hooks::Stage::Pre,
		image_name: manifest.general.name.clone(),
		arches: images.iter().map(|i| i.arch).collect(),
		image_ref,
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
	// paths still reassemble it at push time to stay self-contained. A targeted
	// `build --target <arch>` skips it (see `target`), stopping at the member tag.
	if target.is_none() {
		manifest_list(&images, &manifest.local_list_ref(), &mut job.child("manifest list"))?;
	}
	// `build.post` runs as the build's very last step — after the manifest list is
	// assembled — so `image_ref` (the local list ref) names a ref that actually exists
	// by the time the hook can act on it.
	meta.stage = crate::hooks::Stage::Post;
	crate::hooks::run(&meta, hooks.build.post.as_deref(), job)?;
	Ok(())
}

/// For `--skip-build`: make sure the run's target arch(es)' container member(s) are
/// in local rootless storage for the consuming phase to read, **pulling from the
/// configured registry** when the store doesn't already have one. The seeding
/// counterpart to [`run()`] — same postcondition (each member resident under its
/// [`ImageRef::tag`]), reached by a pull instead of a build. `target` is the run's
/// selection ([`Manifest::images_for`]), so a `provision --skip-build --target
/// <arch>` only fetches that arch.
///
/// A member already present is reused untouched (no network), e.g. one a prior
/// `bootcher build` left behind. A missing member is pulled out of
/// `<registry>/<name>:latest` — the multi-arch list `deploy` pushes — and tagged as
/// its local member ref, so a `provision --skip-build` on an empty store (a fresh CI
/// runner) builds the disk from the exact image `deploy` published, no container
/// rebuild.
///
/// Registry mode only: with no `[deploy] registry` there's nowhere to pull a missing
/// member from, so it's a hard error pointing at `bootcher build`. Pulling a private
/// registry needs this host logged in (`podman login`) with at least read access —
/// the same credential `deploy` uses to push.
///
/// # Errors
///
/// Returns an error if a member is missing with no registry to pull it from, or a
/// registry pull/tag fails.
pub(crate) fn ensure_local(
	manifest: &Manifest,
	target: Option<Arch>,
	job: &mut Scope,
) -> Result<()> {
	// Only the absent members need fetching; the present ones (e.g. from a prior
	// `bootcher build`) are reused untouched. Filtering first also keeps the whole
	// step a silent no-op when nothing is missing.
	let missing: Vec<ImageRef> =
		manifest.images_for(target).into_iter().filter(|i| !image_present(&i.tag())).collect();
	if missing.is_empty() {
		return Ok(());
	}
	// The multi-arch list `deploy` pushed; absent it there's nothing to pull from.
	let Some(list_ref) = manifest.registry_list_ref() else {
		let absent: Vec<_> = missing.iter().map(|i| i.arch.to_string()).collect();
		bail!(
			"`--skip-build` needs the container in local storage, but it's missing for {} \
			 and no `[deploy] registry` is configured to pull it from — run `bootcher build` \
			 first, or set a registry in bootcher.toml",
			absent.join(", ")
		);
	};
	job.set_total(missing.len() as u64);
	for image in &missing {
		job.step(format!("fetch {}", image.arch));
		// Pull this arch's member out of the multi-arch list and tag it as the local
		// member ref the disk phase reads ([`ImageRef::tag`]). `--arch` resolves the
		// member from the list; pulling the same list ref for a second arch only
		// re-points the bare `list_ref` tag — which nothing relies on, since the disk
		// build consumes the per-arch member tags created here.
		run!(job, "podman", "pull", "--os", "linux", "--arch", image.arch.oci_arch(), &list_ref)?;
		run!(job, "podman", "tag", &list_ref, image.tag())?;
	}
	Ok(())
}

/// Whether `tag` resolves to an image already in local rootless storage
/// (`podman image exists`, exit 0 when present). Used by [`ensure_local`] to skip
/// the registry pull for members a prior build already left behind.
fn image_present(tag: &str) -> bool {
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
