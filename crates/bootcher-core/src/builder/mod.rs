//! Where `podman build` and the `image-builder` disk-image step run.
//!
//! Both the container build (`jobs::build`) and the disk-image build
//! (`jobs::disk`) can run *natively* in-process, or be relocated onto a
//! native-arch builder. The motivating axis is cross-arch: the disk-image step
//! (and the cross-arch `podman build` that feeds it) runs under qemu-user (binfmt)
//! emulation — slower than native, but sound. The relocation knobs below let a
//! project move that work off the host process when the emulation overhead isn't
//! worth it: a real native-arch remote runs it without emulation (the only way to
//! actually go faster), while a VM is a clean throwaway sandbox but boots the
//! foreign arch under full-system TCG, slower still.
//!
//! Selection is *explicit* (see `choose`) and comes from the project
//! manifest's `[builder]` table ([`crate::context::BuilderConfig`]): the default
//! is the in-process [`LocalBuilder`] (`local`); `vm` boots a throwaway local VM,
//! and `user@host` targets a real native-arch remote. The table has two roles —
//! `build` (container build) and `image` (the disk-image step, the
//! emulation-prone one), so a project can build locally yet run the image step
//! on a VM. A VM is just "a remote you launched
//! yourself" — [`select`] boots a `VmInstance` and hands its ssh parameters to a
//! [`RemoteBuilder`], so the only [`Builder`] impls are [`LocalBuilder`] and
//! [`RemoteBuilder`] (plus a thin tuple pairing the two for the VM case).
//!
//! Every builder honours the same contracts: after [`Builder::build_image`] the
//! container is built (locally, or on the builder), [`Builder::export_home`]
//! guarantees it is in local rootless storage under [`ImageRef::tag`], and after
//! [`Builder::build_disk`] `target.output_dir()` holds the artifacts (a flat
//! `disk.<ext>`, e.g. `disk.qcow2`) with `source_ref` recorded as the bootc
//! origin — so the `disk.post` hook and `deploy` downstream are oblivious to
//! where the work ran.

mod local;
mod remote;
mod vm;

pub(crate) use local::LocalBuilder;
pub(crate) use remote::RemoteBuilder;

use crate::context::{Arch, BuilderSpec, DiskTarget, ImageRef};
use crate::progress::Scope;
use crate::ssh::Ssh;
use anyhow::Result;
use std::fmt;

/// `image-builder` image, **pinned by digest**. `:latest` rolls, and a bad build
/// can break emulated cross-arch — so every builder runs this exact known-good
/// digest. Bump deliberately after verifying a newer build.
///
/// This is osbuild's unified `image-builder` (the successor to the now-deprecated
/// `bootc-image-builder`; see <https://osbuild.org/docs/bootc/deprecation-notice/>).
/// Its default entrypoint is `/usr/bin/image-builder` (see the builder modules).
pub(crate) const IMAGE_BUILDER_IMAGE: &str = "ghcr.io/osbuild/image-builder-cli@sha256:787460b1a4ce89a113329cf582161e85f04db4c24c02c9b3a2d414676e1b7857";

/// The `image-builder` sub-command and flags (everything after the binary path)
/// that turn `image`'s container — under `source_ref` — into a disk image, shared
/// by the local and remote builders so the one CLI contract lives in a single
/// place.
///
/// - `<type>` is the positional image type (`qcow2`, `raw`, …; see
///   [`crate::context::DiskType`]).
/// - `--output-dir /output --output-name disk` makes the artifact land as a flat
///   `/output/disk.<ext>` (e.g. `disk.qcow2`), regardless of type — both builders
///   bind `/output` to the target's [`DiskTarget::output_dir`].
/// - `--ignore-warnings` keeps non-fatal manifest/blueprint warnings from failing
///   the build (`image-builder` treats them as errors by default).
/// - `--progress verbose` is line-oriented, so it streams fine without a tty.
///
/// `cfg_flag` is the `--blueprint <path>` fragment (empty when there's no
/// provisioning config); the container path it names is set up by each builder.
pub(crate) fn image_builder_args(target: &DiskTarget, source_ref: &str, cfg_flag: &str) -> String {
	format!(
		"build {ty} --bootc-ref {source_ref} --bootc-default-fs {rootfs} --arch {arch} \
		 --output-dir /output --output-name disk {cfg_flag} --ignore-warnings --progress verbose",
		ty = target.disk_type,
		rootfs = target.image.rootfs,
		arch = target.image.arch,
	)
}

