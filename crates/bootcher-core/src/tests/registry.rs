//! Integration test for the LAN mini-registry ([`crate::registry`]).
//!
//! Builds a tiny throwaway image with podman (`FROM scratch` + a file, so one
//! real layer and no network), serves it through the registry, and pulls it
//! back into an *isolated* podman store — forcing every blob over the wire and
//! exercising the whole v2 read path: the `/v2/` version probe, tag→manifest
//! resolution via `index.json`, blob serving, and the `Docker-Content-Digest`
//! headers (podman verifies digests on pull, so a bad header fails the pull).
//!
//! Hard-fails when podman isn't available — the test can't mean anything
//! without it.

use crate::context::{Arch, ImageRef, Rootfs};
use crate::progress::Scope;
use crate::registry;
use duct::cmd;
use tempfile::TempDir;

/// Assert podman is usable, hard-failing the test if it isn't — this test is
/// pointless without it, so a missing podman is a failure to surface, not a
/// reason to silently pass.
fn require_podman() {
	let ok = cmd!("podman", "--version")
		.stdout_null()
		.stderr_null()
		.unchecked()
		.run()
		.is_ok_and(|o| o.status.success());
	assert!(ok, "podman is required to run this test, but `podman --version` failed");
}

#[test]
fn serves_a_local_image_that_podman_can_pull() {
	require_podman();

	// Host-arch tag so the pull-back has no platform mismatch.
	let arch = if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 };
	let name = format!("bootcher-regtest-{}", std::process::id());

	// Self-contained build context: one layer, no base image to fetch. The
	// `TempDir`s clean themselves up on drop.
	let ctx = TempDir::new().unwrap();
	std::fs::write(ctx.path().join("hello.txt"), b"bootcher registry test\n").unwrap();
	std::fs::write(ctx.path().join("Containerfile"), b"FROM scratch\nCOPY hello.txt /hello.txt\n")
		.unwrap();

	let image = ImageRef {
		name: name.clone(),
		arch,
		rootfs: Rootfs::Ext4,
		build_ctx: ctx.path().to_path_buf(),
		registry: None,
	};
	let tag = image.tag();

	cmd!("podman", "build", "--platform", arch.podman_platform(), "-t", &tag, ctx.path())
		.run()
		.expect("podman build of the test image failed");

	// Serve + pull back into an isolated store so every blob crosses the wire.
	let store = TempDir::new().unwrap();
	let runroot = TempDir::new().unwrap();
	let pulled = {
		let oci_ref = format!("latest-{}", image.arch);
		let reg = registry::serve(&image.tag(), &oci_ref, &Scope::standalone())
			.expect("starting the mini-registry");
		let pull_ref = format!("127.0.0.1:{}/{}:latest-{}", reg.port(), image.name, image.arch);
		cmd!(
			"podman",
			"--root",
			store.path(),
			"--runroot",
			runroot.path(),
			"pull",
			"--tls-verify=false",
			&pull_ref
		)
		.unchecked()
		.run()
		.expect("running podman pull")
		// `reg` drops here, stopping the server thread and removing the layout.
	};

	// Best-effort: drop the test image from the main store (the temp dirs are
	// cleaned up by their own `Drop`).
	crate::podman::rmi([&tag]);

	assert!(pulled.status.success(), "podman pull from the mini-registry failed");
}
