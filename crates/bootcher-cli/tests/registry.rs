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
	assert_eq!(idx["manifests"][0]["annotations"]["bootcher.release"], "v1");
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
	assert_eq!(w.raw("stable").unwrap()["manifests"][0]["annotations"]["bootcher.release"], "v1");
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
fn promote_from_copies_what_the_source_serves_unless_it_lags() {
	let w = World::new(5980);
	w.bootcher(&["release", "publish", "v1"]).assert().success();

	// The source must be a declared channel that serves something, other than the target.
	w.bootcher(&["channel", "promote", "--from", "next", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("serves nothing yet"));
	w.bootcher(&["channel", "promote", "--from", "beta", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("declared channel"));
	w.bootcher(&["channel", "promote", "--from", "stable", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("names the channel being moved"));

	w.bootcher(&["channel", "promote", "v1", "next", "-y"]).assert().success();
	// Off a TTY a missing target is an error that suggests the `--from` form.
	w.bootcher(&["channel", "promote", "--from", "next", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("bootcher channel promote --from next <channel>"));
	w.bootcher(&["channel", "promote", "--from", "next", "stable", "-y"])
		.assert()
		.success()
		.stderr(stderr_has(":next serves v1"));
	assert_eq!(w.digest("stable"), w.digest("v1"));

	// v1 rebuilt on a newer base: :next lags it, so `--from next` refuses rather than
	// hand :stable a build :next never ran.
	w.push_base("base v2");
	w.bootcher(&["release", "refresh", "v1"]).assert().success();
	let before = w.digest("stable");
	w.bootcher(&["channel", "promote", "--from", "next", "stable", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has(":next lags v1"));
	assert_eq!(w.digest("stable"), before, "a refused promote moved the channel");
	// Once :next runs the rebuild, it promotes on.
	w.bootcher(&["channel", "promote", "v1", "next", "-y"]).assert().success();
	w.bootcher(&["channel", "promote", "--from", "next", "stable", "-y"]).assert().success();
	assert_eq!(w.digest("stable"), w.digest("v1"));
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

// ------------------------------------------------------------------ status, list, refresh

#[test]
fn status_and_list_report_release_commit_staleness_and_channels() {
	let w = World::new(5978);
	let head = w.git(&["rev-parse", "HEAD"]);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();

	let out = w
		.bootcher(&["channel", "status", "--format", "json"])
		.assert()
		.success()
		.get_output()
		.stdout
		.clone();
	let rows: Value = serde_json::from_slice(&out).unwrap();
	let stable = rows.as_array().unwrap().iter().find(|r| r["channel"] == "stable").unwrap();
	assert_eq!(stable["release"], "v1");
	assert_eq!(stable["commit"], head.as_str());
	assert_eq!((stable["stale"].as_bool(), stable["lags"].as_bool()), (Some(false), Some(false)));
	let next = rows.as_array().unwrap().iter().find(|r| r["channel"] == "next").unwrap();
	assert_eq!(next["published"], false);

	// The single-channel template CI gates on.
	w.bootcher(&["channel", "status", "stable", "--format", "{{ .Release }} {{ .Stale }}"])
		.assert()
		.success()
		.stdout("v1 false\n");
	w.bootcher(&["channel", "status", "--format", "{{ .Stale }}"])
		.assert()
		.failure()
		.stderr(stderr_has("exactly one channel"));

	w.push_base("base v2");
	w.bootcher(&["channel", "status", "stable", "--format", "{{ .Stale }}"])
		.assert()
		.success()
		.stdout("true\n");

	let out = w
		.bootcher(&["release", "list", "--format", "json"])
		.assert()
		.success()
		.get_output()
		.stdout
		.clone();
	let rows: Value = serde_json::from_slice(&out).unwrap();
	let v1 = &rows.as_array().unwrap()[0];
	assert_eq!(v1["release"], "v1");
	assert_eq!(v1["channels"], serde_json::json!(["stable"]));
	assert_eq!(v1["stale"], true);
	// The channel tags themselves aren't listed as releases.
	assert_eq!(rows.as_array().unwrap().len(), 1);
	w.bootcher(&["release", "list", "--format", "{{ .Release }}={{ .Commit }}"])
		.assert()
		.success()
		.stdout(format!("v1={head}\n"));
}

#[test]
fn channel_refresh_rebuilds_repoints_and_honours_exclude() {
	let w = World::new(5979);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "next", "-y"]).assert().success();
	let d1 = w.digest("v1");
	w.bootcher(&["build"]).assert().success();
	let latest = format!("localhost/app:latest-{}", Arch::host().unwrap_or(Arch::X86_64));
	let latest_id = w.local_image_id(&latest).unwrap();

	// Nothing stale: nothing happens.
	w.bootcher(&["channel", "refresh", "-y", "--skip-remotes"]).assert().success();
	assert_eq!(w.digest("v1"), d1);

	// Moving a channel needs -y even with --skip-remotes.
	w.push_base("base v2");
	w.bootcher(&["channel", "refresh", "--skip-remotes"])
		.assert()
		.failure()
		.stderr(stderr_has("-y"));

	// HEAD moves on (unreleased work): the refresh rebuilds v1's own commit.
	let commit = w.source("v1");
	w.write("app.txt", "unreleased\n");
	w.commit("unreleased");
	w.bootcher(&["channel", "refresh", "-y", "--skip-remotes", "--exclude", "next"])
		.assert()
		.success();
	let d2 = w.digest("v1");
	assert_ne!(d2, d1);
	assert_eq!(w.source("v1"), commit);
	assert_eq!(w.digest("stable"), d2, "stable re-pointed at the refreshed v1");
	assert_eq!(w.digest("next"), d1, "--exclude leaves next alone");
	assert_eq!(w.local_image_id(&latest).unwrap(), latest_id, "latest-<arch> untouched");
	w.bootcher(&["channel", "status", "stable", "--format", "{{ .Stale }} {{ .Lags }}"])
		.assert()
		.success()
		.stdout("false false\n");
	// next now lags its (refreshed) release: a refresh re-copies it without a rebuild.
	w.bootcher(&["channel", "status", "next", "--format", "{{ .Lags }}"])
		.assert()
		.success()
		.stdout("true\n");
	w.bootcher(&["channel", "refresh", "next", "-y", "--skip-remotes"]).assert().success();
	assert_eq!(w.digest("next"), d2);
}

#[test]
fn channel_refresh_skip_build_needs_one_release_built_from_its_commit() {
	let w = World::new(5969);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	w.write("app.txt", "app v2\n");
	w.commit("v2");
	w.bootcher(&["release", "publish", "v2"]).assert().success();
	w.bootcher(&["channel", "promote", "v2", "next", "-y"]).assert().success();
	w.push_base("base v2");

	// Two releases served: --skip-build can't take local images for both.
	w.bootcher(&["channel", "refresh", "-y", "--skip-remotes", "--skip-build"])
		.assert()
		.failure()
		.stderr(stderr_has("serve v1, v2"));
	// The local image is v2's commit, not v1's: refused.
	w.bootcher(&["build"]).assert().success();
	w.bootcher(&["channel", "refresh", "stable", "-y", "--skip-remotes", "--skip-build"])
		.assert()
		.failure()
		.stderr(stderr_has("rebuild, or check out"));
	// The right commit's local image: published without a build here.
	w.bootcher(&["channel", "refresh", "next", "-y", "--skip-remotes", "--skip-build"])
		.assert()
		.success();
	assert_eq!(w.digest("next"), w.digest("v2"));
	w.bootcher(&["channel", "status", "next", "--format", "{{ .Stale }}"])
		.assert()
		.success()
		.stdout("false\n");
}

// ------------------------------------------------------------------ fresh-project enroll, resume

impl World {
	/// Prepend `remotes` pointing at closed loopback ports (not-yet-booted devices).
	fn add_unbooted_remotes(&self) {
		let manifest = std::fs::read_to_string(self.proj.join("bootcher.toml")).unwrap();
		let remotes: Vec<String> = (0..2)
			.map(|_| {
				let p = bootcher_core::qemu::free_port().unwrap();
				format!(
					"{{ remote = \"ssh://admin@127.0.0.1:{p}\", ssh_opts = [\"-o\", \
					 \"ConnectTimeout=2\"] }}"
				)
			})
			.collect();
		self.write("bootcher.toml", &format!("remotes = [{}]\n{manifest}", remotes.join(", ")));
	}

	/// Append `[hooks.<phase>] <stage> = "<cmd>"` to the manifest.
	fn add_hook(&self, phase: &str, stage: &str, cmd: &str) {
		let manifest = std::fs::read_to_string(self.proj.join("bootcher.toml")).unwrap();
		let cmd = cmd.replace('\\', "\\\\").replace('"', "\\\"");
		self.write("bootcher.toml", &format!("{manifest}\n[hooks.{phase}]\n{stage} = \"{cmd}\"\n"));
	}

	fn manifest_has_key(&self) -> bool {
		std::fs::read_to_string(self.proj.join("bootcher.toml")).unwrap().contains("key = ")
	}

	/// `skopeo copy` between two tags of the project repo, by plain HTTP.
	fn copy_tag(&self, from: &str, to: &str) {
		self.tool(
			"skopeo",
			&[
				"copy",
				"--all",
				"--preserve-digests",
				"--src-tls-verify=false",
				"--dest-tls-verify=false",
				&format!("docker://{}", self.tag_ref(from)),
				&format!("docker://{}", self.tag_ref(to)),
			],
		)
		.stdout_null()
		.run()
		.unwrap();
	}
}

/// Enrolling before the first provision just works: remotes declared but not yet
/// booted, nothing served, no TTY and no `-y`.
#[test]
fn enroll_on_a_fresh_project_needs_no_devices_and_no_yes() {
	let w = World::new(5968);
	w.add_unbooted_remotes();
	w.commit("remotes");
	w.bootcher(&["signing", "enroll"]).assert().success();
	assert!(w.manifest_has_key());
	assert!(w.proj.join("cosign.key").is_file());
	assert!(w.proj.join("sysroot/usr/lib/bootc/install/30-bootcher-signing.toml").is_file());
}

/// With a release served and the devices unreachable, `-y` is needed (there is
/// something to sign) but the unreachable devices are only a warning.
#[test]
fn enroll_warns_about_unreachable_devices_and_a_failed_run_resumes() {
	let w = World::new(5967);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	w.add_unbooted_remotes();
	// The signing step fails (a failing publish hook brackets each re-sign).
	let marker = w.proj.parent().unwrap().join("fail-publish");
	std::fs::write(&marker, "").unwrap();
	w.add_hook("publish", "pre", &format!("test ! -e {}", marker.display()));
	w.commit("remotes, hook");
	w.bootcher(&["signing", "enroll"]).assert().failure().stderr(stderr_has("-y"));
	w.bootcher(&["signing", "enroll", "-y"]).assert().failure();
	assert!(!w.manifest_has_key(), "`[registry] key` written before the run succeeded");
	assert!(w.proj.join("cosign.key").is_file());
	// A wrong passphrase doesn't reuse the keypair.
	std::fs::remove_file(&marker).unwrap();
	w.bootcher(&["signing", "enroll", "-y"])
		.env("BOOTCHER_SIGN_PASSPHRASE", "not-it")
		.assert()
		.failure()
		.stderr(stderr_has("doesn't open"));
	// The re-run reuses the keypair and succeeds, the devices only warned about.
	w.bootcher(&["signing", "enroll", "-y"])
		.assert()
		.success()
		.stderr(stderr_has("reusing cosign.key").and(stderr_has("unreachable")));
	assert!(w.manifest_has_key());
	w.bootcher(&["signing", "verify", "stable"]).assert().success();
}

/// A rotate that fails part-way resumes with the keypair it wrote; `signing retire`
/// refuses while a served ref isn't signed by the current key.
#[test]
fn rotate_resumes_and_retire_checks_what_is_served() {
	let w = World::new(5966);
	w.bootcher(&["release", "publish", "v0"]).assert().success();
	w.enroll();
	w.write("app.txt", "app v1\nsigned\n");
	w.commit("v1");
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();

	let marker = w.proj.parent().unwrap().join("fail-publish");
	std::fs::write(&marker, "").unwrap();
	w.add_hook("publish", "pre", &format!("test ! -e {}", marker.display()));
	w.commit("hook");
	w.bootcher(&["signing", "rotate", "cosign-next", "-y"])
		.assert()
		.failure()
		.stderr(stderr_has("signing rotate cosign-next"));
	assert!(
		std::fs::read_to_string(w.proj.join("bootcher.toml")).unwrap().contains("cosign.key"),
		"a failed rotate repointed the config"
	);
	let pem = std::fs::read_to_string(w.proj.join("cosign-next.pub")).unwrap();
	std::fs::remove_file(&marker).unwrap();
	w.bootcher(&["signing", "rotate", "cosign-next", "-y"])
		.assert()
		.success()
		.stderr(stderr_has("resuming with cosign-next.key"));
	assert_eq!(std::fs::read_to_string(w.proj.join("cosign-next.pub")).unwrap(), pem);
	w.commit("rotated");
	let new_pub = w.proj.join("cosign-next.pub");
	w.bootcher(&["signing", "verify", "stable", "--pubkey", new_pub.to_str().unwrap()])
		.assert()
		.success();

	// An unsigned release copied onto a channel behind bootcher's back: retire refuses.
	w.copy_tag("v0", "next");
	w.bootcher(&["signing", "retire", "-y"]).assert().failure().stderr(
		stderr_has("not signed by the current key").and(stderr_has(":v0 (served by next)")),
	);
	// Once every served ref verifies, retire goes ahead (no device to update here).
	w.copy_tag("v1", "next");
	w.bootcher(&["signing", "retire", "-y"]).assert().success();
}

// ------------------------------------------------------------------ git tag preflight

#[test]
fn an_invalid_git_tag_name_fails_before_anything_is_pushed() {
	let w = World::new(5965);
	// A valid OCI tag, but not a valid git ref.
	w.bootcher(&["release", "publish", "a..b", "--git-tag", "local"])
		.assert()
		.failure()
		.stderr(stderr_has("valid git tag"));
	assert!(w.raw("a..b").is_none(), "pushed before the git tag check");
	w.bootcher(&["release", "publish", "a..b", "--git-tag", "no"]).assert().success();
}

// ------------------------------------------------------------------ refresh races

/// A promote that moves a channel while `channel refresh` rebuilds its release:
/// the refresh leaves that channel alone.
#[test]
fn channel_refresh_skips_a_channel_a_concurrent_promote_moved() {
	let w = World::new(5964);
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	w.write("app.txt", "app v2\n");
	w.commit("v2");
	w.bootcher(&["release", "publish", "v2"]).assert().success();
	w.bootcher(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	w.push_base("base v2");
	// The "concurrent promote": v2 copied onto :stable right after v1's re-push.
	let to = format!("docker://{}", w.tag_ref("stable"));
	let from = format!("docker://{}", w.tag_ref("v2"));
	w.add_hook(
		"publish",
		"post",
		&format!(
			"skopeo copy --all --preserve-digests --src-tls-verify=false \
			 --dest-tls-verify=false {from} {to}"
		),
	);
	let d1 = w.digest("v1");
	w.bootcher(&["channel", "refresh", "stable", "-y", "--skip-remotes"])
		.assert()
		.success()
		.stderr(stderr_has("moved since this run read it"));
	assert_ne!(w.digest("v1"), d1, "v1 was rebuilt");
	assert_eq!(w.digest("stable"), w.digest("v2"), "the refresh overwrote the promote");
}

/// A rebuild that doesn't land on the base the run judged current fails loudly
/// instead of re-pushing the release.
#[test]
fn refresh_fails_when_the_rebuild_is_not_on_the_current_base() {
	let w = World::new(5963);
	// The release's own config carries a build hook that, when armed, moves `base:1`
	// to another image just before the rebuild pulls it.
	let marker = w.proj.parent().unwrap().join("move-base");
	let spare = format!("docker://localhost:{}/base:spare", w.port);
	let base = format!("docker://localhost:{}/base:1", w.port);
	w.add_hook(
		"build",
		"pre",
		&format!(
			"if [ -e {m} ]; then rm {m}; skopeo copy --src-tls-verify=false \
			 --dest-tls-verify=false {spare} {base}; fi",
			m = marker.display()
		),
	);
	w.commit("build hook");
	w.bootcher(&["release", "publish", "v1"]).assert().success();
	let d1 = w.digest("v1");
	w.push_base("base v2");
	w.tool("skopeo", &["copy", "--src-tls-verify=false", "--dest-tls-verify=false", &base, &spare])
		.stdout_null()
		.run()
		.unwrap();
	w.push_base("base v3");
	std::fs::write(&marker, "").unwrap();
	// The run reads base v3 as current; the hook swaps in v2 before the build.
	w.bootcher(&["release", "refresh", "v1"])
		.assert()
		.failure()
		.stderr(stderr_has("still isn't on the current base"));
	assert_eq!(w.digest("v1"), d1, "a release not on the current base was pushed");
}

// ------------------------------------------------------------------ project in a subdirectory

/// A project that isn't at the top of its git repository: the refreshes build the
/// release's commit from the same subdirectory of the worktree.
#[test]
fn refreshes_work_for_a_project_in_a_subdirectory() {
	let w = World::new(5962);
	let sub = w.proj.join("image");
	std::fs::create_dir_all(&sub).unwrap();
	for f in ["bootcher.toml", "Containerfile", "app.txt"] {
		w.git(&["mv", f, &format!("image/{f}")]);
	}
	w.commit("move the project into image/");
	let run = |args: &[&str]| {
		let mut c = w.bootcher(args);
		c.current_dir(&sub);
		c
	};
	run(&["release", "publish", "v1"]).assert().success();
	let commit = w.source("v1");
	run(&["channel", "promote", "v1", "stable", "-y"]).assert().success();
	let d1 = w.digest("v1");
	// HEAD moves on, so a refresh really builds from the worktree.
	w.write("image/app.txt", "unreleased\n");
	w.commit("unreleased");
	w.push_base("base v2");
	run(&["release", "refresh", "v1"]).assert().success();
	let d2 = w.digest("v1");
	assert_ne!(d2, d1);
	assert_eq!(w.source("v1"), commit);
	w.push_base("base v3");
	run(&["channel", "refresh", "stable", "-y", "--skip-remotes"]).assert().success();
	assert_ne!(w.digest("stable"), d2);
	assert_eq!(w.digest("stable"), w.digest("v1"));
	assert_eq!(w.source("v1"), commit);
}
