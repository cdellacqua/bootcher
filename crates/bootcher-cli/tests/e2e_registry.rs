//! End-to-end VM test for the **plain (unsigned) registry lifecycle**, plus the
//! two transitions that only make sense on a registry-mode device: rotating the
//! pull token, and *enrolling signing* on a fleet that was provisioned unsigned.
//! One provisioned disk, booted once, carries every phase — the same shape as
//! `e2e_vm.rs` — because a fresh image-builder build + boot is the slow part and these
//! phases are independent steps along one device's life:
//!
//! 1. provisioned `--anonymous` (no signing, no pull secret) → the bootc origin is
//!    the registry ref, the device pulls anonymously, and nothing enforces signatures;
//! 2. a plain `deploy` pushes an unsigned multi-arch image and the device upgrades
//!    from the registry and reboots into it;
//! 3. `rotate pull-token` *creates* `/etc/ostree/auth.json` on the (previously
//!    credential-less) device — the anonymous→authenticated transition;
//! 4. `sign enroll` + `rotate sign-key` + a signing `deploy` turns enforcement on
//!    *without reprovisioning*, after which an unsigned push to the same ref is
//!    rejected on `bootc upgrade` — the security property, proven on the
//!    enrolled device.
//!
//! Slow and heavy, so **opt-in** exactly like the other VM e2e tests:
//!
//! - gated by the `e2e` feature (plain `cargo test` skips it),
//! - probes its prerequisites (podman, qemu, KVM, UEFI firmware, passwordless
//!   `sudo`), via [`common::Prereqs`].
//!
//! ## Registry reachability
//!
//! Like `e2e_registry_sign.rs`, this stands up a throwaway `registry:2` on a free
//! host port and addresses it by the host's own primary IP (`ip route get`), so
//! the build host reaches it locally and the guest reaches the same `<ip>:<port>`
//! via qemu user-net's NAT under one identical reference string. The registry is
//! plain HTTP, so both sides are told it's insecure (a `registries.conf` drop-in
//! baked into the image for the guest, and one in the test's throwaway `$HOME` for
//! the build host). The registry is anonymous: the device provisions with no pull
//! secret at all, and the credential `rotate pull-token` later writes is a dummy
//! pair carried purely to exercise the `auth.json` plumbing (an anonymous registry
//! can't *enforce* it — see `phase_rotate_pull_token`).
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
const STORE_DIR: &str = "/var/tmp/bootcher-e2e-reg-store";
/// The throwaway registry container name (distinct from the signing e2e's).
const REG_NAME: &str = "bootcher-e2e-reg";
/// Signing passphrase used by the enrollment phase (env-driven, no TTY).
const SIGN_PASS: &str = "e2e-reg-signing-pass";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn registry_lifecycle_unsigned_then_rotate_token_then_enroll_signing() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();

	let h = Harness::setup(&env);
	h.provision();

	let _vm = h.boot(&scope);
	h.wait_for_ssh(&scope, "VM never answered ssh as admin");

	phase_unsigned_origin_no_enforcement(&h);
	phase_unsigned_deploy_succeeds(&h);
	phase_rotate_pull_token(&h);
	phase_enroll_signing(&h);

	eprintln!("e2e/reg: all phases passed");
}

// ----------------------------------------------------------------- phases

/// The provisioned disk records the registry ref as its bootc origin and ships
/// none of the signing-enforcement files — a plain registry device.
fn phase_unsigned_origin_no_enforcement(h: &Harness) {
	eprintln!("e2e/reg phase: unsigned origin, no enforcement");
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v1", "baked sentinel mismatch");

	// Origin points at the registry ref (so the device self-updates from the
	// registry), and it is *not* the signature-enforcing variant.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(status.contains(&h.ns), "bootc origin isn't the registry ref:\n{status}");
	assert!(
		!status.contains("signature: containerPolicy"),
		"a plain registry provision must not record a signature-enforcing origin:\n{status}"
	);

	// No signing-enforcement files baked: the on-device policy is the fedora-bootc
	// permissive default (no sigstoreSigned requirement), and there's no trusted key.
	let policy = h.ssh_out("cat /etc/containers/policy.json").unwrap_or_default();
	assert!(
		!policy.contains("sigstoreSigned"),
		"a plain registry provision must not enforce a signature:\n{policy}"
	);
	assert!(
		!h.ssh_ok(&format!("test -f /etc/pki/containers/{}.pub", h.name)),
		"a plain registry provision must not ship a cosign public key"
	);

	// Provisioned `--anonymous`: no pull secret was baked, so the device pulls the
	// registry anonymously. `phase_rotate_pull_token` proves one can be added later.
	assert!(
		!h.ssh_ok("sudo test -f /etc/ostree/auth.json"),
		"an anonymous provision must not bake /etc/ostree/auth.json"
	);
}

