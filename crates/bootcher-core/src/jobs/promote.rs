//! `bootcher promote`: move a release channel onto a build that's already in the
//! registry, without rebuilding it.
//!
//! The build is named by its immutable version tag (the `CalVer` tag every deploy
//! pushes). Promotion reads that tag's manifest and writes the very same bytes under
//! the channel tag, so the channel ends up on the identical digest: the signature
//! made when the build was deployed, which is keyed by digest, covers it unchanged,
//! and no layer moves (the blobs are already in the repository). With signing
//! configured, the build's signature is verified before the channel moves, so an
//! unsigned or wrongly signed build never reaches devices that enforce signatures.
//!
//! Promote is a registry operation only: it doesn't SSH into `[deploy] remotes`.
//! Devices following the channel pick the build up on their next auto-update.

use crate::context::Manifest;
use crate::distribution::Client;
use crate::jobs::signing;
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Check the external tools promote needs: `podman` only when signing is configured
/// (the signature check is a verifying `podman pull`). The retag itself is plain
/// HTTP to the registry.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(manifest: &Manifest) -> Result<()> {
	let mut checks = Checks::default();
	if manifest.signing().is_some() {
		checks.bin("podman", preflight::PODMAN_HINT);
	}
	checks.finish()
}

/// Check that `version` can name a build to promote: a valid registry tag, and not
/// one of the project's channels (a channel is a moving pointer, so "promoting" one
/// would copy whatever it happens to point at now, with no durable name for it).
///
/// # Errors
///
/// Returns an error describing what's wrong with `version`.
pub fn check_version(version: &str, channels: &[&str]) -> Result<()> {
	let mut chars = version.chars();
	let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
		&& version.len() <= 128;
	if !valid {
		bail!(
			"`{version}` isn't a valid image tag; pass the version tag a deploy pushed \
			 (e.g. `20261106.142233.g0a1b2c3d4e5`)"
		);
	}
	if channels.contains(&version) {
		bail!(
			"`{version}` is a channel, not a version: pass the immutable version tag of the \
			 build to promote (e.g. `20261106.142233.g0a1b2c3d4e5`)"
		);
	}
	Ok(())
}

/// Promote the build tagged `version` to `channel` for the project in `manifest`.
/// `channel` is validated like `deploy --channel` (it must be declared, and promote
/// needs a registry), and the version's signature is verified first when the
/// manifest configures signing.
///
/// # Errors
///
/// Returns an error in LAN mode, for an undeclared channel or a bad version, if the
/// version tag doesn't exist, if its signature doesn't verify, or if the registry
/// refuses or alters the retag.
pub fn run(manifest: &Manifest, version: &str, channel: &str, tls_verify: bool) -> Result<()> {
	if manifest.registry().is_none() {
		bail!(
			"`promote` needs a registry: it moves a channel tag in `[deploy] registry`, and LAN \
			 deploys have no channels or version tags. Use `bootcher deploy` instead."
		);
	}
	let channel = manifest.resolve_channel(Some(channel))?;
	check_version(version, &manifest.declared_channels())?;
	let channel_ref = manifest.registry_list_ref(&channel).context("registry checked above")?;
	let repo = signing::image_repo(&channel_ref);
	let signing_cfg = manifest.signing();
	let pubkey = signing_cfg.as_ref().map(crate::context::SigningConfig::public_key_path);
	let job = Scope::standalone();
	let digest =
		promote(repo, version, &channel, pubkey.as_deref().map(Path::new), tls_verify, &job)?;
	job.println(format!(
		"promoted {channel_ref} to {version} ({digest}); devices following `{channel}` pick it \
		 up on their next auto-update{}",
		if manifest.deploy_remotes().is_empty() {
			""
		} else {
			" (promote doesn't SSH into `[deploy] remotes`)"
		}
	));
	Ok(())
}

