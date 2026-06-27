//! Provision-time secret collection + `image-builder` blueprint generation.
//!
//! Secrets are never persisted to the project tree. At provision/image time they
//! are collected — env vars if set (the scriptable/CI path, also honoured on a
//! TTY), else a TTY prompt, else a hard error — and injected into the freshly
//! provisioned device's persistent `/etc` via
//! a blueprint `customizations.files` entry. Those land as local `/etc` additions, so
//! the ostree 3-way merge keeps them across upgrades (the image ships no
//! `/usr/etc` default for them). The built container itself stays secret-free.

use crate::context::{
	DEVICE_ADMIN_AUTHORIZED_KEYS, DEVICE_AUTH_JSON, DEVICE_COSIGN_PUBKEY_DIR, DEVICE_POLICY_JSON,
	DEVICE_REGISTRIES_D, Manifest, SigningConfig,
};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Serialize;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Env vars supplying the registry pull credential on a non-TTY (CI) provision,
/// where there's no prompt.
const ENV_PULL_USER: &str = "BOOTCHER_PULL_USER";
const ENV_PULL_TOKEN: &str = "BOOTCHER_PULL_TOKEN";

/// The files to inject into a freshly provisioned device's `/etc`.
pub struct Provisioning {
	/// Admin `authorized_keys` content (one SSH public key per line).
	authorized_keys: String,
	/// Registry pull secret (`auth.json` JSON), present only in registry mode.
	auth_json: Option<String>,
	/// Signature-enforcement files, present only when signing is configured in `[deploy] registry`.
	signing: Option<SigningFiles>,
}

/// The signing-mode device files: the cosign public key plus the rendered
/// `policy.json` and `registries.d` config that make `bootc upgrade` require a
/// valid signature. All three land in the device's persistent `/etc` (see
/// [`Provisioning::blueprint`]); `rotate sign-key` later edits the policy + adds
/// keys over ssh.
struct SigningFiles {
	/// Image name — the on-device public-key filename ([`device_pubkey_path`]).
	name: String,
	/// Cosign public-key PEM, verbatim from the project's `<key>.pub`.
	pubkey_pem: String,
	/// `policy.json`: `default: reject` + a `sigstoreSigned`/`keyPath` requirement
	/// for the registry namespace.
	policy_json: String,
	/// `registries.d` YAML enabling `use-sigstore-attachments` for the namespace,
	/// so the device fetches the signature alongside the image on pull.
	registries_d: String,
}

impl Provisioning {
	/// Collect the admin SSH key (always) and, when the manifest configures a
	/// `registry`, the pull credential (rendered to `auth.json`). `key` is a
	/// private SSH key path (the `.pub` sibling is read for the public key content),
	/// or `None` to discover/prompt on a TTY. `skip_pull_check` (`--skip-pull-check`)
	/// bypasses the registry-credential verification (see `verify_pull_login`).
	/// `anonymous` (`--anonymous`) declares the registry public: no pull credential is
	/// collected and no `auth.json` is baked, so the device pulls anonymously. It's a
	/// no-op in LAN mode (no registry ⇒ no credential either way).
	///
	/// # Panics
	///
	/// Panics if signing is configured but `registry` is absent (structural invariant
	/// upheld by manifest validation).
	///
	/// # Errors
	///
	/// Returns an error if key or credential collection fails, or registry verification fails.
	pub fn collect(
		manifest: &Manifest,
		key: Option<&str>,
		skip_pull_check: bool,
		anonymous: bool,
	) -> Result<Self> {
		Ok(Self::collect_with_key(manifest, key, skip_pull_check, anonymous)?.0)
	}

