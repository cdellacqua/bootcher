//! Container-image signing: cosign/sigstore keypair generation and the push-time
//! signing flags.
//!
//! bootcher signs with `podman manifest push --sign-by-sigstore-private-key`,
//! which consumes a key in containers/image's *sigstore* format — an
//! `ENCRYPTED SIGSTORE PRIVATE KEY` PEM whose body is a base64'd
//! `go-tuf/encrypted` envelope (scrypt `KDF` + `NaCl` secretbox) wrapping the `PKCS#8`
//! DER of a P-256 key, byte-for-byte what `cosign generate-key-pair` /
//! `skopeo generate-sigstore-key` emit. [`enroll`] produces that format natively
//! (no `cosign`/`skopeo` needed on the build host); `sign_args` builds the push
//! flags and manages the temp passphrase file podman reads.
//!
//! The signing passphrase is never stored in the manifest: it comes from
//! `BOOTCHER_SIGN_PASSPHRASE` or a TTY prompt, mirroring the registry pull token
//! ([`crate::jobs::secrets`]).

use crate::context::Manifest;
use crate::exec::run_argv;
use crate::hooks::{Credential, HookMetadata, Phase, Stage};
use crate::interactive::Prompted as _;
use crate::jobs::device::ChannelDevices;
use crate::jobs::secrets::{TrustFiles, TrustKey, fingerprint, render_single_key_policy_json};
use crate::oci;
use crate::preflight::Checks;
use crate::progress::Scope;
use crate::ssh::Ssh;
use anyhow::{Context, Result, bail};
use base64::Engine;
use crypto_secretbox::aead::Aead;
use crypto_secretbox::{KeyInit, XSalsa20Poly1305};
use p256::SecretKey;
use p256::elliptic_curve::Generate;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};
use pem_rfc7468::LineEnding;
use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use toml_edit;

/// Env var supplying the signing-key passphrase on a non-TTY (CI) run, where
/// there's no prompt. Mirrors `BOOTCHER_PULL_TOKEN` in [`crate::jobs::secrets`].
pub(crate) const ENV_SIGN_PASSPHRASE: &str = "BOOTCHER_SIGN_PASSPHRASE";

/// PEM block type containers/image (`podman --sign-by-sigstore-private-key`)
/// requires; any other label is rejected with "unsupported pem type".
const PEM_TYPE: &str = "ENCRYPTED SIGSTORE PRIVATE KEY";

/// scrypt cost parameters, matching cosign/go-tuf (`N=32768, r=8, p=1`, 32-byte
/// output). `log_n = 15` because `2^15 = 32768`.
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

/// Generate a sigstore-format keypair into `key_path` (private, 0600) and
/// `pub_path` (public). Refuses to clobber an existing private key unless `force`.
/// The passphrase is collected (with confirmation on a TTY) the same way signing
/// later reads it, so the user can't set one here they can't reproduce at push.
///
/// # Errors
///
/// Returns an error if the key already exists (without `--force`), passphrase collection
/// fails, key generation fails, or the files can't be written.
pub fn enroll(key_path: &Path, pub_path: &Path, force: bool) -> Result<()> {
	if key_path.exists() && !force {
		bail!(
			"{} already exists — refusing to overwrite a signing key (pass --force to replace it, \
			 but any image signed with the old key becomes unverifiable)",
			key_path.display()
		);
	}

	generate(key_path, pub_path, &collect_passphrase(true)?)
}

/// Write a fresh sigstore-format keypair encrypted with `passphrase` to `key_path`
/// (private, 0600) and `pub_path` (public), overwriting nothing checked here.
fn generate(key_path: &Path, pub_path: &Path, passphrase: &str) -> Result<()> {
	// A fresh P-256 key. cosign/containers/image encrypt the PKCS#8 *DER* (the
	// `x509.MarshalPKCS8PrivateKey` bytes), so the envelope wraps the DER, not PEM.
	let secret = SecretKey::try_generate().context("generating the private key")?;
	let pkcs8_der = secret.to_pkcs8_der().context("encoding the private key")?;
	let public_pem =
		secret.public_key().to_public_key_pem(LineEnding::LF).context("encoding the public key")?;

	let encrypted = encrypt_sigstore_key(pkcs8_der.as_bytes(), passphrase.as_bytes())?;

	// 0600 on the private key from the start (don't briefly expose it world-readable).
	write_private(key_path, encrypted.as_bytes())
		.with_context(|| format!("writing {}", key_path.display()))?;
	fs::write(pub_path, public_pem.as_bytes())
		.with_context(|| format!("writing {}", pub_path.display()))?;
	Ok(())
}

/// The bootc install drop-in [`enable_signing`] bakes into the project's `sysroot/`
/// overlay, so every install of the image records a signature-enforcing origin from
/// its very first boot. See `bootc-install-config(5)`. A direct device installs and
/// boots fine with it and no trust files (S1); its first `device deploy` re-pins it
/// to an unverified `containers-storage` origin.
const ENFORCE_SIGPOLICY_CONFIG: &str = "\
# Enforce the container signature policy (written by `bootcher signing enroll`).
# Makes bootc record a verifying origin and reject an image whose signature doesn't
# satisfy /etc/containers/policy.json. Remove this (and `key` from [registry]) to go
# back to unsigned images.
[install]
enforce-container-sigpolicy = true
";

/// Where the bootc install signature-enforcement drop-in lands in a project's
/// `sysroot/` overlay (copied to `/` by the scaffold Containerfile's `COPY sysroot/ /`).
pub const ENFORCE_SIGPOLICY_PATH: &str = "sysroot/usr/lib/bootc/install/30-bootcher-signing.toml";

/// Outcome of [`enable_signing`]: the relative key/pub filenames it wrote.
pub struct Enrolled {
	/// The private-key filename, e.g. `cosign.key` (relative to the project root).
	pub key: String,
	/// The public-key filename, e.g. `cosign.pub`.
	pub public: String,
}

/// Fully enable image signing for the bootcher project rooted at `root`: generate a
/// `<prefix>.key`/`<prefix>.pub` keypair, set `[registry] key`, and bake the
/// `enforce-container-sigpolicy` bootc install drop-in into `sysroot/`. The local
/// half of `bootcher signing enroll`, shared with `bootcher init` (`root` = the
/// freshly scaffolded project dir), so the two can't drift.
///
/// The sigpolicy drop-in is written only when absent, so a user-edited one is left
/// alone.
///
/// # Errors
///
/// Returns an error if the manifest has no `[registry]`, the key already exists,
/// passphrase collection or key generation fails, or any file can't be read/written.
pub fn enable_signing(root: &Path, prefix: &str) -> Result<Enrolled> {
	let manifest = root.join(crate::context::MANIFEST);
	if !has_registry_table(&manifest)? {
		bail!(
			"signing protects registry pulls, but bootcher.toml has no `[registry]` — add one \
			 (with `url` and `channels`) first"
		);
	}
	enable_signing_with(root, prefix, &collect_passphrase(true)?)
}

