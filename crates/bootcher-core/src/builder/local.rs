//! In-process builder: runs `podman build` and the `image-builder` disk step on
//! the host. The default for every target — same-arch natively, cross-arch under
//! qemu-user (binfmt) emulation. Cross-arch is slower than native but works end to
//! end (see `build_disk` for the one emulation quirk it has to work around), and is
//! an order of magnitude faster than a full-system TCG VM, so it stays the default;
//! a project that wants the work offloaded sets `[builder] image = "vm"` or a
//! native-arch remote in bootcher.toml.

use super::{Builder, IMAGE_BUILDER_IMAGE, image_builder_args};
use crate::context::{DiskTarget, ImageRef};
use crate::exec::{self, sh_quote};
use crate::progress::Scope;
use anyhow::{Context, Result};
use duct::cmd;
use std::ffi::OsString;

/// Absolute path of the `image-builder` binary inside [`IMAGE_BUILDER_IMAGE`] —
/// i.e. that image's default entrypoint. We override the entrypoint to a shell
/// (to slot a `podman tag` ahead of the build) and then `exec` this. Tied to the
/// digest-pinned [`IMAGE_BUILDER_IMAGE`]; a pin bump should re-verify it with
/// `podman image inspect --format '{{.Config.Entrypoint}}'`.
const IMAGE_BUILDER_ENTRYPOINT: &str = "/usr/bin/image-builder";

pub(crate) struct LocalBuilder {
	/// Extra `podman` flags from this builder's `[builder]` spec (`podman_opts`),
	/// spliced into the container `podman build` (`build` role) and the
	/// image-builder `podman run` (`image` role). Empty unless configured.
	podman_opts: Vec<String>,
}

impl LocalBuilder {
	/// In-process builder carrying the spec's extra `podman` flags.
	pub(crate) fn new(podman_opts: Vec<String>) -> Self {
		Self { podman_opts }
	}
}

impl Builder for LocalBuilder {
	fn build_image(&self, image: &ImageRef, job: &mut Scope) -> Result<()> {
		// `build`, then this builder's configured `podman_opts`, then the fixed flags
		// with the positional context last — so the extra options land before the
		// context arg, where podman expects options.
		let mut args: Vec<OsString> = vec!["build".into()];
		// Splice the configured `podman_opts` straight in, exactly as `Ssh::args`
		// does with `ssh_opts` — a plain `&[String]` → argv map needs no helper. (The
		// shell-string sites can't do this; they use `podman::opts_shell` to re-quote.)
		args.extend(self.podman_opts.iter().map(OsString::from));
		#[rustfmt::skip]
		args.extend::<[OsString; 8]>([
			"--os".into(), "linux".into(), "--arch".into(), image.arch.oci_arch().into(),
			"-t".into(), image.tag().into(),
			"-f".into(), image.build_ctx.join("Containerfile").into(),
		]);
		// Provenance `--label`s (git revision/describe); empty unless the build path
		// set them. Spliced like `podman_opts` — a plain `&[String]` → argv map.
		args.extend(image.labels.iter().map(OsString::from));
		args.push(image.build_ctx.clone().into());
		let build_cmd = cmd("podman", args);

		// Drive a count bar from podman's `STEP N/M:` lines. Total starts unknown
		// because we only learn it from the first matched line. If podman ever
		// changes the format, the bar silently stops advancing — the build itself
		// is unaffected. The fill interpolates on a fine scale (see `parse_step`)
		// while the printed counter stays the coarse `s/t` stage marker.
		let pb = job.count_labeled(None);
		exec::run_command_with(job, build_cmd, |line| {
			if let Some(p) = parse_step(line) {
				// `set_length` is idempotent, so set it unconditionally rather than
				// reading the bar's current length back to guard a no-op.
				pb.set_length(p.len);
				pb.set_position(p.pos);
				pb.set_message(p.label);
			}
		})?;
		pb.finish();
		Ok(())
	}

	/// Already local rootless storage — nothing to bring home.
	fn export_home(&self, _image: &ImageRef, _job: &mut Scope) -> Result<()> {
		Ok(())
	}