/// A place that can build `bootcher`'s images for an [`ImageRef`].
pub(crate) trait Builder {
	/// `podman build` one image (the caller builds its base dependency first).
	/// [`LocalBuilder`] builds into local rootless storage; [`RemoteBuilder`]
	/// ships the build context up and `sudo podman build`s into the remote's root
	/// storage, recording residency so a following [`Builder::build_disk`] skips
	/// the transfer.
	///
	/// # Errors
	///
	/// Returns an error if the build command fails.
	fn build_image(&self, image: &ImageRef, job: &mut Scope) -> Result<()>;

	/// Run `image-builder` to turn `target`'s container into a disk image of
	/// `target.disk_type`, building from (and recording as the bootc origin)
	/// `source_ref`. Ensures the container is present on the builder first
	/// (transferred from local storage unless already resident from
	/// [`Builder::build_image`]). On `Ok`, `target.output_dir()` contains the
	/// artifacts (a flat `disk.<ext>`).
	///
	/// `config` is an optional blueprint file (TOML contents) passed via
	/// `--blueprint`; bootcher uses it to inject provision-time files (the admin
	/// SSH key, and the registry pull secret in registry mode) into the image's
	/// `/etc` as `customizations.files`. The builder writes it wherever
	/// `image-builder` will run (a local tempfile, or uploaded to the remote).
	///
	/// # Errors
	///
	/// Returns an error if the `image-builder` command fails.
	fn build_disk(
		&self,
		target: &DiskTarget,
		source_ref: &str,
		config: Option<&str>,
		job: &mut Scope,
	) -> Result<()>;

	/// Ensure the built container is in *local* rootless storage under
	/// [`ImageRef::tag`] — the postcondition `deploy`/`upgrade` rely on. A no-op
	/// for [`LocalBuilder`]; [`RemoteBuilder`] saves on the builder and loads home.
	///
	/// # Errors
	///
	/// Returns an error if the export or transfer fails.
	fn export_home(&self, image: &ImageRef, job: &mut Scope) -> Result<()>;
}

/// Which builder a spec resolves to. Separated from construction so the (pure)
/// decision is unit-testable without booting anything.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
	/// ssh destination to drive as a [`RemoteBuilder`].
	Remote(String),
	/// In-process [`LocalBuilder`] (the default).
	Local,
	/// Throwaway local VM, driven as a [`RemoteBuilder`] (the literal `vm` spec).
	Vm,
}

/// The resolution policy, as a pure function of the `spec` (a manifest
/// `[builder]` value; see [`crate::context::BuilderConfig`]).
///
/// The literal `vm` ⇒ [`Choice::Vm`] (a throwaway local VM); the literal `local`
/// (or a blank spec) ⇒ the in-process [`Choice::Local`]; any other non-blank
/// value ⇒ [`Choice::Remote`] (an ssh destination). Selection is explicit, so a
/// cross-arch target is *not* auto-routed to a VM (the caller warns; see
/// [`select`]).
fn choose(spec: &str) -> Choice {
	match spec.trim() {
		"vm" => Choice::Vm,
		"" | "local" => Choice::Local,
		dest => Choice::Remote(dest.into()),
	}
}

/// Whether `spec` resolves to the in-process [`LocalBuilder`] (the `local`/blank
/// spec) — work that runs on *this* host rather than over ssh.
#[must_use]
pub(crate) fn is_local(spec: &BuilderSpec) -> bool {
	choose(spec.spec()) == Choice::Local
}

/// Whether `spec` resolves to a throwaway local VM (the `vm` spec) — booted here
/// under qemu and driven over its forwarded ssh port.
#[must_use]
pub(crate) fn is_vm(spec: &BuilderSpec) -> bool {
	choose(spec.spec()) == Choice::Vm
}

