//! ssh-driven builder: run `podman build` and the `image-builder` disk step on a
//! native-arch host (a real box, or the local VM that [`super::vm`] boots and
//! [`super::select`] points us at — this builder is unaware which). The remote
//! needs rootful `podman`, outbound network (to pull `image-builder`), `tar`, and
//! passwordless key-based ssh with passwordless `sudo` — the norm for a dedicated
//! build host, and exactly what the build VM provides.
//!
//! It honours the three [`Builder`] contracts across ssh:
//!
//! - **`build_image`** — ship the build context up into a throwaway remote dir
//!   (tar over ssh) and `sudo podman build` it natively into the remote's *root*
//!   containers-storage (the remote is target-arch, so no emulation). The built
//!   tag is recorded `resident` so a following `build_disk` skips the transfer.
//! - **`build_disk`** — ensure the container is in the remote's root storage (tag
//!   the resident one, or transfer it up via the loopback-registry-over-`ssh -R`
//!   trick of a LAN deploy, [`crate::registry`]), run `image-builder` into a
//!   throwaway remote dir, then `tar`-stream the artifacts into
//!   `target.output_dir()`.
//! - **`export_home`** — stream the built container out of the remote's root
//!   storage (`sudo podman save` over ssh) into local rootless `podman load`,
//!   so `deploy`/`upgrade` find it under [`ImageRef::tag`] as usual.
//!
//! A [`RemoteScratch`] guard removes the remote temp dirs and image refs on the
//! way out — success or failure alike.

use super::{Builder, IMAGE_BUILDER_IMAGE, image_builder_args};
use crate::context::{DiskTarget, ImageRef};
use crate::progress::Scope;
use crate::ssh::Ssh;
use crate::{exec, fetch, registry, signals};
use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

pub(crate) struct RemoteBuilder {
	/// The build-host connection: host, any extra args, and the shared
	/// non-interactive/host-key base. All ssh interaction delegates to it; a build
	/// host runs a POSIX login shell, so this uses [`Ssh::argv`]/[`Ssh::read`]
	/// (command as the ssh arg) rather than the deploy path's `sh`-on-stdin.
	ssh: Ssh,
	/// Tags `build_image` has built into the remote's root storage, so a
	/// following `build_disk` tags `source_ref` onto the resident image rather than
	/// re-transferring it. Interior-mutable: the trait methods take `&self`.
	resident: RefCell<HashSet<String>>,
}

impl RemoteBuilder {
	/// Builder against a user-provided ssh host (default ssh config).
	#[must_use]
	pub(crate) fn new(ssh: Ssh) -> Self {
		Self { ssh, resident: RefCell::new(HashSet::new()) }
	}
}

impl Builder for RemoteBuilder {
	fn build_image(&self, image: &ImageRef, job: &mut Scope) -> Result<()> {
		// Each phase below renders as a leaf bar/spinner on `job` (the upload's bytes
		// bar, the build spinner), never a header step — so when this runs concurrently
		// under a per-arch header (the multi-arch fan-out) the arch label stays put.
		let tag = image.tag();

		// 1. Ship the build context into a throwaway remote dir. The guard removes
		//    it on every exit path; nothing else needs it after the build.
		let ctx_dir = self.ssh.read("mktemp -d /var/tmp/bootcher-ctx.XXXXXX")?;
		if ctx_dir.is_empty() {
			bail!("remote mktemp returned an empty path");
		}
		let _ctx = RemoteScratch { builder: self, dir: ctx_dir.clone(), image_refs: vec![] };
		self.upload_context(&image.build_ctx, &ctx_dir, job)?;

		// 2. Build natively on the remote (it's target-arch, so `--platform` here
		//    is a no-op rather than emulation — the whole point of relocating).
		//    `FROM localhost/base:…` resolves because the caller built base first.
		let build = format!(
			"sudo podman build --platform {plat} -t {tag} -f {ctx_dir}/Containerfile {ctx_dir}",
			plat = image.arch.podman_platform(),
		);
		let argv = self.ssh.argv(&[], &build);
		exec::run_argv_labeled(job, &argv, "podman build")?;

		self.resident.borrow_mut().insert(tag);
		Ok(())
		// `_ctx` drops here, rm -rf'ing the uploaded context.
	}

