//! End-to-end VM test of **image signing**, on real devices:
//!
//! 1. **S1, as a permanent regression test**: in a signing project a *direct* disk
//!    installs and boots with a signature-enforcing origin and **no** trust files
//!    (`device provision --direct` bakes none), and the first `device deploy` re-pins
//!    it to an unverified `containers-storage` origin and boots the new image.
//! 2. A **channel** device provisioned from a signed `:stable` gets the trust files
//!    and a verifying origin; a signed promote upgrades it and an unsigned push to
//!    `:stable` is rejected on `bootc upgrade`.
//! 3. **Rotation with no window**: `signing rotate` adds a key (served releases gain a
//!    new-key signature, devices trust both), a release signed only by the new key
//!    then upgrades the device, `signing retire` drops the old key, and the next
//!    upgrade still succeeds.
//!
//! Slow and heavy, so **opt-in** (`e2e` feature; prerequisites probed by
//! [`common::Prereqs`]). The registry is a throwaway `registry:2` addressed by the
//! host's primary IP, so the cosign `matchRepository` identity is the same string on
//! both sides (see [`common::Lab`]).

use bootcher_core::progress::Scope;
use bootcher_core::qemu;

mod common;
use common::{Lab, Prereqs};

const STORE_DIR: &str = "/var/tmp/bootcher-e2e-sign-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
#[allow(clippy::too_many_lines)]
fn signing_direct_install_channel_enforcement_rotate_retire() {
	let env = Prereqs::probe_disk_build()
		.expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();
	let lab = Lab::new(&env, "e2esign", STORE_DIR, Some("bootcher-e2e-sign"));
	let direct_port = qemu::free_port().expect("free ssh port");
	let channel_port = qemu::free_port().expect("free ssh port");
	lab.write_manifest(&[direct_port, channel_port], &["stable"], "");
	lab.bootcher(&["signing", "enroll"]).assert().success();
	assert!(lab.proj.join("sysroot/usr/lib/bootc/install/30-bootcher-signing.toml").is_file());
	lab.commit("signing project");

	// 1. S1: a direct install in a signing project needs no trust files.
	lab.bootcher(&[
		"device",
		"provision",
		"--direct",
		"--ssh-key",
		lab.admin_key.to_str().unwrap(),
	])
	.assert()
	.success();
	let direct_disk = lab.keep_disk("direct");
	let direct_only = format!("ssh://admin@127.0.0.1:{direct_port}");
	let channel_only = format!("ssh://admin@127.0.0.1:{channel_port}");
	{
		let _vm = lab.boot(&direct_disk, direct_port, "sign-direct", &scope);
		let status = lab.bootc_status(direct_port);
		assert!(
			status.contains("signature: containerPolicy"),
			"S1: disk origin should verify:\n{status}"
		);
		assert!(
			!lab.ssh(direct_port).ok("test -f /etc/pki/containers/e2esign.pub"),
			"D4: nothing is baked on a direct install"
		);
		lab.set_sentinel("sentinel-direct-v2");
		lab.bootcher(&["device", "deploy", "-y", "--remote", &direct_only]).assert().success();
		assert_eq!(lab.sentinel(direct_port), "sentinel-direct-v2");
		let status = lab.bootc_status(direct_port);
		assert!(
			status.contains("containers-storage"),
			"deploy should re-pin to containers-storage:\n{status}"
		);
		assert!(
			!status.contains("signature: containerPolicy"),
			"a direct device's origin is unverified:\n{status}"
		);
	}
	lab.git(&["checkout", "-q", "--", "."]);

	// 2. A signed channel device.
	lab.bootcher(&["release", "publish", "v1"]).assert().success();
	lab.bootcher(&["channel", "promote", "v1", "stable", "-y", "--skip-remotes"])
		.assert()
		.success();
	lab.bootcher(&[
		"device",
		"provision",
		"--channel",
		"stable",
		"--anonymous",
		"--ssh-key",
		lab.admin_key.to_str().unwrap(),
	])
	.assert()
	.success();
	let channel_disk = lab.keep_disk("channel");
	let _vm = lab.boot(&channel_disk, channel_port, "sign-channel", &scope);
	let status = lab.bootc_status(channel_port);
	assert!(
		status.contains(&lab.tag_ref("stable")) && status.contains("signature: containerPolicy"),
		"{status}"
	);
	let policy = lab.ssh(channel_port).out("cat /etc/containers/policy.json").unwrap_or_default();
	assert!(
		policy.contains("sigstoreSigned"),
		"trust files missing on a channel install:\n{policy}"
	);

	lab.set_sentinel("sentinel-v2");
	lab.commit("v2");
	lab.bootcher(&[
		"release",
		"publish",
		"v2",
		"--promote",
		"stable",
		"-y",
		"--remote",
		&channel_only,
	])
	.assert()
	.success();
	assert_eq!(
		lab.sentinel(channel_port),
		"sentinel-v2",
		"the signed promote didn't upgrade the device"
	);

	lab.set_sentinel("sentinel-tampered");
	lab.bootcher(&["build"]).assert().success();
	lab.registry().push_unsigned("stable");
	assert!(
		!lab.ssh(channel_port).capture("sudo bootc upgrade").status.success(),
		"unsigned image accepted"
	);
	lab.git(&["checkout", "-q", "--", "."]);
	// Put the signed v2 back on the channel (the unsigned push moved it).
	lab.bootcher(&["channel", "promote", "v2", "stable", "-y", "--skip-remotes"])
		.assert()
		.success();

	// 3. Rotate, upgrade on the new key, retire, upgrade again — never rejected.
	lab.bootcher(&["signing", "rotate", "cosign-next", "-y", "--remote", &channel_only])
		.assert()
		.success();
	lab.commit("rotate");
	let keys = lab.ssh(channel_port).out("ls /etc/pki/containers").unwrap_or_default();
	assert_eq!(
		keys.lines().filter(|l| l.starts_with("e2esign-")).count(),
		2,
		"both keys trusted:\n{keys}"
	);
	lab.set_sentinel("sentinel-v3");
	lab.commit("v3");
	lab.bootcher(&[
		"release",
		"publish",
		"v3",
		"--promote",
		"stable",
		"-y",
		"--remote",
		&channel_only,
	])
	.assert()
	.success();
	assert_eq!(
		lab.sentinel(channel_port),
		"sentinel-v3",
		"a new-key release must upgrade a device trusting both"
	);
	lab.bootcher(&["signing", "retire", "-y", "--remote", &channel_only]).assert().success();
	let keys = lab.ssh(channel_port).out("ls /etc/pki/containers").unwrap_or_default();
	assert_eq!(
		keys.lines().filter(|l| l.starts_with("e2esign-")).count(),
		1,
		"retire keeps one key:\n{keys}"
	);
	lab.set_sentinel("sentinel-v4");
	lab.commit("v4");
	lab.bootcher(&[
		"release",
		"publish",
		"v4",
		"--promote",
		"stable",
		"-y",
		"--remote",
		&channel_only,
	])
	.assert()
	.success();
	assert_eq!(lab.sentinel(channel_port), "sentinel-v4");
	eprintln!("e2e/sign: all phases passed");
}