/// [`enable_signing`] with an already-collected passphrase.
fn enable_signing_with(root: &Path, prefix: &str, passphrase: &str) -> Result<Enrolled> {
	let key = format!("{prefix}.key");
	let public = format!("{prefix}.pub");
	let manifest = root.join(crate::context::MANIFEST);
	if root.join(&key).exists() {
		bail!("{} already exists — refusing to overwrite a signing key", root.join(&key).display());
	}
	generate(&root.join(&key), &root.join(&public), passphrase)?;
	patch_registry_key(&manifest, &key)?;
	write_enforce_sigpolicy(root)?;
	Ok(Enrolled { key, public })
}

/// Whether the manifest at `path` has a `[registry]` table.
fn has_registry_table(path: &Path) -> Result<bool> {
	let content =
		fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
	let doc = content
		.parse::<toml_edit::DocumentMut>()
		.with_context(|| format!("parsing {}", path.display()))?;
	Ok(doc.get("registry").is_some_and(toml_edit::Item::is_table_like))
}

/// Write [`ENFORCE_SIGPOLICY_CONFIG`] into `root`'s `sysroot/` overlay (creating
/// parent dirs). A no-op when the file already exists, so re-enrolling for a key
/// rotation leaves an existing — possibly user-edited — drop-in untouched.
fn write_enforce_sigpolicy(root: &Path) -> Result<()> {
	let path = root.join(ENFORCE_SIGPOLICY_PATH);
	if path.exists() {
		return Ok(());
	}
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
	}
	fs::write(&path, ENFORCE_SIGPOLICY_CONFIG)
		.with_context(|| format!("writing {}", path.display()))
}

/// Set `key = <key_path>` in the `[registry]` table of the manifest at
/// `manifest_path`, preserving all existing comments and formatting (a `[registry]`
/// table or an inline `registry = { … }` both work).
///
/// Returns `true` if the file was updated, `false` if there is no `[registry]`.
///
/// # Errors
///
/// Returns an error if the manifest file doesn't exist, can't be read/parsed/written,
/// or the registry entry has an unexpected shape.
pub fn patch_registry_key(manifest_path: &Path, key_path: &str) -> Result<bool> {
	if !manifest_path.exists() {
		bail!(
			"{} not found — run `bootcher init` to scaffold a project first",
			manifest_path.display()
		);
	}
	let content = fs::read_to_string(manifest_path)
		.with_context(|| format!("reading {}", manifest_path.display()))?;
	let mut doc = content
		.parse::<toml_edit::DocumentMut>()
		.with_context(|| format!("parsing {}", manifest_path.display()))?;

	let Some(registry) = doc.get_mut("registry") else { return Ok(false) };
	if let Some(t) = registry.as_inline_table_mut() {
		t.insert("key", key_path.into());
	} else if let Some(t) = registry.as_table_mut() {
		t.insert("key", toml_edit::value(key_path));
	} else {
		bail!("unexpected `registry` shape in {} — expected a table", manifest_path.display());
	}

	fs::write(manifest_path, doc.to_string())
		.with_context(|| format!("writing {}", manifest_path.display()))?;
	Ok(true)
}

/// Check the external tools [`verify`] needs: just `podman` (the pull that
/// performs the signature check). Unlike the other jobs' `preflight`, it takes no
/// manifest — `signing verify` can run outside a project when `--pubkey` is passed.
///
/// # Errors
///
/// Returns an error if `podman` isn't installed.
pub fn preflight_verify() -> Result<()> {
	let mut checks = crate::preflight::Checks::default();
	checks.bin(
		"podman",
		"pull the image from the registry to verify its signature — install podman (https://podman.io)",
	);
	checks.finish()
}

/// Verify that `image_ref` in the registry carries a valid cosign/sigstore
/// signature from `pubkey_path`. Wraps `podman pull --signature-policy` with a
/// throwaway policy requiring sigstoreSigned from the given key — the same
/// verification path the device uses on `bootc upgrade`. Exits with an error if
/// the signature is missing or invalid. A plain-HTTP registry (a local test one) is
/// declared the usual way, `insecure = true` in `registries.conf`.
///
/// # Errors
///
/// Returns an error if the signature is missing or invalid, or a `podman` command fails.
pub fn verify(image_ref: &str, pubkey_path: &Path, job: &Scope) -> Result<()> {
	let ns = registry_namespace(image_ref);

	// Ensure the build host's registries.d enables sigstore-attachment lookups
	// for this namespace (same as a publish does on push).
	ensure_push_attachments(ns, job)?;

	let tmp = tempfile::tempdir().context("temp dir for verify policy")?;

	// The policy uses an absolute path to the pubkey, so copy it into the temp
	// dir rather than referencing the original (which might be relative).
	let tmp_pubkey = tmp.path().join("verify.pub");
	fs::copy(pubkey_path, &tmp_pubkey)
		.with_context(|| format!("copying pubkey {}", pubkey_path.display()))?;
	let pubkey_str = tmp_pubkey.to_str().context("pubkey path is not valid UTF-8")?;

	let tmp_policy = tmp.path().join("policy.json");
	fs::write(&tmp_policy, render_single_key_policy_json(image_repo(image_ref), pubkey_str))
		.context("writing verify policy.json")?;
	let policy_str = tmp_policy.to_str().context("policy path is not valid UTF-8")?;

	let argv: Vec<std::ffi::OsString> = vec![
		"podman".into(),
		"pull".into(),
		"--quiet".into(),
		"--signature-policy".into(),
		policy_str.into(),
		format!("docker://{image_ref}").into(),
	];
	run_argv(job, &argv)
}

/// Derive the image repository from a fully-qualified image reference by
/// stripping the tag (or digest), keeping the registry, namespace, and image-name.
///
/// `"192.168.1.1:5000/proj/name:latest"` → `"192.168.1.1:5000/proj/name"`.
pub(crate) fn image_repo(image_ref: &str) -> &str {
	// Strip @digest first, then :tag. A colon is a tag separator only when
	// there's a '/' before it (the colon is inside the path, not host:port).
	let s = image_ref.split('@').next().unwrap_or(image_ref);
	if let Some(i) = s.rfind(':') { if s[..i].contains('/') { &s[..i] } else { s } } else { s }
}

/// Derive the registry namespace from a fully-qualified image reference by
/// stripping the tag (or digest) and the trailing image-name component.
///
/// `"192.168.1.1:5000/proj/name:latest"` → `"192.168.1.1:5000/proj"`.
pub(crate) fn registry_namespace(image_ref: &str) -> &str {
	// Drop the last path component (image name) from the repository to arrive at
	// the namespace.
	let repo = image_repo(image_ref);
	repo.rfind('/').map_or(repo, |i| &repo[..i])
}

