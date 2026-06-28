//! End-to-end test for the **cross-arch VM builder**: provision a *foreign-arch*
//! bootc disk on this host — the container built locally under qemu-user, the
//! image-builder step run inside a foreign Fedora Cloud builder VM under TCG
//! (`[builder] image = "vm"`, the emulation-safe path) — then boot the produced
//! foreign disk under TCG and prove it comes up: the admin key and sentinel baked
//! in are present over ssh. That is the whole point of the cross-arch builder —
//! that an `x86_64` workstation can produce a working `aarch64` image (or vice
//! versa) — and nothing exercised it end to end before.
//!
//! This is **much** slower than the other e2e tests (every foreign instruction is
//! emulated: the builder VM boots under TCG, image-builder runs there, *and* the produced
//! disk boots under TCG), and it has different prerequisites (foreign
//! `qemu-system`, foreign UEFI firmware, a qemu-user binfmt handler, network for
//! the builder's one-time image pulls — but no KVM and no host `sudo`). So it's
//! gated behind its **own** feature, `e2e_cross`, separate from `e2e`:
//!
//! - plain `cargo test` and `--features e2e` both skip it (`#[ignore]`),
//! - `--features e2e_cross` un-ignores it (`just e2e-cross`).
//!
//! Scope is deliberately just provision → boot → verify: a LAN `deploy` round-trip
//! would mean a *second* cross-arch build + a foreign pull and roughly double an
//! already-very-long run, for little added signal over "the cross-built disk boots
//! and carries its provisioned secrets". The build/deploy round-trip is covered
//! natively by `e2e_vm.rs`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	ADMIN_KEYS, CrossPrereqs, SCRATCH_BASE, StoreGuard, VM_USER, find_file, keygen, read_pub_key,
};

/// On-device path of the image-baked sentinel (under `/usr`, like the LAN e2e).
const SENTINEL_PATH: &str = "/usr/lib/bootcher-e2e-sentinel";
/// Podman store for this e2e (own dir, out of the user's real store). Wiped on
/// teardown by default ([`StoreGuard`]); set `BOOTCHER_E2E_KEEP_STORE` to keep it.
/// (The separate `CACHE_DIR` download cache below is always kept.)
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-cross-store";
/// Persistent bootcher cache (the builder VM's downloaded cloud image + warm
/// prepared overlay). Kept across runs — outside the per-run temp `$HOME` and the
/// user's real cache — so the multi-hundred-MB download + one-time cloud-init
/// prepare happen once, not every run.
const CACHE_DIR: &str = "/var/tmp/bootcher-e2e-cross-cache";
/// Generous boot budget — a foreign-arch bootc disk's *first* boot runs entirely
/// under TCG, so it's far slower than the KVM boots the other e2e tests wait on.
const TCG_SSH_TIMEOUT: Duration = Duration::from_hours(2);

#[test]
#[cfg_attr(
	not(feature = "e2e_cross"),
	ignore = "very slow cross-arch e2e; opt in with --feature=e2e_cross"
)]
fn cross_arch_builder_produces_a_bootable_disk() {
	let env =
		CrossPrereqs::probe().expect("prerequisites not satisfied by the current environment");
	eprintln!("e2e/cross: building a {} disk on a {} host (all emulated)", env.cross, env.host);
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision("sentinel-v1");

	let _vm = h.boot(&scope);
	h.wait_for_ssh(&scope, "cross-arch VM never answered ssh as admin");

	// The cross-built disk booted and carries what provision injected.
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v1", "baked sentinel mismatch on the cross-built disk");

	let keys = h.ssh_out(&format!("cat {ADMIN_KEYS}")).expect("read admin keys");
	assert!(keys.contains(&h.admin_pub()), "provisioned admin key not on the cross-built device");

	// And the guest really is the foreign arch — proof image-builder built for the target,
	// not the host.
	let machine = h.ssh_out("uname -m").expect("read uname -m").trim().to_owned();
	assert_eq!(
		Arch::from_uname(&machine),
		Some(h.cross),
		"booted guest arch {machine:?} isn't the cross target {}",
		h.cross
	);

	eprintln!("e2e/cross: passed");
}

// ----------------------------------------------------------------- harness

struct Harness {
	cross: Arch,
	cross_firmware: PathBuf,
	home: tempfile::TempDir,
	proj: PathBuf,
	port: u16,
	admin_key: PathBuf,
	overlay: PathBuf,
	serial_log: PathBuf,
	// RAII teardown for the persistent podman store (wiped unless opted out).
	_store: StoreGuard,
}