	fn build_disk(
		&self,
		target: &DiskTarget,
		source_ref: &str,
		config: Option<&str>,
		job: &mut Scope,
	) -> Result<()> {
		let image = &target.image;
		// Each phase renders as a leaf bar/spinner on `job` (the transfer/fetch bytes
		// bars, the labelled image-builder spinner), never a header step, so a per-arch
		// header above it (the multi-arch fan-out) keeps showing the arch.

		// 1. Make `source_ref` resolvable in the remote's root storage — tag the
		//    image `build_image` already built here, or ship it up from local
		//    storage. `pull_ref` is the ephemeral transfer ref to untag (if any).
		let pull_ref = self.ensure_on_remote(image, source_ref, job)?;

		// A throwaway remote dir for image-builder's `/output`; the guard removes it
		// (and the image refs — the resident/built tag, `source_ref`, and any pull ref)
		// on every exit path below.
		let scratch_dir = self.ssh.read("mktemp -d /var/tmp/bootcher-disk.XXXXXX")?;
		if scratch_dir.is_empty() {
			bail!("remote mktemp returned an empty path");
		}
		let mut image_refs = vec![source_ref.to_owned(), image.tag()];
		image_refs.extend(pull_ref);
		let _scratch = RemoteScratch { builder: self, dir: scratch_dir.clone(), image_refs };

		// Upload the optional blueprint into its *own* throwaway remote dir — kept
		// out of {scratch_dir} (mounted as /output and tar'd back) so the
		// secret-bearing blueprint never returns with the artifacts. The TOML is
		// piped to the remote `cat` over ssh's stdin, so it never hits the shell
		// command line (no quoting to get wrong); the guard removes the dir on exit.
		let (_cfg_scratch, cfg_mount, cfg_flag) = match config {
			Some(toml) => {
				let dir = self.ssh.read("mktemp -d /var/tmp/bootcher-cfg.XXXXXX")?;
				if dir.is_empty() {
					bail!("remote mktemp returned an empty path");
				}
				let guard = RemoteScratch { builder: self, dir: dir.clone(), image_refs: vec![] };
				let argv = self.ssh.argv(&[], &format!("cat > {dir}/blueprint.toml"));
				let label = argv.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
				let (program, rest) = argv.split_first().expect("non-empty argv");
				exec::run_command(
					job,
					duct::cmd(program, rest).stdin_bytes(toml.as_bytes()),
					label,
				)?;
				(
					Some(guard),
					format!("-v {dir}/blueprint.toml:/blueprint.toml:ro"),
					"--blueprint /blueprint.toml",
				)
			}
			None => (None, String::new(), ""),
		};

		// 2. Run image-builder on the remote, writing into the scratch dir. No `-it`:
		//    the ssh session has no tty and the progress is line-oriented anyway, so the
		//    labelled spinner + line forwarding render it the same as locally. The
		//    image's default entrypoint is the binary, so the `build …` args follow the
		//    image name directly (no `podman tag` to inject — `ensure_on_remote` already
		//    tagged `source_ref` in the remote's root storage).
		let ib_args = image_builder_args(target, source_ref, cfg_flag);
		let build = format!(
			"sudo podman run --rm --privileged --pull=missing \
			 --security-opt label=type:unconfined_t \
			 -v {scratch_dir}:/output \
			 -v /var/lib/containers/storage:/var/lib/containers/storage \
			 {cfg_mount} {IMAGE_BUILDER_IMAGE} {ib_args}",
		);
		let argv = self.ssh.argv(&[], &build);
		exec::run_argv_labeled(job, &argv, "image builder")?;

		// 3. Stream the artifacts back into the local output dir. `tar` over ssh
		//    keeps this rsync-free; `sudo` on the remote reads image-builder's
		//    root-owned output, and the local untar restores it under the invoking user.
		self.fetch(&scratch_dir, target, job)?;

		Ok(())
	}

	fn export_home(&self, image: &ImageRef, job: &mut Scope) -> Result<()> {
		// Renders only its own leaf bars (the `load_home` byte pump), so it can run
		// on the worker's arch scope right after `build_image`'s nested children.
		self.load_home(image, job)?;
		// The container is home now; drop the remote copy (best-effort) and forget
		// its residency so a later `build_disk` on this builder re-transfers cleanly.
		let tag = image.tag();
		let cleanup = self.ssh.argv(&[], &format!("sudo podman rmi --ignore {tag}"));
		let (program, rest) = cleanup.split_first().expect("non-empty argv");
		crate::exec::best_effort(&duct::cmd(program, rest));
		self.resident.borrow_mut().remove(&tag);
		Ok(())
	}
}

