//! Integration tests for the `bootcher` binary.
//!
//! Each test scaffolds its own throwaway project with `bootcher init` in a fresh
//! `tempdir` and runs commands there (`current_dir`), so tests are isolated and
//! run in parallel. They stop before anything that needs podman/qemu — the
//! secret-collection + manifest plumbing is what's exercised here (SSH key parsing
//! and blueprint generation are unit-tested in `bootcher_core::jobs::secrets`).
//!
//! This is the full check of the standalone-CLI project plumbing (the scaffold
//! shape, manifest, and command guards) — there's no separate shell smoke test.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `bootcher` command rooted at `dir` with a throwaway `HOME` so that key
/// discovery, registry config writes, and cosign state stay out of the real home.
fn bootcher_in(home: &Path, dir: &Path) -> Command {
	let mut c = Command::cargo_bin("bootcher").unwrap();
	c.current_dir(dir).env("HOME", home);
	c
}

/// Scaffold a fresh project named `name` in a new tempdir and return the tempdir
/// (kept alive for the test's lifetime) and the project path inside it.
fn project(name: &str) -> (TempDir, PathBuf) {
	let tmp = tempfile::tempdir().unwrap();
	// `-y`: the tests run non-interactively (assert_cmd has no TTY).
	bootcher_in(tmp.path(), tmp.path()).args(["init", "-y", name]).assert().success();
	let proj = tmp.path().join(name);
	assert!(proj.join("bootcher.toml").is_file(), "init did not scaffold a manifest");
	(tmp, proj)
}