/// A plain `deploy` builds + pushes an unsigned multi-arch image, the device
/// switches its origin, pulls from the registry, upgrades, and reboots into it.
fn phase_unsigned_deploy_succeeds(h: &Harness) {
	eprintln!("e2e/reg phase: unsigned deploy succeeds");
	h.set_sentinel("sentinel-v2");
	h.bootcher(&["deploy"]).assert().success();

	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v2", "device did not pick up the unsigned upgrade");

	// Still an unsigned origin — `deploy` carried no `--enforce-container-sigpolicy`.
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		!status.contains("signature: containerPolicy"),
		"unsigned deploy must not flip the origin to signature-enforcing:\n{status}"
	);
}

/// `rotate pull-token` on a device that had **no** `auth.json` (provisioned
/// `--anonymous`) must *create* it — the anonymous→authenticated transition. This
/// is the lightweight version of that scenario: the throwaway registry is anonymous
/// so it can't *enforce* the credential, but the test proves the mechanics — a
/// device with no pull secret gets one written, scoped to the registry namespace,
/// and still upgrades afterwards. (Enforcement against an auth-requiring registry
/// would need an htpasswd fixture; out of scope here.)
fn phase_rotate_pull_token(h: &Harness) {
	eprintln!("e2e/reg phase: rotate pull-token (create on anonymous device)");
	// Precondition: the anonymous provision left no pull secret.
	assert!(
		!h.ssh_ok("sudo test -f /etc/ostree/auth.json"),
		"expected no auth.json before the rotation (anonymous provision)"
	);

	// Rotate in a credential. `--skip-pull-check` bypasses only the eager local
	// `podman login` (the registry is anonymous); the device still runs its own
	// `podman manifest inspect` check against the registry ref before committing.
	h.bootcher_creds(&["rotate", "pull-token", "--skip-pull-check"], "puser2", "ptoken2")
		.assert()
		.success();

	// The credential now exists where there was none, scoped to the namespace.
	let after = h.ssh_out("sudo cat /etc/ostree/auth.json").expect("read auth.json after");
	assert!(after.contains("auths"), "created auth.json isn't a valid pull secret:\n{after}");
	assert!(
		after.contains(&h.ns),
		"created auth.json should scope the registry namespace:\n{after}"
	);

	// And the device still upgrades from the registry with the credential in place.
	h.set_sentinel("sentinel-v3");
	h.bootcher(&["deploy"]).assert().success();
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v3", "device did not upgrade after token rotation");
}

