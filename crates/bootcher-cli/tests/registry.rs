//! Registry-level tests for the publishing commands, against a loopback
//! `registry:2` (via podman). Each test gets its own registry port, a throwaway
//! `$HOME` (so the `registries.conf.d` drop-in marking the registry plain-HTTP, the
//! sigstore-attachment config and the signing keys stay out of the real one), an
//! isolated podman store under the target dir, and a git repo holding a tiny project
//! built `FROM` a base image in the same registry — so "a newer base" is just
//! pushing another `base:1`.
//!
//! These need podman and skopeo (and network once, to pull `registry:2`); they
//! don't need sudo, qemu or a VM.

mod common;

use assert_cmd::Command;
use bootcher_core::context::Arch;
use predicates::prelude::*;
use serde_json::Value;
use std::path::PathBuf;

/// One test's world: registry, home, store, project repo.
struct World {
	home: PathBuf,
	store: String,
	proj: PathBuf,
	port: u16,
	// Dropped in this order: the registry, then the store (through `podman unshare`,
	// which can remove its subuid-owned files), then the rest of the dir.
	_reg: common::RegistryGuard,
	_store_guard: common::StoreGuard,
	_dir: tempfile::TempDir,
}

const PASS: &str = "test-passphrase";

impl World {
	/// A world on registry port `port`, with base `base:1` pushed and the project
	/// committed (channels `stable` and `next`).
	fn new(port: u16) -> Self {
		let dir = tempfile::Builder::new()
			.prefix("bootcher-reg-")
			.tempdir_in(env!("CARGO_TARGET_TMPDIR"))
			.unwrap();
		let home = dir.path().join("home");
		let proj = dir.path().join("proj");
		let store = dir.path().join("store").to_str().unwrap().to_owned();
		std::fs::create_dir_all(home.join(".config/containers/registries.conf.d")).unwrap();
		std::fs::write(
			home.join(".config/containers/registries.conf.d/test.conf"),
			format!("[[registry]]\nlocation = \"localhost:{port}\"\ninsecure = true\n"),
		)
		.unwrap();
		std::fs::create_dir_all(&proj).unwrap();
		let store_guard = common::StoreGuard::new(&store);
		let reg = common::RegistryGuard::start(&format!("bootcher-regtest-{port}"), port);
		let w = Self { home, store, proj, port, _reg: reg, _store_guard: store_guard, _dir: dir };
		w.push_base("base v1");
		let arch = Arch::host().unwrap_or(Arch::X86_64);
		w.write(
			"bootcher.toml",
			&format!(
				"[general]\nname = \"app\"\n[targets]\n{arch} = \"qcow2\"\n\
				 [registry]\nurl = \"localhost:{port}/t\"\nchannels = [\"stable\", \"next\"]\n"
			),
		);
		w.write("Containerfile", &format!("FROM localhost:{port}/base:1\nCOPY app.txt /app.txt\n"));
		w.write("app.txt", "app v1\n");
		w.write(".gitignore", "*.key\n");
		w.git(&["init", "-q", "-b", "main"]);
		w.git(&["config", "user.email", "t@example.com"]);
		w.git(&["config", "user.name", "Test"]);
		w.git(&["config", "commit.gpgsign", "false"]);
		w.git(&["config", "tag.gpgsign", "false"]);
		w.commit("init");
		w
	}

