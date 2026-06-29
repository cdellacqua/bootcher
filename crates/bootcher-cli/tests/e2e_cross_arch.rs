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
use std::time::Duration;

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	ADMIN_KEYS, CrossPrereqs, SCRATCH_BASE, SENTINEL_PATH, Ssh, StoreGuard, VM_USER, keygen,
	read_pub_key,
};

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
		let proj = common::scaffold_project(hp, "e2ecross");
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
		common::set_sentinel(&h.proj, "sentinel-v1");
		h
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
		common::make_overlay(&self.built_disk(), &self.overlay, None);
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

	fn wait_for_ssh(&self, scope: &Scope, what: &str) {
		common::wait_for_ssh(
			VM_USER,
			self.port,
			&self.admin_key,
			TCG_SSH_TIMEOUT,
			scope,
			&self.serial_log,
			"cross",
			what,
		);
	}

	fn built_disk(&self) -> PathBuf {
		common::built_disk(&self.proj)
	}

	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		// No agent socket: cross-arch provision is LAN mode but bootcher's ssh isn't
		// exercised here (provision only builds), and the e2e uses raw `-i key` ssh.
		let mut c = common::bootcher_cmd(&self.proj, self.home.path(), STORE_DIR, None);
		c.args(args)
			// Persist the builder VM's cloud-image download + prepared overlay across
			// runs (the builder reads `XDG_CACHE_HOME`), out of the throwaway $HOME.
			.env("XDG_CACHE_HOME", CACHE_DIR);
		c
	}

	/// An ssh view to the guest with the admin key (no agent — only `-i admin`).
	fn ssh(&self) -> Ssh<'_> {
		Ssh {
			home: self.home.path(),
			user: VM_USER,
			port: self.port,
			key: &self.admin_key,
			agent_sock: None,
		}
	}

	fn ssh_out(&self, cmd: &str) -> Option<String> {
		self.ssh().out(cmd)
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
