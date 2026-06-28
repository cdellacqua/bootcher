//! End-to-end VM test for **registry-mode image signing**: build a *signing-
//! enforcing* bootc disk, boot it, prove a correctly-signed `deploy` upgrades it,
//! and prove a **tampered/unsigned** push to the same ref is **rejected** on the
//! device's `bootc upgrade`. This is the one pass that proves the whole signing
//! chain end to end on a real device — enroll → signed push → on-device policy
//! enforcement — rather than any single seam. Slow and heavy, so **opt-in**:
//!
//! - gated by the `e2e` feature (plain `cargo test` skips it)
//! - probes its prerequisites (podman, qemu, KVM, UEFI firmware, passwordless
//!   `sudo`), via [`common::Prereqs`].
//!
//! ## Registry reachability
//!
//! Signing needs a *real* registry both the build host and the guest can pull a
//! signed image (with its sigstore attachment) from — and crucially under the
//! **same** reference string, so the cosign signature's `matchRepository` identity
//! holds. We run a throwaway `registry:2` published on a free host port and address
//! it by the **host's own primary IP** (`ip route get`): the build host reaches it
//! locally, and the guest reaches the very same `<ip>:<port>` via qemu user-net's
//! NAT. The registry is plain HTTP, so both sides are told it's insecure
//! (a `registries.conf` drop-in baked into the image for the guest, and one in the
//! test's throwaway `$HOME` for the build host).
//!
//! The registry container is torn down on drop.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	Agent, Prereqs, RegistryGuard, SCRATCH_BASE, SSH_TIMEOUT, StoreGuard, VM_USER, find_file,
	host_primary_ip, insecure_registries_conf, keygen,
};

/// On-device sentinel, under `/usr` so an upgrade swaps it atomically.
const SENTINEL_PATH: &str = "/usr/lib/bootcher-e2e-sentinel";
/// Podman store for this e2e (own dir, out of the user's real store). Wiped on
/// teardown by default ([`StoreGuard`]); set `BOOTCHER_E2E_KEEP_STORE` to keep it.
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-sign-store";
/// The throwaway registry container name.
const REG_NAME: &str = "bootcher-e2e-sign-reg";
/// Signing passphrase used throughout (env-driven, no TTY).
const SIGN_PASS: &str = "e2e-signing-pass";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn registry_signing_enforced_signed_accepted_unsigned_rejected() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision();

	let _vm = h.boot(&scope);
	h.wait_for_ssh(&scope, "VM never answered ssh as admin");

	phase_enforcement_files_present(&h);
	phase_signed_upgrade_succeeds(&h);
	phase_unsigned_upgrade_rejected(&h);
	phase_rotate_sign_key(&h);

	eprintln!("e2e/sign: all phases passed");
}

// ----------------------------------------------------------------- phases

/// The provisioned disk shipped the signing-enforcement config into `/etc`.
fn phase_enforcement_files_present(h: &Harness) {
	eprintln!("e2e/sign phase: enforcement files present");
	let policy = h.ssh_out("cat /etc/containers/policy.json").expect("read policy.json");
	assert!(policy.contains("sigstoreSigned"), "policy.json missing sigstoreSigned: {policy}");
	assert!(policy.contains("\"reject\""), "policy.json default isn't reject: {policy}");
	let pubkey =
		h.ssh_out(&format!("cat /etc/pki/containers/{}.pub", h.name)).expect("read pubkey");
	assert!(pubkey.contains("PUBLIC KEY"), "cosign public key not on device: {pubkey}");
}

/// A correctly-signed `deploy` builds + signs + pushes the image, the device
/// verifies the signature, upgrades, and reboots into the new revision.
fn phase_signed_upgrade_succeeds(h: &Harness) {
	eprintln!("e2e/sign phase: signed upgrade succeeds");
	h.set_sentinel("sentinel-v2");
	h.bootcher(&["deploy"]).assert().success();

	// `deploy` already rebooted the device and waited for it online; confirm it
	// picked up the signed v2.
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the signed upgrade");

	// The recorded origin enforces signature policy: `bootc status` reports the
	// booted image's `signature: containerPolicy` (ostree-ext's ContainerPolicy —
	// what `--enforce-container-sigpolicy` records), not the unsigned default.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains("signature: containerPolicy"),
		"bootc origin isn't signature-enforcing:\n{status}"
	);
}

