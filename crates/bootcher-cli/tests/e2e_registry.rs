//! End-to-end VM test of the **channel lifecycle** (no signing): one channel device,
//! booted once, carried through every registry-side step a fleet goes through:
//!
//! 1. `release publish v1` + `channel promote v1 stable --skip-remotes`, then `device
//!    provision --channel stable --anonymous`: the disk is built from what `:stable`
//!    serves (pulled, never built), its origin is the channel ref, nothing enforces
//!    signatures and no pull secret is baked;
//! 2. `release publish v2 --promote stable`: the promote's device step updates the
//!    device (channel step: switch + upgrade + reboot) — and `device deploy` skips it
//!    (it only ever updates direct devices);
//! 3. base patches: `release refresh v2 --force` rebuilds v2 from its own commit
//!    (HEAD has moved on) without moving the channel, then `channel refresh stable`
//!    re-copies the lagging channel and updates the device;
//! 4. `device rotate pull-token` *creates* `/etc/ostree/auth.json` on the
//!    credential-less device, verified against its own channel;
//! 5. `signing enroll` on the running fleet signs what `:stable` serves, installs the
//!    trust files and flips the device to a signature-enforcing origin; an unsigned
//!    push to `:stable` is then rejected on `bootc upgrade`.
//!
//! Slow and heavy, so **opt-in** like the other VM e2e tests (the `e2e` feature;
//! prerequisites probed by [`common::Prereqs`]). The throwaway registry is anonymous
//! and plain HTTP, addressed by the host's primary IP so the build host and the guest
//! share one reference string (see [`common::Lab`]).

use bootcher_core::progress::Scope;

mod common;
use common::{Lab, Prereqs};

const STORE_DIR: &str = "/var/tmp/bootcher-e2e-reg-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
#[allow(clippy::too_many_lines)]
fn channel_lifecycle_publish_promote_refresh_rotate_enroll() {
	let env = Prereqs::probe_disk_build()
		.expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();
	let lab = Lab::new(&env, "e2ereg", STORE_DIR, Some("bootcher-e2e-reg"));
	let port = bootcher_core::qemu::free_port().expect("free ssh port");
	lab.write_manifest(&[port], &["stable", "next"], "");
	lab.commit("v1");

	// 1. Publish, promote (no device yet), provision from the channel.
	lab.bootcher(&["release", "publish", "v1"]).assert().success();
	lab.bootcher(&["channel", "promote", "v1", "stable", "-y", "--skip-remotes"])
		.assert()
		.success();
	eprintln!("e2e/reg: provisioning from :stable — the slow part");
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
	let disk = lab.keep_disk("stable");
	let _vm = lab.boot(&disk, port, "reg", &scope);

	assert_eq!(lab.sentinel(port), "sentinel-v1");
	let status = lab.bootc_status(port);
	assert!(status.contains(&lab.tag_ref("stable")), "origin isn't the channel ref:\n{status}");
	assert!(
		!status.contains("signature: containerPolicy"),
		"unsigned project, verifying origin:\n{status}"
	);
	assert!(
		!lab.ssh(port).ok("sudo test -f /etc/ostree/auth.json"),
		"--anonymous baked a pull secret"
	);
	lab.bootcher(&["device", "status", "--format", "{{ .Class }} {{ .Release }}"])
		.assert()
		.success()
		.stdout(predicates::str::contains("channel v1"));

	// 2. A new release, promoted with its device step. `device deploy` leaves the
	//    channel device alone.
	lab.set_sentinel("sentinel-v2");
	lab.commit("v2");
	lab.bootcher(&["device", "deploy", "-y", "--skip-build"]).assert().success().stderr(
		predicates::str::contains("skipped").and(predicates::str::contains("release publish")),
	);
	lab.bootcher(&["release", "publish", "v2", "--promote", "stable", "-y"]).assert().success();
	assert_eq!(
		lab.sentinel(port),
		"sentinel-v2",
		"the promote's device step didn't update the device"
	);

	// 3. Base patches without shipping unreleased work.
	lab.set_sentinel("sentinel-unreleased");
	lab.commit("unreleased");
	let before = lab.registry().digest("stable");
	lab.bootcher(&["release", "refresh", "v2", "--force"]).assert().success();
	assert_eq!(
		lab.registry().digest("stable"),
		before,
		"a release refresh must not move the channel"
	);
	lab.bootcher(&["channel", "status", "stable", "--format", "{{ .Lags }}"])
		.assert()
		.success()
		.stdout("true\n");
	lab.bootcher(&["channel", "refresh", "stable", "-y"]).assert().success();
	assert_ne!(lab.registry().digest("stable"), before);
	assert_eq!(lab.sentinel(port), "sentinel-v2", "a refresh rebuilds v2's commit, not HEAD");

	// 4. A pull credential appears on the credential-less device.
	lab.bootcher(&["device", "rotate", "pull-token", "--skip-pull-check", "-y"]).assert().success();
	let auth =
		lab.ssh(port).out("sudo cat /etc/ostree/auth.json").expect("auth.json after rotation");
	assert!(
		auth.contains(lab.ns.as_deref().unwrap()),
		"auth.json isn't scoped to the registry:\n{auth}"
	);

	// 5. Enroll signing on the running fleet.
	lab.bootcher(&["signing", "enroll", "-y"]).assert().success();
	lab.commit("enroll signing");
	let status = lab.bootc_status(port);
	assert!(
		status.contains("signature: containerPolicy"),
		"enroll didn't flip the origin:\n{status}"
	);
	let policy = lab.ssh(port).out("cat /etc/containers/policy.json").unwrap_or_default();
	assert!(policy.contains("sigstoreSigned") && policy.contains("keyPaths"), "{policy}");
	lab.bootcher(&["signing", "verify", "stable"]).assert().success();

	// The security property: an unsigned push to the channel is rejected.
	lab.set_sentinel("sentinel-tampered");
	lab.bootcher(&["build"]).assert().success();
	lab.registry().push_unsigned("stable");
	let out = lab.ssh(port).capture("sudo bootc upgrade");
	assert!(!out.status.success(), "bootc upgrade accepted an unsigned image");
	assert_eq!(lab.sentinel(port), "sentinel-v2");
	eprintln!("e2e/reg: all phases passed");
}

use predicates::prelude::PredicateBooleanExt as _;