impl RemoteBuilder {
	/// Make `source_ref` resolvable in the remote's root storage for image-builder. If
	/// `build_image` already built the image here (resident), just tag
	/// `source_ref` onto it; otherwise ship it up from local storage. Returns the
	/// ephemeral `127.0.0.1:<port>/…` pull ref to untag afterwards (`None` when it
	/// was already resident, so nothing transient was created).
	fn ensure_on_remote(
		&self,
		image: &ImageRef,
		source_ref: &str,
		job: &Scope,
	) -> Result<Option<String>> {
		let tag = image.tag();
		if self.resident.borrow().contains(&tag) {
			// Built here already: image-builder builds from `source_ref`, so in registry mode
			// alias it onto the resident localhost tag (a no-op when they're equal).
			if source_ref != tag {
				let argv = self.ssh.argv(&[], &format!("sudo podman tag {tag} {source_ref}"));
				exec::run_argv_labeled(job, &argv, "stage image")?;
			}
			Ok(None)
		} else {
			Ok(Some(self.transfer(image, source_ref, job)?))
		}
	}

	/// Tar `ctx` up to the remote `dest_dir` over ssh: local `tar -cf -` piped
	/// through this process (for a bytes bar via [`fetch::pump`]) into a remote
	/// `tar -xmf -`. Mirrors [`Self::fetch`] in the opposite direction. `-m` on
	/// extraction skips mtime restores we don't care about. The local tar's
	/// stderr is nulled (it can't share the binary stream); the remote tar's is
	/// captured to a temp file and folded into the error on failure.
	fn upload_context(&self, ctx: &Path, dest_dir: &str, job: &Scope) -> Result<()> {
		let ctx = ctx.to_str().context("build context path is not utf-8")?;

		// Local side: stream the context tarball to our stdout. Registered for
		// signal-driven killing so an interrupt doesn't leave us blocked in `read`.
		let reader =
			Arc::new(duct::cmd("tar", ["-C", ctx, "-cf", "-", "."]).stderr_null().reader()?);
		let _kill = signals::kill_on_signal({
			let reader = Arc::clone(&reader);
			move |sig| {
				for pid in reader.pids() {
					signals::signal_pid(pid, sig);
				}
			}
		});

		// Remote side: extract from our stdin into the dest dir. stderr to a temp
		// file (not a pipe) to report diagnostics on failure without a pump deadlock.
		let mut errfile =
			tempfile::tempfile().context("creating temp file for remote tar stderr")?;
		let recv = self.ssh.argv(&[], &format!("tar -C {dest_dir} -xmf -"));
		let (program, rest) = recv.split_first().expect("non-empty argv");
		let mut up = Command::new(program)
			.args(rest)
			.stdin(Stdio::piped())
			.stderr(Stdio::from(errfile.try_clone().context("cloning remote tar stderr handle")?))
			.spawn()
			.context("spawning ssh to upload build context")?;
		let mut stdin = up.stdin.take().expect("stdin piped");

		fetch::pump(job, None, "upload build context", reader.as_ref(), |chunk| {
			stdin.write_all(chunk).context("writing build context to ssh")?;
			Ok(())
		})?;
		drop(stdin); // EOF so the remote tar finishes.

		let status = up.wait().context("waiting for ssh to upload build context")?;
		if !status.success() {
			use std::io::{Read, Seek};
			errfile.rewind().ok();
			let mut stderr = String::new();
			errfile.read_to_string(&mut stderr).ok();
			bail!("uploading build context failed: ssh exited with {status}\n{}", stderr.trim());
		}
		signals::check()
	}

