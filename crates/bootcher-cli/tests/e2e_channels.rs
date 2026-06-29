//! End-to-end VM test for the **release-channel mechanism** (registry mode).
//! One provisioned disk, booted once, carries every phase — the same shape as
//! `e2e_registry.rs` — because the image-builder build + boot is the slow part and the
//! channel behaviours are independent steps along one device's life:
//!
//! 1. provisioned `--channel stable` → the bootc origin is the *channel* ref
//!    (`<registry>/<name>:stable`), not `:latest`, and nothing is pushed yet;
//! 2. `deploy --channel stable` pushes the multi-arch list under `:stable` (and the
//!    shared immutable `:CalVer` tag) and the device upgrades from it — proving a
//!    channel deploy reaches a device that tracks that channel;
//! 3. **isolation**: a default (`latest`) `deploy --skip-bootc-upgrade` pushes a
//!    *different* image under `:latest` without touching the device; the device's own
//!    `bootc upgrade` (which pulls from its `:stable` origin) does **not** pick that
//!    content up — a device follows its channel, not `:latest`. This is the
//!    timer/self-update path: `--skip-bootc-upgrade` is what keeps the deployer from
//!    `bootc switch`ing the device onto the channel being pushed;
//! 4. re-deploying `:stable` with new content *does* upgrade the device — the channel
//!    is a live, mutable pointer the device keeps following.
//!
//! Throughout, the registry is queried directly (`podman manifest inspect`) to prove
//! each push landed under the right tag and that a channel push never silently
//! creates the other channel's tag.
//!
//! Slow and heavy, so **opt-in** exactly like the other VM e2e tests:
//!
//! - gated by the `e2e` feature (plain `cargo test` skips it),
//! - probes its prerequisites (podman, qemu, KVM, UEFI firmware, passwordless
//!   `sudo`), via [`common::Prereqs`].
//!
//! ## Registry reachability
//!
//! Like `e2e_registry.rs`, this stands up a throwaway anonymous `registry:2` on a free
//! host port and addresses it by the host's own primary IP (`ip route get`), so the
//! build host and the guest reach the same `<ip>:<port>` reference. Plain HTTP, so both
//! sides are told it's insecure (a `registries.conf` drop-in baked into the image for
//! the guest, and one in the test's throwaway `$HOME` for the build host). The registry
//! container is torn down on drop.

use std::path::PathBuf;

use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	Agent, Prereqs, RegistryClient, RegistryGuard, SCRATCH_BASE, SENTINEL_PATH, SSH_TIMEOUT, Ssh,
	StoreGuard, VM_USER, host_primary_ip, insecure_registries_conf, keygen,
};

/// Podman store for this e2e (own dir, out of the user's real store). Wiped on
/// teardown by default ([`StoreGuard`]); set `BOOTCHER_E2E_KEEP_STORE` to keep it.
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-chan-store";
/// The throwaway registry container name (distinct from the other e2es').
const REG_NAME: &str = "bootcher-e2e-chan";
/// The non-default channel this project provisions onto and tracks.
const CHANNEL: &str = "stable";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn channel_provisioned_device_tracks_its_channel_not_latest() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision();

	let _vm = h.boot(&scope);
	h.wait_for_ssh(&scope, "VM never answered ssh as admin");

	phase_provisioned_origin_is_channel(&h);
	phase_deploy_channel_upgrades_device(&h, &scope);
	phase_latest_deploy_leaves_channel_device(&h);
	phase_redeploy_channel_updates_device(&h, &scope);

	eprintln!("e2e/chan: all phases passed");
}

// ----------------------------------------------------------------- phases

/// The provisioned disk records the **channel** ref as its bootc origin (not
/// `:latest`), and provision pushed nothing — the registry has neither tag yet.
fn phase_provisioned_origin_is_channel(h: &Harness) {
	eprintln!("e2e/chan phase: provisioned origin is the channel ref");
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v1", "baked sentinel mismatch");

	// Origin is the channel ref `<ns>/<name>:stable`, so the device self-updates from
	// that channel — never the default `:latest`.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains(&h.channel_ref()),
		"bootc origin should be the channel ref {}:\n{status}",
		h.channel_ref()
	);
	assert!(
		!status.contains(&h.latest_ref()),
		"a `--channel {CHANNEL}` provision must not record the `:latest` origin:\n{status}"
	);

	// Provision builds a disk but pushes nothing: neither tag exists in the registry yet.
	assert!(!h.registry_has_tag(CHANNEL), "provision must not push the channel tag");
	assert!(!h.registry_has_tag("latest"), "provision must not push the latest tag");
}