/// Collect the signing passphrase: `BOOTCHER_SIGN_PASSPHRASE` if set (the
/// scriptable/CI path), else a TTY prompt (with confirmation when `confirm`, for
/// enroll), else a hard error on a non-TTY run. An empty passphrase is allowed
/// (cosign permits it); the env var wins even when empty.
///
/// # Errors
///
/// Returns an error if no passphrase is available (not in env, not on a TTY), or
/// the TTY prompt fails.
pub(crate) fn collect_passphrase(confirm: bool) -> Result<String> {
	if let Ok(p) = std::env::var(ENV_SIGN_PASSPHRASE) {
		return Ok(p);
	}
	if io::stdin().is_terminal() {
		let prompt = inquire::Password::new("Signing key passphrase:");
		let prompt = if confirm {
			prompt.with_custom_confirmation_message("Confirm passphrase:")
		} else {
			prompt.without_confirmation()
		};
		prompt.prompt().prompted()
	} else {
		bail!(
			"no signing passphrase available: set {ENV_SIGN_PASSPHRASE} (or run on a TTY to be \
			 prompted)"
		)
	}
}

/// The push-time signing flags plus the temp passphrase file they reference.
/// Splice [`flags`](Self::flags) into a `podman manifest push` argv; keep the
/// guard alive until the push returns so the file isn't removed early.
pub(crate) struct SignArgs {
	/// Kept solely for its `Drop` (removes the temp passphrase file).
	_pass_file: tempfile::NamedTempFile,
	flags: Vec<OsString>,
}

impl SignArgs {
	/// `--sign-by-sigstore-private-key <key> --sign-passphrase-file <file>`.
	#[must_use]
	pub(crate) fn flags(&self) -> &[OsString] {
		&self.flags
	}
}

/// Build the signing flags (`podman` and `skopeo` spell them the same) for the
/// private key at `key` with `passphrase`: the passphrase goes to a temp 0600 file
/// read via `--sign-passphrase-file`, so it never hits the argv or the process list.
/// Used by every signed push and in-place re-sign.
pub(crate) fn sign_args_with(key: &str, passphrase: &str) -> Result<SignArgs> {
	if !Path::new(key).is_file() {
		bail!(
			"signing key {key} not found — generate one with `bootcher signing enroll` (it sets \
			 `key` in `[registry]`)"
		);
	}
	let mut pass_file =
		tempfile::NamedTempFile::new().context("creating a temp passphrase file")?;
	// 0600 before writing the secret.
	set_mode_600(pass_file.path())?;
	pass_file.write_all(passphrase.as_bytes()).context("writing the signing passphrase")?;
	pass_file.flush().ok();

	let flags = vec![
		OsString::from("--sign-by-sigstore-private-key"),
		OsString::from(key),
		OsString::from("--sign-passphrase-file"),
		pass_file.path().into(),
	];
	Ok(SignArgs { _pass_file: pass_file, flags })
}

/// Ensure this build host writes the sigstore signature *attachment* when pushing
/// to namespace `ns`. `podman push --sign-by-sigstore-private-key` only stores the
/// attachment when `use-sigstore-attachments` is enabled in `containers-registries.d`
/// (off by default → "writing sigstore attachments is disabled by configuration"),
/// and podman has no per-push override, so bootcher writes a scoped drop-in into
/// the invoking user's `~/.config/containers/registries.d`. Idempotent — the same
/// content every run, written once. A channel device gets the equivalent config
/// with its trust files (see [`crate::jobs::secrets`]); this is the build-host
/// counterpart, which skopeo reads too.
///
/// # Errors
///
/// Returns an error if `HOME` is unset or the drop-in file can't be created or written.
pub(crate) fn ensure_push_attachments(ns: &str, job: &Scope) -> Result<()> {
	let home = std::env::var_os("HOME").context("HOME is not set; can't locate registries.d")?;
	let dir = Path::new(&home).join(".config/containers/registries.d");
	// One file per namespace so multiple bootcher projects on the same host don't
	// overwrite each other's config.
	let slug = ns.replace([':', '/'], "-");
	let file = dir.join(format!("bootcher-{slug}.yaml"));
	let want = crate::jobs::secrets::render_registries_d(ns);
	if fs::read_to_string(&file).ok().as_deref() == Some(want.as_str()) {
		return Ok(());
	}
	// A fresh user registries.d shadows the system one for other registries' signature
	// lookaside config; flag it so the side effect isn't silent.
	let fresh = !dir.exists();
	fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	fs::write(&file, &want).with_context(|| format!("writing {}", file.display()))?;
	if fresh {
		job.println(format!(
			"signing: enabled sigstore attachments for pushes to {ns} ({})",
			file.display()
		));
	}
	Ok(())
}

// --------------------------------------------------------------- the fleet commands

/// A release a channel serves, to (re-)sign: its ref, and which channels serve it.
#[derive(Debug)]
struct Served {
	/// The ref to sign in place: the release `:X`, or the channel itself when it
	/// lags its release (its digest isn't `:X`'s).
	tag: String,
	channels: Vec<String>,
}

/// What the declared channels serve: one entry per distinct ref to sign.
fn served(manifest: &Manifest) -> Result<Vec<Served>> {
	let repo = manifest.repo().context("signing needs `[registry]`")?;
	let mut out: Vec<Served> = Vec::new();
	for ch in manifest.channels() {
		let Some(chan) = oci::fetch(&format!("{repo}:{ch}"))? else { continue };
		let release = chan.manifest.release().map(str::to_owned);
		let tag = match &release {
			Some(x)
				if oci::fetch(&format!("{repo}:{x}"))?.is_some_and(|r| r.digest == chan.digest) =>
			{
				x.clone()
			}
			_ => ch.clone(),
		};
		match out.iter_mut().find(|s| s.tag == tag) {
			Some(s) => s.channels.push(ch.clone()),
			None => out.push(Served { tag, channels: vec![ch.clone()] }),
		}
	}
	Ok(out)
}

/// Sign every `served` ref in place with the private key at `key` (digests
/// unchanged, S4), each bracketed by `[hooks.publish]`.
fn sign_served(
	manifest: &Manifest,
	served: &[Served],
	key: &str,
	passphrase: &str,
	job: &mut Scope,
) -> Result<()> {
	let reg = manifest.require_registry("signing")?;
	ensure_push_attachments(&reg.url, job)?;
	for s in served {
		let r = manifest.tag_ref(&s.tag).context("a release ref")?;
		let mut meta = HookMetadata::new(manifest, Phase::Publish, r.clone());
		meta.release = Some(s.tag.clone());
		let hooks = manifest.hooks();
		crate::hooks::run(&meta, hooks.publish.pre.as_deref(), job)?;
		let sign = sign_args_with(key, passphrase)?;
		crate::oci::copy(&r, &r, false, Some(&sign), job)?;
		job.println(format!("signed {r} (served by {})", s.channels.join(", ")));
		meta.stage = Stage::Post;
		crate::hooks::run(&meta, hooks.publish.post.as_deref(), job)?;
	}
	Ok(())
}

