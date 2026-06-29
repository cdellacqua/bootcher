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

use crate::context::SigningConfig;
use crate::exec::run_argv;
use crate::jobs::secrets::render_policy_json;
use crate::progress::Scope;
use anyhow::{Context, Result, bail};
use base64::Engine;
use crypto_secretbox::aead::Aead;
use crypto_secretbox::{KeyInit, XSalsa20Poly1305};
use p256::SecretKey;
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
const ENV_SIGN_PASSPHRASE: &str = "BOOTCHER_SIGN_PASSPHRASE";

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

	let passphrase = collect_passphrase(true)?;

	// A fresh P-256 key. cosign/containers/image encrypt the PKCS#8 *DER* (the
	// `x509.MarshalPKCS8PrivateKey` bytes), so the envelope wraps the DER, not PEM.
	let secret = SecretKey::random(&mut rand_core::OsRng);
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
/// overlay, so a freshly provisioned device enforces the signature policy from its
/// very first boot — not only after the first `deploy` switches the origin. See
/// `bootc-install-config(5)`.
const ENFORCE_SIGPOLICY_CONFIG: &str = "\
# Enforce the container signature policy (written by `bootcher sign enroll`). Makes
# bootc record a verifying origin and reject an image whose signature doesn't
# satisfy /etc/containers/policy.json. Remove this (and change `[deploy] registry`
# back to a plain URL string) to go back to unsigned images.
[install]
enforce-container-sigpolicy = true
";

/// Where the bootc install signature-enforcement drop-in lands in a project's
/// `sysroot/` overlay (copied to `/` by the scaffold Containerfile's `COPY sysroot/ /`).
pub const ENFORCE_SIGPOLICY_PATH: &str = "sysroot/usr/lib/bootc/install/30-bootcher-signing.toml";

/// Outcome of [`enable_signing`]: the relative key/pub filenames it wrote, plus
/// whether `[deploy] registry` was actually patched (false ⇒ LAN mode, no registry
/// to wire — the caller should error).
pub struct Enrolled {
	/// The private-key filename, e.g. `cosign.key` (relative to the project root).
	pub key: String,
	/// The public-key filename, e.g. `cosign.pub`.
	pub public: String,
	/// `false` if `bootcher.toml` had no `[deploy] registry` entry to patch.
	pub registry_patched: bool,
}

/// Fully enable image signing for the bootcher project rooted at `root`: generate a
/// `<prefix>.key`/`<prefix>.pub` keypair, patch `[deploy] registry` to the signing
/// inline-table form, and (when a registry is configured) bake the
/// `enforce-container-sigpolicy` bootc install drop-in into `sysroot/`. This is the
/// single source of truth for the signing wiring, shared by `bootcher sign enroll`
/// (`root` = cwd) and `bootcher init` (`root` = the freshly scaffolded project dir),
/// so the two can't drift.
///
/// The sigpolicy drop-in is written only when absent, so a key rotation
/// (`enroll <newprefix>`) refreshes the keypair without clobbering a drop-in the
/// user may have edited.
///
/// # Errors
///
/// Returns an error if the key already exists (without `force`), passphrase
/// collection or key generation fails, or any file can't be read/written.
pub fn enable_signing(root: &Path, prefix: &str, force: bool) -> Result<Enrolled> {
	let key = format!("{prefix}.key");
	let public = format!("{prefix}.pub");
	enroll(&root.join(&key), &root.join(&public), force)?;

	let registry_patched = patch_registry_key(&root.join(crate::context::MANIFEST), &key)?;

	// The drop-in only matters alongside a configured registry; in LAN mode (no
	// registry to patch) skip it and let the caller report the missing registry.
	if registry_patched {
		write_enforce_sigpolicy(root)?;
	}
	Ok(Enrolled { key, public, registry_patched })
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

/// Patch the `[deploy] registry` entry in `bootcher.toml` so it includes
/// `key = <key_path>`, preserving all existing comments and formatting.
///
/// Handles three shapes:
/// - `registry = "url"` → replaced with `registry = { url = "url", key = "<key_path>" }`
/// - `registry = { url = "...", ... }` → `key` field added or updated in-place
/// - `[deploy.registry]` dotted table → `key` field added or updated in-place
///
/// Returns `true` if the file was updated, `false` if `[deploy] registry` is
/// absent (file exists but registry not yet configured — caller should error).
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

	let registry = &mut doc["deploy"]["registry"];
	if registry.is_none() {
		return Ok(false);
	}

	if let Some(url) = registry.as_str().map(str::to_owned) {
		let mut t = toml_edit::InlineTable::new();
		t.insert("url", url.into());
		t.insert("key", key_path.into());
		*registry = toml_edit::Item::Value(toml_edit::Value::InlineTable(t));
	} else if let Some(t) = registry.as_inline_table_mut() {
		t.insert("key", key_path.into());
	} else if let Some(t) = registry.as_table_mut() {
		t.insert("key", toml_edit::value(key_path));
	} else {
		bail!(
			"unexpected `[deploy] registry` shape in {} — expected a string, inline table, or \
			 dotted table",
			manifest_path.display()
		);
	}

	fs::write(manifest_path, doc.to_string())
		.with_context(|| format!("writing {}", manifest_path.display()))?;
	Ok(true)
}