/// Point `<repo>:<channel>` at the manifest `<repo>:<version>` names, byte for byte,
/// and return its digest. With `pubkey`, the build's signature must verify against it
/// first. A channel already on that digest is left alone.
///
/// # Errors
///
/// Returns an error if `version` doesn't exist in `repo`, its signature doesn't
/// verify, the registry refuses the write, or the channel ends up on any other digest.
pub(crate) fn promote(
	repo: &str,
	version: &str,
	channel: &str,
	pubkey: Option<&Path>,
	tls_verify: bool,
	job: &Scope,
) -> Result<String> {
	let mut client = Client::connect(repo, tls_verify)?;
	let Some(build) = client.get_manifest(version)? else {
		bail!(
			"no build tagged `{version}` in {repo}: promote moves a channel onto a build that's \
			 already been deployed — deploy it first, or check the version tag"
		);
	};
	let digest = build.digest.clone();

	// Verify the exact digest about to be promoted, not the tag (which could move in
	// between). Signatures are keyed by digest, so this is the check a device makes
	// when it pulls the channel.
	if let Some(pubkey) = pubkey {
		signing::verify(&format!("{repo}@{digest}"), pubkey, tls_verify, job).with_context(
			|| {
				format!(
					"refusing to promote {repo}:{version} ({digest}) to `{channel}`: it has no valid \
				 signature from {} — devices that enforce signatures would reject it",
					pubkey.display()
				)
			},
		)?;
	}

	if client.get_manifest(channel)?.is_some_and(|m| m.digest == digest) {
		job.println(format!("{repo}:{channel} is already {version} ({digest})"));
		return Ok(digest);
	}

	let written = client.put_manifest(channel, &build)?;
	// Re-read the channel rather than trust the write: the one signature covers the
	// channel only if it resolves to the identical digest.
	let now = client.get_manifest(channel)?.map(|m| m.digest);
	if written != digest || now.as_deref() != Some(digest.as_str()) {
		bail!(
			"promoting {repo}:{version} ({digest}) left `{channel}` on {} — the registry \
			 rewrote the manifest, so the build's signature doesn't cover the channel",
			now.unwrap_or_else(|| "nothing".into())
		);
	}
	Ok(digest)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn check_version_accepts_calver_and_rejects_channels() {
		let channels = ["latest", "stable"];
		for ok in
			["20261106.142233.g0a1b2c3d4e5", "20261106.142233", "20260113.1233.gabc", "v1.2.3"]
		{
			check_version(ok, &channels).unwrap();
		}
		for bad in
			["latest", "stable", "", ".hidden", "-x", "a:b", "a/b", "sha256:abc", &"x".repeat(129)]
		{
			assert!(check_version(bad, &channels).is_err(), "accepted {bad:?}");
		}
	}

	fn manifest(extra: &str) -> Manifest {
		toml::from_str(&format!(
			"[general]\nname = \"app\"\n[targets]\nx86_64 = \"qcow2\"\n{extra}"
		))
		.unwrap()
	}

	#[test]
	fn run_refuses_lan_mode_before_any_network() {
		let err = run(&manifest(""), "20261106.142233", "latest", true).unwrap_err();
		assert!(err.to_string().contains("needs a registry"), "{err}");
	}

	#[test]
	fn run_validates_channel_and_version_before_any_network() {
		let m = manifest("[deploy]\nregistry = \"reg.invalid/org\"\nchannels = [\"stable\"]\n");
		let err = run(&m, "20261106.142233", "nightly", true).unwrap_err();
		assert!(err.to_string().contains("isn't a declared channel"), "{err}");
		let err = run(&m, "stable", "stable", true).unwrap_err();
		assert!(err.to_string().contains("is a channel, not a version"), "{err}");
	}

	/// Build a one-member manifest list from `image` in local storage and push it to
	/// `dest`, signed when `sign` is given; returns the pushed list's digest.
	fn push_list(
		image: &str,
		list: &str,
		dest: &str,
		sign: Option<&signing::SignArgs>,
		dir: &Path,
	) -> String {
		let _ = duct::cmd!("podman", "manifest", "rm", list).stderr_null().unchecked().run();
		duct::cmd!("podman", "manifest", "create", list).stdout_null().run().unwrap();
		duct::cmd!("podman", "manifest", "add", list, format!("containers-storage:{image}"))
			.stdout_null()
			.run()
			.unwrap();
		let digest_file = dir.join("digest");
		let mut argv: Vec<std::ffi::OsString> = vec![
			"manifest".into(),
			"push".into(),
			"--all".into(),
			"--tls-verify=false".into(),
			"--digestfile".into(),
			digest_file.clone().into(),
		];
		if let Some(sign) = sign {
			argv.extend_from_slice(sign.flags());
		}
		argv.extend([list.into(), dest.into()]);
		duct::cmd("podman", &argv).run().expect("pushing the test list failed");
		let _ = duct::cmd!("podman", "manifest", "rm", list).stdout_null().unchecked().run();
		std::fs::read_to_string(digest_file).unwrap().trim().to_owned()
	}

	/// End to end against a real registry: promoting a signed build moves the channel
	/// onto the identical digest, `verify` accepts the channel, and a re-run is a no-op;
	/// an unsigned build is refused with signing on (the channel stays put) and a
	/// missing version fails. Needs podman and network (a cached busybox pull), like the
	/// signing tests.
	#[test]
	fn promote_keeps_the_digest_and_the_signature() {
		const PORT: u16 = 5996;
		const REPO: &str = "localhost:5996/test/app";
		const SIGNED: &str = "20260101.000000.gaaaaaaaaaaaa";
		const UNSIGNED: &str = "20260101.000001.gbbbbbbbbbbbb";

		let _env = crate::tests::support::SIGN_ENV_LOCK
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("cosign.key");
		let pubk = dir.path().join("cosign.pub");
		// SAFETY: SIGN_ENV_LOCK serializes the env-mutating signing tests and no other
		// thread reads this var meanwhile.
		unsafe { std::env::set_var("BOOTCHER_SIGN_PASSPHRASE", "promote-pass") };
		signing::enroll(&key, &pubk, false).unwrap();
		let _reg = crate::tests::support::TestRegistry::start("bootcher-promotetest-reg", PORT);
		let scope = Scope::standalone();
		signing::ensure_push_attachments("localhost:5996/test", &scope).unwrap();

		duct::cmd!("podman", "pull", "-q", "docker.io/library/busybox:latest")
			.stdout_null()
			.run()
			.expect("could not pull busybox");
		// A second, distinct image for the unsigned build (its own digest, so the
		// signed build's signature can't cover it).
		let ctx = dir.path().join("ctx");
		std::fs::create_dir_all(&ctx).unwrap();
		std::fs::write(
			ctx.join("Containerfile"),
			"FROM docker.io/library/busybox:latest\nLABEL bootcher-promote-test=unsigned\n",
		)
		.unwrap();
		let unsigned_img = "localhost:5996/local/unsigned:latest";
		duct::cmd!("podman", "build", "-q", "-t", unsigned_img, ctx.to_str().unwrap())
			.stdout_null()
			.run()
			.unwrap();

		let cfg = crate::context::SigningConfig { key: key.to_str().unwrap().into() };
		let sign = signing::sign_args(&cfg).unwrap();
		let signed_digest = push_list(
			"docker.io/library/busybox:latest",
			"localhost:5996/local/list:signed",
			&format!("{REPO}:{SIGNED}"),
			Some(&sign),
			dir.path(),
		);
		push_list(
			unsigned_img,
			"localhost:5996/local/list:unsigned",
			&format!("{REPO}:{UNSIGNED}"),
			None,
			dir.path(),
		);

		// The signed build lands on the channel under its own digest, and the
		// channel verifies the way a device would check it.
		let got = promote(REPO, SIGNED, "stable", Some(&pubk), false, &scope).unwrap();
		assert_eq!(got, signed_digest);
		let mut client = Client::connect(REPO, false).unwrap();
		let channel = client.get_manifest("stable").unwrap().expect("channel exists");
		let version = client.get_manifest(SIGNED).unwrap().expect("version exists");
		assert_eq!(channel.digest, signed_digest, "the channel isn't on the version's digest");
		assert_eq!(channel.bytes, version.bytes, "the channel's manifest isn't byte-identical");
		signing::verify(&format!("{REPO}:stable"), &pubk, false, &scope)
			.expect("the promoted channel doesn't verify");
		// Idempotent.
		assert_eq!(
			promote(REPO, SIGNED, "stable", Some(&pubk), false, &scope).unwrap(),
			signed_digest
		);

		// An unsigned build is refused while signing is on, leaving the channel alone.
		let err = promote(REPO, UNSIGNED, "stable", Some(&pubk), false, &scope).unwrap_err();
		assert!(format!("{err:#}").contains("refusing to promote"), "{err:#}");
		let channel = client.get_manifest("stable").unwrap().unwrap();
		assert_eq!(channel.digest, signed_digest, "a refused promotion moved the channel");

		// A version that was never pushed.
		let err = promote(REPO, "20260101.000002.gcccccccccccc", "stable", None, false, &scope)
			.unwrap_err();
		assert!(format!("{err:#}").contains("no build tagged"), "{err:#}");
	}
}
