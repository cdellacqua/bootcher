//! End-to-end VM test: build a *real* bootc disk from `bootcher provision`, boot it
//! under qemu/KVM, and exercise the live, ssh-driven operations against it — the
//! one pass that proves the whole chain (image-builder build → bootable disk → provisioned
//! admin key → on-device credential rotation → upgrade) rather than any single
//! seam. Slow and heavy by nature, so it is **opt-in**:
//!
//! - gated by Cargo feature e2e (plain `cargo test` skips it, `cargo test --features=e2e` runs it)
//! - it probes its prerequisites (podman, qemu, KVM, UEFI firmware, and
//!   passwordless `sudo sh` — bootcher's privileged image-builder step runs `sudo sh`.
//!
//! Scope: the **LAN** lifecycle, which is self-contained — `bootcher deploy`
//! ships the image to the device over the same ssh channel (an `-R`-tunnelled
//! mini-registry), so no external registry is needed. It covers a provisioned
//! admin key + a sentinel baked into the image, then `rotate key` (the happy path
//! *and* the lockout-safe rollback path), then an upgrade round-trip. Registry-mode
//! `rotate pull-token` (which needs a registry the guest can pull from) lives in
//! its own harness, `e2e_registry.rs` — not covered here.
//!
//! The connection model is all native, no config file: the manifest remote is the
//! `ssh://admin@127.0.0.1:<port>` URL (the forwarded loopback port rides in the URL,
//! no `-p` flag), with per-remote `ssh_opts` pinning `UserKnownHostsFile=/dev/null`
//! so the VM's fresh, throwaway host key (auto-accepted by bootcher's `accept-new`
//! base) isn't written anywhere. The key(s) ride an **ssh-agent** —
//! deliberately *not* a config `IdentityFile` — so that `rotate key`'s validation
//! probe (which disables the agent and offers only the new key) is cleanly isolated
//! and a wrong new key can't be "validated" by the old key.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	ADMIN_KEYS, Agent, Prereqs, SCRATCH_BASE, SSH_TIMEOUT, StoreGuard, VM_USER, find_file, keygen,
	pub_key_path, read_pub_key,
};

/// On-device path of the image-baked sentinel. Lives under `/usr` (atomically
/// swapped on upgrade) so the upgrade round-trip sees the new value deterministically.
const SENTINEL_PATH: &str = "/usr/lib/bootcher-e2e-sentinel";

/// Persistent podman store for the e2e, pointed at via `XDG_DATA_HOME` — out of the
/// user's real store so the test's images don't pollute it. Wiped on teardown by
/// default (see [`StoreGuard`]); set `BOOTCHER_E2E_KEEP_STORE` to keep it across
/// runs so the base image is pulled only once.
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn lan_lifecycle_build_boot_rotate_upgrade() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision("sentinel-v1");

	// Boot a writable overlay on the built disk so the original output stays clean.
	let _vm = h.boot(&scope);
	h.wait_for_ssh(&h.old_key, &scope, "VM never answered ssh as admin");

	phase_provisioned(&h);
	phase_rotate_key_happy(&h);
	phase_rotate_key_rollback(&h);
	phase_upgrade_round_trip(&h, &scope);

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

/// `rotate key` happy path: the new key logs in, the old key is retired.
fn phase_rotate_key_happy(h: &Harness) {
	eprintln!("e2e/phase: rotate key (happy)");
	h.bootcher(&["rotate", "ssh-key", "--path", h.new_key.to_str().unwrap()]).assert().success();

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
	h.bootcher(&["rotate", "ssh-key", "--path", h.decoy.to_str().unwrap()]).assert().failure();

	// The device is untouched: still on the key it had before the failed rotation.
	assert!(h.reachable_with(&h.new_key), "device must remain reachable on the prior key");
	let keys = h.ssh_out(&h.new_key, &format!("cat {ADMIN_KEYS}")).expect("read admin keys");
	assert!(keys.contains(&h.new_pub()), "prior key must survive a rolled-back rotation");
	assert!(!keys.contains(&h.decoy_pub_contents()), "decoy key must not be left behind");
}