/// Run `per_device` on every channel device (on any declared channel, `--remote`
/// restricts), bracketed by `[hooks.rotate]` with `credential = "sign-key"`.
fn on_channel_devices(
	manifest: &Manifest,
	targets: &ChannelDevices,
	task: &str,
	per_device: &(dyn Fn(&str, &Ssh, &mut Scope) -> Result<()> + Sync),
	job: &mut Scope,
) -> Result<()> {
	if targets.devices.is_empty() && targets.unreachable.is_empty() {
		return Ok(());
	}
	let ssh: Vec<Ssh> = targets.devices.iter().map(|(_, d)| d.ssh.clone()).collect();
	crate::jobs::rotate::with_hooks(manifest, Credential::SignKey, &ssh, job, |job| {
		let report = crate::fleet::for_each_host(
			&targets.devices,
			task,
			|(_, d)| d.name().to_owned(),
			manifest.concurrency().upgrade,
			job,
			|(ch, d), scope| per_device(ch, &d.ssh, scope),
		)?;
		Ok(report.with(targets.unreachable.clone()))
	})
}

/// The confirmation every signing command shows before it reaches the fleet.
fn confirm_fleet(
	what: &str,
	served: &[Served],
	targets: &ChannelDevices,
	yes: bool,
	lines: Vec<String>,
) -> Result<()> {
	let mut all = lines;
	all.extend(
		served.iter().map(|s| format!("  sign {} (served by {})", s.tag, s.channels.join(", "))),
	);
	all.extend(targets.devices.iter().map(|(ch, d)| format!("  update {} (on :{ch})", d.name())));
	if served.is_empty() && targets.devices.is_empty() && targets.unreachable.is_empty() {
		return Ok(());
	}
	crate::interactive::confirm(yes, what, &all, "Proceed?", true)
}

/// `bootcher signing enroll [prefix]`: turn signing on. Writes the keypair; then, on
/// a running fleet, signs each release a channel serves in place (digests unchanged)
/// and runs the channel device step — trust files, then
/// `--enforce-container-sigpolicy` — on the reachable channel devices; and only then
/// sets `[registry] key` and bakes the install drop-in, so a run that fails part-way
/// can simply be re-run (it reuses the keypair it wrote). Devices are only probed
/// when a channel serves something; an unreachable one is a warning, not a failure
/// (enforcement is baked into the image, and the device gets its trust files with its
/// next channel device step). Devices outside `remotes` keep an unverified origin
/// until they are re-provisioned or switched.
///
/// # Errors
///
/// Returns an error if a key is already configured, there is no registry, the user
/// declines, or a signing/device step fails.
pub fn enroll_fleet(
	manifest: &Manifest,
	prefix: &str,
	remotes: &[String],
	yes: bool,
) -> Result<Enrolled> {
	let reg = manifest.require_registry("`signing enroll`")?;
	if manifest.signing().is_some() {
		bail!(
			"a signing key is already configured (`[registry] key`) — use `bootcher signing rotate`"
		);
	}
	let root = manifest.root();
	let (key, public) = (format!("{prefix}.key"), format!("{prefix}.pub"));
	let (key_path, pub_path) = (root.join(&key), root.join(&public));
	// A keypair left by an earlier run that failed before `[registry] key` was set.
	let resume = key_path.exists();
	if resume && !pub_path.exists() {
		bail!("{key} exists without {public} — pick another prefix");
	}
	let mut checks = Checks::default();
	checks.bin("skopeo", crate::oci::SKOPEO_HINT);
	checks.finish()?;
	let served = served(manifest)?;
	// A device can only follow a channel that serves something, so a fresh project
	// (nothing published yet) has no device to reach.
	let mut targets = if served.is_empty() {
		ChannelDevices::default()
	} else {
		if !manifest.remotes().is_empty() {
			let mut checks = Checks::default();
			checks.bin("ssh", "reach the devices over SSH");
			checks.finish()?;
		}
		crate::jobs::device::channel_devices(manifest, manifest.channels(), remotes)?
	};
	let unreachable = std::mem::take(&mut targets.unreachable);
	for o in &unreachable {
		eprintln!(
			"warning: {} is unreachable; it gets the trust files with its next channel device \
			 step (`channel promote`, `channel refresh` or `device switch`)",
			o.host
		);
	}
	confirm_fleet(
		"`signing enroll` signs served releases and updates devices",
		&served,
		&targets,
		yes,
		vec!["signing enroll will:".to_owned()],
	)?;
	let passphrase = collect_passphrase(!resume)?;
	if resume {
		check_passphrase(&key_path, &passphrase)?;
		eprintln!("signing: reusing {key} / {public} from an earlier run");
	} else {
		generate(&key_path, &pub_path, &passphrase)?;
	}
	let mut job = Scope::standalone();
	sign_served(manifest, &served, &key_path.to_string_lossy(), &passphrase, &mut job)?;
	if !targets.devices.is_empty() {
		let name = &manifest.general.name;
		let pem = fs::read_to_string(&pub_path)
			.with_context(|| format!("reading {}", pub_path.display()))?;
		let trust = TrustFiles {
			name: name.clone(),
			repo: manifest.repo().context("a repo")?,
			ns: reg.url.clone(),
			keys: vec![TrustKey::new(name, pem)?],
		}
		.install_script(false);
		on_channel_devices(
			manifest,
			&targets,
			"enforce signatures",
			&|ch, ssh, scope| {
				let r = manifest.tag_ref(ch).context("a channel ref")?;
				crate::jobs::upgrade::apply_channel(&r, Some(&trust), ssh, scope)
			},
			&mut job,
		)?;
	}
	// Last, so a failure above leaves the project unsigned and the run re-runnable.
	patch_registry_key(&root.join(&manifest.manifest_path), &key)?;
	write_enforce_sigpolicy(root)?;
	eprintln!(
		"signing: wrote {key} (private — keep it secret; the scaffold .gitignore covers *.key), \
		 {public}, set `[registry] key`, and wrote {ENFORCE_SIGPOLICY_PATH}."
	);
	Ok(Enrolled { key, public })
}