	fn build_disk(
		&self,
		target: &DiskTarget,
		source_ref: &str,
		config: Option<&str>,
		job: &mut Scope,
	) -> Result<()> {
		let tag = target.image.tag();
		let output = target.output_dir();

		// Cross-arch local disk builds carry the libpod lock-SHM crutch as a thin
		// COPY-only layer baked on top of the arch member (see [`build_lock_layer`]).
		// That layer is built **rootless on the host**, not inside the privileged
		// `:O`-mounted store below: a layer-committing `podman build` there dies
		// extracting the base layers ("readdirent: no such file or directory" — overlay
		// on overlay), while the host rootless store builds it fine, and a COPY-only
		// cross-arch build needs no emulation. The result lands in a dedicated
		// arch-scoped `locked` tag that the in-container `prep_step` only *tags* as
		// `source_ref` — the one store operation proven safe in the `:O` upper.
		let locked = (crate::context::Arch::host() != Some(target.image.arch))
			.then(|| format!("{tag}-bootcher-locked"));
		if let Some(locked) = &locked {
			build_lock_layer(&tag, locked, target.image.arch, job)?;
		}

		// Remove the built image(s) from rootless storage on every exit path.
		// image-builder reads them through the `:O` mount (no tag lookup needed at run
		// time), so this is safe to set up immediately — the images are gone only once
		// this scope ends.
		let _cleanup =
			LocalScratch { tags: std::iter::once(tag.clone()).chain(locked.clone()).collect() };

		// No image transfer: image-builder reads the rootless-built image straight from
		// our user container store, mounted `:O` (copy-on-write) into the privileged
		// container — skipping the `podman save | podman load` round-trip (minutes on a
		// multi-GB image). bootc reads the image through containers/storage's image API,
		// which yields the layers' *logical* (uid-0) ownership regardless of the rootless
		// on-disk uid shift, so the installed rootfs comes out root-owned.
		//
		// Because each arch's run gets its own ephemeral `:O` upper, the `source_ref`
		// tag below is created *inside that private upper* — invisible to the other
		// arches, so the multi-arch fan-out can't collide on the shared suffix-free
		// `:latest` ref.
		//
		// image-builder still needs root (loop mounts, parted, `mount(2)`), so the
		// `podman run` goes through the one authenticated `sudo sh` session opened on a
		// quiet terminal by `disk::run` — one password prompt for the whole fan-out,
		// no re-prompt under the concurrent bars. `--progress verbose` is line-oriented
		// and streams fine without a tty; output is bursty (disk assembly), and the
		// session spinner signals liveness between bursts.

		// The rootless graphroot to mount. Queried (not assumed) so a custom
		// `XDG_DATA_HOME`/`graphroot` is honoured. Runs rootless as the invoking user —
		// the store we built into — not under `sudo`.
		let store = cmd!("podman", "info", "--format", "{{.Store.GraphRoot}}")
			.read()
			.context("querying the rootless podman graphroot")?;
		let store = store.trim();

		// image-builder's osbuild step hardcodes `/var/lib/containers/storage` (it
		// ignores a `storage.conf` graphroot), so the store must mount at that path.
		// Mounted there, the store's libpod DB — which recorded the *original*
		// graphroot — would reject the mismatch ("database configuration mismatch").
		// We shadow its `db.sql` with an empty file: libpod only tracks containers/pods
		// (images+layers live in c/storage, left intact), so it harmlessly
		// reinitialises against the canonical graphroot. The empty file is a host
		// tempfile — libpod's writes land there, never the real store (which `:O` also
		// keeps read-only). Held alive until the run finishes.
		let empty_db = tempfile::NamedTempFile::new().context("creating empty libpod db shadow")?;

		// Optional blueprint (provision-time /etc injection): write it to a tempfile
		// mounted read-only into the image-builder container. Held until the run
		// finishes so the tempdir isn't reaped mid-build. Root reads the user-written
		// file fine.
		let cfg = config
			.map(|toml| -> Result<_> {
				let dir = tempfile::tempdir()?;
				let path = dir.path().join("blueprint.toml");
				std::fs::write(&path, toml)?;
				Ok((dir, path))
			})
			.transpose()?;
		let (cfg_mount, cfg_flag) = match &cfg {
			Some((_dir, path)) => (
				format!("-v {}:/blueprint.toml:ro", sh_quote(&path.to_string_lossy())),
				"--blueprint /blueprint.toml",
			),
			None => (String::new(), ""),
		};

		// The in-container command, run by the shell we override the entrypoint to:
		// make the arch member available under `source_ref` in this container's private
		// COW upper (so image-builder builds from — and records as the bootc origin —
		// the suffix-free localhost ref, or the registry ref in registry mode), then
		// `exec` image-builder. This is a pure metadata retag — the one store operation
		// proven safe in the `:O` upper. The refs, flags and container paths are all safe
		// tokens, so the inner stays free of the single quotes the outer `-v` paths use
		// (single quotes don't nest).
		//
		// Cross-arch: tag the host-built `locked` layer (the libpod lock-SHM crutch, see
		// [`build_lock_layer`]). Same-arch: retag the plain arch member. `source_ref`
		// always differs from the arch member tag (`:latest` vs `:latest-<arch>`), but
		// guard the same-arch case anyway.
		let prep_step = match &locked {
			Some(locked) => format!("podman tag {locked} {source_ref} && "),
			None if source_ref == tag => String::new(),
			None => format!("podman tag {tag} {source_ref} && "),
		};
		let args = image_builder_args(target, source_ref, cfg_flag);
		let inner = format!("{prep_step}exec {IMAGE_BUILDER_ENTRYPOINT} {args}");

		// `sh`-quoted for the outer root shell: the output dir (this arch's, relative
		// to the project root the session inherits as cwd), the store, the empty-db
		// shadow, and the whole inner command. The longer `db.sql` mount destination
		// makes podman layer the shadow on top of the `:O` store mount.
		let out = sh_quote(&format!("./{}", output.display()));
		// This builder's configured `podman_opts` (e.g. `--network=host`), re-quoted
		// and spliced in right after `run`.
		let run_opts = crate::podman::opts_shell(&self.podman_opts);
		let build_cmd = format!(
			"podman run{run_opts} --rm --privileged --pull=missing \
			 --security-opt label=type:unconfined_t \
			 --entrypoint /bin/sh \
			 -v {out}:/output \
			 -v {store}:/var/lib/containers/storage:O \
			 -v {empty_db}:/var/lib/containers/storage/db.sql \
			 {cfg_mount} {IMAGE_BUILDER_IMAGE} -c {inner}",
			store = sh_quote(store),
			empty_db = sh_quote(&empty_db.path().to_string_lossy()),
			inner = sh_quote(&inner),
		);
		let built = crate::sudo::run_as_root(job, "image builder", &build_cmd);

		// image-builder writes the artifacts (the flat `disk.<ext>`) as root, since the
		// container — and so its `/output` writes — runs privileged. Hand
		// them back to whoever owns the bind-mounted output dir (the invoking user,
		// not necessarily root under `sudo`), so the build's products aren't left
		// root-owned and undeletable. `--reference` matches the dir's own uid/gid;
		// best-effort, on success or failure, so partial root-owned output is still
		// reclaimable. The `.` includes the dir itself (harmless — it already matches).
		let _ = crate::sudo::run_as_root(
			job,
			"chown output",
			&format!("chown -R --reference={out} {out}"),
		);

		built
	}
}

