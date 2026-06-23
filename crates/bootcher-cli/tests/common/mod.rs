//! Shared scaffolding for the VM end-to-end tests — the LAN lifecycle
//! (`e2e_vm.rs`), the registry-mode signing path (`e2e_registry_sign.rs`), the
//! plain registry lifecycle + signing enrollment (`e2e_registry.rs`), and the
//! LAN→registry origin switch (`e2e_lan_to_registry.rs`). Holds the environment
//! probe, the ssh-agent wrapper, keypair generation, the throwaway-registry guard,
//! and the small filesystem/process helpers the harnesses share; each test keeps
//! its own `Harness` (the flows differ).
//!
//! Not a test binary itself — it's a module each test file pulls in with
//! `mod common;`. `dead_code` is allowed because not every helper is used by every
//! test that includes it.
#![allow(dead_code)]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use bootcher_core::context::Arch;

/// The provisioned admin `authorized_keys` file (mirrors the scaffold sshd config).
pub(crate) const ADMIN_KEYS: &str = "/etc/ssh/authorized_keys.d/admin";
/// The ssh user the scaffold provisions.
pub(crate) const VM_USER: &str = "admin";
/// Generous boot/ssh budget — a cold first boot of a real bootc disk, even on KVM.
pub(crate) const SSH_TIMEOUT: Duration = Duration::from_mins(10);
/// Root for the throwaway run dir *and* the podman store — disk-backed `/var/tmp`,
/// not tmpfs `/tmp` (might be too small for a ~2GB bootc image) and not the user's home.
pub(crate) const SCRATCH_BASE: &str = "/var/tmp";

// ----------------------------------------------------------------- ssh-agent

/// A spawned `ssh-agent`, killed on drop. Carries the socket bootcher and `ssh-add`
/// talk to.
pub(crate) struct Agent {
	sock: PathBuf,
	pid: u32,
}

impl Agent {
	pub(crate) fn start(home: &Path) -> Self {
		let sock = home.join("agent.sock");
		// `-a <sock>`: bind a known socket and daemonize; stdout prints the PID.
		let out = duct::cmd!("ssh-agent", "-a", &sock)
			.stdout_capture()
			.run()
			.expect("ssh-agent must be installed for the e2e");
		let stdout = String::from_utf8_lossy(&out.stdout);
		let pid = stdout
			.lines()
			.find_map(|l| l.trim().strip_prefix("SSH_AGENT_PID="))
			.and_then(|v| v.trim_end_matches(';').split(';').next())
			.and_then(|v| v.trim().parse().ok())
			.expect("parse ssh-agent PID");
		Self { sock, pid }
	}

	pub(crate) fn sock(&self) -> &Path {
		&self.sock
	}

	pub(crate) fn add(&self, key: &Path) {
		duct::cmd!("ssh-add", key)
			.env("SSH_AUTH_SOCK", &self.sock)
			.run()
			.unwrap_or_else(|_| panic!("ssh-add of {} failed", key.display()));
	}
}

impl Drop for Agent {
	fn drop(&mut self) {
		let _ = duct::cmd!("kill", self.pid.to_string()).unchecked().run();
	}
}

// ----------------------------------------------------------------- prereqs

/// The host facts the e2e needs, or `None` (with a printed reason) to skip.
pub(crate) struct Prereqs {
	pub arch: Arch,
	pub firmware: PathBuf,
}

impl Prereqs {
	pub(crate) fn probe() -> Result<Self> {
		let Some(arch) = Arch::host() else {
			bail!("unrecognised host arch");
		};

		for bin in [arch.qemu_system_bin(), "qemu-img", "podman", "ssh", "ssh-agent", "ssh-keygen"]
		{
			if !is_binary_available(bin) {
				bail!("missing `{bin}`");
			}
		}
		if !Path::new("/dev/kvm").exists() {
			bail!("/dev/kvm not present (KVM required)");
		}
		let Some(firmware) = bootcher_core::qemu::find_uefi_firmware(arch) else {
			bail!("no UEFI firmware (install edk2-ovmf / AAVMF)");
		};
		// image-builder runs a privileged container, and bootcher opens it through one `sudo sh`
		// root session (see bootcher_core::sudo). The e2e needs passwordless
		// `sudo sh` specifically since while it's running there's no TTY
		// for a password prompt.
		let sudo_ok = duct::cmd!("sudo", "-n", "sh", "-c", "true")
			.stdout_null()
			.stderr_null()
			.unchecked()
			.run()
			.is_ok_and(|o| o.status.success());
		if !sudo_ok {
			bail!(
				"passwordless `sudo sh` not available. bootcher's privileged \
				 image build runs `sudo sh` — \
				 grant e.g. `{user} ALL=(ALL) NOPASSWD: ALL` (dev box) in sudoers",
				user = std::env::var("USER").unwrap_or_else(|_| "<you>".into()),
			);
		}
		Ok(Self { arch, firmware })
	}
}

