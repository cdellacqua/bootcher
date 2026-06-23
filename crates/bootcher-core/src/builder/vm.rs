//! Local-VM instance: when the user passes `--builder vm`, boot a throwaway
//! *target-arch* guest and expose its ssh connection parameters so a
//! [`super::RemoteBuilder`] can drive it exactly like a real remote — a VM is
//! just a remote you launched yourself. This module owns only the VM's
//! lifecycle; it is not itself a [`super::Builder`].
//!
//! Unlike running `image-builder` directly under qemu-user, a full
//! system-emulation guest runs a real target-arch kernel and userspace, so the
//! cross-arch emulation trouble that motivates this whole module (slow and
//! historically fragile — the predecessor crashed outright) is sidestepped. The
//! cost: a foreign-arch guest runs under TCG (no KVM across arches), which is slow
//! — this is the "works anywhere, no extra hardware" fallback. A same-arch guest
//! uses KVM when `/dev/kvm` is accessible, so it runs at near-native speed.
//!
//! Caching keeps the per-build cost down in three tiers under
//! `~/.cache/bootcher/builder/`:
//! 1. `base-<arch>.qcow2` — the stock Fedora Cloud image, downloaded and
//!    sha256-verified once.
//! 2. `prepared-<arch>.qcow2` — a copy-on-write overlay on the base, booted once so
//!    cloud-init installs podman, injects our ssh key, and pre-pulls the
//!    `image-builder` image; then powered off. The warm build VM.
//! 3. each build boots the prepared overlay *writable* and drives it, so podman's
//!    image store persists between runs — a later build only pulls the layer diff
//!    instead of re-fetching the whole `image-builder` image under emulation.
//!
//! [`VmInstance::boot`] returns once the guest answers ssh; [`super::select`]
//! then points a [`super::RemoteBuilder`] at the guest's [`Ssh`]
//! ([`VmInstance::ssh`]) and pairs the two so the build is identical to a real
//! remote. The instance owns the qemu child, so dropping it (alongside that
//! builder) kills the guest.

use super::IMAGE_BUILDER_IMAGE;
use crate::context::Arch;
use crate::exec::{self, run};
use crate::fetch;
use crate::progress::Scope;
use crate::qemu::{self, Vm, VmConfig};
use crate::ssh::Ssh;
use anyhow::{Context, Result};
use duct::cmd;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Fedora Cloud Base Generic qcow2, pinned by version + sha256 (the cloud
/// equivalent of the digest-pinned [`IMAGE_BUILDER_IMAGE`]). Bump deliberately
/// after re-checking the release CHECKSUM file.
struct CloudImage {
	url: &'static str,
	sha256: &'static str,
}

/// Stock cloud image for `arch`. Both arches are needed: cross-arch boots a
/// foreign image under TCG; same-arch boots natively under KVM.
fn cloud_image(arch: Arch) -> CloudImage {
	match arch {
		Arch::Aarch64 => CloudImage {
			url: "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/aarch64/images/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2",
			sha256: "55c60a3b80d3616a08705afd0459e75fe9f03c54aba7a46e4002a41a72fa0d5b",
		},
		Arch::X86_64 => CloudImage {
			url: "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2",
			sha256: "28680fe5b371a5a82ebf43a31926e086a168e59949d03969c5093e7071f90b7f",
		},
	}
}

/// Default cloud-image login user.
const VM_USER: &str = "fedora";
/// Guest RAM (MiB) and vCPUs.
const VM_MEM_MIB: &str = "4096";
const VM_SMP: &str = "4";
/// Absolute size the prepared overlay is grown to before first boot, so the
/// guest (cloud-init growpart) has room for the image-builder build + artifacts.
const VM_DISK_SIZE: &str = "40G";
/// How long to wait for the guest to answer ssh after a boot.
const SSH_TIMEOUT: Duration = Duration::from_mins(10);
/// How long to wait for qemu to exit after a `poweroff`.
const POWEROFF_TIMEOUT: Duration = Duration::from_mins(2);

/// A booted, ssh-reachable builder VM. Owns the qemu child (via [`Vm`]), so the
/// guest is killed when this drops — i.e. when the [`super::RemoteBuilder`] that
/// holds it drops. Exposes the [`Ssh`] the builder uses to drive the guest.
pub(crate) struct VmInstance {
	/// The running guest; its `Drop` kills (and reaps) qemu.
	_vm: Vm,
	/// The connection driving the guest (loopback, user-net forwarded port).
	ssh: Ssh,
}

impl VmInstance {
	/// Boot a throwaway `arch` builder guest and return once it answers ssh.
	///
	/// Tiers 1 & 2 (stock image download, warm prepared overlay) are cached, so
	/// the steady state skips straight to tier 3: boot the prepared overlay
	/// *writable* (so podman's image store persists across builds) and wait for
	/// ssh. The returned instance keeps the guest alive; the overlay's writes
	/// persist on disk after it drops.
	pub(crate) fn boot(arch: Arch, job: &Scope) -> Result<Self> {
		let firmware = firmware(arch)?; // fail early with a clear message if UEFI firmware is missing
		let cache = crate::cache::subdir("builder")?;
		let key = ensure_keypair(&cache)?;

		let base = ensure_base(arch, &cache, job)?;
		let prepared = ensure_prepared(arch, &cache, &base, &key, job)?;

		let port = qemu::free_port()?;
		let log = cache.join(format!("run-{arch}.log"));
		let spin = job.spinner("booting builder VM");
		let vm = Vm::spawn(
			&builder_vm_config(arch, &prepared, None, firmware.as_deref(), port, &log),
			job,
		)?;
		spin.finish();
		let ssh = qemu::ssh(VM_USER, port, &key);
		ssh.wait_until_reachable(SSH_TIMEOUT, job)?;

		Ok(Self { _vm: vm, ssh })
	}

	/// The [`Ssh`] driving the guest (loopback, user-net forwarded port). Handed to a
	/// [`super::RemoteBuilder`] by [`super::select`], so the build is identical to one
	/// against a real remote.
	pub(crate) fn ssh(&self) -> &Ssh {
		&self.ssh
	}
}

/// Pick the best accelerator for `arch`: KVM when the host arch matches and
/// `/dev/kvm` is accessible (same-arch build, hardware available), TCG otherwise.
fn accel(arch: Arch) -> &'static str {
	if Arch::host() == Some(arch) && std::path::Path::new("/dev/kvm").exists() {
		"kvm"
	} else {
		"tcg,thread=multi"
	}
}