/// An unsigned image pushed to the same ref must be rejected on `bootc upgrade` —
/// the device stays on the prior (signed) revision. This is the security property.
fn phase_unsigned_upgrade_rejected(h: &Harness) {
	eprintln!("e2e/sign phase: unsigned upgrade rejected");
	// Build a *different* image (v3) and push it UNSIGNED to the same registry ref,
	// bypassing bootcher's signing push — i.e. a tampered/compromised-registry push.
	h.set_sentinel("sentinel-v3");
	h.bootcher(&["build"]).assert().success();
	h.push_unsigned();

	// The device's `bootc upgrade` pulls from the signed-scheme origin; with no valid
	// signature, policy must reject it.
	let out = h.ssh_capture("sudo bootc upgrade");
	assert!(!out.status.success(), "bootc upgrade unexpectedly accepted an unsigned image");
	let combined =
		format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
	assert!(
		combined.contains("signature")
			|| combined.contains("rejected")
			|| combined.contains("policy"),
		"rejection wasn't signature-related:\n{combined}"
	);

	// And the device is untouched — still booted on the signed v2.
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device must stay on the signed revision");
}

/// `rotate sign-key`: generate a fresh signing keypair, push its trust to the
/// device, then redeploy with the new key and confirm the device upgrades.
fn phase_rotate_sign_key(h: &Harness) {
	eprintln!("e2e/sign phase: sign-key rotation");

	// Generate the new keypair. `bootcher sign enroll cosign2` writes `cosign2.key`
	// and `cosign2.pub` into the project dir (same passphrase via env var).
	h.bootcher(&["sign", "enroll", "cosign2"]).assert().success();

	// Push the new key's trust to the device. The device now only trusts cosign2.
	h.bootcher(&["rotate", "sign-key", "--pubkey", "cosign2.pub"]).assert().success();

	// Verify the on-device key file was replaced with cosign2.pub's content.
	let on_device = h
		.ssh_out(&format!("cat /etc/pki/containers/{}.pub", h.name))
		.expect("read pubkey from device after rotation");
	let expected =
		std::fs::read_to_string(h.proj.join("cosign2.pub")).expect("read cosign2.pub locally");
	assert_eq!(on_device.trim(), expected.trim(), "device pubkey should match cosign2.pub");

	// Switch the project to sign with the new key and redeploy.
	std::fs::write(
		h.proj.join("bootcher.toml"),
		manifest(&h.name, h.arch, &h.ns, h.ssh_port, "cosign2"),
	)
	.expect("update bootcher.toml to cosign2");
	h.set_sentinel("sentinel-v4");
	h.bootcher(&["deploy"]).assert().success();

	let sentinel =
		h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel after key rotation");
	assert_eq!(
		sentinel.trim(),
		"sentinel-v4",
		"device should upgrade under the rotated signing key"
	);
}

// ----------------------------------------------------------------- harness

struct Harness {
	arch: Arch,
	firmware: PathBuf,
	home: tempfile::TempDir,
	proj: PathBuf,
	name: String,
	/// `<host_ip>:<reg_port>/<repo>` — the registry namespace both sides use.
	ns: String,
	/// `<host_ip>:<reg_port>` — the registry endpoint (for the insecure-registry config).
	reg_addr: String,
	ssh_port: u16,
	agent: Agent,
	admin_key: PathBuf,
	overlay: PathBuf,
	serial_log: PathBuf,
	// RAII teardown for the throwaway registry container.
	_reg: RegistryGuard,
	// RAII teardown for the persistent podman store (wiped unless opted out).
	_store: StoreGuard,
}