/// Host facts the **cross-arch** builder e2e needs, or `None` (with a reason) to
/// skip. Unlike [`Prereqs`] this targets the *foreign* arch: the image-builder step runs in
/// a foreign Fedora Cloud builder VM under TCG (no KVM), and the produced disk is
/// booted under TCG too — so it needs the foreign `qemu-system-<arch>` and foreign
/// UEFI firmware, qemu-user emulation for the cross-arch `podman build`, and
/// network for the builder's one-time cloud-image + base-image pulls. It does *not*
/// need `/dev/kvm` (everything foreign is emulated) or passwordless host `sudo`
/// (the privileged image-builder step runs inside the builder VM, not in-process).
pub(crate) struct CrossPrereqs {
	pub host: Arch,
	pub cross: Arch,
	/// UEFI firmware for the *cross* arch, to boot the produced disk.
	pub cross_firmware: PathBuf,
}

impl CrossPrereqs {
	pub(crate) fn probe() -> Result<Self> {
		let Some(host) = Arch::host() else {
			bail!("unrecognised host arch");
		};
		// The cross target is whichever arch the host is not.
		let cross = match host {
			Arch::X86_64 => Arch::Aarch64,
			Arch::Aarch64 => Arch::X86_64,
		};

		for bin in [cross.qemu_system_bin(), "qemu-img", "podman", "ssh", "ssh-keygen"] {
			if !is_binary_available(bin) {
				bail!("missing `{bin}` (needed to build/boot a {cross} guest)");
			}
		}
		// The cross-arch container build (`[builder] build = "local"`) runs under
		// qemu-user via binfmt_misc — without a registered handler `podman build
		// --platform` fails. Probe for *any* registered qemu binfmt entry.
		if !qemu_user_registered() {
			bail!(
				"no qemu-user binfmt_misc handler registered — the cross-arch `podman build` needs \
				 one (install qemu-user-static, e.g. `podman run --rm --privileged \
				 docker.io/multiarch/qemu-user-static --reset -p yes`)"
			);
		}
		let Some(cross_firmware) = bootcher_core::qemu::find_uefi_firmware(cross) else {
			bail!("no UEFI firmware for {cross} (install edk2-ovmf / edk2-aarch64 / AAVMF)");
		};
		Ok(Self { host, cross, cross_firmware })
	}
}

/// True if any `qemu-*` handler is registered in `binfmt_misc` — a cheap proxy for
/// "cross-arch `podman build` can emulate the foreign userspace".
fn qemu_user_registered() -> bool {
	let Ok(entries) = std::fs::read_dir("/proc/sys/fs/binfmt_misc") else {
		return false;
	};
	entries.flatten().any(|e| {
		e.file_name().to_string_lossy().starts_with("qemu-")
			&& std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("enabled"))
	})
}

// ----------------------------------------------------------------- helpers

pub(crate) fn is_binary_available(bin: &str) -> bool {
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

/// Generate an unencrypted ed25519 keypair `<dir>/<name>` (+ `.pub`), returning the
/// private-key path.
pub(crate) fn keygen(dir: &Path, name: &str) -> PathBuf {
	let key = dir.join(name);
	duct::cmd!("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", &key)
		.run()
		.unwrap_or_else(|_| panic!("ssh-keygen for {name} failed"));
	key
}

pub(crate) fn pub_key_path(key: &Path) -> PathBuf {
	let mut s = key.as_os_str().to_owned();
	s.push(".pub");
	PathBuf::from(s)
}

/// The base64 body of a public key (the field sshd actually stores), for substring
/// assertions against the on-device `authorized_keys`.
pub(crate) fn read_pub_key(key: &Path) -> String {
	std::fs::read_to_string(pub_key_path(key))
		.unwrap()
		.split_whitespace()
		.nth(1)
		.unwrap()
		.to_owned()
}

/// First file named `name` anywhere under `root`.
pub(crate) fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		for entry in std::fs::read_dir(&dir).ok()?.flatten() {
			let path = entry.path();
			if path.is_dir() {
				stack.push(path);
			} else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
				return Some(path);
			}
		}
	}
	None
}