/// Bump the sentinel, `bootcher deploy` (build the new container + ship it to the
/// device over the LAN ssh tunnel + `bootc switch`), reboot, and confirm the device
/// booted the new revision. `deploy` is build-then-ship; `upgrade` alone assumes an
/// already-built container, like `image` does.
fn phase_upgrade_round_trip(h: &Harness, scope: &Scope) {
	eprintln!("e2e/phase: upgrade round-trip");
	h.set_sentinel("sentinel-v2");

	h.bootcher(&["deploy"]).assert().success();

	// `bootc` applies the staged image on the next boot.
	let _ = h.ssh_out(&h.new_key, "sudo systemctl reboot"); // connection drops; ignore
	h.wait_for_ssh(&h.new_key, scope, "VM never came back after upgrade reboot");

	let sentinel = h.ssh_out(&h.new_key, &format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the upgrade");
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
		// On disk-backed /var/tmp, not tmpfs /tmp (a bootc disk image overflows it).
		let home = tempfile::Builder::new()
			.prefix("bootcher-e2e-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
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

		// Throwaway project scaffolded with `bootcher init -y`, then pointed at the VM.
		let proj = hp.join("e2e");
		AssertCommand::cargo_bin("bootcher")
			.unwrap()
			.args(["init", "-y", "e2e"])
			.current_dir(hp)
			.env("HOME", hp)
			.assert()
			.success();

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
		h.set_sentinel("sentinel-v1");
		h
	}

	/// Write the image-baked sentinel into the project's sysroot overlay.
	fn set_sentinel(&self, value: &str) {
		let p = self.proj.join("sysroot/usr/lib/bootcher-e2e-sentinel");
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, format!("{value}\n")).unwrap();
	}

	/// `bootcher provision --key <old>` → builds the container *and* its bootc
	/// disk (with the admin key + sentinel baked in). `image` alone only runs image-builder on
	/// an already-built container; `provision` is the build-then-image front door.
	/// `expect` names the sentinel value for log readability.
	fn provision(&self, expect: &str) {
		eprintln!("e2e: provisioning disk ({expect}) — this is the slow part");
		self.bootcher(&["provision", "--ssh-key", self.old_key.to_str().unwrap()])
			.assert()
			.success();
	}

	/// Boot a writable overlay on the freshly built disk and return the running VM.
	fn boot(&self, scope: &Scope) -> Vm {
		let disk = self.built_disk();
		// CoW overlay so boots don't mutate the image-builder output and a re-boot starts clean.
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

	/// Copy the guest's serial log somewhere that survives the temp dir's cleanup
	/// and print its tail — the only window into a boot that never answered ssh.
	fn dump_serial(&self) {
		let dest = PathBuf::from(SCRATCH_BASE).join("bootcher-e2e-serial.log");
		let _ = std::fs::copy(&self.serial_log, &dest);
		eprintln!("--- guest serial log (saved to {}) ---", dest.display());
		if let Ok(s) = std::fs::read_to_string(&self.serial_log) {
			let lines: Vec<&str> = s.lines().collect();
			for line in lines.iter().rev().take(50).rev() {
				eprintln!("{line}");
			}
		}
	}

	/// Wait for the guest to answer ssh as `key`; on timeout, dump the serial log
	/// (which the temp dir would otherwise take with it) and panic with `what`.
	fn wait_for_ssh(&self, key: &Path, scope: &Scope, what: &str) {
		if let Err(e) = qemu::ssh(VM_USER, self.port, key).wait_until_reachable(SSH_TIMEOUT, scope)
		{
			self.dump_serial();
			panic!("{what}: {e:#}");
		}
	}

	/// Locate the image-builder-produced qcow2 under `output/`.
	fn built_disk(&self) -> PathBuf {
		let output = self.proj.join("output");
		find_file(&output, "disk.qcow2")
			.unwrap_or_else(|| panic!("no disk.qcow2 under {}", output.display()))
	}

	/// A `bootcher` command rooted at the project, with the throwaway `$HOME` and the
	/// agent socket — so its ambient ssh resolves the alias + keys.
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
		c.args(args)
			.current_dir(&self.proj)
			.env("HOME", self.home.path())
			.env("SSH_AUTH_SOCK", self.agent.sock())
			// Point rootless podman at the e2e's own persistent store (out of /tmp and
			// out of the user's real store), so the base is cached across runs.
			.env("XDG_DATA_HOME", STORE_DIR);
		c
	}

	/// A raw `ssh` to the guest authenticating with exactly `key` (the throwaway
	/// `$HOME` and no agent, so only `-i key` is offered — the basis for the "this
	/// key works / is rejected" assertions).
	fn ssh(&self, key: &Path, cmd: &str) -> Command {
		let argv = qemu::ssh(VM_USER, self.port, key).argv(&[], cmd);
		let (program, rest) = argv.split_first().unwrap();
		let mut c = Command::new(program);
		c.args(rest).env("HOME", self.home.path()).env_remove("SSH_AUTH_SOCK");
		c
	}

	/// `true` iff a fresh login with `key` runs `cmd` to a zero exit (single shot).
	fn ssh_ok(&self, key: &Path, cmd: &str) -> bool {
		self.ssh(key, cmd)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.is_ok_and(|s| s.success())
	}

	/// `true` iff a login with `key` succeeds within a few seconds. The single-shot
	/// connection can transiently fail at the *transport* layer right after a
	/// rotation's burst of connections — `Connection timed out during banner
	/// exchange`, the VM's sshd answering slowly over qemu's user-net (SLIRP), with
	/// no bearing on auth (the key is already proven) — so the "should be reachable"
	/// assertions retry through it, as bootcher's own `reachable`/`validate_login` do.
	fn reachable_with(&self, key: &Path) -> bool {
		(0..20).any(|i| {
			if i > 0 {
				std::thread::sleep(Duration::from_millis(500));
			}
			self.ssh_ok(key, "true")
		})
	}

	/// Capture stdout of `cmd` over ssh with `key`, retrying through the same
	/// transport blip [`reachable_with`] rides. Used for reads we expect to succeed.
	fn ssh_out(&self, key: &Path, cmd: &str) -> Option<String> {
		for i in 0..20 {
			if i > 0 {
				std::thread::sleep(Duration::from_millis(500));
			}
			if let Ok(out) = self.ssh(key, cmd).output()
				&& out.status.success()
			{
				return Some(String::from_utf8_lossy(&out.stdout).into_owned());
			}
		}
		None
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