	/// Like [`Self::collect`], but also returns the resolved admin **private** key
	/// path. `bootcher takeover` needs it as the post-reboot `admin@host` SSH
	/// identity (the stock cloud user is gone by then), not just the public half
	/// baked into `authorized_keys`. The picker/discovery on a TTY (when `key` is
	/// `None`) runs exactly once.
	///
	/// # Panics
	///
	/// Panics if signing is configured but `registry` is absent (structural invariant
	/// upheld by manifest validation).
	///
	/// # Errors
	///
	/// Returns an error if key or credential collection fails, or registry verification fails.
	pub fn collect_with_key(
		manifest: &Manifest,
		key: Option<&str>,
		skip_pull_check: bool,
		anonymous: bool,
	) -> Result<(Self, PathBuf)> {
		let (authorized_keys, key_path) = collect_authorized_keys(key)?;
		let auth_json = match manifest.registry() {
			// `--anonymous`: the registry is public, so collect no pull credential and
			// bake no `auth.json` — the device pulls anonymously. Without it, a
			// registry-mode provision requires a credential (env or TTY, else a hard
			// error); the flag is the explicit opt-in that keeps that default fail-closed.
			Some(_) if anonymous => None,
			// `collect_pull_credential` returns `None` when the operator declares the
			// registry public (a blank username at the TTY prompt) — same outcome as
			// `--anonymous`: no credential, no auth.json.
			Some(ns) => match collect_pull_credential(ns)? {
				Some((user, token)) => {
					// Sanity-check before this is baked into the disk: prove the credential
					// authenticates to the registry, catching a typo'd token now rather than
					// after a device is provisioned. Opt out with `--skip-pull-check` when the
					// registry is known-unreachable from the build host.
					if !skip_pull_check {
						verify_pull_login(ns, &user, &token)?;
					}
					Some(render_auth_json(ns, &user, &token))
				}
				None => None,
			},
			None => None,
		};
		// Signing files only when `[deploy] registry` is the WithSigning form. The
		// public key is read from the project; the policy/registries.d are rendered
		// for the registry namespace.
		let signing = match manifest.signing() {
			Some(cfg) => {
				let ns = manifest.registry().expect("signing ⇒ registry (structural)");
				Some(SigningFiles::collect(&cfg, ns, &manifest.general.name)?)
			}
			None => None,
		};
		Ok((Self { authorized_keys, auth_json, signing }, key_path))
	}

	/// The provisioning files as `(device path, octal mode, content)` triples, in a
	/// stable order (admin key, then the registry-mode pull secret, then the three
	/// signing files when configured). The single source of truth for *what* gets
	/// injected into a device's `/etc`, independent of *how*: [`Self::blueprint`]
	/// wraps each into an `image-builder` `customizations.files` entry for disk
	/// provisioning, while [`crate::jobs::takeover`] installs each straight into a
	/// live host's staged deployment `/etc` over SSH — same files, same modes, same
	/// resulting `/etc`.
	///
	/// Every path is absolute and under `/etc` (the ostree 3-way merge then
	/// preserves them across upgrades, since the image ships no `/usr/etc` default).
	#[must_use]
	pub(crate) fn files(&self) -> Vec<DeviceFile> {
		// 0644 unless noted: world-readable is fine for an authorized_keys / public
		// key / policy; the pull secret is the one credential, root-only (0600).
		let mut files =
			vec![DeviceFile::new(DEVICE_ADMIN_AUTHORIZED_KEYS, "0644", &self.authorized_keys)];
		if let Some(auth) = &self.auth_json {
			files.push(DeviceFile::new(DEVICE_AUTH_JSON, "0600", auth));
		}
		if let Some(s) = &self.signing {
			// The public key + policy + registries.d that turn on signature enforcement.
			files.push(DeviceFile::new(&device_pubkey_path(&s.name), "0644", &s.pubkey_pem));
			files.push(DeviceFile::new(DEVICE_POLICY_JSON, "0644", &s.policy_json));
			files.push(DeviceFile::new(&device_registries_d_path(), "0644", &s.registries_d));
		}
		files
	}

