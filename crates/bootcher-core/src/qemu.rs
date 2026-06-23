//! Headless qemu guests: a RAII handle ([`Vm`]) that boots a disk and tears the
//! guest down on drop, plus the one connection detail reaching it — a user-net
//! loopback-forwarded [`Ssh`] ([`ssh`]), driven from there through the shared
//! [`crate::ssh`] path like any other remote. Both consumers boot a throwaway guest
//! and drive it over ssh, so the lifecycle (spawn → wait-for-ssh → use →
//! kill/poweroff) and the connection options are identical and live here:
//!
//! - the cross-arch builder boots a Fedora Cloud guest under TCG (foreign arch,
//!   no KVM) with a cloud-init seed;
//! - the end-to-end tests boot a real bootc disk under KVM (host arch) with UEFI
//!   firmware and no seed.
//!
//! What differs between them — arch, accel, RAM, firmware, whether a cloud-init
//! seed is attached — is passed in via [`VmConfig`]; everything mechanical (the
//! qemu argv, the signal-driven kill, the [`Ssh`] readiness poll) is shared.

use crate::context::Arch;
use crate::progress::Scope;
use crate::ssh::Ssh;
use anyhow::{Context, Result, bail};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How a single [`Vm`] should be booted. The mechanical bits (user-net ssh
/// forwarding, headless serial-to-log, kill-on-drop) are fixed; this carries only
/// what the two callers genuinely differ on.
pub struct VmConfig<'a> {
	/// Guest architecture — picks the `qemu-system-<arch>` binary and `-machine`.
	pub arch: Arch,
	/// Boot disk, opened *writable* so the guest's changes persist into it.
	pub disk: &'a Path,
	/// Optional cloud-init `NoCloud` seed disk (raw), attached read-only-ish as a
	/// second drive. `None` for a self-contained disk (e.g. a provisioned bootc
	/// image).
	pub seed: Option<&'a Path>,
	/// UEFI firmware blob for `-bios`. `None` boots the arch default (`SeaBIOS` on
	/// x86); a bootc/UEFI-only disk needs `Some` (OVMF on x86, AAVMF on aarch64).
	pub firmware: Option<&'a Path>,
	/// Loopback host port forwarded to the guest's sshd (port 22).
	pub port: u16,
	/// File the guest's serial console is written to (boot diagnostics).
	pub log: &'a Path,
	/// qemu `-accel` value, e.g. `"kvm"` for a same-arch guest or
	/// `"tcg,thread=multi"` for a foreign-arch one.
	pub accel: &'a str,
	/// Guest RAM in MiB (`-m`) and vCPU count (`-smp`), as qemu wants them.
	pub mem_mib: &'a str,
	pub smp: &'a str,
}

/// A running qemu guest. Drop kills it (then reaps); [`Vm::wait_for_exit`] instead
/// awaits a clean self-shutdown after a `poweroff`.
pub struct Vm {
	child: Child,
	/// Kills qemu directly on a signal, in case the main thread is blocked in ssh
	/// I/O (a quiet remote command) rather than in one of the poll loops.
	/// Deregisters when the `Vm` drops.
	_kill: crate::signals::KillGuard,
}