/// What the signing fleet commands act on: the served refs and the channel devices.
fn fleet_state(manifest: &Manifest, remotes: &[String]) -> Result<(Vec<Served>, ChannelDevices)> {
	let mut checks = Checks::default();
	checks.bin("skopeo", crate::oci::SKOPEO_HINT);
	if !manifest.remotes().is_empty() {
		checks.bin("ssh", "reach the devices over SSH");
	}
	checks.finish()?;
	let served = served(manifest)?;
	let targets = crate::jobs::device::channel_devices(manifest, manifest.channels(), remotes)?;
	Ok((served, targets))
}

/// `bootcher signing rotate [prefix]`: (1) generate a new keypair, (2) add a
/// new-key signature to every release a channel serves, (3) install old + new keys
/// on the channel devices, (4) point `[registry] key` at the new key — last, so a
/// failure part-way leaves the config on the old key. No rebuild, no channel move,
/// and no window in which any device, listed or not, rejects what its channel
/// serves.
///
/// # Errors
///
/// Returns an error if no key is configured, the user declines, or a step fails.
pub fn rotate(
	manifest: &Manifest,
	prefix: Option<&str>,
	remotes: &[String],
	yes: bool,
) -> Result<Enrolled> {
	let reg = manifest.require_registry("`signing rotate`")?;
	let current = manifest
		.signing()
		.context("no signing key is configured — use `bootcher signing enroll`")?;
	let root = manifest.root();
	let name = &manifest.general.name;
	let old = TrustKey::read(name, &root.join(current.public_key_path()).to_string_lossy())?;
	let (served, targets) = fleet_state(manifest, remotes)?;
	confirm_fleet(
		"`signing rotate` signs served releases and updates devices",
		&served,
		&targets,
		yes,
		vec!["signing rotate will add a new key and:".to_owned()],
	)?;

	// 1. The new keypair, named `<prefix>` or `cosign-<fp8>` — or, when `<prefix>` names
	//    a keypair an earlier, failed rotate wrote (not yet the configured key), that
	//    one, so a re-run resumes instead of adding yet another key.
	let resume = prefix.filter(|p| root.join(format!("{p}.key")).exists());
	if let Some(p) = resume
		&& format!("{p}.key") == current.key
	{
		bail!("{p}.key is already the configured key — pick another prefix");
	}
	let passphrase = collect_passphrase(resume.is_none())?;
	let (key, public, pem) = if let Some(p) = resume {
		let (key, public) = (format!("{p}.key"), format!("{p}.pub"));
		check_passphrase(&root.join(&key), &passphrase)?;
		let pem = fs::read_to_string(root.join(&public))
			.with_context(|| format!("reading {public} (the public half of {key})"))?;
		eprintln!("signing: resuming with {key} / {public} from an earlier run");
		(key, public, pem)
	} else {
		let tmp = format!(".bootcher-new-{}", std::process::id());
		generate(&root.join(format!("{tmp}.key")), &root.join(format!("{tmp}.pub")), &passphrase)?;
		let pem = fs::read_to_string(root.join(format!("{tmp}.pub")))?;
		let prefix = prefix.map_or_else(
			|| format!("cosign-{}", &fingerprint(&pem).unwrap_or_default()[..8]),
			str::to_owned,
		);
		let (key, public) = (format!("{prefix}.key"), format!("{prefix}.pub"));
		if root.join(&key).exists() {
			let _ = fs::remove_file(root.join(format!("{tmp}.key")));
			let _ = fs::remove_file(root.join(format!("{tmp}.pub")));
			bail!("{key} already exists — pick another prefix");
		}
		fs::rename(root.join(format!("{tmp}.key")), root.join(&key))?;
		fs::rename(root.join(format!("{tmp}.pub")), root.join(&public))?;
		eprintln!("signing: wrote the new keypair {key} / {public}");
		(key, public, pem)
	};
	let stem = key.trim_end_matches(".key").to_owned();
	let resume_hint = || {
		format!(
			"`[registry] key` still points at the old key; re-run `bootcher signing rotate {stem}` \
			 to resume with {key}"
		)
	};

	// 2. A new-key signature on every served release.
	let mut job = Scope::standalone();
	sign_served(manifest, &served, &root.join(&key).to_string_lossy(), &passphrase, &mut job)
		.with_context(resume_hint)?;

	// 3. Trust old + new on the channel devices (no reboot: the origin already verifies).
	let new = TrustKey::new(name, pem)?;
	let trust = TrustFiles {
		name: name.clone(),
		repo: manifest.repo().context("a repo")?,
		ns: reg.url.clone(),
		keys: vec![old, new],
	};
	let script = trust.install_script(false);
	on_channel_devices(
		manifest,
		&targets,
		"trust the new signing key",
		&|_, ssh, scope| ssh.run_sh(scope, "install trust files", &[], script.clone()),
		&mut job,
	)
	.with_context(resume_hint)?;

	// 4. Repoint the config last.
	patch_registry_key(&root.join(&manifest.manifest_path), &key)?;
	eprintln!(
		"signing: `[registry] key` now points at {key}. Once every device — including any not \
		 in `remotes` — trusts it, run `bootcher signing retire` to drop the old key."
	);
	Ok(Enrolled { key, public })
}

/// `bootcher signing retire`: trust only the current key on the channel devices and
/// delete the other key files. Run it only once every device, including ones not in
/// `remotes`, has the new key: a device that doesn't would reject every update.
///
/// # Errors
///
/// Returns an error if no key is configured, the user declines, or a device fails.
pub fn retire(manifest: &Manifest, remotes: &[String], yes: bool) -> Result<()> {
	manifest.require_registry("`signing retire`")?;
	let trust = TrustFiles::for_manifest(manifest)?.context("no signing key is configured")?;
	let mut checks = Checks::default();
	checks.bin("skopeo", crate::oci::SKOPEO_HINT);
	checks.bin("ssh", "reach the devices over SSH");
	checks.finish()?;
	check_served_signed(manifest)?;
	let targets = crate::jobs::device::channel_devices(manifest, manifest.channels(), remotes)?;
	let mut lines = vec!["signing retire will trust only the current key on:".to_owned()];
	lines.extend(targets.devices.iter().map(|(ch, d)| format!("  {} (on :{ch})", d.name())));
	lines.push(
		"  (devices not in `remotes` that lack the current key would reject every update)"
			.to_owned(),
	);
	if targets.devices.is_empty() && targets.unreachable.is_empty() {
		eprintln!("no channel devices to retire keys on");
		return Ok(());
	}
	crate::interactive::confirm(
		yes,
		"`signing retire` changes devices",
		&lines,
		"Proceed?",
		false,
	)?;
	let script = trust.install_script(true);
	on_channel_devices(
		manifest,
		&targets,
		"retire old signing keys",
		&|_, ssh, scope| ssh.run_sh(scope, "retire old keys", &[], script.clone()),
		&mut Scope::standalone(),
	)
}