	/// The `image-builder` blueprint (TOML) injecting the device files into the
	/// device's `/etc` as `customizations.files`.
	///
	/// # Errors
	///
	/// Returns an error if serializing the blueprint to TOML fails.
	pub fn blueprint(&self) -> Result<String> {
		// Only files: each `customizations.files` entry's parent directory must
		// already exist in the image (image-builder doesn't create it), and they do —
		// the scaffold's Containerfile makes `/etc/ssh/authorized_keys.d` (the dir the
		// `AuthorizedKeysFile` drop-in points at) and `/etc/pki/containers` /
		// `/etc/containers/registries.d`, and `/etc/ostree` (the auth.json parent) is
		// part of the base. We must *not* also declare the dirs as
		// `customizations.directories` entries: the osbuild `mkdir` stage isn't
		// idempotent, so creating a dir the image already ships fails the build with
		// `FileExistsError: … /etc/ssh/authorized_keys.d`.
		let files = self
			.files()
			.into_iter()
			.map(|f| FileCustomization { path: f.path, mode: f.mode, data: f.data })
			.collect();
		let cfg = Blueprint { customizations: Customizations { files } };
		toml::to_string(&cfg).context("serializing the blueprint")
	}
}

/// One file to inject into a device's persistent `/etc`: its absolute device path,
/// octal mode string (e.g. `"0600"`), and verbatim content. The transport-neutral
/// unit yielded by [`Provisioning::files`].
pub(crate) struct DeviceFile {
	pub(crate) path: String,
	pub(crate) mode: String,
	pub(crate) data: String,
}

impl DeviceFile {
	fn new(path: &str, mode: &str, data: &str) -> Self {
		Self { path: path.to_owned(), mode: mode.to_owned(), data: data.to_owned() }
	}
}

/// On-device path of the cosign public key the policy verifies against — the
/// `keyPath` in [`DEVICE_POLICY_JSON`]. Named after the image so it's
/// self-describing in `/etc/pki/containers`.
pub(crate) fn device_pubkey_path(name: &str) -> String {
	format!("{DEVICE_COSIGN_PUBKEY_DIR}/{name}.pub")
}

/// On-device path of bootcher's `use-sigstore-attachments` drop-in. A fixed name
/// (one project ⇒ one registry) keeps `rotate sign-key` and provision in lockstep.
pub(crate) fn device_registries_d_path() -> String {
	format!("{DEVICE_REGISTRIES_D}/bootcher-signing.yaml")
}

impl SigningFiles {
	/// Read the project's cosign public key and render the device policy +
	/// registries.d for registry namespace `ns` and image `name`.
	fn collect(cfg: &SigningConfig, ns: &str, name: &str) -> Result<Self> {
		let pub_path = cfg.public_key_path();
		let pubkey_pem = fs::read_to_string(&pub_path).with_context(|| {
			format!(
				"reading the signing public key {pub_path} — generate a keypair with \
				 `bootcher sign enroll`"
			)
		})?;
		Ok(Self {
			name: name.to_owned(),
			policy_json: render_policy_json(ns, &[&device_pubkey_path(name)]),
			registries_d: render_registries_d(ns),
			pubkey_pem,
		})
	}
}

/// Render the device `policy.json`: a `default: reject` baseline (so the
/// `ostree-image-signed` origin's `ContainerPolicy` is satisfied — it refuses a
/// permissive default) plus a `sigstoreSigned` requirement pinning the registry
/// namespace to the on-device public key(s), with `matchRepository` identity (so
/// any tag/arch under the namespace verifies against the same key). A single key
/// uses `keyPath`; several (a `rotate sign-key` transition trusting both the
/// current and incoming key) use `keyPaths`. `containers-storage` stays permissive
/// so the device's already-pulled local images keep working.
pub(crate) fn render_policy_json(ns: &str, keypaths: &[&str]) -> String {
	let mut req = serde_json::json!({
		"type": "sigstoreSigned",
		"signedIdentity": { "type": "matchRepository" }
	});
	let obj = req.as_object_mut().expect("just built an object");
	if let [single] = keypaths {
		obj.insert("keyPath".into(), serde_json::json!(single));
	} else {
		obj.insert("keyPaths".into(), serde_json::json!(keypaths));
	}
	let doc = serde_json::json!({
		"default": [{ "type": "reject" }],
		"transports": {
			"docker": { ns: [req] },
			"containers-storage": { "": [{ "type": "insecureAcceptAnything" }] }
		}
	});
	serde_json::to_string_pretty(&doc).expect("policy.json is a fixed, serializable shape") + "\n"
}

