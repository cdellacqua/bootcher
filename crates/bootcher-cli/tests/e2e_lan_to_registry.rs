//! End-to-end VM test for the **LAN → registry transition** ("enroll after
//! provision"): provision a device in LAN mode, run a LAN deploy to prove that
//! lifecycle, then *reconfigure the project to registry mode* and `deploy` — and
//! confirm the device's bootc origin moves off the local containers-storage ref
//! onto the registry ref and the device upgrades from the registry. One provisioned
//! disk, booted once, like the other VM e2e tests.
//!
//! This is the transition the docs gesture at but no test exercised: a fleet first
//! shipped over the trusted SSH channel, later pointed at a registry for
//! hands-off auto-updates. If a future change breaks `bootc switch` from a
//! containers-storage origin to a registry one, this is the test that catches it.
//!
//! Slow and heavy, so **opt-in** exactly like the other VM e2e tests (gated by the
//! `e2e` feature; probes podman/qemu/KVM/UEFI/`sudo` via [`common::Prereqs`]).
//!
//! ## Registry reachability & auth
//!
//! Like `e2e_registry.rs`, a throwaway anonymous `registry:2` is addressed by the
//! host's primary IP so build host and guest share one reference string, and it's
//! plain HTTP so both are told it's insecure. Crucially the device is provisioned
//! knowing *nothing* about any registry — that's the premise — so the guest's
//! insecure-registry drop-in is **not** baked in; it's pushed over ssh at the
//! moment of transition, as test-only setup. In production this step doesn't exist:
//! a real registry is TLS, which needs no insecure drop-in, so a LAN-provisioned
//! device can be switched to it with nothing pre-installed. Because the registry is
//! anonymous the device also needs no `/etc/ostree/auth.json`; a transition to an
//! *authenticated* registry would first inject one with `rotate pull-token`
//! (covered by `e2e_registry.rs`), which is orthogonal to the origin switch proven
//! here.
//!
//! The registry container is torn down on drop.

use std::io::Write;
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
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-lan2reg-store";
/// The throwaway registry container name.
const REG_NAME: &str = "bootcher-e2e-lan2reg-reg";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn lan_provisioned_device_switches_to_registry_origin() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision();

	let _vm = h.boot(&scope);
	h.wait_for_ssh(&scope, "VM never answered ssh as admin");

	phase_lan_provisioned(&h);
	phase_lan_deploy_succeeds(&h);
	phase_switch_to_registry(&h);

	eprintln!("e2e/lan2reg: all phases passed");
}

// ----------------------------------------------------------------- phases

/// A LAN-provisioned device: the sentinel is baked, the bootc origin is the local
/// `localhost/...` ref (not the registry), and no pull secret was injected.
fn phase_lan_provisioned(h: &Harness) {
	eprintln!("e2e/lan2reg phase: LAN provisioned");
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v1", "baked sentinel mismatch");

	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains("localhost/"),
		"a LAN provision should record a localhost origin:\n{status}"
	);
	assert!(
		!status.contains(&h.ns),
		"a LAN provision must not already point at the registry:\n{status}"
	);
	// LAN mode bakes no registry pull secret…
	assert!(
		!h.ssh_ok("sudo test -f /etc/ostree/auth.json"),
		"a LAN provision must not ship /etc/ostree/auth.json"
	);
	// …and no knowledge of our registry at all: the premise of the transition is that
	// the device starts registry-unaware.
	assert!(
		!h.ssh_ok(&format!(
			"grep -rq {} /etc/containers/registries.conf.d/ 2>/dev/null",
			h.reg_addr
		)),
		"a LAN provision must not already carry our registry config"
	);
}

/// The self-contained LAN lifecycle still works: a LAN `deploy` ships the image
/// over the ssh tunnel and the device reboots into it.
fn phase_lan_deploy_succeeds(h: &Harness) {
	eprintln!("e2e/lan2reg phase: LAN deploy");
	h.set_sentinel("sentinel-v2");
	h.bootcher(&["deploy"]).assert().success();
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the LAN upgrade");
}

/// The transition: rewrite the manifest to registry mode and `deploy`. The push
/// lands the image in the registry, and each device's `bootc switch <reg_ref>`
/// moves its origin off containers-storage onto the registry ref before upgrading.
fn phase_switch_to_registry(h: &Harness) {
	eprintln!("e2e/lan2reg phase: switch to registry origin");

	// Test-only: teach the (registry-unaware) device that our throwaway registry is
	// plain HTTP — the one thing a TLS registry wouldn't require. Pushed over ssh now,
	// not baked at provision, so the device genuinely starts registry-unaware.
	h.install_root_file(
		"/etc/containers/registries.conf.d/10-bootcher-e2e.conf",
		&insecure_registries_conf(&h.reg_addr),
	);

	std::fs::write(
		h.proj.join("bootcher.toml"),
		registry_manifest(&h.name, h.arch, &h.ns, h.ssh_port),
	)
	.expect("rewrite manifest to registry mode");

	h.set_sentinel("sentinel-v3");
	h.bootcher(&["deploy"]).assert().success();

	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(
		sentinel.trim(),
		"sentinel-v3",
		"device did not upgrade after switching to registry"
	);

	// The origin now points at the registry ref — the device will auto-update from
	// the registry from here on, not from a LAN push.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains(&h.ns),
		"bootc origin should have switched to the registry ref:\n{status}"
	);
}