impl Vm {
	/// Spawn `qemu-system-<arch>` headless per `cfg`: serial → `cfg.log`, user-net
	/// SSH forwarding from `127.0.0.1:cfg.port` to the guest's port 22, the disk
	/// (and optional seed) attached as virtio drives. The full argv is logged so a
	/// failed boot can be reproduced by hand.
	///
	/// # Errors
	///
	/// Returns an error if the log file can't be created or `qemu-system-<arch>` fails to spawn.
	pub fn spawn(cfg: &VmConfig<'_>, job: &Scope) -> Result<Self> {
		let logfile = std::fs::File::create(cfg.log)
			.with_context(|| format!("creating {}", cfg.log.display()))?;
		let mut cmd = Command::new(cfg.arch.qemu_system_bin());
		cmd.args(["-machine", machine(cfg.arch), "-accel", cfg.accel]);
		cmd.args(["-cpu", "max", "-smp", cfg.smp, "-m", cfg.mem_mib]);
		if let Some(fw) = cfg.firmware {
			cmd.arg("-bios").arg(fw);
		}
		let drive = format!("file={},if=virtio,format=qcow2", cfg.disk.display());
		cmd.arg("-drive").arg(drive);
		if let Some(seed) = cfg.seed {
			cmd.arg("-drive").arg(format!("file={},if=virtio,format=raw", seed.display()));
		}
		cmd.args([
			"-netdev",
			&format!("user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22", cfg.port),
			"-device",
			"virtio-net-pci,netdev=net0",
		]);
		// Headless: no display, no monitor, serial console to the log file.
		cmd.args(["-display", "none", "-monitor", "none"]);
		cmd.arg("-serial").arg(format!("file:{}", cfg.log.display()));
		cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(logfile);

		// Surface the full argv so a failed boot can be reproduced by hand.
		let rendered = std::iter::once(cmd.get_program())
			.chain(cmd.get_args())
			.map(|a| a.to_string_lossy())
			.collect::<Vec<_>>()
			.join(" ");
		job.println(format!("spawning {rendered}"));

		let child = cmd.spawn().with_context(|| {
			format!("spawning {} (is qemu installed?)", cfg.arch.qemu_system_bin())
		})?;

		// SIGTERM qemu by pid on a signal. Done here rather than only in `Drop` so an
		// interrupt while this thread is blocked in ssh I/O — not in a poll loop —
		// tears the guest down at once instead of waiting for the unwind to reach
		// `Drop`. SIGTERM (gentler than the `Drop` SIGKILL) lets qemu shut the guest
		// down cleanly; `Drop`'s hard kill on the unwind is the backstop if it hangs.
		let pid = child.id();
		let kill = crate::signals::kill_on_signal(move |sig| crate::signals::signal_pid(pid, sig));
		Ok(Self { child, _kill: kill })
	}

	/// Wait up to `timeout` for the guest to exit on its own (after `poweroff`).
	/// Times out into an error rather than hanging if the shutdown stalls.
	///
	/// # Errors
	///
	/// Returns an error if polling qemu fails, a signal interrupts, or the timeout elapses.
	pub fn wait_for_exit(mut self, timeout: Duration) -> Result<()> {
		const POLL: Duration = Duration::from_secs(2);
		let start = Instant::now();
		loop {
			if let Some(_status) = self.child.try_wait().context("polling qemu")? {
				return Ok(());
			}
			// On interrupt, bail; `self` drops, killing the still-running guest.
			crate::signals::check()?;
			if start.elapsed() > timeout {
				bail!("qemu did not exit within {}s", timeout.as_secs());
			}
			std::thread::sleep(POLL);
		}
		// `self` drops here on the error path, killing the stuck qemu.
	}
}