/// Whether `spec` resolves to a real remote host reached over ssh (any non-blank
/// spec that isn't `local`/`vm`).
#[must_use]
pub(crate) fn is_remote(spec: &BuilderSpec) -> bool {
	matches!(choose(spec.spec()), Choice::Remote(_))
}

/// Whether `spec` resolves to the in-process [`LocalBuilder`], whose disk-image
/// step runs privileged `sudo podman` on *this* host (unlike a remote/VM builder,
/// which `sudo`s on the far side). A multi-arch fan-out primes the local sudo
/// credential up front when this holds for any arch, so that prompt lands on the
/// quiet terminal before the concurrent per-arch bars start, not under them (see
/// [`crate::jobs::disk::run`]).
#[must_use]
pub(crate) fn runs_local_root(spec: &BuilderSpec) -> bool {
	is_local(spec)
}

/// Pick (and, for a VM, boot) the builder for `image`, mapping its spec string
/// (`local`, `vm`, or an `[user@]host` ssh destination) to a concrete builder.
/// Eager: a `vm` spec boots the guest here, under `job`, so the builder
/// returned is ready to use and — held by one caller across build+image — boots
/// the VM exactly once.
///
/// # Errors
///
/// Returns an error if a VM spec fails to boot the guest.
pub(crate) fn select(
	image: &ImageRef,
	spec: &BuilderSpec,
	job: &Scope,
) -> Result<Box<dyn Builder>> {
	Ok(match choose(spec.spec()) {
		Choice::Remote(dest) => Box::new(RemoteBuilder::new(
			Ssh::new(dest, spec.ssh_opts()),
			spec.podman_opts().to_vec(),
		)),
		Choice::Local => {
			// Local is the default even cross-arch — it works under emulation, just
			// slower than native. Note the overhead rather than silently routing away.
			if Arch::host() != Some(image.arch) {
				job.println(format!(
					"building {image} cross-arch in-process under qemu-user emulation — slower \
					 than native; point this builder at a native-arch remote in the \
					 `[builder]` table of bootcher.toml to offload it"
				));
			}
			Box::new(LocalBuilder::new(spec.podman_opts().to_vec()))
		}
		Choice::Vm => {
			// A VM is just a remote we booted: drive it with a plain
			// `RemoteBuilder`, and keep the `VmInstance` alongside it so the guest
			// is torn down when the builder drops. The remote stays unaware a VM is
			// involved (see the tuple `Builder` impl below).
			let vm = vm::VmInstance::boot(image.arch, job)?;
			let remote = RemoteBuilder::new(vm.ssh().clone(), spec.podman_opts().to_vec());
			Box::new((vm, remote))
		}
	})
}

/// Which build step [`prompt_spec`] is choosing a backend for. Drives the
/// context-specific help text; every context pre-selects the fast in-process
/// `local`.
#[derive(Clone, Copy)]
pub(crate) enum BuilderRole {
	/// The container build (`podman build`). Runs under qemu-user emulation
	/// cross-arch — slow, but otherwise sound.
	Build,
	/// The `image-builder` disk-image step — the heavier cross-arch one, run under
	/// qemu-user emulation in-process.
	Image,
}

