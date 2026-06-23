//! Integration test for the local multi-arch manifest list
//! ([`crate::jobs::build::manifest_list`]).
//!
//! It builds two tiny throwaway images (one per arch) with podman, then asserts
//! that assembling them yields a real manifest list under the suffix-free
//! `localhost/<name>:latest` ref carrying both arches. Each image is `FROM
//! scratch` plus a file: no base to fetch and — crucially — no executed step, so
//! the foreign-arch build is a metadata-only relabel needing no qemu/binfmt
//! emulation. This covers the local half of the multi-arch story; pushing the
//! same object to a registry (`deploy`/`upgrade`) needs a real registry to
//! exercise. Hard-fails when podman isn't available — the test can't mean
//! anything without it.

use crate::context::{Arch, ImageRef, Rootfs};
use crate::jobs;
use crate::progress::Scope;
use duct::cmd;
use std::path::PathBuf;
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

/// A throwaway single-arch member: `FROM scratch` + one file, built for `arch`.
/// No `RUN` step, so the cross-arch build is a metadata-only relabel — no
/// emulation needed.
fn build_member(image: &ImageRef, ctx: &std::path::Path) {
	cmd!("podman", "build", "--platform", image.arch.podman_platform(), "-t", image.tag(), ctx)
		.run()
		.unwrap_or_else(|e| panic!("podman build of {} failed: {e}", image.tag()));
}

#[test]
fn assembles_a_multi_arch_manifest_list_from_per_arch_members() {
	require_podman();

	let name = format!("bootcher-mltest-{}", std::process::id());

	// Self-contained build context: one layer, no base image to fetch.
	let ctx = TempDir::new().unwrap();
	std::fs::write(ctx.path().join("hello.txt"), b"bootcher manifest-list test\n").unwrap();
	std::fs::write(ctx.path().join("Containerfile"), b"FROM scratch\nCOPY hello.txt /hello.txt\n")
		.unwrap();

	let image = |arch: Arch| ImageRef {
		name: name.clone(),
		arch,
		rootfs: Rootfs::Ext4,
		build_ctx: PathBuf::from("."),
		registry: None,
	};
	let images = [image(Arch::X86_64), image(Arch::Aarch64)];

	for img in &images {
		build_member(img, ctx.path());
	}

	// The unit under test: collapse the per-arch members into the suffix-free list.
	let local_list_ref = format!("localhost/{name}:latest");
	jobs::build::manifest_list(&images, &local_list_ref, &mut Scope::standalone())
		.expect("assembling the manifest list");

	// The list must resolve to a real manifest list (not a single image) carrying
	// both arches. `podman manifest inspect` prints the OCI index, whose
	// `manifests[].platform.architecture` are `amd64`/`arm64`.
	let inspect = cmd!("podman", "manifest", "inspect", &local_list_ref)
		.read()
		.expect("podman manifest inspect of the assembled list");

	// Clean up before asserting, so a failed assert still tears the test images
	// down (the temp dir cleans itself).
	crate::exec::best_effort(&cmd!("podman", "manifest", "rm", &local_list_ref));
	crate::podman::rmi(images.iter().map(ImageRef::tag));

	assert!(inspect.contains("\"architecture\": \"amd64\""), "amd64 member missing:\n{inspect}");
	assert!(inspect.contains("\"architecture\": \"arm64\""), "arm64 member missing:\n{inspect}");
}