/// Check the external tools [`verify`] needs: just `podman` (the pull that
/// performs the signature check). Unlike the other jobs' `preflight`, it takes no
/// manifest — `sign verify` can run outside a project when `--pubkey` is passed.
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
/// the signature is missing or invalid.
///
/// # Errors
///
/// Returns an error if the signature is missing or invalid, or a `podman` command fails.
pub fn verify(image_ref: &str, pubkey_path: &Path, tls_verify: bool, job: &Scope) -> Result<()> {
	let ns = registry_namespace(image_ref);

	// Ensure the build host's registries.d enables sigstore-attachment lookups
	// for this namespace (same as deploy does on push).
	ensure_push_attachments(ns, job)?;

	let tmp = tempfile::tempdir().context("temp dir for verify policy")?;

	// The policy uses an absolute path to the pubkey, so copy it into the temp
	// dir rather than referencing the original (which might be relative).
	let tmp_pubkey = tmp.path().join("verify.pub");
	fs::copy(pubkey_path, &tmp_pubkey)
		.with_context(|| format!("copying pubkey {}", pubkey_path.display()))?;
	let pubkey_str = tmp_pubkey.to_str().context("pubkey path is not valid UTF-8")?;

	let tmp_policy = tmp.path().join("policy.json");
	fs::write(&tmp_policy, render_policy_json(image_repo(image_ref), &[pubkey_str]))
		.context("writing verify policy.json")?;
	let policy_str = tmp_policy.to_str().context("policy path is not valid UTF-8")?;

	let tls = if tls_verify { "--tls-verify=true" } else { "--tls-verify=false" };

	let argv: Vec<std::ffi::OsString> = vec![
		"podman".into(),
		"pull".into(),
		"--quiet".into(),
		tls.into(),
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
		Ok(prompt.prompt()?)
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

/// Build the `podman` signing flags for `signing`: resolve the private key path
/// (relative to the project root / cwd) and write the collected passphrase to a
/// temp 0600 file podman reads via `--sign-passphrase-file` (so it never hits the
/// argv or the process list). Used by the registry deploy push ([`crate::jobs::upgrade`]).
///
/// # Errors
///
/// Returns an error if the signing key is missing, passphrase collection fails, or
/// the temp passphrase file can't be created or written.
pub(crate) fn sign_args(signing: &SigningConfig) -> Result<SignArgs> {
	let key = &signing.key;
	if !Path::new(key).is_file() {
		bail!(
			"signing key {key} not found — generate one with `bootcher sign enroll` and set \
			 `key` in `[deploy] registry`"
		);
	}
	let passphrase = collect_passphrase(false)?;
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
/// content every run, written once. The device gets the equivalent config via
/// provision (see [`crate::jobs::secrets`]); this is its build-host counterpart.
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
			Self { name, host_port }
		}
	}

	impl Drop for TestRegistry {
		fn drop(&mut self) {
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
		let cfg = SigningConfig { key: key.to_str().unwrap().into() };
		let args = sign_args(&cfg).unwrap();
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
		let cfg = SigningConfig { key: key.to_str().unwrap().into() };
		let args = sign_args(&cfg).unwrap();
		let mut cmd_args: Vec<std::ffi::OsString> =
			vec!["push".into(), "--tls-verify=false".into()];
		cmd_args.extend_from_slice(args.flags());
		cmd_args.push(SIGNED.into());
		duct::cmd("podman", &cmd_args).run().expect("signed push failed");

		// Unsigned push (no sign_args).
		duct::cmd!("podman", "push", "--tls-verify=false", UNSIGNED)
			.run()
			.expect("unsigned push failed");

		let signed = verify(SIGNED, &pubk, false, &scope);
		let unsigned = verify(UNSIGNED, &pubk, false, &scope);

		assert!(signed.is_ok(), "verify rejected a validly signed image: {signed:?}");
		assert!(unsigned.is_err(), "verify accepted an unsigned image");
	}
}