// ----------------------------------------------------------------- harness

struct Harness {
	arch: Arch,
	firmware: PathBuf,
	home: tempfile::TempDir,
	proj: PathBuf,
	name: String,
	/// `<host_ip>:<reg_port>/<repo>` — the registry namespace used after the switch.
	ns: String,
	/// `<host_ip>:<reg_port>` — the registry endpoint (for the insecure-registry config).
	reg_addr: String,
	ssh_port: u16,
	agent: Agent,
	admin_key: PathBuf,
	overlay: PathBuf,
	serial_log: PathBuf,
	_reg: RegistryGuard,
	// RAII teardown for the persistent podman store (wiped unless opted out).
	_store: StoreGuard,
}

impl Harness {
	fn setup(env: &Prereqs) -> Self {
		let home = tempfile::Builder::new()
			.prefix("bootcher-e2e-lan2reg-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
		let store = StoreGuard::new(STORE_DIR);
		let hp = home.path();
		let name = "e2elan2reg".to_owned();

		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let admin_key = keygen(&keydir, "admin");
		let agent = Agent::start(hp);
		agent.add(&admin_key);

		let ssh_port = qemu::free_port().expect("free ssh port");
		let reg_port = qemu::free_port().expect("free registry port");
		let reg_addr = format!("{}:{reg_port}", host_primary_ip());
		let ns = format!("{reg_addr}/proj");

		let reg = RegistryGuard::start(REG_NAME, reg_port);

		// Build host: the registry is plain HTTP (for the post-switch push).
		let conf_dir = hp.join(".config/containers");
		std::fs::create_dir_all(&conf_dir).unwrap();
		std::fs::write(conf_dir.join("registries.conf"), insecure_registries_conf(&reg_addr))
			.unwrap();

		// Scaffold the project, then write a *LAN* manifest (no registry) over it.
		let proj = hp.join(&name);
		AssertCommand::cargo_bin("bootcher")
			.unwrap()
			.args(["init", "-y", &name])
			.current_dir(hp)
			.env("HOME", hp)
			.assert()
			.success();
		std::fs::write(proj.join("bootcher.toml"), lan_manifest(&name, env.arch, ssh_port))
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
		// Nothing registry-related is baked: this is a genuine LAN provision. The
		// plain-HTTP exception the guest needs is pushed over ssh at transition time
		// (see `phase_switch_to_registry`), not pre-installed.
		h.set_sentinel("sentinel-v1");
		h
	}

	/// `bootcher provision` in LAN mode — only the admin key is collected (no
	/// registry, so no pull credential and no `--skip-pull-check` needed).
	fn provision(&self) {
		eprintln!("e2e/lan2reg: provisioning the LAN disk — the slow part");
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

	/// Write `content` to a root-owned `path` on the device over ssh (creating the
	/// parent dir) — the mechanism bootcher's own `rotate` uses, here standing in for
	/// the test-only plain-HTTP registry config a TLS registry wouldn't need.
	fn install_root_file(&self, path: &str, content: &str) {
		let cmd = format!("sudo mkdir -p \"$(dirname {path})\" && sudo tee {path} >/dev/null");
		let argv = qemu::ssh(VM_USER, self.ssh_port, &self.admin_key).argv(&[], &cmd);
		let (program, rest) = argv.split_first().unwrap();
		let mut child = Command::new(program)
			.args(rest)
			.env("HOME", self.home.path())
			.env_remove("SSH_AUTH_SOCK")
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.expect("spawn ssh");
		child.stdin.take().unwrap().write_all(content.as_bytes()).expect("write to ssh stdin");
		assert!(child.wait().expect("ssh wait").success(), "failed to install {path} on device");
	}

	fn set_sentinel(&self, value: &str) {
		let p = self.proj.join("sysroot/usr/lib/bootcher-e2e-sentinel");
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, format!("{value}\n")).unwrap();
	}

	fn dump_serial(&self) {
		let dest = PathBuf::from(SCRATCH_BASE).join("bootcher-e2e-lan2reg-serial.log");
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

	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
		c.args(args)
			.current_dir(&self.proj)
			.env("HOME", self.home.path())
			.env("SSH_AUTH_SOCK", self.agent.sock())
			.env("XDG_DATA_HOME", STORE_DIR);
		c
	}

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

	fn ssh_ok(&self, cmd: &str) -> bool {
		self.ssh_capture(cmd).status.success()
	}

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

/// A LAN `bootcher.toml` (no registry) pinned at the guest's forwarded ssh port.
fn lan_manifest(name: &str, arch: Arch, ssh_port: u16) -> String {
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
		 remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{ssh_port}\", \
		 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]\n"
	)
}

/// The same project rewritten to registry mode — the only change is the added
/// `registry` field, the move that flips `deploy` onto the registry backend.
fn registry_manifest(name: &str, arch: Arch, ns: &str, ssh_port: u16) -> String {
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
		 registry = \"{ns}\"\n\
		 remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{ssh_port}\", \
		 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]\n"
	)
}