/// Render the device `registries.d` drop-in enabling `use-sigstore-attachments`
/// for the registry namespace, so the signature stored alongside the image is
/// fetched and checked on pull. A tiny fixed-shape YAML, built by hand (no YAML
/// dep): the namespace is the only variable and is quoted.
pub(crate) fn render_registries_d(ns: &str) -> String {
	format!("docker:\n  {ns:?}:\n    use-sigstore-attachments: true\n")
}

#[derive(Serialize)]
struct Blueprint {
	customizations: Customizations,
}
#[derive(Serialize)]
struct Customizations {
	files: Vec<FileCustomization>,
}
#[derive(Serialize)]
struct FileCustomization {
	path: String,
	mode: String,
	data: String,
}

/// Collect the read-only registry pull credential for namespace `ns`:
/// `BOOTCHER_PULL_USER`/`BOOTCHER_PULL_TOKEN` if both are set (the scriptable/CI
/// path, also honoured on a TTY so the prompt is skipped when the env is already
/// populated), else a TTY prompt, else a hard error. `Ok(Some((user, token)))` is
/// the raw credential — [`render_auth_json`] turns it into the `auth.json` shipped
/// to the device, and [`verify_pull_login`] uses the pair directly. `Ok(None)` means
/// the registry is **public**: on a TTY, a blank username opts into anonymous pull
/// (the non-TTY equivalent is provision's `--anonymous`). Shared by provision and
/// [`crate::jobs::rotate`].
pub(crate) fn collect_pull_credential(ns: &str) -> Result<Option<(String, String)>> {
	if let (Ok(user), Ok(token)) = (std::env::var(ENV_PULL_USER), std::env::var(ENV_PULL_TOKEN)) {
		return Ok(Some((user, token)));
	}
	if io::stdin().is_terminal() {
		let user = inquire::Text::new(&format!(
			"Read-only pull username for {ns} (leave blank if the registry is public):"
		))
		.prompt()?;
		// Blank username ⇒ public registry: collect no token, bake no auth.json.
		if user.trim().is_empty() {
			eprintln!(
				"no username entered — treating {ns} as a public registry (anonymous pull, no \
				 auth.json baked)"
			);
			return Ok(None);
		}
		let token =
			inquire::Password::new("Read-only pull token (e.g. read_registry deploy token):")
				.without_confirmation()
				.prompt()?;
		Ok(Some((user, token)))
	} else {
		let user = std::env::var(ENV_PULL_USER).with_context(|| {
			format!(
				"registry '{ns}' is configured but no pull credential available: \
				 set {ENV_PULL_USER} + {ENV_PULL_TOKEN}, run on a TTY to be prompted, or pass \
				 --anonymous if the registry is public"
			)
		})?;
		let token = std::env::var(ENV_PULL_TOKEN)
			.with_context(|| format!("registry '{ns}' configured but {ENV_PULL_TOKEN} is unset"))?;
		Ok(Some((user, token)))
	}
}

/// Render the `auth.json` pull secret for registry namespace `ns`. The single
/// `auths` key is the full namespace prefix, which bootc/containers
/// longest-prefix-matches when pulling. Serialization of this fixed shape can't
/// fail. Shared by provision and [`crate::jobs::rotate`], so a provision-time
/// pull secret and a rotated one are byte-for-byte the same shape.
pub(crate) fn render_auth_json(ns: &str, user: &str, token: &str) -> String {
	let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
	let doc = serde_json::json!({ "auths": { ns: { "auth": auth } } });
	serde_json::to_string_pretty(&doc).expect("auth.json is a fixed, serializable shape") + "\n"
}