/// Interactive builder picker for `bootcher init`, persisted verbatim into the
/// manifest's `[builder]` table (see [`crate::context::BuilderConfig`]). Returns
/// a concrete spec — `local`, `vm`, or a free-text `[user@]host` /
/// `ssh://[user@]host[:port]`, never blank — so the scaffolded manifest records
/// every key explicitly.
///
/// `role` and `cross_arch` only steer the guidance, not the result: the help
/// line explains the tradeoffs for that exact context. Every context defaults to
/// the fast in-process `local`.
///
/// # Errors
///
/// Returns an error if the interactive prompt fails (e.g. non-TTY, user aborts).
pub(crate) fn prompt_spec(question: &str, role: BuilderRole, cross_arch: bool) -> Result<String> {
	let choices = vec![BuilderChoice::Local, BuilderChoice::Vm, BuilderChoice::Remote];
	// Always pre-select Local: cross-arch now works in-process under emulation (just
	// slower than native), so there's no context where another backend is the safer
	// default — only a faster one the help text points at.
	let start = 0;
	let help = match (role, cross_arch) {
		(_, false) => {
			"Local is fastest but needs sudo on this host; VM is a clean sandbox but wants \
			 qemu + KVM (/dev/kvm) to be usable; Remote offloads to another machine."
		}
		(BuilderRole::Image, true) => {
			"cross-arch image building runs under qemu-user emulation: Local works but is slower \
			 than native; a same-arch Remote is by far the fastest; a VM is a clean sandbox but \
			 uses full-system emulation, slower still."
		}
		(BuilderRole::Build, true) => {
			"cross-arch build runs under emulation — Local works but is slow; a same-arch \
			 Remote is much faster; VM works but is very slow here too."
		}
	};
	let chosen = inquire::Select::new(question, choices)
		.with_starting_cursor(start)
		.with_help_message(help)
		.prompt()?;
	Ok(match chosen {
		BuilderChoice::Local => "local".to_owned(),
		BuilderChoice::Vm => "vm".to_owned(),
		BuilderChoice::Remote => {
			inquire::Text::new("Remote SSH destination ([user@]host or ssh://[user@]host[:port]):")
				.prompt()?
		}
	})
}

/// The choices offered by [`prompt_spec`]; each maps to a literal spec `choose`
/// recognises (`local`, `vm`, or a free-text ssh destination).
enum BuilderChoice {
	Local,
	Vm,
	Remote,
}

impl fmt::Display for BuilderChoice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			BuilderChoice::Local => "Local — build in-process on this machine (native arch)",
			BuilderChoice::Vm => "VM — throwaway local VM (needed for cross-arch builds)",
			BuilderChoice::Remote => {
				"Remote — a native-arch host over SSH ([user@]host or ssh://[user@]host[:port])"
			}
		})
	}
}

/// A VM-backed builder: a booted `VmInstance` kept alive next to the
/// [`RemoteBuilder`] that drives it (over its forwarded loopback port), so the
/// guest is killed when this drops. Every operation delegates to the remote —
/// which never learns it's talking to a VM rather than a real host. Built by
/// [`select`] for the `vm` spec.
impl Builder for (vm::VmInstance, RemoteBuilder) {
	fn build_image(&self, image: &ImageRef, job: &mut Scope) -> Result<()> {
		self.1.build_image(image, job)
	}

	fn build_disk(
		&self,
		target: &DiskTarget,
		source_ref: &str,
		config: Option<&str>,
		job: &mut Scope,
	) -> Result<()> {
		self.1.build_disk(target, source_ref, config, job)
	}

	fn export_home(&self, image: &ImageRef, job: &mut Scope) -> Result<()> {
		self.1.export_home(image, job)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn image_builder_image_is_digest_pinned() {
		// A rolling `:latest` would reintroduce the cross-arch breakage this pin
		// exists to prevent, so assert it stays pinned by digest.
		assert!(
			IMAGE_BUILDER_IMAGE.contains("@sha256:"),
			"image-builder image must be digest-pinned, not a tag"
		);
		let digest = IMAGE_BUILDER_IMAGE.split("@sha256:").nth(1).unwrap();
		assert_eq!(digest.len(), 64);
		assert!(digest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
	}

	#[test]
	fn blank_or_local_is_local() {
		// Whitespace-only and the explicit `local` literal both mean in-process.
		assert_eq!(choose("  "), Choice::Local);
		assert_eq!(choose("local"), Choice::Local);
		assert_eq!(choose(" local "), Choice::Local);
	}

	#[test]
	fn remote_destination_is_remote() {
		assert_eq!(choose("me@host"), Choice::Remote("me@host".into()));
	}

	#[test]
	fn vm_keyword_selects_vm() {
		// The literal `vm` is the one reserved keyword.
		assert_eq!(choose("vm"), Choice::Vm);
		// A surrounding-whitespace `vm` still resolves (spec is trimmed).
		assert_eq!(choose(" vm "), Choice::Vm);
		// But `vm` as part of a destination is a remote, not the keyword.
		assert_eq!(choose("vm@host"), Choice::Remote("vm@host".into()));
	}
}