impl Harness {
	fn setup(env: &CrossPrereqs) -> Self {
		let home = tempfile::Builder::new()
			.prefix("bootcher-e2e-cross-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
		let store = StoreGuard::new(STORE_DIR);
		std::fs::create_dir_all(CACHE_DIR).expect("create builder cache dir");
		let hp = home.path();

		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let admin_key = keygen(&keydir, "admin");

		let port = qemu::free_port().expect("free port");
		let proj = hp.join("e2ecross");
		AssertCommand::cargo_bin("bootcher")
			.unwrap()
			.args(["init", "-y", "e2ecross"])
			.current_dir(hp)
			.env("HOME", hp)
			.assert()
			.success();
		std::fs::write(proj.join("bootcher.toml"), manifest(env.cross, port)).unwrap();

		let h = Self {
			cross: env.cross,
			cross_firmware: env.cross_firmware.clone(),
			proj,
			port,
			admin_key,
			overlay: hp.join("disk-overlay.qcow2"),
			serial_log: hp.join("serial.log"),
			home,
			_store: store,
		};
		h.set_sentinel("sentinel-v1");
		h
	}

	fn set_sentinel(&self, value: &str) {
		let p = self.proj.join("sysroot/usr/lib/bootcher-e2e-sentinel");
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, format!("{value}\n")).unwrap();
	}

	/// `bootcher provision` for the cross arch: the container builds locally under
	/// qemu-user, the disk image builds inside the foreign builder VM (`image =
	/// "vm"`). LAN mode, so only the admin key is collected.
	fn provision(&self, expect: &str) {
		eprintln!(
			"e2e/cross: provisioning ({expect}) — boots a foreign builder VM under TCG, very slow"
		);
		self.bootcher(&["provision", "--ssh-key", self.admin_key.to_str().unwrap()])
			.assert()
			.success();
	}

	fn boot(&self, scope: &Scope) -> Vm {
		let disk = self.built_disk();
		duct::cmd!(
			"qemu-img",
			"create",
			"-q",
			"-f",
			"qcow2",
			"-F",
			"qcow2",
			"-b",
			&disk,
			&self.overlay
		)
		.run()
		.expect("creating boot overlay");
		Vm::spawn(
			&VmConfig {
				arch: self.cross,
				disk: &self.overlay,
				seed: None,
				firmware: Some(&self.cross_firmware),
				port: self.port,
				log: &self.serial_log,
				// Foreign arch ⇒ no KVM; full system emulation.
				accel: "tcg,thread=multi",
				mem_mib: "2048",
				smp: "2",
			},
			scope,
		)
		.expect("spawning qemu")
	}

	fn dump_serial(&self) {
		let dest = PathBuf::from(SCRATCH_BASE).join("bootcher-e2e-cross-serial.log");
		let _ = std::fs::copy(&self.serial_log, &dest);
		eprintln!("--- guest serial log (saved to {}) ---", dest.display());
		if let Ok(s) = std::fs::read_to_string(&self.serial_log) {
			for line in s.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
				eprintln!("{line}");
			}
		}
	}

	fn wait_for_ssh(&self, scope: &Scope, what: &str) {
		if let Err(e) = qemu::ssh(VM_USER, self.port, &self.admin_key)
			.wait_until_reachable(TCG_SSH_TIMEOUT, scope)
		{
			self.dump_serial();
			panic!("{what}: {e:#}");
		}
	}

	fn built_disk(&self) -> PathBuf {
		let output = self.proj.join("output");
		find_file(&output, "disk.qcow2")
			.unwrap_or_else(|| panic!("no disk.qcow2 under {}", output.display()))
	}

	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
		c.args(args)
			.current_dir(&self.proj)
			.env("HOME", self.home.path())
			.env("XDG_DATA_HOME", STORE_DIR)
			// Persist the builder VM's cloud-image download + prepared overlay across
			// runs (the builder reads `XDG_CACHE_HOME`), out of the throwaway $HOME.
			.env("XDG_CACHE_HOME", CACHE_DIR);
		c
	}

	fn ssh_capture(&self, cmd: &str) -> std::process::Output {
		let argv = qemu::ssh(VM_USER, self.port, &self.admin_key).argv(&[], cmd);
		let (program, rest) = argv.split_first().unwrap();
		Command::new(program)
			.args(rest)
			.env("HOME", self.home.path())
			.env_remove("SSH_AUTH_SOCK")
			.stdin(Stdio::null())
			.output()
			.expect("ssh")
	}

	fn ssh_out(&self, cmd: &str) -> Option<String> {
		for i in 0..20 {
			if i > 0 {
				std::thread::sleep(Duration::from_millis(500));
			}
			let out = self.ssh_capture(cmd);
			if out.status.success() {
				return Some(String::from_utf8_lossy(&out.stdout).into_owned());
			}
		}
		None
	}

	fn admin_pub(&self) -> String {
		read_pub_key(&self.admin_key)
	}
}

// ----------------------------------------------------------------- fixtures

/// A cross-arch LAN `bootcher.toml`: the foreign target arch, the container built
/// locally (qemu-user) but the image-builder step routed to a throwaway builder `vm` (the
/// recommended cross-arch config), and a LAN remote at the guest's forwarded port.
fn manifest(cross: Arch, ssh_port: u16) -> String {
	format!(
		"[general]\n\
		 name = \"e2ecross\"\n\
		 \n\
		 [targets]\n\
		 {cross} = \"qcow2\"\n\
		 \n\
		 [builder]\n\
		 build = \"local\"\n\
		 image = \"vm\"\n\
		 \n\
		 [deploy]\n\
		 remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{ssh_port}\", \
		 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]\n"
	)
}