/// Removes the arch-tagged image(s) from local rootless storage when it drops —
/// on success or on any early return / failure out of [`LocalBuilder::build_disk`].
/// Holds the arch member plus, for a cross-arch build, the host-built `locked`
/// layer ([`build_lock_layer`]). Best-effort and silent, matching `RemoteScratch`'s
/// contract in [`super::remote`].
struct LocalScratch {
	tags: Vec<String>,
}

impl Drop for LocalScratch {
	fn drop(&mut self) {
		crate::podman::rmi(&self.tags);
	}
}

/// Bake the cross-arch file-lock crutch into a thin `COPY`-only layer on top of
/// `base`, tagged `locked`, built **rootless on the host** for `arch`.
///
/// A cross-arch in-process disk build runs the inner `bootc install` under qemu-user
/// emulation, and the nested podman that sets up imgstorage opens a
/// `/dev/shm/libpod_lock` whose recorded `num_locks` doesn't match what it computes —
/// the sole ERANGE path in podman's `shm_lock.c` (an *existing* mismatched segment,
/// since osbuild hands each stage a fresh, isolated `/dev/shm`). We sidestep the SHM
/// lock manager by telling that podman to use `lock_type = "file"` (flock, no shm
/// segment) via a `containers.conf.d` drop-in. The buildroot podman reads its config
/// from the target image (osbuild assembles the bootc buildroot from it), so the
/// drop-in must live *in the image*: we derive a thin `COPY`-only layer (no `RUN`, so
/// no emulation) adding it and build the disk from that.
///
/// The build runs rootless on the host, not inside the privileged `:O`-mounted store
/// the disk step uses: a layer-committing `podman build` there fails extracting the
/// base layers (overlay on overlay), whereas the host rootless store builds it fine.
/// Its layers land in that same store, so the disk step sees `locked` through the `:O`
/// mount and only has to *tag* it as `source_ref`.
///
/// That drop-in is a build-time crutch, not the user's config — leaving it on the
/// installed device would be a permanent override the device never asked for. So the
/// same layer also ships a self-removing oneshot: a systemd unit, enabled by a baked
/// `multi-user.target.wants` symlink, whose only job is to `rm` the drop-in, the
/// symlink and itself on the device's first boot — before any device-side podman runs
/// — leaving a clean `/etc`. (A `bootc upgrade` to the registry image would also drop
/// it, but the oneshot bounds the residue to that very first boot.) The layer is
/// COPY-only, so the symlink is baked from a `wants/` dir in the context — a directory
/// COPY preserves it as a symlink (where `RUN systemctl enable` would re-introduce
/// emulation). The unit's `ConditionPathExists` makes it a no-op if `/etc` is already
/// clean.
fn build_lock_layer(
	base: &str,
	locked: &str,
	arch: crate::context::Arch,
	job: &mut Scope,
) -> Result<()> {
	use std::os::unix::fs::symlink;

	let cleanup_unit = "/etc/systemd/system/bootcher-filelock-cleanup.service";
	let drop_in = "/etc/containers/containers.conf.d/99-bootcher-filelock.conf";
	let wants_link =
		"/etc/systemd/system/multi-user.target.wants/bootcher-filelock-cleanup.service";

	let dir = tempfile::tempdir().context("creating cross-arch lock-layer build context")?;
	let ctx = dir.path();
	std::fs::write(ctx.join("lock.conf"), "[engine]\nlock_type = \"file\"\n")?;
	std::fs::write(
		ctx.join("cleanup.service"),
		format!(
			"[Unit]\n\
			 Description=Remove bootcher cross-arch build lock override\n\
			 ConditionPathExists={drop_in}\n\
			 [Service]\n\
			 Type=oneshot\n\
			 ExecStart=/usr/bin/rm -f {drop_in} {wants_link} {cleanup_unit}\n\
			 [Install]\n\
			 WantedBy=multi-user.target\n"
		),
	)?;
	std::fs::create_dir(ctx.join("wants"))?;
	symlink(
		"../bootcher-filelock-cleanup.service",
		ctx.join("wants/bootcher-filelock-cleanup.service"),
	)?;
	std::fs::write(
		ctx.join("Containerfile"),
		format!(
			"FROM {base}\n\
			 COPY lock.conf {drop_in}\n\
			 COPY cleanup.service {cleanup_unit}\n\
			 COPY wants /etc/systemd/system/multi-user.target.wants\n"
		),
	)?;

	// COPY-only and internal (no RUN step, no network), so it carries no
	// user `podman_opts` — those target the user-facing build/image commands.
	#[rustfmt::skip]
	let build = cmd!(
		"podman", "build",
		"--os", "linux", "--arch", arch.oci_arch(),
		"-t", locked,
		"-f", ctx.join("Containerfile"),
		ctx,
	);
	exec::run_command(job, build, "cross-arch file-lock layer")
}