/// Local check that a freshly collected pull credential authenticates to the
/// registry, run up front — before provision bakes it into the disk, or before
/// `bootcher rotate pull-token` starts its rollout — so a fat-fingered token surfaces
/// immediately rather than after a device is provisioned or mid-fleet. Skipped
/// entirely when the caller passes `--skip-pull-check`.
///
/// A failed probe is ambiguous — a bad token, or a private registry simply
/// unreachable from the build host — so the resolution depends on whether
/// there's someone to ask. On a TTY the warning becomes a prompt: the user
/// decides whether to proceed (defaulting to yes, the common false-alarm case),
/// and declining aborts before the long operation. On a non-TTY (CI) run there's
/// no one to ask, so it fails closed — a broken token can't silently ride into a
/// provisioned image; `--skip-pull-check` is the explicit, scriptable opt-out.
///
/// This is a registry-*auth* check (`podman login`), not an image-pull one: at
/// provision the image generally isn't pushed yet, and this runs on the build
/// host, which may not even reach the registry. The stronger pull-level check
/// runs on the *device* — which can reach the registry and where the image
/// exists — on every `rotate token` (per device, as the commit guard) and
/// auto-update.
pub(crate) fn verify_pull_login(ns: &str, user: &str, token: &str) -> Result<()> {
	let Err(e) = probe_registry_login(ns, user, token) else {
		return Ok(());
	};
	let warning = format!(
		"couldn't verify the pull credential against {ns} from this machine ({e:#}) — the \
		 registry may simply be unreachable from here, and the credential is checked again \
		 on the device at deploy/upgrade time"
	);
	if io::stdin().is_terminal() {
		eprintln!("warning: {warning}");
		if !inquire::Confirm::new("Continue with this credential anyway?")
			.with_default(true)
			.prompt()?
		{
			bail!("aborted: pull credential not verified against {ns}");
		}
		Ok(())
	} else {
		bail!(
			"{warning}\n\
			 the check failed and stdin is not a TTY to confirm — re-run with --skip-pull-check \
			 to bypass verification (e.g. when the registry isn't reachable from CI)"
		)
	}
}

/// `podman login` to `ns` with the read-only pull credential, against a
/// throwaway authfile so the user's real credential store is untouched. The
/// token rides stdin (`--password-stdin`), never the argv; output is suppressed
/// (the caller turns a non-zero exit into its own warning). Auth-level, so it
/// needs no image pushed — it works at first-provision time.
fn probe_registry_login(ns: &str, user: &str, token: &str) -> Result<()> {
	let authfile = tempfile::NamedTempFile::new().context("creating a temp authfile")?;
	// Seed it with an empty-but-valid JSON object. `podman login` reads and parses
	// the authfile before merging the new credential in; a 0-byte file (what
	// `NamedTempFile` leaves) fails that parse on newer podman with "unexpected end
	// of JSON input", so the probe would error before ever touching the registry.
	fs::write(authfile.path(), "{}").context("seeding the temp authfile")?;
	let mut child = Command::new("podman")
		.args(["login", "--username", user, "--password-stdin", "--authfile"])
		.arg(authfile.path())
		.arg(ns)
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.stderr(Stdio::piped())
		.spawn()
		.context("running `podman login`")?;
	// Feed the token, then close stdin (EOF) so login proceeds.
	child
		.stdin
		.take()
		.expect("stdin piped")
		.write_all(token.as_bytes())
		.context("writing the token to `podman login`")?;
	let out = child.wait_with_output().context("waiting for `podman login`")?;
	if !out.status.success() {
		// Surface podman's own diagnostic — the caller wraps this in a softer
		// "may simply be unreachable" warning, but without the underlying line a
		// CI failure (where the probe fails closed) is undebuggable.
		let detail = String::from_utf8_lossy(&out.stderr);
		let detail = detail.trim();
		if detail.is_empty() {
			bail!("`podman login {ns}` failed");
		}
		bail!("`podman login {ns}` failed: {detail}");
	}
	Ok(())
}