/// Read a file inside the scaffolded project, panicking with context on failure.
fn read(proj: &Path, rel: &str) -> String {
	fs::read_to_string(proj.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

// ---------------------------------------------------------------- init

#[test]
fn init_scaffolds_a_named_project() {
	let (_tmp, proj) = project("demo");
	assert!(proj.join("Containerfile").is_file());
	assert!(proj.join(".gitignore").is_file(), "scaffold ships a .gitignore");
	assert!(proj.join("sysroot/usr/libexec/derive-userdb.sh").is_file());

	let manifest = read(&proj, "bootcher.toml");
	assert!(manifest.contains("[general]"), "manifest keys live under [general]: {manifest}");
	assert!(manifest.contains(r#"name = "demo""#), "manifest: {manifest}");
	// init seeds the build matrix with the host arch so `--platform` is optional; the
	// arch shows up as a key under [targets] mapped to a default qcow2.
	assert!(manifest.contains("[targets]"), "manifest: {manifest}");
	assert!(
		manifest.contains("x86_64 = [\"qcow2\"]") || manifest.contains("aarch64 = [\"qcow2\"]"),
		"manifest: {manifest}"
	);

	// init emits the JSON Schemas under `schemas/` and binds the manifest to its
	// schema with the Taplo `#:schema` directive on the very first line, so editors
	// validate/autocomplete. The BOOTCHER_METADATA schema is emitted alongside it.
	assert!(
		proj.join("schemas/bootcher.schema.json").is_file(),
		"init did not write the manifest schema"
	);
	assert!(
		proj.join("schemas/metadata.schema.json").is_file(),
		"init did not write the BOOTCHER_METADATA schema"
	);
	assert_eq!(
		manifest.lines().next(),
		Some("#:schema ./schemas/bootcher.schema.json"),
		"manifest must open with the schema directive: {manifest}",
	);

	// `.containerignore` belongs at the build-context root (the project root, where
	// podman reads it), not inside the `sysroot/` overlay, and excludes artifacts.
	assert!(proj.join(".containerignore").is_file(), ".containerignore at project root");
	assert!(
		!proj.join("sysroot/.containerignore").exists(),
		"no stray .containerignore in sysroot/"
	);
	assert!(
		read(&proj, ".containerignore").contains("output/"),
		"containerignore excludes output/"
	);
}

#[test]
fn init_scaffold_is_secret_free_and_from_fedora_bootc() {
	let (_tmp, proj) = project("demo");

	// Starts from the public base, with the hardening layered inline — no local
	// base image, no arch pin on the FROM line.
	let containerfile = read(&proj, "Containerfile");
	assert_eq!(
		containerfile.lines().next(),
		Some("FROM quay.io/fedora/fedora-bootc:latest"),
		"Containerfile should start FROM stock fedora-bootc",
	);

	// Secrets are injected at provision, never baked: no rendered files, and the
	// Containerfile doesn't chmod/COPY a pull secret.
	assert!(!proj.join("sysroot/usr/lib/userdb/admin.user-privileged").exists());
	assert!(!proj.join("sysroot/etc/ostree/auth.json").exists());
	assert!(
		!containerfile.contains("chmod 0600 /etc/ostree/auth.json"),
		"the pull secret must not be baked",
	);

	// sshd reads the admin key from the provision-injected path.
	let sshd = read(&proj, "sysroot/etc/ssh/sshd_config.d/00-authorized-keys.conf");
	assert!(sshd.contains("/etc/ssh/authorized_keys.d/%u"), "sshd drop-in: {sshd}");

	// The conventional `~/.ssh/authorized_keys` is symlinked to that real path so a
	// future admin who looks in the home directory finds the keys, not an empty dir.
	let tmpfiles = read(&proj, "sysroot/usr/lib/tmpfiles.d/home-admin.conf");
	assert!(
		tmpfiles.contains(
			"L /var/home/admin/.ssh/authorized_keys - - - - /etc/ssh/authorized_keys.d/admin"
		),
		"home tmpfiles must symlink authorized_keys to the managed path: {tmpfiles}",
	);
}

#[test]
fn init_refuses_existing_directory() {
	let (tmp, _proj) = project("demo");
	bootcher_in(tmp.path(), tmp.path())
		.args(["init", "-y", "demo"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("already exists"));
}

#[test]
fn init_creates_nested_path_and_names_after_leaf() {
	// A path with separators creates the missing parents; the image name is the
	// final component.
	let tmp = tempfile::tempdir().unwrap();
	bootcher_in(tmp.path(), tmp.path()).args(["init", "-y", "a/b/c"]).assert().success();
	let proj = tmp.path().join("a/b/c");
	assert!(proj.join("bootcher.toml").is_file(), "nested project not scaffolded");
	assert!(read(&proj, "bootcher.toml").contains(r#"name = "c""#), "name should be the leaf");
}

#[test]
fn init_resolves_relative_parent_path() {
	// `../sibling` from a subdir resolves to a sibling of that subdir.
	let tmp = tempfile::tempdir().unwrap();
	let work = tmp.path().join("work");
	fs::create_dir(&work).unwrap();
	bootcher_in(tmp.path(), &work).args(["init", "-y", "../sibling"]).assert().success();
	let proj = tmp.path().join("sibling");
	assert!(proj.join("bootcher.toml").is_file(), "sibling not created at the resolved path");
	assert!(
		read(&proj, "bootcher.toml").contains(r#"name = "sibling""#),
		"name should be 'sibling'"
	);
}

#[test]
fn init_without_yes_on_non_tty_fails() {
	// No -y and no TTY (assert_cmd) → can't prompt and defaults weren't opted into.
	let tmp = tempfile::tempdir().unwrap();
	bootcher_in(tmp.path(), tmp.path())
		.args(["init", "demo"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("needs a TTY"));
	assert!(!tmp.path().join("demo").exists(), "no project should be created");
}

#[test]
fn init_no_name_scaffolds_empty_cwd_in_place() {
	// No name → scaffold the current directory (which is empty here).
	let tmp = tempfile::tempdir().unwrap();
	bootcher_in(tmp.path(), tmp.path()).args(["init", "-y"]).assert().success();
	assert!(tmp.path().join("bootcher.toml").is_file(), "no manifest in cwd");
	assert!(tmp.path().join("Containerfile").is_file(), "no Containerfile in cwd");
	// No subdir was created.
	assert!(!tmp.path().join("bootcher.toml").parent().unwrap().join("demo").exists());
}

#[test]
fn init_no_name_in_non_empty_cwd_fails() {
	let tmp = tempfile::tempdir().unwrap();
	fs::write(tmp.path().join("existing.txt"), "x").unwrap();
	bootcher_in(tmp.path(), tmp.path())
		.args(["init", "-y"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("isn't empty"));
	assert!(!tmp.path().join("bootcher.toml").exists(), "must not scaffold over existing content");
}

// ---------------------------------------------------------------- project resolution

#[test]
fn build_outside_a_project_points_at_init() {
	// An empty dir is not a bootcher project: the manifest load must fail with a
	// pointer to `init` before anything touches podman.
	let tmp = tempfile::tempdir().unwrap();
	bootcher_in(tmp.path(), tmp.path())
		.args(["build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("not a bootcher project"));
}

#[test]
fn manifest_flag_loads_an_alternate_file() {
	// `--manifest <path>` redirects which file is parsed, leaving the default
	// `bootcher.toml` untouched. Prove it by writing a sibling manifest with a
	// distinct image name plus a build.pre hook that dumps BOOTCHER_METADATA and
	// exits non-zero (aborting before podman, like the other hook tests): if the
	// alternate was loaded, the metadata carries *its* name, not the default's.
	let (tmp, proj) = project("demo");
	let alt = read(&proj, "bootcher.toml").replace(r#"name = "demo""#, r#"name = "demo-ci""#)
		+ "\n[hooks.build]\npre = \"echo \\\"$BOOTCHER_METADATA\\\" > meta.json; exit 1\"\n";
	fs::write(proj.join("bootcher.ci.toml"), alt).unwrap();

	// The flag is global, so it's accepted before the subcommand (the documented form).
	bootcher_in(tmp.path(), &proj)
		.args(["--manifest", "bootcher.ci.toml", "build"])
		.assert()
		.failure();

	let meta = read(&proj, "meta.json");
	assert!(meta.contains(r#""image_name":"demo-ci""#), "alternate manifest not loaded: {meta}");
}

#[test]
fn manifest_flag_error_names_the_missing_file() {
	// A `--manifest` pointing at a nonexistent file fails naming *that* file, proving
	// the flag — not the default `bootcher.toml` — drives the path that's read.
	let (tmp, proj) = project("demo");
	bootcher_in(tmp.path(), &proj)
		.args(["--manifest", "nope.toml", "build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("no nope.toml"));
}

/// Write an alternate manifest in the project that `extend`s `bootcher.toml` and
/// adds a build.pre hook dumping `BOOTCHER_METADATA` (then exits non-zero, aborting
/// before podman). The hook is how each test reads back what the merged manifest
/// resolved to, podman-free. Returns the alternate manifest's filename.
fn extending_manifest(proj: &Path, body: &str) -> &'static str {
	let name = "bootcher.ci.toml";
	let manifest = format!(
		"extend = \"bootcher.toml\"\n{body}\n\
		 [hooks.build]\npre = \"echo \\\"$BOOTCHER_METADATA\\\" > meta.json; exit 1\"\n"
	);
	fs::write(proj.join(name), manifest).unwrap();
	name
}

#[test]
fn extend_inherits_the_base_and_layers_overrides() {
	// A child manifest that only declares `extend` + a `[hooks]` table inherits the
	// base's `[general]` and `[targets]` wholesale — the merged metadata still
	// carries the base project's name and local image ref, proving the parent was
	// loaded underneath.
	let (tmp, proj) = project("demo");
	let ci = extending_manifest(&proj, "");
	bootcher_in(tmp.path(), &proj).args(["--manifest", ci, "build"]).assert().failure();
	let meta = read(&proj, "meta.json");
	assert!(meta.contains(r#""image_name":"demo""#), "base [general] not inherited: {meta}");
	assert!(meta.contains(r#""image_ref":"localhost/demo:latest""#), "meta: {meta}");
}

#[test]
fn extend_child_overrides_a_base_scalar() {
	// The child overrides one `[general]` scalar (the image name) while inheriting the
	// rest: the merge is a deep one, not a wholesale section replacement that would
	// drop the base's `[targets]` and fail to build.
	let (tmp, proj) = project("demo");
	let ci = extending_manifest(&proj, "[general]\nname = \"demo-ci\"");
	bootcher_in(tmp.path(), &proj).args(["--manifest", ci, "build"]).assert().failure();
	let meta = read(&proj, "meta.json");
	assert!(meta.contains(r#""image_name":"demo-ci""#), "child override not applied: {meta}");
}

#[test]
fn extend_missing_parent_is_a_clear_error() {
	// Extending a file that doesn't exist fails naming both the child and the missing
	// parent, rather than a bare "not a bootcher project".
	let (tmp, proj) = project("demo");
	fs::write(proj.join("bootcher.ci.toml"), "extend = \"nope.toml\"\n").unwrap();
	bootcher_in(tmp.path(), &proj)
		.args(["--manifest", "bootcher.ci.toml", "build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("extends \"nope.toml\""));
}

#[test]
fn extend_cycle_is_rejected() {
	// Two manifests extending each other must be caught, not looped on.
	let (tmp, proj) = project("demo");
	fs::write(proj.join("a.toml"), "extend = \"b.toml\"\n").unwrap();
	fs::write(proj.join("b.toml"), "extend = \"a.toml\"\n").unwrap();
	bootcher_in(tmp.path(), &proj)
		.args(["--manifest", "a.toml", "build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("cycle"));
}

// ---------------------------------------------------------------- hooks

/// Append a raw `[hooks.<phase>]` table (the caller writes the whole block) to a
/// scaffolded manifest. The scaffold ships the section only as commented
/// examples, so tests opt in by writing real keys.
fn set_hooks(proj: &Path, table: &str) {
	let path = proj.join("bootcher.toml");
	let manifest = fs::read_to_string(&path).unwrap();
	fs::write(&path, format!("{manifest}\n{table}\n")).unwrap();
}

#[test]
fn init_scaffolds_commented_hooks_examples() {
	// The extension point is discoverable: `init` documents the `[hooks.*]` tables
	// as comments, but a fresh project has no *live* hooks table (an empty one would
	// read as "configured but blank").
	let (_tmp, proj) = project("demo");
	let manifest = read(&proj, "bootcher.toml");
	assert!(manifest.contains("# [hooks.build]"), "manifest documents hooks: {manifest}");
	assert!(
		manifest.contains("# [hooks.upgrade]"),
		"manifest documents the upgrade hook: {manifest}"
	);
	assert!(
		!manifest.lines().any(|l| l.trim().starts_with("[hooks")),
		"a hookless project must not ship a live [hooks.*] table: {manifest}",
	);
}

#[test]
fn build_pre_hook_runs_from_project_root() {
	// A `[hooks.build] pre` hook fires before the container build and from the
	// project root. The hook writes a sentinel (a relative path — so its presence
	// proves the cwd) and then exits non-zero, which aborts the run *before* the
	// build touches podman — keeping this test in the same podman-free envelope as
	// the rest.
	let (tmp, proj) = project("demo");
	set_hooks(&proj, "[hooks.build]\npre = \"echo ran > hook-ran.txt; exit 1\"");

	bootcher_in(tmp.path(), &proj).args(["build"]).assert().failure();
	assert_eq!(read(&proj, "hook-ran.txt").trim(), "ran", "build.pre hook didn't run in the cwd");
}

#[test]
fn hooks_receive_the_bootcher_metadata_env() {
	// Every hook is handed a `BOOTCHER_METADATA` JSON env var. The build.pre hook
	// dumps it to a file and exits non-zero (aborting before podman, like the other
	// hook tests), so we can assert the contract reached the child process: the
	// phase/stage, the project facts, and that build omits the disk/upgrade-only keys.
	let (tmp, proj) = project("demo");
	set_hooks(
		&proj,
		"[hooks.build]\npre = \"echo \\\"$BOOTCHER_METADATA\\\" > meta.json; exit 1\"",
	);

	bootcher_in(tmp.path(), &proj).args(["build"]).assert().failure();

	let meta = read(&proj, "meta.json");
	// serde_json::to_string emits compact `"key":value` pairs (no spaces).
	assert!(meta.contains(r#""phase":"build""#), "phase: {meta}");
	assert!(meta.contains(r#""stage":"pre""#), "stage: {meta}");
	assert!(meta.contains(r#""image_name":"demo""#), "image_name: {meta}");
	assert!(meta.contains(r#""image_ref":"localhost/demo:latest""#), "image_ref: {meta}");
	// build carries none of the disk/upgrade-only fields.
	assert!(!meta.contains("\"targets\""), "build must omit targets: {meta}");
	assert!(!meta.contains("\"output_dir\""), "build must omit output_dir: {meta}");
	assert!(!meta.contains("\"remotes\""), "build must omit remotes: {meta}");
}

#[test]
fn a_failing_hook_aborts_the_command() {
	// A non-zero hook exit must abort with a clear error naming the hook — the
	// pipeline doesn't soldier on past a failed extension step.
	let (tmp, proj) = project("demo");
	set_hooks(&proj, "[hooks.build]\npre = \"exit 3\"");

	bootcher_in(tmp.path(), &proj)
		.args(["build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("build.pre hook failed"));
}

#[test]
fn provision_without_key_on_non_tty_errors() {
	// Inside a project, `provision` collects the admin SSH key up front. With no
	// --ssh-key and a non-TTY stdin (assert_cmd), collection must fail fast — before
	// the build — rather than producing a keyless device. `--skip-build` exercises
	// the disk-only path (no container build) while still requiring the key.
	let (tmp, proj) = project("demo");
	// No --platform: the arch defaults to the manifest's `[targets]` keys
	// (seeded by init).
	bootcher_in(tmp.path(), &proj)
		.args(["provision", "--skip-build"])
		.assert()
		.failure()
		.stderr(predicate::str::contains("no key"));
}

#[test]
fn provision_skip_build_without_registry_or_local_image_errors() {
	// `--skip-build` reuses an already-built container: from local storage, else
	// pulled from the registry. A freshly-scaffolded project is LAN mode (no
	// `[deploy] registry`) and its container was never built, so there's nothing to
	// reuse and nowhere to pull from — the seeding step must fail with a pointer to
	// `bootcher build`, not an opaque podman error. (A registry-mode reuse needs a
	// real registry to pull from, so it lives in the e2e suite.)
	let (tmp, proj) = project("demo");
	// A key so secret collection passes and we actually reach the seeding step;
	// `provision` reads only the `.pub` sibling, so a placeholder line is enough.
	let key = proj.join("admin_key");
	fs::write(format!("{}.pub", key.display()), "ssh-ed25519 AAAA placeholder\n").unwrap();
	bootcher_in(tmp.path(), &proj)
		.args(["provision", "--skip-build", "--ssh-key"])
		.arg(&key)
		.assert()
		.failure()
		.stderr(predicate::str::contains("no `[deploy] registry`"));
}