/// Virtual width each build stage occupies on the progress bar. A `t`-stage
/// build has a length of `t · STAGE_SLICE`; within a stage the inner `STEP n/m`
/// fills its slice proportionally. The value is arbitrary — it only sets the
/// resolution of the intra-stage fraction (one part in 1000 here).
const STAGE_SLICE: u64 = 1000;

/// Bar progress parsed from a `podman build` line: `pos`/`len` drive the smooth
/// fill, `label` is the coarse counter text shown to the user (decoupled from
/// the fill — see [`Scope::count_labeled`]).
#[derive(Debug, PartialEq, Eq)]
struct StepProgress {
	pos: u64,
	len: u64,
	label: String,
}

/// Extract progress from a `podman build` line.
///
/// Two formats, depending on the Containerfile:
/// - Single-stage: `STEP <n>/<m>: …` — fill and label both track the per-step
///   count `<n>/<m>`.
/// - Multi-stage:  `[<s>/<t>] STEP <n>/<m>: …` — podman prefixes each line with
///   a stage marker, and the inner `STEP <n>/<m>` resets at every stage. We give
///   every stage an equal slice (`STAGE_SLICE`) of a `t`-slice-wide bar, and fill
///   the current slice by the intra-stage fraction:
///
///   ```text
///   pos = (s - 1) · STAGE_SLICE + n · STAGE_SLICE / m
///   len =       t · STAGE_SLICE
///   ```
///
///   `(s - 1)` counts *completed* stages, so the bar spans 0→100% exactly:
///   `[1/4] STEP 1/100` ≈ 0, `[4/4] STEP 100/100` = full. It advances
///   monotonically and never jumps at stage boundaries. The label stays the
///   coarse `s/t` stage marker, since that fine `pos`/`len` scale is for the fill
///   only. The one inaccuracy is the implicit assumption that all stages take
///   equal wall-time; a stage with few but slow steps still gets the same slice
///   width as a fast many-step one.
fn parse_step(line: &str) -> Option<StepProgress> {
	if let Some(after_bracket) = line.strip_prefix('[') {
		// Multi-stage: map the `[s/t]` stage marker plus the inner `STEP n/m`
		// onto the sliced bar. Only on actual STEP lines — podman emits other
		// bracketed lines we don't want to drive the bar from.
		let (stage, rest) = after_bracket.split_once("] ")?;
		let step = rest.strip_prefix("STEP ")?.split_once(':')?.0;
		let (s, t) = parse_ratio(stage)?;
		let (n, m) = parse_ratio(step)?;
		if m == 0 {
			return None;
		}
		let pos = s.saturating_sub(1) * STAGE_SLICE + n * STAGE_SLICE / m;
		return Some(StepProgress { pos, len: t * STAGE_SLICE, label: format!("{s}/{t}") });
	}
	let nums = line.strip_prefix("STEP ")?.split_once(':')?.0;
	let (n, m) = parse_ratio(nums)?;
	Some(StepProgress { pos: n, len: m, label: format!("{n}/{m}") })
}