	fn env(&self) -> Vec<(&'static str, PathBuf)> {
		let mut env: Vec<(&'static str, PathBuf)> = common::store_env(&self.store).into();
		env.push(("HOME", self.home.clone()));
		env.push(("XDG_CONFIG_HOME", self.home.join(".config")));
		env
	}

	/// `bootcher <args>` in the project, non-interactive.
	fn bootcher(&self, args: &[&str]) -> Command {
		let mut c = Command::cargo_bin("bootcher").unwrap();
		c.current_dir(&self.proj)
			.envs(self.env())
			.env("BOOTCHER_SIGN_PASSPHRASE", PASS)
			.args(args)
			.write_stdin("");
		c
	}

	fn tool(&self, prog: &str, args: &[&str]) -> duct::Expression {
		let mut e = duct::cmd(prog, args).dir(&self.proj);
		for (k, v) in self.env() {
			e = e.env(k, v);
		}
		e
	}

	fn git(&self, args: &[&str]) -> String {
		self.tool("git", args).read().unwrap()
	}

	fn write(&self, rel: &str, content: &str) {
		let p = self.proj.join(rel);
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(p, content).unwrap();
	}

	fn commit(&self, msg: &str) -> String {
		self.git(&["add", "-A"]);
		self.git(&["commit", "-qm", msg]);
		self.git(&["rev-parse", "HEAD"])
	}

	/// Build and push `base:1` with `content`, then drop the local copy so project
	/// builds pull it (and see a newer one) from the registry.
	fn push_base(&self, content: &str) {
		let ctx = self.proj.parent().unwrap().join("base-ctx");
		std::fs::create_dir_all(&ctx).unwrap();
		std::fs::write(ctx.join("base.txt"), content).unwrap();
		std::fs::write(ctx.join("Containerfile"), "FROM scratch\nCOPY base.txt /base.txt\n")
			.unwrap();
		let r = format!("localhost:{}/base:1", self.port);
		let ctx_s = ctx.to_str().unwrap();
		self.tool("podman", &["build", "-q", "-t", &r, ctx_s]).stdout_null().run().unwrap();
		self.tool("podman", &["push", "-q", &r]).run().unwrap();
		self.tool("podman", &["rmi", "-f", &r]).stdout_null().unchecked().run().unwrap();
	}

	fn tag_ref(&self, tag: &str) -> String {
		format!("localhost:{}/t/app:{tag}", self.port)
	}

	/// The raw index `tag` points at, if it exists.
	fn raw(&self, tag: &str) -> Option<Value> {
		let out = self
			.tool("skopeo", &["inspect", "--raw", &format!("docker://{}", self.tag_ref(tag))])
			.stdout_capture()
			.stderr_null()
			.unchecked()
			.run()
			.unwrap();
		out.status.success().then(|| serde_json::from_slice(&out.stdout).unwrap())
	}

	/// The digest `tag` points at.
	fn digest(&self, tag: &str) -> String {
		let out = self
			.tool("skopeo", &["inspect", "--raw", &format!("docker://{}", self.tag_ref(tag))])
			.stdout_capture()
			.run()
			.unwrap();
		bootcher_digest(&out.stdout)
	}

	/// The `bootcher.source` of `tag`'s first member.
	fn source(&self, tag: &str) -> String {
		let idx = self.raw(tag).unwrap();
		let member = idx["manifests"][0]["digest"].as_str().unwrap().to_owned();
		let r = format!("docker://localhost:{}/t/app@{member}", self.port);
		let cfg: Value = serde_json::from_str(
			&self.tool("skopeo", &["inspect", "--config", &r]).read().unwrap(),
		)
		.unwrap();
		cfg["config"]["Labels"]["bootcher.source"].as_str().unwrap_or_default().to_owned()
	}

	fn local_image_id(&self, tag: &str) -> Option<String> {
		self.tool("podman", &["image", "inspect", "-f", "{{.Id}}", tag])
			.stderr_null()
			.unchecked()
			.read()
			.ok()
			.filter(|s| !s.is_empty())
	}

	/// `signing enroll` the project and commit the result.
	fn enroll(&self) {
		self.bootcher(&["signing", "enroll"]).assert().success();
		self.commit("enroll signing");
	}
}

fn bootcher_digest(raw: &[u8]) -> String {
	use sha2::Digest;
	let d = sha2::Sha256::digest(raw);
	let hex = d.iter().fold(String::new(), |mut out, b| {
		use std::fmt::Write as _;
		let _ = write!(out, "{b:02x}");
		out
	});
	format!("sha256:{hex}")
}

fn stderr_has(s: &'static str) -> predicates::str::ContainsPredicate {
	predicate::str::contains(s)
}

// ------------------------------------------------------------------ publish

#[test]
fn publish_absent_same_commit_rebuild_other_commit_and_dirty() {
	let w = World::new(5971);
	let head = w.git(&["rev-parse", "HEAD"]);

	// Absent: built, annotated, pushed as an OCI index with exactly the arch member.
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	let idx = w.raw("v1").expect("v1 published");
	assert_eq!(idx["mediaType"], "application/vnd.oci.image.index.v1+json");
	assert_eq!(idx["annotations"]["bootcher.release"], "v1");
	assert_eq!(idx["manifests"].as_array().unwrap().len(), 1);
	assert_eq!(w.source("v1"), head);
	let d1 = w.digest("v1");

	// Same commit: nothing built, nothing pushed.
	w.bootcher(&["release", "publish", "v1"])
		.assert()
		.success()
		.stderr(stderr_has("already published"));
	assert_eq!(w.digest("v1"), d1);

	// `--rebuild` rebuilds and re-pushes (the source stays HEAD).
	w.bootcher(&["release", "publish", "v1", "--rebuild"]).assert().success();
	assert_eq!(w.source("v1"), head);

	// Dirty tree: refused.
	w.write("app.txt", "app v2\n");
	w.bootcher(&["release", "publish", "v2"])
		.assert()
		.failure()
		.stderr(stderr_has("uncommitted changes"));

	// Another commit over v1: refused (the one rule).
	w.commit("v2");
	w.bootcher(&["release", "publish", "v1"])
		.assert()
		.failure()
		.stderr(stderr_has("only ever holds one commit"));
	assert_eq!(w.source("v1"), head);

	// Invalid and reserved names are refused before anything is built.
	for bad in ["stable", "ci-x", "sha256-x", "-v"] {
		w.bootcher(&["release", "publish", bad]).assert().failure();
	}
}

#[test]
fn publish_skip_build_checks_the_local_source() {
	let w = World::new(5972);
	w.bootcher(&["build"]).assert().success();
	let tag = format!("localhost/app:latest-{}", Arch::host().unwrap_or(Arch::X86_64));
	let id = w.local_image_id(&tag).unwrap();
	// HEAD moves on: the local image records the old commit, so --skip-build refuses.
	w.write("app.txt", "app v2\n");
	w.commit("v2");
	w.bootcher(&["release", "publish", "v2", "--skip-build"])
		.assert()
		.failure()
		.stderr(stderr_has("rebuild, or check out"));
	assert!(w.raw("v2").is_none());
	// A fresh build at HEAD then publishes without rebuilding.
	w.bootcher(&["build"]).assert().success();
	let id2 = w.local_image_id(&tag).unwrap();
	assert_ne!(id, id2);
	w.bootcher(&["release", "publish", "v2", "--skip-build"]).assert().success();
	assert_eq!(w.source("v2"), w.git(&["rev-parse", "HEAD"]));
	// `latest-<arch>` is what was published, untouched.
	assert_eq!(w.local_image_id(&tag).unwrap(), id2);
}

#[test]
fn publish_creates_and_pushes_the_git_tag() {
	let w = World::new(5973);
	let bare = w.proj.parent().unwrap().join("remote.git");
	duct::cmd!("git", "init", "-q", "--bare", &bare).run().unwrap();
	w.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
	w.git(&["push", "-q", "-u", "origin", "main"]);

	w.bootcher(&["release", "publish", "v1", "--git-tag", "push"]).assert().success();
	let remote_tags = duct::cmd!("git", "-C", &bare, "tag").read().unwrap();
	assert!(remote_tags.lines().any(|t| t == "v1"), "{remote_tags}");

	w.write("app.txt", "app v2\n");
	w.commit("v2");
	w.bootcher(&["release", "publish", "v2", "--git-tag", "local"]).assert().success();
	assert!(w.git(&["tag"]).lines().any(|t| t == "v2"));
	assert!(!duct::cmd!("git", "-C", &bare, "tag").read().unwrap().contains("v2"));

	// A git tag at another commit refuses before publishing.
	w.write("app.txt", "app v3\n");
	w.commit("v3");
	w.bootcher(&["release", "publish", "v2"]).assert().failure().stderr(stderr_has("points at"));
	// Off a TTY with no flag, no tag is created.
	w.bootcher(&["release", "publish", "v3"]).assert().success();
	assert!(!w.git(&["tag"]).lines().any(|t| t == "v3"));
}

// ------------------------------------------------------------------ promote

#[test]
fn promote_copies_by_digest_checks_the_base_and_needs_yes() {
	let w = World::new(5974);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	let d1 = w.digest("v1");

	// Moving a channel needs -y off a TTY, even with --skip-remotes.
	w.bootcher(&["channel", "promote", "v1", "stable", "--skip-remotes"])
		.assert()
		.failure()
		.stderr(stderr_has("-y"));
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	// A byte-for-byte copy: same digest, annotation preserved.
	assert_eq!(w.digest("stable"), d1);
	assert_eq!(w.raw("stable").unwrap()["annotations"]["bootcher.release"], "v1");
	// Re-running doesn't write.
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"])
		.assert()
		.success()
		.stderr(stderr_has("not moved"));

	// Unknown release / undeclared channel.
	w.bootcher(&["channel", "promote", "v9", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("publish it first"));
	w.bootcher(&["channel", "promote", "v1", "beta", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("declared channel"));

	// A newer base makes v1 stale: refused unless --allow-stale-base.
	w.push_base("base v2");
	w.bootcher(&["channel", "promote", "v1", "next", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("release refresh v1"));
	assert!(w.raw("next").is_none());
	w.bootcher(&["channel", "promote", "v1", "next", "-y", "--allow-stale-base"])
		.assert()
		.success();
	assert_eq!(w.digest("next"), d1);

	// `release refresh` rebuilds the stale release from its commit (even with HEAD
	// elsewhere and a dirty tree) and moves no channel.
	let commit = w.source("v1");
	w.write("app.txt", "unreleased\n");
	w.commit("unreleased work");
	w.write("dirty.txt", "x\n");
	w.bootcher(&["release", "refresh", "v1"]).assert().success();
	let d1b = w.digest("v1");
	assert_ne!(d1b, d1);
	assert_eq!(w.source("v1"), commit, "a refresh keeps the release's source");
	assert_eq!(w.digest("stable"), d1, "a release refresh moves no channel");
	// Now current again: promotable.
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	assert_eq!(w.digest("stable"), d1b);
	// Nothing stale: a no-op.
	w.bootcher(&["release", "refresh", "v1"])
		.assert()
		.success()
		.stderr(stderr_has("nothing to refresh"));
}

#[test]
fn promote_refuses_a_channel_holding_a_release_of_its_own_name() {
	let w = World::new(5975);
	// Publish a release called `beta` while it isn't a channel, then declare it one.
	w.bootcher(&["release", "publish", "beta"]).assert().success();
	let manifest = std::fs::read_to_string(w.proj.join("bootcher.toml")).unwrap();
	w.write(
		"bootcher.toml",
		&manifest.replace("[\"stable\", \"next\"]", "[\"stable\", \"next\", \"beta\"]"),
	);
	w.commit("declare beta");
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "beta", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("published *as* `beta`"));
}

// ------------------------------------------------------------------ signing

#[test]
fn signed_promote_verifies_and_unsigned_or_old_key_releases_are_refused() {
	let w = World::new(5976);
	// Published before signing: unsigned.
	w.bootcher(&["release", "publish", "v0"]).assert().success();
	w.enroll();
	w.bootcher(&["channel", "promote", "v0", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("isn't signed"));

	// Signed at publish: promotes, and the channel verifies through `signing verify`.
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	w.bootcher(&["signing", "verify", "stable"]).assert().success();
	w.bootcher(&["signing", "verify", "v1"]).assert().success();
	let d1 = w.digest("v1");

	// Another release, not served by any channel.
	w.write("app.txt", "app v2\n");
	w.commit("v2");
	w.bootcher(&["release", "publish", "v2"]).assert().success();

	// Rotate: the served release gains a new-key signature (digest unchanged), the
	// unserved one doesn't.
	let old_pub = w.proj.join("cosign.pub");
	w.bootcher(&["signing", "rotate", "cosign-next", "-y"]).assert().success();
	w.commit("rotate signing");
	assert_eq!(w.digest("v1"), d1);
	let new_pub = w.proj.join("cosign-next.pub");
	for key in [&old_pub, &new_pub] {
		w.bootcher(&["signing", "verify", "stable", "--pubkey", key.to_str().unwrap()])
			.assert()
			.success();
	}
	// v2 carries only the old key's signature: promote (checking the current key)
	// refuses it, with the hint.
	w.bootcher(&["channel", "promote", "v2", "next", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("release refresh v2 --force"));
	// A forced refresh re-signs it with the current key.
	w.bootcher(&["release", "refresh", "v2", "--force"]).assert().success();
	w.bootcher(&["channel", "promote", "v2", "next", "-y"]).assert().success();
	w.bootcher(&["signing", "verify", "next", "--pubkey", new_pub.to_str().unwrap()])
		.assert()
		.success();
}

#[test]
fn enroll_on_a_running_fleet_signs_what_the_channels_serve() {
	let w = World::new(5977);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	let d = w.digest("stable");
	// The served release gets signed in place: digest unchanged, now verifiable.
	w.bootcher(&["signing", "enroll", "-y"]).assert().success();
	assert_eq!(w.digest("stable"), d);
	w.bootcher(&["signing", "verify", "stable"]).assert().success();
	// A second enroll refuses (that's rotate's job).
	w.commit("enroll");
	w.bootcher(&["signing", "enroll", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("signing rotate"));
}