// ----------------------------------------------------------------- registry

/// A throwaway `registry:2` container for the registry-mode e2e tests, removed on
/// drop. Both build host and guest pull a signed/unsigned image from it under the
/// **same** reference string (see [`host_primary_ip`]). Named per test so two
/// registry e2e binaries don't collide on the container name.
pub(crate) struct RegistryGuard {
	name: String,
}

impl RegistryGuard {
	/// Start `registry:2` named `name`, published on host `port`, and block until
	/// it accepts connections. A prior container of the same name is force-removed
	/// first so a crashed earlier run doesn't wedge this one.
	pub(crate) fn start(name: &str, port: u16) -> Self {
		// A prior container of this name (a crashed earlier run) would wedge the
		// `podman run` below; reap it — and its anonymous blob volume — first.
		bootcher_core::podman::reap_container(name);
		duct::cmd!(
			"podman",
			"run",
			"-d",
			"--name",
			name,
			"-p",
			&format!("{port}:5000"),
			"docker.io/library/registry:2"
		)
		.run()
		.expect("could not start the throwaway registry");
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			if TcpStream::connect(("127.0.0.1", port)).is_ok() {
				break;
			}
			assert!(Instant::now() < deadline, "registry did not start within 10s");
			std::thread::sleep(Duration::from_millis(100));
		}
		Self { name: name.to_owned() }
	}
}

impl Drop for RegistryGuard {
	fn drop(&mut self) {
		bootcher_core::podman::reap_container(&self.name);
	}
}

/// A `registries.conf` drop-in marking the throwaway registry (`<ip>:<port>`)
/// plain-HTTP, so both the build host's podman and the guest accept it without TLS.
pub(crate) fn insecure_registries_conf(reg_addr: &str) -> String {
	format!("[[registry]]\nlocation = \"{reg_addr}\"\ninsecure = true\n")
}

/// The host's primary (default-route source) IPv4 — the address both the build
/// host (locally) and the guest (via qemu user-net NAT) reach the registry at.
/// Addressing the registry by this IP (not `localhost`) keeps the reference string
/// identical on both sides, which a cosign `matchRepository` identity requires.
pub(crate) fn host_primary_ip() -> String {
	let out = duct::cmd!("ip", "-4", "route", "get", "1.1.1.1")
		.stdout_capture()
		.run()
		.expect("`ip route get` to discover the host IP");
	let text = String::from_utf8_lossy(&out.stdout);
	// "... src <ip> ..." — the source address the kernel would use.
	text.split_whitespace()
		.skip_while(|w| *w != "src")
		.nth(1)
		.map(str::to_owned)
		.expect("no `src` IP in `ip route get` output")
}

// ----------------------------------------------------------------- store cleanup

/// Set this (to any value) to *keep* a test's persistent podman store after the
/// run — trading disk for a warm cache (cached base image / built layers) on the
/// next run. By default the store is wiped on teardown so repeated e2e runs don't
/// pile up gigabytes of accumulated images under `/var/tmp/bootcher-e2e-*-store`.
pub(crate) const KEEP_STORE_ENV: &str = "BOOTCHER_E2E_KEEP_STORE";

/// Owns a test's persistent podman store dir (the one its commands point
/// `XDG_DATA_HOME` at). Constructing it readies the dir; dropping it removes the
/// dir — wiping the accumulated images — unless [`KEEP_STORE_ENV`] is set. Held as
/// a `Harness` field so the wipe runs at end of test, after the last podman call.
pub(crate) struct StoreGuard {
	dir: PathBuf,
}

impl StoreGuard {
	/// Create (if needed) the podman store dir at `dir` and guard it for cleanup.
	pub(crate) fn new(dir: &str) -> Self {
		std::fs::create_dir_all(dir).expect("create podman store dir");
		Self { dir: PathBuf::from(dir) }
	}
}

impl Drop for StoreGuard {
	fn drop(&mut self) {
		if std::env::var_os(KEEP_STORE_ENV).is_some() {
			return;
		}
		// Go through `podman unshare`: it enters the user namespace where the
		// rootless store's subuid-owned overlay files map to root, so a plain
		// `rm -rf` can remove them (a bare `std::fs::remove_dir_all` would hit
		// EACCES on those files wherever podman uses a subuid range).
		bootcher_core::exec::best_effort(&duct::cmd!("podman", "unshare", "rm", "-rf", &self.dir));
	}
}
