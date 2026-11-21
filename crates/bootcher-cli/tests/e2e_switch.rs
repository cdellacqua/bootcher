//! End-to-end VM test of **`device switch`**, the only verb that moves a device, in
//! both directions on one device:
//!
//! 1. a direct device (provisioned `--direct`, no pull credential) is moved onto
//!    `:stable` with `device switch --channel stable`: it gains a pull credential
//!    (none was baked), its origin becomes the channel ref, and it boots what the
//!    channel serves;
//! 2. `device switch --direct` moves it back: built here, shipped over SSH, and its
//!    origin is `localhost/<name>:latest` under `containers-storage` again.
//!
//! Replaces the old LAN → registry origin-switch test. Slow and heavy, so **opt-in**
//! (`e2e` feature; prerequisites probed by [`common::Prereqs`]). See [`common::Lab`].

use bootcher_core::progress::Scope;
use bootcher_core::qemu;

mod common;
use common::{Lab, Prereqs};

const STORE_DIR: &str = "/var/tmp/bootcher-e2e-switch-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn switch_direct_to_channel_and_back() {
	let env = Prereqs::probe_disk_build()
		.expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();
	let lab = Lab::new(&env, "e2eswitch", STORE_DIR, Some("bootcher-e2e-switch"));
	let port = qemu::free_port().expect("free ssh port");
	lab.write_manifest(&[port], &["stable"], "");
	lab.commit("v1");
	let me = format!("ssh://admin@127.0.0.1:{port}");

	lab.bootcher(&[
		"device",
		"provision",
		"--direct",
		"--ssh-key",
		lab.admin_key.to_str().unwrap(),
	])
	.assert()
	.success();
	let disk = lab.keep_disk("direct");
	let _vm = lab.boot(&disk, port, "switch", &scope);
	assert!(!lab.ssh(port).ok("sudo test -f /etc/ostree/auth.json"));

	// Something for the channel to serve.
	lab.set_sentinel("sentinel-channel");
	lab.commit("channel content");
	lab.bootcher(&["release", "publish", "v1", "--promote", "stable", "-y", "--skip-remotes"])
		.assert()
		.success();

	// 1. direct → channel (the device has no credential: one is collected and installed).
	lab.bootcher(&[
		"device",
		"switch",
		"--channel",
		"stable",
		"--remote",
		&me,
		"--skip-pull-check",
		"-y",
	])
	.assert()
	.success();
	assert_eq!(lab.sentinel(port), "sentinel-channel");
	assert!(
		lab.ssh(port).ok("sudo test -f /etc/ostree/auth.json"),
		"switch didn't install a pull credential"
	);
	let status = lab.bootc_status(port);
	assert!(status.contains(&lab.tag_ref("stable")), "origin isn't the channel:\n{status}");
	// Switching again is a no-op.
	lab.bootcher(&["device", "switch", "--channel", "stable", "--remote", &me, "-y"])
		.assert()
		.success()
		.stderr(predicates::str::contains("nothing to do"));

	// 2. channel → direct.
	lab.set_sentinel("sentinel-direct-again");
	lab.bootcher(&["device", "switch", "--direct", "--remote", &me, "-y"]).assert().success();
	assert_eq!(lab.sentinel(port), "sentinel-direct-again");
	let status = lab.bootc_status(port);
	assert!(status.contains("containers-storage"), "origin isn't containers-storage:\n{status}");
	lab.bootcher(&["device", "status", "--format", "{{ .Class }}"])
		.assert()
		.success()
		.stdout("direct\n");
	eprintln!("e2e/switch: all phases passed");
}