impl Drop for Vm {
	fn drop(&mut self) {
		// If it already exited (clean `wait_for_exit` consumes `self`, so this is the
		// kill path), `kill` is a harmless no-op.
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

/// Pick a free loopback TCP port by binding `:0` and reading the assigned port
/// (same trick as the LAN registry). Racy in theory, fine in practice — qemu
/// rebinds it microseconds later.
///
/// # Errors
///
/// Returns an error if binding the loopback port fails.
pub fn free_port() -> Result<u16> {
	let listener = TcpListener::bind("127.0.0.1:0").context("binding a free port")?;
	Ok(listener.local_addr()?.port())
}

/// qemu `-machine` for `arch`.
#[must_use]
pub(crate) fn machine(arch: Arch) -> &'static str {
	match arch {
		Arch::Aarch64 => "virt",
		Arch::X86_64 => "q35",
	}
}

/// Locate a UEFI firmware blob suitable for `-bios` on `arch`, or `None` if none
/// of the well-known distro paths exist. aarch64 `virt` has no BIOS fallback (so a
/// guest needs this); x86 `q35` boots `SeaBIOS` by default, so firmware is only
/// needed to boot a UEFI-only disk (a bootc image). Returns `None` rather than
/// erroring so each caller decides whether firmware is mandatory for its guest.
pub fn find_uefi_firmware(arch: Arch) -> Option<PathBuf> {
	// Prefer a combined CODE+VARS image, which boots correctly via `-bios` (a bare
	// CODE blob has no NVRAM region). Distros disagree on both the directory and the
	// filename (Fedora's `QEMU_EFI.fd`/`OVMF.fd`, Arch's `*.4m.fd`), so cast wide.
	let candidates: &[&str] = match arch {
		Arch::Aarch64 => &[
			"/usr/share/edk2/aarch64/QEMU_EFI.fd",
			"/usr/share/edk2/aarch64/QEMU_EFI.silent.4m.fd",
			"/usr/share/edk2/aarch64/QEMU_EFI.4m.fd",
			"/usr/share/AAVMF/AAVMF_CODE.fd",
			"/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
			"/usr/share/edk2-armvirt/aarch64/QEMU_EFI.fd",
		],
		Arch::X86_64 => &[
			"/usr/share/edk2/ovmf/OVMF.fd",
			"/usr/share/OVMF/OVMF.fd",
			"/usr/share/edk2/x64/OVMF.fd",
			"/usr/share/edk2/x64/OVMF.4m.fd",
			"/usr/share/qemu/OVMF.fd",
			"/usr/share/edk2-ovmf/x64/OVMF.fd",
		],
	};
	candidates.iter().map(Path::new).find(|p| p.is_file()).map(Path::to_path_buf)
}

/// The [`Ssh`] reaching a guest over its forwarded loopback port. A user-net VM
/// always answers on `user@127.0.0.1`; the forwarded port, identity key and
/// throwaway known-hosts policy ride as opts (see `ssh_opts`). Past this the guest is
/// driven through the same [`Ssh`] path as a real deploy/build remote — readiness
/// poll ([`Ssh::wait_until_reachable`]), `cloud-init status --wait`, `poweroff`.
#[must_use]
pub fn ssh(user: &str, port: u16, key: &Path) -> Ssh {
	Ssh::new(format!("{user}@127.0.0.1"), ssh_opts(port, key))
}

/// ssh options reaching a guest over its forwarded loopback port: the port, the
/// identity key, and the throwaway known-hosts policy. The guest's host key isn't
/// worth tracking, so pin known-hosts to `/dev/null`; a short `ConnectTimeout` keeps
/// the readiness probe from hanging on a not-yet-listening guest.
fn ssh_opts(port: u16, key: &Path) -> Vec<String> {
	vec![
		"-p".into(),
		port.to_string(),
		"-i".into(),
		key.display().to_string(),
		"-o".into(),
		"UserKnownHostsFile=/dev/null".into(),
		"-o".into(),
		"GlobalKnownHostsFile=/dev/null".into(),
		"-o".into(),
		"ConnectTimeout=5".into(),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ssh_opts_carry_port_key_and_throwaway_known_hosts() {
		let opts = ssh_opts(2222, Path::new("/tmp/k"));
		// Flags come in -flag/value pairs, so an even count and findable pairs.
		assert_eq!(opts.len() % 2, 0);
		let pair = |flag: &str, val: &str| opts.windows(2).any(|w| w[0] == flag && w[1] == val);
		assert!(pair("-p", "2222"), "missing port");
		assert!(pair("-i", "/tmp/k"), "missing identity");
		assert!(pair("-o", "UserKnownHostsFile=/dev/null"), "host key not pinned to /dev/null");
	}

	#[test]
	fn ssh_targets_loopback_with_port_and_carries_the_command() {
		let argv: Vec<String> = ssh("admin", 2222, Path::new("/tmp/k"))
			.argv(&[], "echo hi")
			.into_iter()
			.map(|a| a.to_string_lossy().into_owned())
			.collect();
		assert!(argv.contains(&"admin@127.0.0.1".to_string()), "wrong destination: {argv:?}");
		assert_eq!(argv.last().unwrap(), "echo hi", "command not last: {argv:?}");
		assert!(argv.windows(2).any(|w| w[0] == "-p" && w[1] == "2222"), "port missing: {argv:?}");
		assert!(argv.windows(2).any(|w| w[0] == "-i" && w[1] == "/tmp/k"), "key missing: {argv:?}");
	}

	#[test]
	fn machine_and_qemu_bin_match_arch() {
		assert_eq!(machine(Arch::Aarch64), "virt");
		assert_eq!(machine(Arch::X86_64), "q35");
		assert_eq!(Arch::Aarch64.qemu_system_bin(), "qemu-system-aarch64");
		assert_eq!(Arch::X86_64.qemu_system_bin(), "qemu-system-x86_64");
	}
}