	/// Stream the built container out of the remote's root storage into local
	/// rootless storage: `ssh remote sudo podman save <tag>` piped through this
	/// process (bytes bar) into a local `podman load`.
	fn load_home(&self, image: &ImageRef, job: &Scope) -> Result<()> {
		let tag = image.tag();

		let send = self.ssh.argv(&[], &format!("sudo podman save {tag}"));
		let (program, rest) = send.split_first().expect("non-empty argv");
		let reader = Arc::new(duct::cmd(program, rest).stderr_null().reader()?);
		let _kill = signals::kill_on_signal({
			let reader = Arc::clone(&reader);
			move |sig| {
				for pid in reader.pids() {
					signals::signal_pid(pid, sig);
				}
			}
		});

		let mut errfile =
			tempfile::tempfile().context("creating temp file for podman load stderr")?;
		let mut load = Command::new("podman")
			.arg("load")
			.stdin(Stdio::piped())
			.stderr(Stdio::from(errfile.try_clone().context("cloning podman load stderr handle")?))
			.spawn()
			.context("spawning local `podman load`")?;
		let mut stdin = load.stdin.take().expect("stdin piped");

		fetch::pump(job, None, "export image home", reader.as_ref(), |chunk| {
			stdin.write_all(chunk).context("writing image to `podman load`")?;
			Ok(())
		})?;
		drop(stdin);

		let status = load.wait().context("waiting for local `podman load`")?;
		if !status.success() {
			use std::io::{Read, Seek};
			errfile.rewind().ok();
			let mut stderr = String::new();
			errfile.read_to_string(&mut stderr).ok();
			bail!("`podman load` failed with {status}\n{}", stderr.trim());
		}
		signals::check()
	}
	/// Ship `image` into the remote's *root* containers-storage, tagged
	/// `source_ref`. Stands up a loopback-only OCI registry on the builder
	/// ([`registry::serve`]) and has the remote `podman pull` from it over an
	/// `ssh -R` remote forward, so only missing layers cross the (encrypted)
	/// wire. Returns the ephemeral `127.0.0.1:<port>/…` pull ref so the caller's
	/// cleanup guard can untag it afterwards.
	fn transfer(&self, image: &ImageRef, source_ref: &str, job: &Scope) -> Result<String> {
		// One arch's member to one build host: serve it under its own `latest-<arch>`
		// reference (not the suffix-free list tag) to keep the pull ref unambiguous.
		let reg = registry::serve(&image.tag(), &format!("latest-{}", image.arch), job)?;
		let port = reg.port();
		let pull_ref = format!("127.0.0.1:{port}/{}:latest-{}", image.name, image.arch);
		let forward = format!("127.0.0.1:{port}:127.0.0.1:{port}");
		let remote_cmd = format!(
			"sudo podman pull --tls-verify=false {pull_ref} && \
			 sudo podman tag {pull_ref} {source_ref}"
		);
		let argv = self.ssh.argv(&["-R", &forward], &remote_cmd);
		exec::run_argv_labeled(job, &argv, "transfer image")?;
		drop(reg);
		Ok(pull_ref)
	}

	/// `ssh <dest> sudo tar -C <scratch> -cf - . | tar -C <output> -xmf -`:
	/// pull the whole image-builder output tree (the flat `disk.<ext>`) back into the
	/// local output dir, which `jobs::disk` has already created.
	///
	/// The tar stream is routed *through* this process (rather than a direct
	/// `ssh | tar` OS pipe) so [`fetch::pump`] can show a bytes bar: read the
	/// remote tar's stdout, write each chunk into the local tar's stdin, ticking
	/// the bar in between. The total comes from a `du` of the remote tree
	/// ([`remote_dir_size`](Self::remote_dir_size)); the bar lands approximately
	/// (see there), and if the size query fails it simply runs without a total.
	///
	/// `--sparse` so the holes in image-builder's (typically sparse) `disk.raw` aren't read
	/// and shipped as zeros — only the allocated blocks cross the wire, which is
	/// the bulk of the speedup. GNU tar records the sparse map in the member
	/// header, so the local `tar -xmf -` recreates the holes without needing the
	/// flag itself.
	///
	/// The remote tar's stderr can't be merged into the binary stdout stream
	/// without corrupting the tarball, so it's nulled; the local tar's stderr is
	/// captured to a temp file and folded into the error on failure. A non-zero
	/// exit on either side surfaces as an error (the remote via [`fetch::pump`]'s
	/// read, the local via the wait below).
	fn fetch(&self, scratch_dir: &str, target: &DiskTarget, job: &Scope) -> Result<()> {
		let output = target.output_dir();
		let output = output.to_str().context("output dir path is not utf-8")?;
		let total = self.remote_dir_size(scratch_dir).ok();

		// Remote side: stream the tarball to our stdout (stderr nulled so it
		// stays off the binary stream). Registered for signal-driven killing so a
		// quiet transfer past an interrupt doesn't leave us blocked in `read`.
		let send = self.ssh.argv(&[], &format!("sudo tar --sparse -C {scratch_dir} -cf - ."));
		let (program, rest) = send.split_first().expect("non-empty argv");
		let reader = Arc::new(duct::cmd(program, rest).stderr_null().reader()?);
		let _kill = signals::kill_on_signal({
			let reader = Arc::clone(&reader);
			move |sig| {
				for pid in reader.pids() {
					signals::signal_pid(pid, sig);
				}
			}
		});

		// Local side: extract from our stdin into the output dir. stderr goes to a
		// temp file (not a pipe) so we can report tar's own diagnostics on failure
		// without risking a deadlock: a live stderr pipe filling its buffer mid-pump
		// would stall tar's stdin reads while we're still blocked writing to it.
		let mut errfile =
			tempfile::tempfile().context("creating temp file for local tar stderr")?;
		// `-m` (don't restore mtimes): the artifacts are a disk image, so
		// their timestamps are meaningless to us, and skipping the restore avoids
		// tar's post-extraction `utime()` pass on the (root-archived) directories,
		// which fails EPERM as the unprivileged local user and aborts with exit 2
		// *after* every byte is already on disk.
		let mut untar = Command::new("tar")
			.args(["-C", output, "-xmf", "-"])
			.stdin(Stdio::piped())
			.stderr(Stdio::from(errfile.try_clone().context("cloning tar stderr handle")?))
			.spawn()
			.context("spawning local tar to extract artifacts")?;
		let mut stdin = untar.stdin.take().expect("stdin piped");

		fetch::pump(job, total, "fetch artifacts", reader.as_ref(), |chunk| {
			stdin.write_all(chunk).context("writing artifacts to tar")?;
			Ok(())
		})?;
		drop(stdin); // EOF so the local tar finishes and flushes.

		let status = untar.wait().context("waiting for local tar to extract artifacts")?;
		if !status.success() {
			use std::io::{Read, Seek};
			errfile.rewind().ok();
			let mut stderr = String::new();
			errfile.read_to_string(&mut stderr).ok();
			let stderr = stderr.trim();
			bail!("extracting artifacts failed: tar exited with {status}\n{stderr}");
		}
		// A signal-driven kill ends the remote stream at EOF, not a read error;
		// surface it as the interruption it was.
		signals::check()
	}

