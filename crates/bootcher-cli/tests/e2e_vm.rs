//! End-to-end VM test of the **direct-only** lifecycle (no registry): build a *real*
//! bootc disk with `bootcher device provision`, boot it under qemu/KVM, and exercise
//! the live, ssh-driven operations against it — the one pass that proves the whole
//! chain (image-builder build → bootable disk → provisioned admin key → `device
//! status` → on-device credential rotation → `device deploy` → `device refresh`)
//! rather than any single seam. Slow and heavy by nature, so it is **opt-in**:
//!
//! - gated by Cargo feature e2e (plain `cargo test` skips it, `cargo test --features=e2e` runs it)
//! - it probes its prerequisites (podman, qemu, KVM, UEFI firmware, and
//!   passwordless `sudo sh` — bootcher's privileged image-builder step runs `sudo sh`.
//!
//! Without a `[registry]` every device is direct: `device deploy` ships the image to
//! the device over the ssh channel (an `-R`-tunnelled mini-registry), so no external
//! registry is needed. It covers a provisioned admin key + a sentinel baked into the
//! image, the device's class, `device rotate ssh-key` (the happy path *and* the
//! lockout-safe rollback path), a deploy round-trip, and `device refresh` — a clean
//! deploy is rebuilt from its own commit and shipped back, while a dirty one is
//! skipped with a hint.
//!
//! The connection model is all native, no config file: the manifest remote is the
//! `ssh://admin@127.0.0.1:<port>` URL (the forwarded loopback port rides in the URL,
//! no `-p` flag), with per-remote `ssh_opts` pinning `UserKnownHostsFile=/dev/null`
//! so the VM's fresh, throwaway host key (auto-accepted by bootcher's `accept-new`
//! base) isn't written anywhere. The key(s) ride an **ssh-agent** —
//! deliberately *not* a config `IdentityFile` — so that the rotation's validation
//! probe (which disables the agent and offers only the new key) is cleanly isolated
//! and a wrong new key can't be "validated" by the old key.

use std::path::{Path, PathBuf};

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	ADMIN_KEYS, Agent, Prereqs, SENTINEL_PATH, SSH_TIMEOUT, Ssh, StoreGuard, VM_USER, keygen,
	pub_key_path, read_pub_key,
};

/// Persistent podman store for the e2e, pointed at via `XDG_DATA_HOME` — out of the
/// user's real store so the test's images don't pollute it. Wiped on teardown by
/// default (see [`StoreGuard`]); set `BOOTCHER_E2E_KEEP_STORE` to keep it across
/// runs so the base image is pulled only once.
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn direct_lifecycle_build_boot_rotate_deploy_refresh() {
	let env = Prereqs::probe_disk_build()
		.expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision("sentinel-v1");

	// Boot a writable overlay on the built disk so the original output stays clean.
	let _vm = h.boot(&scope);
	h.wait_for_ssh(&h.old_key, &scope, "VM never answered ssh as admin");

	phase_provisioned(&h);
	phase_status(&h);
	phase_rotate_key_happy(&h);
	phase_rotate_key_rollback(&h);
	phase_upgrade_round_trip(&h, &scope);
	phase_refresh(&h);

	eprintln!("e2e: all phases passed");
}

// ----------------------------------------------------------------- phases