/// `deploy --channel stable` pushes the multi-arch list under `:stable` and the device
/// tracking that channel upgrades into it.
fn phase_deploy_channel_upgrades_device(h: &Harness, scope: &Scope) {
	eprintln!("e2e/chan phase: deploy --channel {CHANNEL} upgrades the device");
	h.set_sentinel("sentinel-v2");
	h.bootcher(&["deploy", "--channel", CHANNEL]).assert().success();
	// `deploy` reboots the device and waits for it online before returning; wait again
	// explicitly so the assertions below don't race a still-rebooting guest.
	h.wait_for_ssh(scope, "device never came back after the channel deploy");

	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the channel deploy");

	// The push landed under `:stable`; it did *not* also create `:latest`.
	assert!(h.registry_has_tag(CHANNEL), "deploy --channel must push the channel tag");
	assert!(
		!h.registry_has_tag("latest"),
		"a `--channel {CHANNEL}` deploy must not create the `:latest` tag"
	);
}

/// Isolation: a default (`latest`) deploy pushes a *different* image under `:latest`
/// **without** touching the device (`--skip-bootc-upgrade`, the self-update path — a
/// plain deploy would otherwise `bootc switch` the device onto `:latest`). The device's
/// own `bootc upgrade` pulls from its `:stable` origin, so it must **not** pick up the
/// `:latest` content — it keeps following its channel.
fn phase_latest_deploy_leaves_channel_device(h: &Harness) {
	eprintln!("e2e/chan phase: a latest deploy leaves the channel device untouched");
	// Build + push *different* content under the default `:latest`, device untouched.
	h.set_sentinel("sentinel-v3-latest");
	h.bootcher(&["deploy", "--skip-bootc-upgrade"]).assert().success();

	// Now both tags exist, and they're distinct images (different digests).
	assert!(h.registry_has_tag("latest"), "default deploy should create the latest tag");
	assert!(h.registry_has_tag(CHANNEL), "the channel tag should still be present");
	assert_ne!(
		h.registry_digest("latest"),
		h.registry_digest(CHANNEL),
		"the latest and channel tags should point at distinct images"
	);

	// The device upgrades from its *own* origin (`:stable`), which the latest push did
	// not change — so this is a no-op and the sentinel stays at the channel's v2.
	let out = h.ssh_capture("sudo bootc upgrade");
	let upgrade_log =
		format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
	assert!(out.status.success(), "device bootc upgrade failed:\n{upgrade_log}");
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(
		sentinel.trim(),
		"sentinel-v2",
		"the device must follow its `:{CHANNEL}` channel and ignore the `:latest` push"
	);

	// And its origin is still the channel ref — the latest push never re-pointed it.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains(&h.channel_ref()) && !status.contains(&h.latest_ref()),
		"the device origin must stay the channel ref after a latest push:\n{status}"
	);
}