/// Collect the admin SSH public key(s) — a private key path or `None` to
/// discover `~/.ssh/` keys / prompt on a TTY — and render them as
/// `authorized_keys` file content (one key per line, blanks and `#` comments
/// stripped). The public key is always the `<key>.pub` sibling of the private
/// key, mirroring the convention [`crate::jobs::signing`] uses for the cosign
/// key. Shared by provision ([`Provisioning::collect`]) and
/// [`crate::jobs::rotate::ssh_key`], so a provisioned key file and a rotated one
/// are byte-for-byte the same shape. Returns the content and the private key
/// path (the `<key>.pub` sibling was read, but the private key path is what
/// `rotate key` passes to ssh as the identity).
pub(crate) fn collect_authorized_keys(key: Option<&str>) -> Result<(String, PathBuf)> {
	let (keys, path) = collect_pubkeys(key)?;
	Ok((keys.join("\n") + "\n", path))
}

/// Collect one or more SSH public keys from the `<arg>.pub` sibling of `arg`
/// (a private key path, or `None` to discover keys / prompt on a TTY),
/// stripping blanks and `#` comments. Returns the parsed keys and the private
/// key path.
fn collect_pubkeys(arg: Option<&str>) -> Result<(Vec<String>, PathBuf)> {
	let (raw, key_path) = read_input(arg)?;
	let keys: Vec<String> = raw
		.lines()
		.map(str::trim)
		.filter(|l| !l.is_empty() && !l.starts_with('#'))
		.map(str::to_owned)
		.collect();
	if keys.is_empty() {
		bail!("no SSH keys found in {}.pub", key_path.display());
	}
	Ok((keys, key_path))
}

fn read_input(arg: Option<&str>) -> Result<(String, PathBuf)> {
	match arg {
		Some(key_path) => {
			let pub_path = format!("{key_path}.pub");
			if !Path::new(&pub_path).is_file() {
				bail!("public key not found: {pub_path} — expected the .pub sibling of {key_path}");
			}
			Ok((fs::read_to_string(&pub_path)?, PathBuf::from(key_path)))
		}
		None if io::stdin().is_terminal() => prompt_key(),
		None => bail!("no key provided and stdin is not a TTY\nPass --key <path>."),
	}
}

fn prompt_key() -> Result<(String, PathBuf)> {
	let candidates = discover_keys();
	if candidates.is_empty() {
		let path_str = inquire::Text::new("SSH key path:").prompt()?;
		let pub_path = format!("{path_str}.pub");
		if !Path::new(&pub_path).is_file() {
			bail!("public key not found: {pub_path} — expected the .pub sibling of {path_str}");
		}
		return Ok((fs::read_to_string(&pub_path)?, PathBuf::from(path_str)));
	}

	let mut options: Vec<KeyOption> = candidates
		.into_iter()
		.map(|c| KeyOption {
			label: c.label,
			kind: OptionKind::File { key_path: c.key_path, pub_contents: c.pub_contents },
		})
		.collect();
	options.push(KeyOption { label: "Enter path…".into(), kind: OptionKind::Path });

	let chosen = inquire::Select::new("Select admin SSH key:", options).prompt()?;
	match chosen.kind {
		OptionKind::File { key_path, pub_contents } => Ok((pub_contents, PathBuf::from(key_path))),
		OptionKind::Path => {
			let path_str = inquire::Text::new("SSH key path:").prompt()?;
			let pub_path = format!("{path_str}.pub");
			if !Path::new(&pub_path).is_file() {
				bail!("public key not found: {pub_path} — expected the .pub sibling of {path_str}");
			}
			Ok((fs::read_to_string(&pub_path)?, PathBuf::from(path_str)))
		}
	}
}

struct KeyOption {
	label: String,
	kind: OptionKind,
}

enum OptionKind {
	File { key_path: String, pub_contents: String },
	Path,
}

impl fmt::Display for KeyOption {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.label)
	}
}

struct DiscoveredKey {
	label: String,
	key_path: String,
	pub_contents: String,
}