impl Harness {
	fn setup(env: &Prereqs) -> Self {
		let home = tempfile::Builder::new()
			.prefix("bootcher-e2e-sign-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
		let store = StoreGuard::new(STORE_DIR);
		let hp = home.path();
		let name = "e2esign".to_owned();

		// Admin keypair + agent (bootcher's ambient ssh uses the agent).
		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let admin_key = keygen(&keydir, "admin");
		let agent = Agent::start(hp);
		agent.add(&admin_key);

		let ssh_port = qemu::free_port().expect("free ssh port");
		let reg_port = qemu::free_port().expect("free registry port");
		// Address the registry by the host's own primary IP: the build host reaches it
		// locally and the guest reaches the same `<ip>:<port>` via qemu user-net's NAT,
		// so the cosign `matchRepository` identity matches on both sides — with no
		// privileged loopback alias.
		let reg_addr = format!("{}:{reg_port}", host_primary_ip());
		let ns = format!("{reg_addr}/proj");

		// A throwaway registry on the free port (real podman env → cached registry:2).
		let reg = RegistryGuard::start(REG_NAME, reg_port);

		// Tell the build host's podman the registry is plain HTTP, in the throwaway
		// $HOME (so the user's real config is untouched).
		let conf_dir = hp.join(".config/containers");
		std::fs::create_dir_all(&conf_dir).unwrap();
		std::fs::write(conf_dir.join("registries.conf"), insecure_registries_conf(&reg_addr))
			.unwrap();

		// Scaffold the project, then write a registry+signing manifest over it.
		let proj = hp.join(&name);
		AssertCommand::cargo_bin("bootcher")
			.unwrap()
			.args(["init", "-y", &name])
			.current_dir(hp)
			.env("HOME", hp)
			.assert()
			.success();
		std::fs::write(
			proj.join("bootcher.toml"),
			manifest(&name, env.arch, &ns, ssh_port, "cosign"),
		)
		.unwrap();

		let h = Self {
			arch: env.arch,
			firmware: env.firmware.clone(),
			proj,
			name,
			ns,
			reg_addr,
			ssh_port,
			agent,
			admin_key,
			overlay: hp.join("disk-overlay.qcow2"),
			serial_log: hp.join("serial.log"),
			_reg: reg,
			home,
			_store: store,
		};

		// Enroll the cosign keypair the manifest references. `sign enroll` also bakes
		// the enforce-container-sigpolicy install drop-in into sysroot/ (we trust it to,
		// rather than hand-writing the file). The guest's insecure-registry drop-in is
		// test infra `enroll` knows nothing about, so write that one ourselves.
		h.bootcher(&["sign", "enroll", "cosign"]).assert().success();
		assert!(
			h.proj.join("sysroot/usr/lib/bootc/install/30-bootcher-signing.toml").is_file(),
			"sign enroll should have baked the enforce-container-sigpolicy drop-in",
		);
		h.write_sysroot(
			"etc/containers/registries.conf.d/10-bootcher-e2e.conf",
			&insecure_registries_conf(&h.reg_addr),
		);
		h.set_sentinel("sentinel-v1");
		h
	}

	/// `bootcher provision` in registry mode: a dummy pull credential (the registry
	/// is anonymous) with `--skip-pull-check`, plus the signing passphrase.
	fn provision(&self) {
		eprintln!("e2e/sign: provisioning the signing-enforcing disk — the slow part");
		self.bootcher(&[
			"provision",
			"--ssh-key",
			self.admin_key.to_str().unwrap(),
			"--skip-pull-check",
		])
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
				arch: self.arch,
				disk: &self.overlay,
				seed: None,
				firmware: Some(&self.firmware),
				port: self.ssh_port,
				log: &self.serial_log,
				accel: "kvm",
				mem_mib: "2048",
				smp: "2",
			},
			scope,
		)
		.expect("spawning qemu")
	}

	/// Push the locally-built `localhost/<name>:latest` list to the registry ref
	/// **without** signing — the tamper case. Runs under the throwaway `$HOME`/store
	/// (insecure-registry config + the freshly built image both live there).
	fn push_unsigned(&self) {
		let reg_ref = format!("{}/{}:latest", self.ns, self.name);
		let list = format!("localhost/{}:latest", self.name);
		duct::cmd!("podman", "manifest", "push", "--all", &list, &reg_ref)
			.dir(&self.proj)
			.env("HOME", self.home.path())
			.env("XDG_DATA_HOME", STORE_DIR)
			.run()
			.unwrap_or_else(|_| panic!("unsigned push to {reg_ref} failed"));
	}

	/// Write `content` to `sysroot/<rel>` in the project (created as needed).
	fn write_sysroot(&self, rel: &str, content: &str) {
		let p = self.proj.join("sysroot").join(rel);
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, content).unwrap();
	}

	fn set_sentinel(&self, value: &str) {
		let p = self.proj.join("sysroot/usr/lib/bootcher-e2e-sentinel");
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, format!("{value}\n")).unwrap();
	}

	fn dump_serial(&self) {
		let dest = PathBuf::from(SCRATCH_BASE).join("bootcher-e2e-sign-serial.log");
		let _ = std::fs::copy(&self.serial_log, &dest);
		eprintln!("--- guest serial log (saved to {}) ---", dest.display());
		if let Ok(s) = std::fs::read_to_string(&self.serial_log) {
			for line in s.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
				eprintln!("{line}");
			}
		}
	}

	fn wait_for_ssh(&self, scope: &Scope, what: &str) {
		if let Err(e) = qemu::ssh(VM_USER, self.ssh_port, &self.admin_key)
			.wait_until_reachable(SSH_TIMEOUT, scope)
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

	/// A `bootcher` command rooted at the project, with the throwaway `$HOME`, agent
	/// socket, dedicated store, signing passphrase, and a dummy registry pull
	/// credential (anonymous registry).
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
		c.args(args)
			.current_dir(&self.proj)
			.env("HOME", self.home.path())
			.env("SSH_AUTH_SOCK", self.agent.sock())
			.env("XDG_DATA_HOME", STORE_DIR)
			.env("BOOTCHER_SIGN_PASSPHRASE", SIGN_PASS)
			.env("BOOTCHER_PULL_USER", "anon")
			.env("BOOTCHER_PULL_TOKEN", "anon");
		c
	}

	/// Raw `ssh` to the guest with the admin key (no agent), capturing output.
	fn ssh_capture(&self, cmd: &str) -> std::process::Output {
		let argv = qemu::ssh(VM_USER, self.ssh_port, &self.admin_key).argv(&[], cmd);
		let (program, rest) = argv.split_first().unwrap();
		Command::new(program)
			.args(rest)
			.env("HOME", self.home.path())
			.env_remove("SSH_AUTH_SOCK")
			.stdin(Stdio::null())
			.output()
			.expect("ssh")
	}

	/// Capture stdout of `cmd` over ssh, retrying through a transient transport blip.
	fn ssh_out(&self, cmd: &str) -> Option<String> {
		for i in 0..20 {
			if i > 0 {
				std::thread::sleep(std::time::Duration::from_millis(500));
			}
			let out = self.ssh_capture(cmd);
			if out.status.success() {
				return Some(String::from_utf8_lossy(&out.stdout).into_owned());
			}
		}
		None
	}
}

// ----------------------------------------------------------------- fixtures

/// A registry+signing `bootcher.toml` pinned at the throwaway registry and the
/// guest's forwarded ssh port. `signing_key` is the basename passed to
/// `bootcher sign enroll` (e.g. `"cosign"` → `cosign.key` / `cosign.pub`).
fn manifest(name: &str, arch: Arch, ns: &str, ssh_port: u16, signing_key: &str) -> String {
	format!(
		"[general]\n\
		 name = \"{name}\"\n\
		 \n\
		 [targets]\n\
		 {arch} = \"qcow2\"\n\
		 \n\
		 [builder]\n\
		 build = \"local\"\n\
		 image = \"local\"\n\
		 \n\
		 [deploy]\n\
		 registry = {{ url = \"{ns}\", key = \"{signing_key}.key\" }}\n\
		 remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{ssh_port}\", \
		 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]\n"
	)
}