/// Re-deploying the channel with new content upgrades the device again — the channel is
/// a live mutable pointer the device keeps tracking.
fn phase_redeploy_channel_updates_device(h: &Harness, scope: &Scope) {
	eprintln!("e2e/chan phase: re-deploy --channel {CHANNEL} updates the device");
	h.set_sentinel("sentinel-v4");
	h.bootcher(&["deploy", "--channel", CHANNEL]).assert().success();
	h.wait_for_ssh(scope, "device never came back after the channel re-deploy");
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v4", "device did not pick up the channel re-deploy");
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
			.prefix("bootcher-e2e-chan-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
		let store = StoreGuard::new(STORE_DIR);
		let hp = home.path();
		let name = "e2echan".to_owned();

		// Admin keypair + agent (bootcher's ambient ssh uses the agent).
		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let admin_key = keygen(&keydir, "admin");
		let agent = Agent::start(hp);
		agent.add(&admin_key);

		let ssh_port = qemu::free_port().expect("free ssh port");
		let reg_port = qemu::free_port().expect("free registry port");
		let reg_addr = format!("{}:{reg_port}", host_primary_ip());
		let ns = format!("{reg_addr}/proj");

		// A throwaway anonymous registry on the free port (real podman env → cached registry:2).
		let reg = RegistryGuard::start(REG_NAME, reg_port);

		// Tell the build host's podman the registry is plain HTTP (throwaway $HOME).
		common::write_host_insecure_registry(hp, &reg_addr);

		// Scaffold the project, then write a registry manifest that *declares* the
		// `stable` channel (so `--channel stable` validates) over it.
		let proj = common::scaffold_project(hp, &name);
		std::fs::write(proj.join("bootcher.toml"), manifest(&name, env.arch, &ns, ssh_port))
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

		// Bake the guest's insecure-registry drop-in — the one image-level config a
		// plain-HTTP registry needs. No signing files: this is a plain registry device.
		common::write_sysroot(
			&h.proj,
			"etc/containers/registries.conf.d/10-bootcher-e2e.conf",
			&insecure_registries_conf(&h.reg_addr),
		);
		common::set_sentinel(&h.proj, "sentinel-v1");
		h
	}

	/// `bootcher provision --channel stable` in plain registry mode against the
	/// anonymous registry (`--anonymous`, no pull credential): the device's bootc origin
	/// is set to the channel ref, so it tracks `:stable` from first boot.
	fn provision(&self) {
		eprintln!("e2e/chan: provisioning the channel disk — the slow part");
		self.bootcher(&[
			"provision",
			"--channel",
			CHANNEL,
			"--ssh-key",
			self.admin_key.to_str().unwrap(),
			"--anonymous",
		])
		.assert()
		.success();
	}

	fn boot(&self, scope: &Scope) -> Vm {
		common::make_overlay(&self.built_disk(), &self.overlay, None);
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

	/// The mutable channel registry ref `<ns>/<name>:stable` (the device's origin).
	fn channel_ref(&self) -> String {
		format!("{}/{}:{CHANNEL}", self.ns, self.name)
	}

	/// The default `<ns>/<name>:latest` ref — the channel this device must *not* follow.
	fn latest_ref(&self) -> String {
		format!("{}/{}:latest", self.ns, self.name)
	}

	/// The throwaway registry as the build host addresses it — used to prove a push
	/// landed under the expected tag and didn't silently create the other channel's.
	fn registry(&self) -> RegistryClient<'_> {
		RegistryClient {
			proj: &self.proj,
			home: self.home.path(),
			store: STORE_DIR,
			ns: &self.ns,
			name: &self.name,
		}
	}

	fn registry_has_tag(&self, tag: &str) -> bool {
		self.registry().has_tag(tag)
	}

	fn registry_digest(&self, tag: &str) -> Option<String> {
		self.registry().digest(tag)
	}

	fn set_sentinel(&self, value: &str) {
		common::set_sentinel(&self.proj, value);
	}

	fn built_disk(&self) -> PathBuf {
		common::built_disk(&self.proj)
	}

	fn wait_for_ssh(&self, scope: &Scope, what: &str) {
		common::wait_for_ssh(
			VM_USER,
			self.ssh_port,
			&self.admin_key,
			SSH_TIMEOUT,
			scope,
			&self.serial_log,
			"chan",
			what,
		);
	}

	/// A `bootcher` command rooted at the project, with a dummy pull credential (the
	/// registry is anonymous, so the pair only exercises the plumbing).
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c =
			common::bootcher_cmd(&self.proj, self.home.path(), STORE_DIR, Some(self.agent.sock()));
		c.args(args).env("BOOTCHER_PULL_USER", "puser1").env("BOOTCHER_PULL_TOKEN", "ptoken1");
		c
	}

	/// An ssh view to the guest with the admin key (no agent — only `-i admin`).
	fn ssh(&self) -> Ssh<'_> {
		Ssh {
			home: self.home.path(),
			user: VM_USER,
			port: self.ssh_port,
			key: &self.admin_key,
			agent_sock: None,
		}
	}

	fn ssh_capture(&self, cmd: &str) -> std::process::Output {
		self.ssh().capture(cmd)
	}

	fn ssh_out(&self, cmd: &str) -> Option<String> {
		self.ssh().out(cmd)
	}
}

// ----------------------------------------------------------------- fixtures

/// A plain (unsigned) registry `bootcher.toml` that declares the `stable` channel,
/// pinned at the throwaway registry and the guest's forwarded ssh port.
fn manifest(name: &str, arch: Arch, ns: &str, ssh_port: u16) -> String {
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
		 channels = [\"{CHANNEL}\"]\n\
		 remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{ssh_port}\", \
		 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }}]\n"
	)
}