fn discover_keys() -> Vec<DiscoveredKey> {
	let Ok(home) = std::env::var("HOME") else {
		return Vec::new();
	};
	let ssh_dir = Path::new(&home).join(".ssh");
	let Ok(entries) = fs::read_dir(&ssh_dir) else {
		return Vec::new();
	};

	let mut out = Vec::new();
	for entry in entries.flatten() {
		let pub_path = entry.path();
		if pub_path.extension().and_then(|e| e.to_str()) != Some("pub") {
			continue;
		}
		let Ok(contents) = fs::read_to_string(&pub_path) else {
			continue;
		};
		let Some(first_line) =
			contents.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))
		else {
			continue;
		};
		let file_stem = pub_path.file_stem().and_then(|n| n.to_str()).unwrap_or("?").to_owned();
		let key_path = pub_path.with_extension("").to_string_lossy().into_owned();
		out.push(DiscoveredKey {
			label: format_label(&file_stem, first_line),
			key_path,
			pub_contents: contents,
		});
	}
	out.sort_by(|a, b| a.label.cmp(&b.label));
	out
}

fn format_label(file_name: &str, line: &str) -> String {
	let mut parts = line.split_whitespace();
	let key_type = parts.next().unwrap_or("?");
	let _b64 = parts.next();
	let comment = parts.collect::<Vec<_>>().join(" ");
	if comment.is_empty() {
		format!("{file_name} ({key_type})")
	} else {
		format!("{file_name} ({key_type}, {comment})")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn blueprint_injects_authorized_keys_in_lan_mode() {
		let p = Provisioning {
			authorized_keys: "ssh-ed25519 AAAA test@host\n".into(),
			auth_json: None,
			signing: None,
		};
		let cfg = p.blueprint().unwrap();
		assert!(cfg.contains("[[customizations.files]]"), "{cfg}");
		assert!(cfg.contains("/etc/ssh/authorized_keys.d/admin"), "{cfg}");
		assert!(cfg.contains("ssh-ed25519 AAAA test@host"), "{cfg}");
		// No directory customization: the image (the scaffold Containerfile) already
		// creates `/etc/ssh/authorized_keys.d`, and the osbuild non-idempotent `mkdir` would
		// fail trying to create a dir that exists.
		assert!(!cfg.contains("[[customizations.directories]]"), "{cfg}");
		// LAN mode injects no pull secret.
		assert!(!cfg.contains("/etc/ostree/auth.json"), "{cfg}");
	}

	#[test]
	fn blueprint_includes_auth_json_in_registry_mode() {
		let p = Provisioning {
			authorized_keys: "k\n".into(),
			auth_json: Some("{\"auths\":{}}\n".into()),
			signing: None,
		};
		let cfg = p.blueprint().unwrap();
		assert!(cfg.contains("/etc/ostree/auth.json"), "{cfg}");
		assert!(cfg.contains("0600"), "{cfg}");
	}

	#[test]
	fn anonymous_registry_provision_collects_no_credential_and_bakes_no_auth_json() {
		// `--anonymous` against a registry-mode manifest must skip the pull-credential
		// collection entirely — no env var, no TTY, no error — and bake no auth.json,
		// so the device pulls anonymously. (Reads no `BOOTCHER_PULL_*`, so it's robust
		// regardless of the test runner's environment.)
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("id");
		fs::write(dir.path().join("id.pub"), "ssh-ed25519 AAAA test@host\n").unwrap();
		let manifest: Manifest = toml::from_str(
			"[general]\nname = \"x\"\nplatform = \"x86_64\"\n\
			 [deploy]\nregistry = \"reg.example.com/org\"\n",
		)
		.unwrap();

		let p = Provisioning::collect(&manifest, Some(key.to_str().unwrap()), false, true).unwrap();
		assert!(p.auth_json.is_none(), "anonymous provision must collect no pull credential");
		let cfg = p.blueprint().unwrap();
		assert!(!cfg.contains("/etc/ostree/auth.json"), "no auth.json must be baked: {cfg}");
		// The admin key is still injected — anonymous only drops the pull secret.
		assert!(cfg.contains("/etc/ssh/authorized_keys.d/admin"), "{cfg}");
	}

	#[test]
	fn policy_json_requires_a_signature_and_rejects_by_default() {
		let policy = render_policy_json("reg.example.com/org", &["/etc/pki/containers/kiosk.pub"]);
		let v: serde_json::Value = serde_json::from_str(&policy).unwrap();
		// `default: reject` — ostree-image-signed's ContainerPolicy refuses a permissive
		// default, and anything outside the signed namespace is denied.
		assert_eq!(v["default"][0]["type"], "reject");
		let req = &v["transports"]["docker"]["reg.example.com/org"][0];
		assert_eq!(req["type"], "sigstoreSigned");
		assert_eq!(req["keyPath"], "/etc/pki/containers/kiosk.pub");
		assert_eq!(req["signedIdentity"]["type"], "matchRepository");
		// Local already-pulled images must keep working.
		assert_eq!(v["transports"]["containers-storage"][""][0]["type"], "insecureAcceptAnything");
	}

	#[test]
	fn policy_json_uses_keypaths_for_a_key_union() {
		// A `rotate sign-key` transition trusts both the current and incoming key, so
		// the requirement carries `keyPaths` (plural) rather than `keyPath`.
		let policy = render_policy_json(
			"reg/org",
			&["/etc/pki/containers/k.pub", "/etc/pki/containers/k-next.pub"],
		);
		let v: serde_json::Value = serde_json::from_str(&policy).unwrap();
		let req = &v["transports"]["docker"]["reg/org"][0];
		assert!(req["keyPath"].is_null(), "{policy}");
		assert_eq!(req["keyPaths"][0], "/etc/pki/containers/k.pub");
		assert_eq!(req["keyPaths"][1], "/etc/pki/containers/k-next.pub");
	}

	#[test]
	fn registries_d_enables_sigstore_attachments_for_the_namespace() {
		let yaml = render_registries_d("reg.example.com/org");
		assert!(yaml.contains("use-sigstore-attachments: true"), "{yaml}");
		assert!(yaml.contains("\"reg.example.com/org\""), "{yaml}");
	}

	#[test]
	fn blueprint_injects_the_three_signing_files() {
		let p = Provisioning {
			authorized_keys: "k\n".into(),
			auth_json: Some("{\"auths\":{}}\n".into()),
			signing: Some(SigningFiles {
				name: "kiosk".into(),
				pubkey_pem: "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----\n".into(),
				policy_json: render_policy_json("reg/org", &["/etc/pki/containers/kiosk.pub"]),
				registries_d: render_registries_d("reg/org"),
			}),
		};
		let cfg = p.blueprint().unwrap();
		assert!(cfg.contains("/etc/pki/containers/kiosk.pub"), "{cfg}");
		assert!(cfg.contains("/etc/containers/policy.json"), "{cfg}");
		assert!(cfg.contains("/etc/containers/registries.d/bootcher-signing.yaml"), "{cfg}");
		assert!(cfg.contains("BEGIN PUBLIC KEY"), "{cfg}");
	}

	#[test]
	fn collect_pubkeys_strips_comments_and_blanks() {
		let dir = tempfile::tempdir().unwrap();
		let key_path = dir.path().join("id");
		fs::write(
			dir.path().join("id.pub"),
			"# comment\n\n  \nssh-ed25519 AAAA a\nssh-ed25519 BBBB b\n",
		)
		.unwrap();
		let (keys, _) = collect_pubkeys(Some(key_path.to_str().unwrap())).unwrap();
		assert_eq!(keys, vec!["ssh-ed25519 AAAA a", "ssh-ed25519 BBBB b"]);
	}

	#[test]
	fn collect_pubkeys_errors_when_no_keys() {
		let dir = tempfile::tempdir().unwrap();
		let key_path = dir.path().join("id");
		fs::write(dir.path().join("id.pub"), "# only a comment\n\n").unwrap();
		assert!(collect_pubkeys(Some(key_path.to_str().unwrap())).is_err());
	}
}