/// The builder's [`VmConfig`]: picks KVM when `arch` matches the host and
/// `/dev/kvm` is accessible, otherwise TCG. Centralises the knobs the builder
/// fixes so both boot sites ([`VmInstance::boot`] and the prepare step) agree.
fn builder_vm_config<'a>(
	arch: Arch,
	disk: &'a Path,
	seed: Option<&'a Path>,
	firmware: Option<&'a Path>,
	port: u16,
	log: &'a Path,
) -> VmConfig<'a> {
	VmConfig {
		arch,
		disk,
		seed,
		firmware,
		port,
		log,
		accel: accel(arch),
		mem_mib: VM_MEM_MIB,
		smp: VM_SMP,
	}
}

/// Firmware for a *cloud-image* builder guest: the `aarch64` `virt` machine needs
/// a UEFI blob (no BIOS fallback), so a missing one is fatal with a clear hint;
/// `x86_64` boots `SeaBIOS`, so it needs none. Thin policy wrapper over
/// [`qemu::find_uefi_firmware`].
fn firmware(arch: Arch) -> Result<Option<PathBuf>> {
	if arch == Arch::X86_64 {
		return Ok(None);
	}
	qemu::find_uefi_firmware(arch).map(Some).ok_or_else(|| {
		anyhow::anyhow!("no aarch64 UEFI firmware found; install edk2-aarch64 / AAVMF")
	})
}