/// The disk booted with the provisioned admin key and the baked sentinel.
fn phase_provisioned(h: &Harness) {
	eprintln!("e2e/phase: provisioned key + sentinel");
	let sentinel = h.ssh_out(&h.old_key, &format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v1", "baked sentinel mismatch");

	let keys = h.ssh_out(&h.old_key, &format!("cat {ADMIN_KEYS}")).expect("read admin keys");
	assert!(keys.contains(&h.old_pub()), "provisioned admin key not on device");
}

/// `device status` classifies the freshly provisioned device as direct.
fn phase_status(h: &Harness) {
	eprintln!("e2e/phase: device status");
	h.bootcher(&["device", "status", "--format", "{{ .Class }}"])
		.assert()
		.success()
		.stdout(predicates::str::contains("direct"));
}

/// `device rotate ssh-key` happy path: the new key logs in, the old key is retired.
fn phase_rotate_key_happy(h: &Harness) {
	eprintln!("e2e/phase: rotate key (happy)");
	h.bootcher(&["device", "rotate", "ssh-key", "--ssh-key", h.new_key.to_str().unwrap(), "-y"])
		.assert()
		.success();

	// New key authenticates; old key is rejected; the file holds only the new key.
	assert!(h.reachable_with(&h.new_key), "new key should log in after rotation");
	assert!(!h.ssh_ok(&h.old_key, "true"), "old key should be rejected after rotation");
	let keys = h.ssh_out(&h.new_key, &format!("cat {ADMIN_KEYS}")).expect("read admin keys");
	assert!(keys.contains(&h.new_pub()), "new key missing from device");
	assert!(!keys.contains(&h.old_pub()), "old key not retired");

	// The agent now carries the new key too, so bootcher's ambient ssh keeps working.
	h.agent.add(&h.new_key);
}

/// `rotate key` rollback: `decoy.pub` holds a key whose private half doesn't match
/// the sibling `decoy` private key, so the validation probe fails and the rotation
/// aborts — leaving the device on its current (new) key with no lockout.
fn phase_rotate_key_rollback(h: &Harness) {
	eprintln!("e2e/phase: rotate key (rollback)");
	// decoy.pub contains a different key's public half (set up in Harness::setup),
	// so the sibling `decoy` private key won't authenticate → validate fails → rollback.
	h.bootcher(&["device", "rotate", "ssh-key", "--ssh-key", h.decoy.to_str().unwrap(), "-y"])
		.assert()
		.failure();

	// The device is untouched: still on the key it had before the failed rotation.
	assert!(h.reachable_with(&h.new_key), "device must remain reachable on the prior key");
	let keys = h.ssh_out(&h.new_key, &format!("cat {ADMIN_KEYS}")).expect("read admin keys");
	assert!(keys.contains(&h.new_pub()), "prior key must survive a rolled-back rotation");
	assert!(!keys.contains(&h.decoy_pub_contents()), "decoy key must not be left behind");
}

/// Bump the sentinel (uncommitted: deploy is the inner loop), `bootcher device
/// deploy` (build + ship over the ssh tunnel + `bootc switch` + reboot), and confirm
/// the device booted the new revision.
fn phase_upgrade_round_trip(h: &Harness, scope: &Scope) {
	eprintln!("e2e/phase: deploy round-trip");
	h.set_sentinel("sentinel-v2");

	h.bootcher(&["device", "deploy", "-y"]).assert().success();
	h.wait_for_ssh(&h.new_key, scope, "VM never came back after the deploy reboot");

	let sentinel = h.ssh_out(&h.new_key, &format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the upgrade");
}

/// `device refresh`: a deploy from a clean commit records it, so a forced refresh
/// rebuilds that commit (here unchanged content, rebuilt `--no-cache`) and ships it
/// back — even with HEAD moved on and the tree dirty, since nothing is built from it.
/// A device deployed from a dirty tree records no commit and is skipped with a hint.
fn phase_refresh(h: &Harness) {
	eprintln!("e2e/phase: device refresh");
	h.set_sentinel("sentinel-v3");
	h.commit("v3");
	h.bootcher(&["device", "deploy", "-y"]).assert().success();
	assert_eq!(
		h.ssh_out(&h.new_key, &format!("cat {SENTINEL_PATH}")).unwrap().trim(),
		"sentinel-v3"
	);
	let before = h.ssh_out(&h.new_key, "sudo bootc status --format json").unwrap();

	// Unreleased work in the checkout: not shipped by a refresh.
	h.set_sentinel("sentinel-unreleased");
	h.bootcher(&["device", "refresh", "--force", "-y"]).assert().success();
	assert_eq!(
		h.ssh_out(&h.new_key, &format!("cat {SENTINEL_PATH}")).unwrap().trim(),
		"sentinel-v3",
		"a refresh rebuilds the device's own commit, never the checkout"
	);
	let after = h.ssh_out(&h.new_key, "sudo bootc status --format json").unwrap();
	assert_ne!(before, after, "the forced refresh should have deployed a rebuilt image");

	// A dirty deploy records no source: refresh skips it, pointing at `device deploy`.
	h.bootcher(&["device", "deploy", "-y"]).assert().success();
	h.bootcher(&["device", "refresh", "--force", "-y"])
		.assert()
		.success()
		.stderr(predicates::str::contains("re-run `bootcher device deploy`"));
}

// ----------------------------------------------------------------- harness

/// Everything a run needs: the throwaway project + `$HOME`, the keypairs, the
/// ssh-agent, and the forwarded port.
struct Harness {
	arch: Arch,
	firmware: PathBuf,
	home: tempfile::TempDir,
	proj: PathBuf,
	port: u16,
	agent: Agent,
	old_key: PathBuf,
	new_key: PathBuf,
	decoy: PathBuf,
	overlay: PathBuf,
	serial_log: PathBuf,
	// RAII teardown for the persistent podman store (wiped unless opted out).
	_store: StoreGuard,
}

impl Harness {
	fn setup(env: &Prereqs) -> Self {
		let home = common::run_dir("bootcher-e2e-");
		let store = StoreGuard::new(STORE_DIR);
		let hp = home.path();

		// Keypairs: old (provisioned), new (rotation target), decoy (rollback).
		// decoy.pub is intentionally overwritten with a different key's public half so
		// that `rotate key --key decoy` reads the mismatched decoy.pub, stages the
		// wrong key, and the decoy private key fails validation → rollback without lockout.
		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let old_key = keygen(&keydir, "old");
		let new_key = keygen(&keydir, "new");
		let decoy = keygen(&keydir, "decoy");
		let aux = keygen(&keydir, "aux");
		std::fs::copy(pub_key_path(&aux), pub_key_path(&decoy)).unwrap();

		// ssh-agent with the provisioned key loaded; bootcher's ambient ssh uses it.
		let agent = Agent::start(hp);
		agent.add(&old_key);

		// Throwaway project scaffolded with `bootcher init -y` in a git repo (a refresh
		// rebuilds a recorded commit), then pointed at the VM.
		let proj = common::scaffold_project(hp, "e2e");
		for args in [
			&["init", "-q", "-b", "main"][..],
			&["config", "user.email", "e2e@example.com"],
			&["config", "user.name", "e2e"],
			&["config", "commit.gpgsign", "false"],
		] {
			duct::cmd("git", args).dir(&proj).run().expect("git setup");
		}

		// Native, no config file and no PATH shim: the remote is an `ssh://` URL that
		// carries the forwarded port, and the object form's `ssh_opts` pins known-hosts
		// to `/dev/null` so the VM's fresh, throwaway host key isn't written anywhere.
		let port = qemu::free_port().expect("free port");
		let manifest_path = proj.join("bootcher.toml");
		let manifest = std::fs::read_to_string(&manifest_path).unwrap();
		std::fs::write(
			&manifest_path,
			manifest.replace(
				"remotes = []",
				&format!(
					"remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{port}\", \
					 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]"
				),
			),
		)
		.unwrap();

		let h = Self {
			arch: env.arch,
			firmware: env.firmware.clone(),
			proj,
			port,
			agent,
			old_key,
			new_key: new_key.clone(),
			decoy: decoy.clone(),
			overlay: hp.join("disk-overlay.qcow2"),
			serial_log: hp.join("serial.log"),
			home,
			_store: store,
		};
		common::set_sentinel(&h.proj, "sentinel-v1");
		h
	}

	/// Commit everything in the project.
	fn commit(&self, msg: &str) {
		duct::cmd!("git", "add", "-A").dir(&self.proj).run().expect("git add");
		duct::cmd!("git", "commit", "-qm", msg).dir(&self.proj).run().expect("git commit");
	}

	/// Write the image-baked sentinel into the project's sysroot overlay.
	fn set_sentinel(&self, value: &str) {
		common::set_sentinel(&self.proj, value);
	}

	/// `bootcher device provision --ssh-key <old>` → builds the container *and* its
	/// bootc disk (with the admin key + sentinel baked in); `--direct` is implied
	/// without a registry. `expect` names the sentinel value for log readability.
	fn provision(&self, expect: &str) {
		eprintln!("e2e: provisioning disk ({expect}) — this is the slow part");
		self.bootcher(&["device", "provision", "--ssh-key", self.old_key.to_str().unwrap()])
			.assert()
			.success();
	}

	/// Boot a writable overlay on the freshly built disk and return the running VM.
	fn boot(&self, scope: &Scope) -> Vm {
		// CoW overlay so boots don't mutate the image-builder output and a re-boot starts clean.
		common::make_overlay(&self.built_disk(), &self.overlay, None);
		Vm::spawn(
			&VmConfig {
				arch: self.arch,
				disk: &self.overlay,
				seed: None,
				firmware: Some(&self.firmware),
				port: self.port,
				log: &self.serial_log,
				accel: "kvm",
				mem_mib: "2048",
				smp: "2",
			},
			scope,
		)
		.expect("spawning qemu")
	}

	/// Wait for the guest to answer ssh as `key`; on timeout, dump the serial log
	/// (which the temp dir would otherwise take with it) and panic with `what`.
	fn wait_for_ssh(&self, key: &Path, scope: &Scope, what: &str) {
		common::wait_for_ssh(
			VM_USER,
			self.port,
			key,
			SSH_TIMEOUT,
			scope,
			&self.serial_log,
			"vm",
			what,
		);
	}

	/// Locate the image-builder-produced qcow2 under `output/`.
	fn built_disk(&self) -> PathBuf {
		common::built_disk(&self.proj)
	}

	/// A `bootcher` command rooted at the project, with the throwaway `$HOME` and the
	/// agent socket — so its ambient ssh resolves the alias + keys.
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c =
			common::bootcher_cmd(&self.proj, self.home.path(), STORE_DIR, Some(self.agent.sock()));
		c.args(args);
		c
	}

	/// An ssh view authenticating with exactly `key` (throwaway `$HOME`, no agent — so
	/// only `-i key` is offered, the basis for the "this key works / is rejected" assertions).
	fn ssh<'a>(&'a self, key: &'a Path) -> Ssh<'a> {
		Ssh { home: self.home.path(), user: VM_USER, port: self.port, key, agent_sock: None }
	}

	/// `true` iff a fresh login with `key` runs `cmd` to a zero exit (single shot).
	fn ssh_ok(&self, key: &Path, cmd: &str) -> bool {
		self.ssh(key).ok(cmd)
	}

	/// `true` iff a login with `key` succeeds within a few seconds (retrying through a
	/// transient post-rotation transport blip — see [`Ssh::reachable`]).
	fn reachable_with(&self, key: &Path) -> bool {
		self.ssh(key).reachable()
	}

	/// Capture stdout of `cmd` over ssh with `key`, retrying through the same
	/// transport blip [`reachable_with`] rides. Used for reads we expect to succeed.
	fn ssh_out(&self, key: &Path, cmd: &str) -> Option<String> {
		self.ssh(key).out(cmd)
	}

	fn old_pub(&self) -> String {
		read_pub_key(&self.old_key)
	}
	fn new_pub(&self) -> String {
		read_pub_key(&self.new_key)
	}
	fn decoy_pub_contents(&self) -> String {
		std::fs::read_to_string(pub_key_path(&self.decoy))
			.unwrap()
			.split_whitespace()
			.nth(1)
			.unwrap()
			.to_owned()
	}
}