/// Parse a `"<n>/<m>"` ratio into `(n, m)`.
fn parse_ratio(nums: &str) -> Option<(u64, u64)> {
	let (n, m) = nums.split_once('/')?;
	Some((n.parse().ok()?, m.parse().ok()?))
}

#[cfg(test)]
mod tests {
	use super::{StepProgress, parse_step};

	fn progress(pos: u64, len: u64, label: &str) -> StepProgress {
		StepProgress { pos, len, label: label.to_owned() }
	}

	#[test]
	fn parses_typical_step_line() {
		// Single-stage: fill and label both track the raw step count.
		assert_eq!(parse_step("STEP 1/5: FROM rockylinux:9"), Some(progress(1, 5, "1/5")));
		assert_eq!(parse_step("STEP 12/12: COPY foo /bar"), Some(progress(12, 12, "12/12")));
	}

	#[test]
	fn maps_multi_stage_step_onto_sliced_bar() {
		// Each stage owns an equal 1000-wide slice of a t·1000 bar; the inner
		// STEP fills its slice. `(s-1)` counts completed stages so the bar spans
		// 0→100%: starts near 0 and the final line lands exactly on full. The
		// label stays the coarse `s/t` stage marker.
		assert_eq!(parse_step("[1/4] STEP 1/100: FROM x"), Some(progress(10, 4000, "1/4")));
		assert_eq!(parse_step("[2/4] STEP 23/100: RUN y"), Some(progress(1230, 4000, "2/4")));
		assert_eq!(parse_step("[4/4] STEP 100/100: COPY z"), Some(progress(4000, 4000, "4/4")));
	}

	#[test]
	fn rejects_non_step_lines() {
		assert_eq!(parse_step("--> a1b2c3d4"), None);
		assert_eq!(parse_step("STEP without colon"), None);
		assert_eq!(parse_step("STEP a/b: not numeric"), None);
		assert_eq!(parse_step("[1/2] not a step"), None);
		assert_eq!(parse_step("[unterminated STEP 1/5: x"), None);
		assert_eq!(parse_step("[1/2] STEP 0/0: divide by zero"), None);
		assert_eq!(parse_step(""), None);
	}
}