/// Enroll signing on the running (unsigned-provisioned) device, the documented
/// "enroll on a running fleet" flow: `sign enroll` generates a keypair and rewrites
/// the manifest to the signing form, `rotate sign-key` pushes the trust + policy,
/// and a signing `deploy` flips the origin to the enforcing transport. After that,
/// an unsigned push to the same ref must be rejected on `bootc upgrade`.
fn phase_enroll_signing(h: &Harness) {
	eprintln!("e2e/reg phase: enroll signing on the running device");

	// 1. Generate the keypair and patch `[deploy] registry` to the signing form.
	h.bootcher(&["sign", "enroll", "cosign"]).assert().success();
	assert!(h.proj.join("cosign.pub").is_file(), "sign enroll didn't write cosign.pub");

	// 2. Push the signing config (pubkey + sigstore policy + registries.d) to the device.
	h.bootcher(&["rotate", "sign-key"]).assert().success();
	let on_device = h
		.ssh_out(&format!("cat /etc/pki/containers/{}.pub", h.name))
		.expect("read pubkey from device after enroll");
	let expected =
		std::fs::read_to_string(h.proj.join("cosign.pub")).expect("read cosign.pub locally");
	assert_eq!(on_device.trim(), expected.trim(), "device pubkey should match cosign.pub");
	let policy = h.ssh_out("cat /etc/containers/policy.json").expect("read policy.json");
	assert!(policy.contains("sigstoreSigned"), "policy.json missing sigstoreSigned after enroll");

	// 3. A signing deploy signs the push and switches the origin with
	//    --enforce-container-sigpolicy, so the device now verifies on every upgrade.
	h.set_sentinel("sentinel-v4");
	h.bootcher(&["deploy"]).assert().success();
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v4", "device did not pick up the signed upgrade");
	let status = h.ssh_out("sudo bootc status").unwrap_or_default();
	assert!(
		status.contains("signature: containerPolicy"),
		"enrolling signing should flip the origin to signature-enforcing:\n{status}"
	);

	// 4. The security property: an unsigned image pushed to the same ref is rejected.
	h.set_sentinel("sentinel-v5");
	h.bootcher(&["build"]).assert().success();
	h.push_unsigned();
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
	let sentinel = h.ssh_out(&format!("cat {SENTINEL_PATH}")).expect("read sentinel");
	assert_eq!(sentinel.trim(), "sentinel-v4", "device must stay on the signed revision");
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
			.prefix("bootcher-e2e-reg-")
			.tempdir_in(SCRATCH_BASE)
			.expect("tempdir under /var/tmp");
		let store = StoreGuard::new(STORE_DIR);
		let hp = home.path();
		let name = "e2ereg".to_owned();

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

		// A throwaway registry on the free port (real podman env → cached registry:2).
		let reg = RegistryGuard::start(REG_NAME, reg_port);

		// Tell the build host's podman the registry is plain HTTP, in the throwaway
		// $HOME (so the user's real config is untouched).
		let conf_dir = hp.join(".config/containers");
		std::fs::create_dir_all(&conf_dir).unwrap();
		std::fs::write(conf_dir.join("registries.conf"), insecure_registries_conf(&reg_addr))
			.unwrap();

		// Scaffold the project, then write a *plain* (unsigned) registry manifest over it.
		let proj = hp.join(&name);
		AssertCommand::cargo_bin("bootcher")
			.unwrap()
			.args(["init", "-y", &name])
			.current_dir(hp)
			.env("HOME", hp)
			.assert()
			.success();
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

		// Bake only the guest's insecure-registry drop-in — the one image-level config a
		// plain-HTTP registry needs. No signing files: this device is provisioned unsigned
		// and only enrolls signing later, over ssh, in `phase_enroll_signing`.
		h.write_sysroot(
			"etc/containers/registries.conf.d/10-bootcher-e2e.conf",
			&insecure_registries_conf(&h.reg_addr),
		);
		h.set_sentinel("sentinel-v1");
		h
	}

	/// `bootcher provision` in plain registry mode against the public (anonymous)
	/// registry: `--anonymous`, so no pull credential is collected and no
	/// `auth.json` is baked — the device starts pulling anonymously.
	/// `phase_rotate_pull_token` later proves a credential can be *created* on it.
	fn provision(&self) {
		eprintln!("e2e/reg: provisioning the registry disk — the slow part");
		self.bootcher(&["provision", "--ssh-key", self.admin_key.to_str().unwrap(), "--anonymous"])
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
		let dest = PathBuf::from(SCRATCH_BASE).join("bootcher-e2e-reg-serial.log");
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

	/// A `bootcher` command rooted at the project, with the default dummy pull
	/// credential. Most phases want this; [`Self::bootcher_creds`] overrides the
	/// credential for the pull-token rotation.
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		self.bootcher_creds(args, "puser1", "ptoken1")
	}

	/// As [`Self::bootcher`] but with an explicit pull credential — the registry is
	/// anonymous, so the pair is only there to exercise the `auth.json` plumbing.
	fn bootcher_creds(&self, args: &[&str], pull_user: &str, pull_token: &str) -> AssertCommand {
		let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
		c.args(args)
			.current_dir(&self.proj)
			.env("HOME", self.home.path())
			.env("SSH_AUTH_SOCK", self.agent.sock())
			.env("XDG_DATA_HOME", STORE_DIR)
			.env("BOOTCHER_SIGN_PASSPHRASE", SIGN_PASS)
			.env("BOOTCHER_PULL_USER", pull_user)
			.env("BOOTCHER_PULL_TOKEN", pull_token);
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

	/// `true` iff a fresh login runs `cmd` to a zero exit (single shot).
	fn ssh_ok(&self, cmd: &str) -> bool {
		self.ssh_capture(cmd).status.success()
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

/// A plain (unsigned) registry `bootcher.toml` pinned at the throwaway registry and
/// the guest's forwarded ssh port. `phase_enroll_signing` later rewrites the
/// `registry` field to the signing form via `bootcher sign enroll`.
fn manifest(name: &str, arch: Arch, ns: &str, ssh_port: u16) -> String {
	format!(
		"[general]\n\
		 name = \"{name}\"\n\
		 \n\
		 [general.disk_types]\n\
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