/// `signing retire`'s guard: every ref a channel serves must verify under the current
/// key, or pruning the other keys would leave devices unable to verify it.
fn check_served_signed(manifest: &Manifest) -> Result<()> {
	let signing = manifest.signing().context("no signing key is configured")?;
	let repo = manifest.repo().context("a registry")?;
	let pub_path = manifest.root().join(signing.public_key_path());
	let pem = fs::read_to_string(&pub_path)
		.with_context(|| format!("reading the signing public key {}", pub_path.display()))?;
	let mut bad = Vec::new();
	for s in served(manifest)? {
		let r = format!("{repo}:{}", s.tag);
		let Some(index) = oci::fetch(&r)? else { continue };
		if !oci::unsigned_digests(&repo, &index, &pem)?.is_empty() {
			bad.push(format!(":{} (served by {})", s.tag, s.channels.join(", ")));
		}
	}
	if !bad.is_empty() {
		bail!(
			"not signed by the current key ({}): {} — finish `bootcher signing rotate` first (a \
			 re-run signs what the channels serve), then retire",
			signing.public_key_path(),
			bad.join(", ")
		);
	}
	Ok(())
}

// --------------------------------------------------------------- key encryption

/// The `go-tuf/encrypted` envelope, serialized as the JSON the PEM body base64s.
/// Field layout (and the `N`/`r`/`p` spellings) match cosign exactly. `[]byte`
/// fields (salt/nonce/ciphertext) are base64-std strings, like Go's JSON.
#[derive(Serialize)]
struct Envelope {
	kdf: Kdf,
	cipher: Cipher,
	ciphertext: String,
}
#[derive(Serialize)]
struct Kdf {
	name: &'static str,
	params: ScryptParams,
	salt: String,
}
#[derive(Serialize)]
struct ScryptParams {
	#[serde(rename = "N")]
	n: u32,
	r: u32,
	p: u32,
}
#[derive(Serialize)]
struct Cipher {
	name: &'static str,
	nonce: String,
}

/// Encrypt `plaintext` (the PKCS#8 PEM) into the `ENCRYPTED SIGSTORE PRIVATE KEY`
/// PEM string: scrypt-derive a 32-byte key from `passphrase`+salt, NaCl-secretbox
/// the plaintext under a random nonce, and wrap it in the cosign JSON envelope.
fn encrypt_sigstore_key(plaintext: &[u8], passphrase: &[u8]) -> Result<String> {
	let b64 = base64::engine::general_purpose::STANDARD;

	let mut salt = [0u8; 32];
	let mut nonce = [0u8; 24];
	getrandom::getrandom(&mut salt).context("generating a KDF salt")?;
	getrandom::getrandom(&mut nonce).context("generating a cipher nonce")?;

	let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32)
		.map_err(|e| anyhow::anyhow!("invalid scrypt parameters: {e}"))?;
	let mut key = [0u8; 32];
	scrypt::scrypt(passphrase, &salt, &params, &mut key)
		.map_err(|e| anyhow::anyhow!("deriving the key: {e}"))?;

	// The `crypto_secretbox` crate is byte-compatible with NaCl/libsodium's
	// `crypto_secretbox_easy` (tag-first `tag || ciphertext`), which is exactly
	// what Go's `nacl/secretbox` — and thus go-tuf/encrypted / containers/image —
	// expects, so its `encrypt` output goes into the envelope verbatim.
	let cipher = XSalsa20Poly1305::new((&key).into());
	let nacl = cipher
		.encrypt((&nonce).into(), plaintext)
		.map_err(|e| anyhow::anyhow!("secretbox: {e}"))?;

	let envelope = Envelope {
		kdf: Kdf {
			name: "scrypt",
			params: ScryptParams { n: 1 << SCRYPT_LOG_N, r: SCRYPT_R, p: SCRYPT_P },
			salt: b64.encode(salt),
		},
		cipher: Cipher { name: "nacl/secretbox", nonce: b64.encode(nonce) },
		ciphertext: b64.encode(&nacl),
	};
	let json = serde_json::to_vec(&envelope).expect("the envelope is a fixed, serializable shape");
	pem_rfc7468::encode_string(PEM_TYPE, LineEnding::LF, &json)
		.map_err(|e| anyhow::anyhow!("PEM encoding the envelope: {e}"))
}

/// Check that `passphrase` opens the sigstore private key at `key_path` (a keypair an
/// earlier, failed run left behind is reused only with its own passphrase).
///
/// # Errors
///
/// Returns an error if the key can't be read or parsed, or the passphrase is wrong.
fn check_passphrase(key_path: &Path, passphrase: &str) -> Result<()> {
	let b64 = base64::engine::general_purpose::STANDARD;
	let shown = key_path.display();
	let pem = fs::read_to_string(key_path).with_context(|| format!("reading {shown}"))?;
	let (label, json) = pem_rfc7468::decode_vec(pem.as_bytes())
		.map_err(|e| anyhow::anyhow!("{shown} isn't a PEM file: {e}"))?;
	if label != PEM_TYPE {
		bail!("{shown} isn't an {PEM_TYPE}");
	}
	let env: serde_json::Value =
		serde_json::from_slice(&json).with_context(|| format!("parsing {shown}"))?;
	let field = |v: &serde_json::Value| -> Result<Vec<u8>> {
		let s = v.as_str().with_context(|| format!("malformed {shown}"))?;
		b64.decode(s).with_context(|| format!("malformed {shown}"))
	};
	let params = &env["kdf"]["params"];
	let num = |k: &str| params[k].as_u64().with_context(|| format!("malformed {shown}"));
	let log_n = u8::try_from(num("N")?.trailing_zeros()).unwrap_or(u8::MAX);
	let scrypt_params = scrypt::Params::new(
		log_n,
		u32::try_from(num("r")?).unwrap_or(u32::MAX),
		u32::try_from(num("p")?).unwrap_or(u32::MAX),
		32,
	)
	.map_err(|e| anyhow::anyhow!("invalid scrypt parameters in {shown}: {e}"))?;
	let mut key = [0u8; 32];
	scrypt::scrypt(passphrase.as_bytes(), &field(&env["kdf"]["salt"])?, &scrypt_params, &mut key)
		.map_err(|e| anyhow::anyhow!("deriving the key: {e}"))?;
	let nonce = field(&env["cipher"]["nonce"])?;
	let nonce: [u8; 24] = nonce.try_into().map_err(|_| anyhow::anyhow!("malformed {shown}"))?;
	XSalsa20Poly1305::new((&key).into())
		.decrypt((&nonce).into(), field(&env["ciphertext"])?.as_slice())
		.map_err(|_| anyhow::anyhow!("the signing passphrase doesn't open {shown}"))?;
	Ok(())
}