	/// Allocated size (bytes) of the remote image-builder output tree, for the
	/// transfer's progress total. `sudo` because the artifacts are root-owned; `du -sB1`
	/// reports the *allocated* size (disk blocks actually used), which is what a
	/// `--sparse` tar ships — versus `--apparent-size`, which would count the
	/// skipped holes too and overshoot wildly for a sparse `disk.raw`. It's still
	/// an estimate (tar adds headers + a sparse map, and detects holes by zero
	/// content rather than allocation), so the bar may land a little before or
	/// after full; that's cosmetic.
	fn remote_dir_size(&self, dir: &str) -> Result<u64> {
		let out = self.ssh.read(&format!("sudo du -sB1 {dir}"))?;
		out.split_whitespace()
			.next()
			.and_then(|n| n.parse().ok())
			.with_context(|| format!("parsing `du -sB1` output: {out:?}"))
	}
}

/// Removes a remote scratch dir and any image refs when it drops — on success
/// or on any early return / failure. Best-effort and silent. `image_refs` may be
/// empty (e.g. the build-context upload guard, which only owns a dir), in which
/// case no `rmi` is issued.
struct RemoteScratch<'a> {
	builder: &'a RemoteBuilder,
	dir: String,
	image_refs: Vec<String>,
}

impl Drop for RemoteScratch<'_> {
	fn drop(&mut self) {
		// Append the `rmi` clause only when there are refs to remove (the
		// context-upload guard owns just a dir).
		let rmi = if self.image_refs.is_empty() {
			String::new()
		} else {
			format!(" ; sudo podman rmi --ignore {}", self.image_refs.join(" "))
		};
		let cmd = format!("sudo rm -rf {}{rmi}", self.dir);
		let argv = self.builder.ssh.argv(&[], &cmd);
		let (program, rest) = argv.split_first().expect("non-empty argv");
		crate::exec::best_effort(&duct::cmd(program, rest));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::OsString;

	/// Render an argv to plain strings for assertions.
	fn strs(argv: &[OsString]) -> Vec<String> {
		argv.iter().map(|a| a.to_string_lossy().into_owned()).collect()
	}

	#[test]
	fn ssh_opts_inject_extra_args_before_host() {
		// VM builder seam: `-p <port> -i <key>` from `opts` must appear, and before
		// the host.
		let b = RemoteBuilder::new(Ssh::new("fedora@127.0.0.1", ["-p", "2222", "-i", "/k"]));
		let argv = strs(&b.ssh.argv(&[], "true"));
		assert!(argv.windows(2).any(|w| w[0] == "-p" && w[1] == "2222"));
		assert!(argv.windows(2).any(|w| w[0] == "-i" && w[1] == "/k"));
		let dest = argv.iter().position(|a| a == "fedora@127.0.0.1").unwrap();
		let port = argv.iter().position(|a| a == "2222").unwrap();
		assert!(port < dest, "opts must precede the host");
	}
}