/// Generate an ephemeral ed25519 keypair for the builder VM if absent, returning
/// the private-key path. Kept across runs because the matching public key is
/// baked into the prepared overlay.
///
/// The keypair is shared across arches (unlike the per-arch base/prepared
/// overlays), so a multi-arch build that boots two VMs concurrently could race
/// two `ssh-keygen`s onto the same path on a cold cache; a process-wide lock
/// serialises the check-and-create so only the first generates it.
fn ensure_keypair(cache: &Path) -> Result<PathBuf> {
	static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
	let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

	let key = cache.join("builder_ed25519");
	if key.is_file() {
		return Ok(key);
	}
	// `-q` keeps it from printing the fingerprint/randomart over the live bars; null stdio guards the rest.
	cmd!("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "bootcher-builder", "-f", &key)
		.stdout_null()
		.stderr_null()
		.run()
		.context("running ssh-keygen")?;
	Ok(key)
}

/// Tier 1: ensure the stock cloud image is present and verified, returning its
/// path. Downloads + checks sha256 on first use; trusts the cached file after.
/// [`fetch::download`] handles the atomic `.part` + checksum + rename dance.
fn ensure_base(arch: Arch, cache: &Path, job: &Scope) -> Result<PathBuf> {
	let base = cache.join(format!("base-{arch}.qcow2"));
	if base.is_file() {
		return Ok(base);
	}
	let img = cloud_image(arch);
	job.println(format!("downloading Fedora Cloud base image for {arch}"));
	fetch::download(job, img.url, &base, Some(fetch::Checksum::Sha256(img.sha256)))?;
	Ok(base)
}

/// Tier 2: ensure the warm prepared overlay exists, returning its path. On first
/// use: make a copy-on-write overlay on `base`, grow it, boot once with a cloud-init seed
/// (installs podman, injects the ssh key, pre-pulls image-builder), wait for cloud-init to
/// finish, then power off cleanly. A `.ready` marker records success so a later
/// run skips straight to the per-build boot.
fn ensure_prepared(
	arch: Arch,
	cache: &Path,
	base: &Path,
	key: &Path,
	job: &Scope,
) -> Result<PathBuf> {
	let prepared = cache.join(format!("prepared-{arch}.qcow2"));
	let ready = cache.join(format!("prepared-{arch}.ready"));
	if prepared.is_file() && ready.is_file() {
		return Ok(prepared);
	}
	// A previous prepare may have died mid-way; start clean.
	let _ = std::fs::remove_file(&prepared);
	let _ = std::fs::remove_file(&ready);

	job.println("preparing builder VM (first run): cloud-init installs podman + pre-pulls image-builder under emulation — slow, but cached after this");

	// Copy-on-write overlay on the immutable base, grown to give the build room.
	run!(job, "qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", base, &prepared)?;
	run!(job, "qemu-img", "resize", "-q", &prepared, VM_DISK_SIZE)?;

	let seed = cache.join(format!("seed-{arch}.img"));
	write_seed(&seed, key)?;

	let port = qemu::free_port()?;
	let log = cache.join(format!("prepare-{arch}.log"));
	let firmware = firmware(arch)?;
	let vm = Vm::spawn(
		&builder_vm_config(arch, &prepared, Some(&seed), firmware.as_deref(), port, &log),
		job,
	)?;
	let ssh = qemu::ssh(VM_USER, port, key);
	ssh.wait_until_reachable(SSH_TIMEOUT, job)?;

	// Block until cloud-init has fully applied (package install + runcmd). Its
	// exit status can be non-zero on a degraded run (e.g. the optional image-builder
	// pre-pull failed); that's fine — the build pulls on demand — so we don't
	// treat it as fatal (hence `let _ =`), only the boot/ssh failing is.
	let _ = exec::run_argv(job, &ssh.argv(&[], "cloud-init status --wait"));
	// Clean shutdown so the overlay is left consistent (ssh drops as the guest
	// powers off, so its non-zero exit is expected and ignored).
	let _ = exec::run_argv(job, &ssh.argv(&[], "sudo poweroff"));
	let off = job.spinner("powering off builder VM");
	vm.wait_for_exit(POWEROFF_TIMEOUT)
		.context("waiting for builder VM to power off after preparation")?;
	off.finish();

	std::fs::remove_file(&seed).ok();
	std::fs::write(&ready, b"").with_context(|| format!("writing {}", ready.display()))?;
	Ok(prepared)
}

/// Build the `NoCloud` seed: a tiny FAT image labelled `CIDATA` holding
/// `user-data` (the cloud-config) and `meta-data`. cloud-init finds it by that
/// filesystem label.
fn write_seed(path: &Path, key: &Path) -> Result<()> {
	let pubkey = std::fs::read_to_string(key.with_extension("pub"))
		.with_context(|| format!("reading {}.pub", key.display()))?;
	let pubkey = pubkey.trim();

	let user_data = format!(
		"#cloud-config\n\
		 users:\n\
		 \x20 - default\n\
		 \x20 - name: {VM_USER}\n\
		 \x20   groups: wheel\n\
		 \x20   sudo: 'ALL=(ALL) NOPASSWD:ALL'\n\
		 \x20   ssh_authorized_keys:\n\
		 \x20     - {pubkey}\n\
		 packages:\n\
		 \x20 - podman\n\
		 \x20 - tar\n\
		 runcmd:\n\
		 \x20 - [ podman, pull, {IMAGE_BUILDER_IMAGE} ]\n"
	);
	let meta_data = "instance-id: bootcher-builder\nlocal-hostname: bootcher-builder\n";

	// 1 MiB FAT image: plenty for two small text files, small enough to format
	// as FAT12 and write in a blink.
	let file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(true)
		.open(path)
		.with_context(|| format!("creating {}", path.display()))?;
	file.set_len(1024 * 1024).context("sizing seed image")?;

	let opts = fatfs::FormatVolumeOptions::new().volume_label(*b"CIDATA     ");
	fatfs::format_volume(&file, opts).context("formatting seed FAT volume")?;
	let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new())
		.context("opening seed FAT volume")?;
	{
		let root = fs.root_dir();
		for (name, body) in [("user-data", user_data.as_str()), ("meta-data", meta_data)] {
			let mut f = root.create_file(name).with_context(|| format!("creating {name}"))?;
			f.truncate().ok();
			f.write_all(body.as_bytes()).with_context(|| format!("writing {name}"))?;
		}
	}
	fs.unmount().context("flushing seed FAT volume")?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Read;

	#[test]
	fn pinned_cloud_images_are_wellformed() {
		// Guards the digest pins against a typo on a future bump: each must be a
		// 64-char lowercase-hex sha256 whose URL actually names that arch.
		for arch in [Arch::Aarch64, Arch::X86_64] {
			let img = cloud_image(arch);
			assert_eq!(img.sha256.len(), 64, "{arch}: sha256 wrong length");
			assert!(
				img.sha256.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
				"{arch}: sha256 not lowercase hex"
			);
			assert!(img.url.starts_with("https://"), "{arch}: url not https");
			assert!(img.url.to_lowercase().ends_with(".qcow2"), "{arch}: url not a qcow2");
			assert!(img.url.contains(&arch.to_string()), "{arch}: url arch mismatch");
		}
		// The two arches must not share a digest (copy-paste guard).
		assert_ne!(cloud_image(Arch::Aarch64).sha256, cloud_image(Arch::X86_64).sha256);
	}

	#[test]
	fn firmware_is_none_for_x86_64() {
		// The cloud-image builder boots x86 on SeaBIOS, so its firmware policy
		// returns None regardless of what OVMF the host has installed.
		assert_eq!(firmware(Arch::X86_64).unwrap(), None);
	}

	/// The crown-jewel test: the seed we hand qemu must be a real `CIDATA`
	/// volume whose `user-data` cloud-config actually carries the injected ssh
	/// key — otherwise the guest would boot with no way in. Round-trips the FAT
	/// image back through fatfs to prove it, no VM required.
	#[test]
	fn seed_is_cidata_volume_with_injected_key() {
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("builder_ed25519");
		let pubkey = "ssh-ed25519 AAAATESTKEY bootcher-builder";
		std::fs::write(key.with_extension("pub"), format!("{pubkey}\n")).unwrap();

		let seed = dir.path().join("seed.img");
		write_seed(&seed, &key).unwrap();

		let file = std::fs::File::open(&seed).unwrap();
		let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).unwrap();
		assert!(fs.volume_label().starts_with("CIDATA"), "wrong volume label");

		let read = |name: &str| {
			let root = fs.root_dir();
			let mut f = root.open_file(name).unwrap_or_else(|_| panic!("missing {name}"));
			let mut s = String::new();
			f.read_to_string(&mut s).unwrap();
			s
		};
		let user_data = read("user-data");
		assert!(user_data.starts_with("#cloud-config"), "user-data not a cloud-config");
		assert!(user_data.contains(pubkey), "ssh key not injected");
		assert!(user_data.contains("podman"), "podman not in packages");
		assert!(user_data.contains(IMAGE_BUILDER_IMAGE), "image-builder image not pre-pulled");
		// meta-data must exist too, or NoCloud ignores the datasource.
		assert!(read("meta-data").contains("instance-id"), "meta-data missing instance-id");
	}
}