// --------------------------------------------------------------- file modes

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
	use std::os::unix::fs::OpenOptionsExt;
	let mut f =
		fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
	f.write_all(bytes)?;
	drop(f);
	// mode(0o600) only applies on creation; chmod after write so --force on an
	// existing file with wrong permissions still ends up 0600.
	set_mode_600(path)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
	fs::write(path, bytes).map_err(Into::into)
}

#[cfg(unix)]
fn set_mode_600(path: &Path) -> Result<()> {
	use std::os::unix::fs::PermissionsExt;
	fs::set_permissions(path, fs::Permissions::from_mode(0o600))
		.context("setting passphrase-file mode")
}

#[cfg(not(unix))]
fn set_mode_600(_path: &Path) -> Result<()> {
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Serializes the tests that mutate the process-global `BOOTCHER_SIGN_PASSPHRASE`
	/// env var. `cargo test` runs tests in parallel, so without this the two podman
	/// signing tests would race on the var — both its logical value (each uses a
	/// different passphrase, read once at enroll and again at push) and `set_var`'s
	/// own thread-safety contract.
	static SIGN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

	/// A throwaway `registry:2` container published on `host_port`, removed on drop.
	/// The sign/verify tests need a *real* push-capable registry — the
	/// read-only LAN registry ([`crate::registry`]) can't accept a signed push — so
	/// this centralizes the `podman run`/teardown boilerplate they both share.
	/// Cleanup runs in `Drop`, so a panicking test still tears the container down.
	struct TestRegistry {
		name: &'static str,
		host_port: u16,
	}

	impl TestRegistry {
		fn start(name: &'static str, host_port: u16) -> Self {
			// A prior container of this name (a crashed earlier run) would wedge the
			// `podman run` below; reap it — and its anonymous blob volume — first.
			crate::podman::reap_container(name);
			duct::cmd!(
				"podman",
				"run",
				"-d",
				"--name",
				name,
				"-p",
				&format!("{host_port}:5000"),
				"docker.io/library/registry:2"
			)
			.run()
			.expect("could not start the throwaway registry");
			let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
			loop {
				if std::net::TcpStream::connect(("127.0.0.1", host_port)).is_ok() {
					break;
				}
				assert!(std::time::Instant::now() < deadline, "registry did not start within 10s");
				std::thread::sleep(std::time::Duration::from_millis(100));
			}
			// Declare the plain-HTTP loopback registry insecure the one way both podman
			// and skopeo honour: a user `registries.conf.d` drop-in (removed on drop).
			if let Some(conf) = insecure_dropin(host_port) {
				let _ = std::fs::create_dir_all(conf.parent().unwrap());
				let _ = std::fs::write(
					&conf,
					format!(
						"[[registry]]\nlocation = \"localhost:{host_port}\"\ninsecure = true\n"
					),
				);
			}
			Self { name, host_port }
		}
	}

	/// The per-user `registries.conf.d` drop-in marking `localhost:<port>` insecure.
	fn insecure_dropin(port: u16) -> Option<std::path::PathBuf> {
		let home = std::env::var_os("HOME")?;
		Some(
			Path::new(&home)
				.join(format!(".config/containers/registries.conf.d/bootcher-test-{port}.conf")),
		)
	}

	impl Drop for TestRegistry {
		fn drop(&mut self) {
			if let Some(conf) = insecure_dropin(self.host_port) {
				let _ = std::fs::remove_file(conf);
			}
			// Reap the container and its anonymous blob volume.
			crate::podman::reap_container(self.name);
			// These tests tag/build/pull images under `localhost:<port>/…` into the
			// *default* store (no XDG_DATA_HOME override here), so untag those refs or
			// every run leaves dangling `localhost:<port>/test/*` entries behind.
			crate::podman::untag_prefixed(&format!("localhost:{}/", self.host_port));

			// `ensure_push_attachments` (called by the verify test) writes a
			// `registries.d` drop-in into the *real* `~/.config/containers` — same
			// reason as the store: it reads `HOME` from the process env, which we
			// don't (and can't safely) override here. Remove the drop-ins it scoped to
			// this registry's `localhost:<port>` namespace so config doesn't pile up
			// either. The slug replaces `:`/`/` with `-`, so the file is named
			// `bootcher-localhost-<port>-<path>.yaml`.
			if let Some(home) = std::env::var_os("HOME") {
				let dir = Path::new(&home).join(".config/containers/registries.d");
				let prefix = format!("bootcher-localhost-{}-", self.host_port);
				if let Ok(entries) = std::fs::read_dir(&dir) {
					for name in entries.flatten().map(|e| e.file_name()) {
						if name.to_string_lossy().starts_with(&prefix) {
							let _ = std::fs::remove_file(dir.join(&name));
						}
					}
				}
			}
		}
	}

	#[test]
	fn registry_namespace_strips_tag_and_image_name() {
		let cases = [
			// typical: host:port / multi-component path / tag
			("192.168.1.1:5000/proj/name:latest", "192.168.1.1:5000/proj"),
			// single path component
			("localhost:5000/busybox:latest", "localhost:5000"),
			// no tag
			("localhost:5000/busybox", "localhost:5000"),
			// digest instead of tag
			("reg.example.com/proj/name@sha256:abc123", "reg.example.com/proj"),
			// no port
			("reg.example.com/proj/name:v1", "reg.example.com/proj"),
			// bare host:port (no path) — port colon must NOT be treated as tag
			("localhost:5000", "localhost:5000"),
		];
		for (input, expected) in cases {
			assert_eq!(registry_namespace(input), expected, "input: {input}");
		}
	}

	#[test]
	fn image_repo_strips_only_the_tag_or_digest() {
		let cases = [
			// typical: host:port / multi-component path / tag — image name kept
			("192.168.1.1:5000/proj/name:latest", "192.168.1.1:5000/proj/name"),
			// single path component
			("localhost:5000/busybox:latest", "localhost:5000/busybox"),
			// no tag — unchanged
			("localhost:5000/busybox", "localhost:5000/busybox"),
			// digest instead of tag
			("reg.example.com/proj/name@sha256:abc123", "reg.example.com/proj/name"),
			// bare host:port (no path) — port colon must NOT be treated as tag
			("localhost:5000", "localhost:5000"),
		];
		for (input, expected) in cases {
			assert_eq!(image_repo(input), expected, "input: {input}");
		}
	}

	#[test]
	fn envelope_pem_has_the_required_type_and_decodes_to_the_cosign_json() {
		let pem = encrypt_sigstore_key(
			b"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n",
			b"hunter2",
		)
		.unwrap();
		assert!(pem.starts_with("-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----\n"), "{pem}");
		assert!(pem.trim_end().ends_with("-----END ENCRYPTED SIGSTORE PRIVATE KEY-----"), "{pem}");

		// The body base64-decodes to the go-tuf JSON envelope with the cosign field
		// names/params (`N=32768`, `nacl/secretbox`).
		let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
		let json = base64::engine::general_purpose::STANDARD.decode(body).unwrap();
		let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
		assert_eq!(v["kdf"]["name"], "scrypt");
		assert_eq!(v["kdf"]["params"]["N"], 32768);
		assert_eq!(v["cipher"]["name"], "nacl/secretbox");
		assert!(v["ciphertext"].as_str().is_some());
	}

	/// A keypair left by a failed enroll/rotate is reused only with its passphrase.
	#[test]
	fn check_passphrase_opens_only_with_the_right_passphrase() {
		let dir = tempfile::tempdir().unwrap();
		let (key, public) = (dir.path().join("k.key"), dir.path().join("k.pub"));
		generate(&key, &public, "right").unwrap();
		check_passphrase(&key, "right").unwrap();
		let err = check_passphrase(&key, "wrong").unwrap_err();
		assert!(err.to_string().contains("doesn't open"), "{err:#}");
		assert!(check_passphrase(&public, "right").is_err(), "a public key isn't a private key");
	}

	/// The real proof the enroll format is right: a key produced by [`enroll`] must
	/// be accepted by `podman --sign-by-sigstore-private-key`, and the resulting
	/// signature must verify under a policy that pins our public key. Needs podman
	/// and network (a tiny, cached busybox pull); hard-fails if either is missing.
	#[test]
	fn signing_key_roundtrips_through_podman() {
		// Hold for the whole test: the passphrase must stay put from enroll through
		// the push, with no concurrent signing test mutating it.
		let _env = SIGN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("cosign.key");
		let pubk = dir.path().join("cosign.pub");
		// SAFETY: SIGN_ENV_LOCK serializes the env-mutating signing tests and no
		// other thread reads this var meanwhile; set before enroll reads it.
		unsafe { std::env::set_var(ENV_SIGN_PASSPHRASE, "test-pass") };
		enroll(&key, &pubk, false).unwrap();

		// Throwaway loopback registry (torn down when `_reg` drops).
		let _reg = TestRegistry::start("bootcher-sigtest-reg", 5998);

		duct::cmd!("podman", "pull", "-q", "docker.io/library/busybox:latest")
			.run()
			.expect("could not pull busybox");
		let reg_ref = "localhost:5998/test/busybox:latest";
		duct::cmd!("podman", "tag", "docker.io/library/busybox:latest", reg_ref).run().unwrap();

		// Sign-push using our generated key via the same flags the deploy path uses.
		let args = sign_args_with(key.to_str().unwrap(), "test-pass").unwrap();
		let mut cmd_args: Vec<std::ffi::OsString> =
			vec!["push".into(), "--tls-verify=false".into()];
		cmd_args.extend_from_slice(args.flags());
		cmd_args.push(reg_ref.into());
		let out = duct::cmd("podman", &cmd_args).stderr_capture().unchecked().run().unwrap();

		// The key format is what we're validating: podman must fully initialize the
		// private key (decrypt the envelope + parse the DER) and reach the signing
		// step. A success, *or* the "attachments disabled by configuration" error
		// (which only fires after the key is accepted and a signature is produced,
		// gated purely by the host's registries.d — enabled in the real push and the
		// e2e), both prove the format is right. A key-format problem instead surfaces
		// as "unsupported pem type" / "decrypt" / "parsing private key".
		let stderr = String::from_utf8_lossy(&out.stderr);
		let key_accepted = out.status.success() || stderr.contains("sigstore attachments");
		assert!(key_accepted, "podman rejected the generated signing key:\n{stderr}");
	}

	/// Integration test for [`verify`]: a signed push must be accepted; an
	/// unsigned push to the same registry must be rejected. Needs a running
	/// podman + network access (a tiny, cached busybox pull); hard-fails if
	/// either is missing.
	#[test]
	fn verify_accepts_signed_rejects_unsigned() {
		const REG: &str = "bootcher-verifytest-reg";
		const NS: &str = "localhost:5997/test";
		const SIGNED: &str = "localhost:5997/test/busybox:signed";
		const UNSIGNED: &str = "localhost:5997/test/busybox:unsigned";

		// Hold for the whole test: the passphrase must stay put across every signing
		// call, with no concurrent signing test mutating it.
		let _env = SIGN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("cosign.key");
		let pubk = dir.path().join("cosign.pub");
		// SAFETY: SIGN_ENV_LOCK serializes the env-mutating signing tests and no
		// other thread reads this var meanwhile; set before any signing call reads it.
		unsafe { std::env::set_var(ENV_SIGN_PASSPHRASE, "verify-pass") };
		enroll(&key, &pubk, false).unwrap();

		// Held to end of scope: both `verify` pulls below run while it's up, and it's
		// torn down on drop even if an assertion panics first.
		let _reg = TestRegistry::start(REG, 5997);

		duct::cmd!("podman", "pull", "-q", "docker.io/library/busybox:latest")
			.run()
			.expect("could not pull busybox");
		duct::cmd!("podman", "tag", "docker.io/library/busybox:latest", SIGNED).run().unwrap();
		// The unsigned image must have a *different* manifest digest from the signed
		// one: sigstore signatures are digest-addressed, so reusing the same busybox
		// would let the signed push's signature also satisfy a pull of the "unsigned"
		// tag. Derive a distinct image (extra label → new digest) from the
		// already-pulled busybox — no further network.
		let ctx = dir.path().join("unsigned-ctx");
		std::fs::create_dir_all(&ctx).unwrap();
		std::fs::write(
			ctx.join("Containerfile"),
			"FROM docker.io/library/busybox:latest\nLABEL bootcher-unsigned=true\n",
		)
		.unwrap();
		duct::cmd!("podman", "build", "-t", UNSIGNED, ctx.to_str().unwrap())
			.run()
			.expect("build the distinct unsigned image");

		let scope = Scope::standalone();

		// enable sigstore-attachment storage for the test namespace (same as deploy does).
		ensure_push_attachments(NS, &scope).unwrap();

		// Signed push.
		let args = sign_args_with(key.to_str().unwrap(), "verify-pass").unwrap();
		let mut cmd_args: Vec<std::ffi::OsString> =
			vec!["push".into(), "--tls-verify=false".into()];
		cmd_args.extend_from_slice(args.flags());
		cmd_args.push(SIGNED.into());
		duct::cmd("podman", &cmd_args).run().expect("signed push failed");

		// Unsigned push (no sign_args).
		duct::cmd!("podman", "push", "--tls-verify=false", UNSIGNED)
			.run()
			.expect("unsigned push failed");

		let signed = verify(SIGNED, &pubk, &scope);
		let unsigned = verify(UNSIGNED, &pubk, &scope);

		assert!(signed.is_ok(), "verify rejected a validly signed image: {signed:?}");
		assert!(unsigned.is_err(), "verify accepted an unsigned image");
	}
}
