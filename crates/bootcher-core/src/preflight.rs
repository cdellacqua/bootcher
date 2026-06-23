//! System-capability probes for the up-front dependency checks. bootcher shells
//! out to `podman`, `ssh`, `sudo` and (for the `vm` builder) `qemu`, but spawns
//! each only at the moment it's needed — so a missing tool would otherwise surface
//! as a bare `No such file or directory (os error 2)` deep into a run, naming
//! neither the tool nor how to get it.
//!
//! This module is a *toolkit*, not a registry of commands: the [`Checks`]
//! accumulator and the primitive probes (a binary on `PATH`, UEFI firmware, a
//! qemu-user binfmt handler, a builder spec's transport). Each job records the
//! tools it needs into a `Checks` and returns [`Checks::finish`] from its own
//! `preflight`, co-located with its `run` — so adding a job touches only that job,
//! not this file. A pipeline chains its jobs' `preflight`s; a CLI subcommand calls
//! one. Either way the check runs *before any work starts* and fails with a single
//! message listing everything missing and how to install it.

use crate::builder;
use crate::context::{Arch, BuilderSpec};
use crate::qemu;
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Hint for the ubiquitous `podman` dependency, shared by the jobs that build,
/// push or inspect images.
pub(crate) const PODMAN_HINT: &str =
	"build, push and inspect container images — install podman (https://podman.io)";

/// Record the tools a non-local builder `spec` for `arch` needs: a `vm` builder
/// boots a local qemu guest (`qemu-system-<arch>` + qemu-img + UEFI firmware) and
/// drives it over ssh; a remote builder just needs ssh; a `local` spec adds
/// nothing here (its caller handles the in-process tools). Shared by the build and
/// image jobs, which differ only in how they treat the `local` case.
pub(crate) fn builder_transport(spec: &BuilderSpec, arch: Arch, checks: &mut Checks) {
	if builder::is_vm(spec) {
		checks.bin(
			arch.qemu_system_bin(),
			"boot a local builder VM (`[builder] = \"vm\"`) — install qemu",
		);
		checks.bin("qemu-img", "prepare the builder VM disk — install qemu/qemu-img");
		checks.bin("ssh", "drive the builder VM over its forwarded SSH port");
		checks.firmware(arch);
	} else if builder::is_remote(spec) {
		checks.bin("ssh", "reach the remote builder over SSH");
	}
}

/// Accumulates missing requirements (deduplicated, since the same tool is reached
/// for by several arches) so one job's check reports its whole gap at once. A job
/// builds one with [`Checks::default`], records its needs through [`Checks::bin`] /
/// [`Checks::firmware`] / [`Checks::qemu_user`] (and [`builder_transport`]), then
/// returns [`Checks::finish`].
#[derive(Default)]
pub(crate) struct Checks {
	/// Formatted `<what> — <hint>` lines for the requirements that failed.
	missing: Vec<String>,
	/// Keys already probed, so a tool needed by many arches is checked once.
	seen: BTreeSet<String>,
}

impl Checks {
	/// Require external binary `bin` on `PATH`, recording it (with `hint`, an
	/// imperative `<verb> … — install <pkg>` clause) if absent.
	pub(crate) fn bin(&mut self, bin: &str, hint: &str) {
		if self.seen.insert(format!("bin:{bin}")) && !binary_available(bin) {
			self.missing.push(format!("`{bin}` — {hint}"));
		}
	}

	/// Require UEFI firmware for `arch` (the `vm` builder boots a UEFI guest).
	pub(crate) fn firmware(&mut self, arch: Arch) {
		if self.seen.insert(format!("firmware:{arch}")) && qemu::find_uefi_firmware(arch).is_none()
		{
			self.missing.push(format!(
				"UEFI firmware for {arch} — install edk2/OVMF (x86_64) or edk2/AAVMF (aarch64) to boot the `vm` builder"
			));
		}
	}

	/// Require a registered qemu-user binfmt handler (cross-arch in-process builds
	/// run the foreign userspace through it).
	pub(crate) fn qemu_user(&mut self) {
		if self.seen.insert("qemu-user".into()) && !qemu_user_registered() {
			self.missing.push(
				"qemu-user binfmt_misc handler — install qemu-user-static for cross-arch \
				 in-process builds, or set `[builder] image = \"vm\"` in bootcher.toml"
					.into(),
			);
		}
	}

	/// Turn the recorded requirements into a result: `Ok` when nothing is missing,
	/// otherwise an error listing every gap. The tail of each job's `preflight`.
	///
	/// # Errors
	///
	/// Returns an error listing every missing prerequisite.
	pub(crate) fn finish(self) -> Result<()> {
		if self.missing.is_empty() {
			return Ok(());
		}
		let mut msg = String::from("missing prerequisites for this command:\n");
		for item in &self.missing {
			let _ = writeln!(msg, "  - {item}");
		}
		let _ = write!(msg, "install the above and retry");
		bail!(msg)
	}
}

/// True if `bin` is on `PATH` and runnable. Tries `--version` first (cheap and
/// true for podman/ssh/qemu/sudo), falling back to `command -v` for anything that
/// doesn't accept it.
fn binary_available(bin: &str) -> bool {
	duct::cmd(bin, ["--version"])
		.stdout_null()
		.stderr_null()
		.unchecked()
		.run()
		.is_ok_and(|o| o.status.success())
		|| duct::cmd("sh", ["-c", &format!("command -v {bin}")])
			.stdout_null()
			.stderr_null()
			.unchecked()
			.run()
			.is_ok_and(|o| o.status.success())
}

/// True if any `qemu-*` handler is registered and enabled in `binfmt_misc` — a
/// cheap proxy for "a cross-arch `podman build` can emulate the foreign userspace".
fn qemu_user_registered() -> bool {
	let Ok(entries) = std::fs::read_dir("/proc/sys/fs/binfmt_misc") else {
		return false;
	};
	entries.flatten().any(|e| {
		e.file_name().to_string_lossy().starts_with("qemu-")
			&& std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("enabled"))
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ubiquitous_tools_resolve_and_bogus_ones_dont() {
		// `sh` is what we shell `command -v` through, so it must be present.
		assert!(binary_available("sh"));
		assert!(!binary_available("definitely-not-a-real-binary-xyzzy"));
	}

	#[test]
	fn satisfied_checks_pass() {
		assert!(Checks::default().finish().is_ok());
	}

	#[test]
	fn missing_tools_are_listed_once_with_hints() {
		let mut checks = Checks::default();
		checks.bin("definitely-not-a-real-binary-xyzzy", "do a thing");
		// Same missing tool reached for twice ⇒ reported once (dedup by key).
		checks.bin("definitely-not-a-real-binary-xyzzy", "do a thing");
		let err = checks.finish().unwrap_err().to_string();
		assert_eq!(err.matches("definitely-not-a-real-binary-xyzzy").count(), 1, "{err}");
		assert!(err.contains("do a thing"), "{err}");
		assert!(err.contains("missing prerequisites"), "{err}");
	}
}
