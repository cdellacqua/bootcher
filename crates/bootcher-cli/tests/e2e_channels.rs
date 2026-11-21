//! End-to-end VM test of a **mixed fleet**: one device follows `:stable`, one is
//! direct, both listed in `remotes`. Proves each device step reaches exactly its own
//! class and never moves a device:
//!
//! 1. `device status` reports one channel and one direct device;
//! 2. `device deploy` updates the direct device and skips the channel one;
//! 3. promoting to `:next` (a channel no device follows) touches neither;
//! 4. `release publish --promote stable` updates the channel device and leaves the
//!    direct one on what `device deploy` shipped;
//! 5. `device deploy --remote <channel device>` refuses, pointing at `device switch`.
//!
//! Slow and heavy, so **opt-in** (`e2e` feature; prerequisites probed by
//! [`common::Prereqs`]). See [`common::Lab`] for the registry setup.

use bootcher_core::progress::Scope;
use bootcher_core::qemu;
use predicates::prelude::*;

mod common;
use common::{Lab, Prereqs};

const STORE_DIR: &str = "/var/tmp/bootcher-e2e-chan-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --feature=e2e")]
fn mixed_fleet_each_step_reaches_only_its_own_devices() {
	let env = Prereqs::probe_disk_build()
		.expect("prerequisites not satisfied by the current environment");
	let scope = Scope::standalone();
	let lab = Lab::new(&env, "e2echan", STORE_DIR, Some("bootcher-e2e-chan"));
	let chan_port = qemu::free_port().expect("free ssh port");
	let direct_port = qemu::free_port().expect("free ssh port");
	lab.write_manifest(&[chan_port, direct_port], &["stable", "next"], "");
	lab.commit("v1");
	let chan = format!("ssh://admin@127.0.0.1:{chan_port}");
	let key = lab.admin_key.to_str().unwrap().to_owned();

	lab.bootcher(&["release", "publish", "v1"]).assert().success();
	lab.bootcher(&["channel", "promote", "v1", "stable", "-y", "--skip-remotes"])
		.assert()
		.success();
	lab.bootcher(&["device", "provision", "--channel", "stable", "--anonymous", "--ssh-key", &key])
		.assert()
		.success();
	let chan_disk = lab.keep_disk("channel");
	lab.bootcher(&["device", "provision", "--direct", "--ssh-key", &key]).assert().success();
	let direct_disk = lab.keep_disk("direct");
	let _c = lab.boot(&chan_disk, chan_port, "chan-channel", &scope);
	let _d = lab.boot(&direct_disk, direct_port, "chan-direct", &scope);

	// 1. One of each class.
	let out = lab
		.bootcher(&["device", "status", "--format", "{{ .Class }}"])
		.assert()
		.success()
		.get_output()
		.stdout
		.clone();
	let classes = String::from_utf8_lossy(&out);
	assert_eq!(classes.lines().collect::<Vec<_>>(), ["channel", "direct"], "{classes}");

	// 2. `device deploy` reaches the direct device only.
	lab.set_sentinel("sentinel-direct");
	lab.bootcher(&["device", "deploy", "-y"])
		.assert()
		.success()
		.stderr(predicate::str::contains("skipped"));
	assert_eq!(lab.sentinel(direct_port), "sentinel-direct");
	assert_eq!(lab.sentinel(chan_port), "sentinel-v1");
	lab.git(&["checkout", "-q", "--", "."]);

	// 3. A channel nobody follows moves nobody.
	lab.set_sentinel("sentinel-next");
	lab.commit("next");
	lab.bootcher(&["release", "publish", "v2", "--promote", "next", "-y"]).assert().success();
	assert_eq!(lab.sentinel(chan_port), "sentinel-v1", "a :next promote moved the :stable device");
	assert_eq!(lab.sentinel(direct_port), "sentinel-direct", "a promote moved the direct device");

	// 4. `--promote stable` reaches the channel device only.
	lab.bootcher(&["channel", "promote", "v2", "stable", "-y"]).assert().success();
	assert_eq!(lab.sentinel(chan_port), "sentinel-next");
	assert_eq!(lab.sentinel(direct_port), "sentinel-direct");
	assert!(
		lab.bootc_status(chan_port).contains(&lab.tag_ref("stable")),
		"the promote switched the device's channel"
	);

	// 5. D3: deploy never moves a device.
	lab.bootcher(&["device", "deploy", "-y", "--remote", &chan])
		.assert()
		.failure()
		.stderr(predicate::str::contains("device switch --direct"));
	eprintln!("e2e/chan: all phases passed");
}
