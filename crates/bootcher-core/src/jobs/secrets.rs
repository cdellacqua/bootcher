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
	DEVICE_REGISTRIES_D, Manifest,
};
use crate::interactive::Prompted;
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Serialize;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Env vars supplying the registry pull credential on a non-TTY (CI) provision,
/// where there's no prompt.
const ENV_PULL_USER: &str = "BOOTCHER_PULL_USER";
const ENV_PULL_TOKEN: &str = "BOOTCHER_PULL_TOKEN";

/// How a freshly installed device is set up to receive updates, which decides
/// what [`Provisioning`] collects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallMode {
	/// Follow a registry channel: bake a pull credential (unless `anonymous`, for a
	/// public registry; verified from here unless `skip_pull_check`) and, with
	/// signing on, the trust files.
	Channel {
		/// The declared channel the device follows.
		channel: String,
		/// The registry is public: bake no pull credential.
		anonymous: bool,
		/// Don't verify the pull credential from this machine.
		skip_pull_check: bool,
	},
	/// Updated over SSH by `device deploy`: no pull credential and no trust files
	/// (a direct device's trust is the SSH channel; S1 showed the install needs none
	/// even in a signing project).
	Direct,
}

/// The files to inject into a freshly provisioned device's `/etc`.
pub struct Provisioning {
	/// Admin `authorized_keys` content (one SSH public key per line).
	authorized_keys: String,
	/// Registry pull secret (`auth.json` JSON), for a channel device with a private
	/// registry.
	auth_json: Option<String>,
	/// Signature-enforcement files, for a channel device of a signing project.
	trust: Option<TrustFiles>,
}

impl Provisioning {
	/// Collect the admin SSH key (always) and, for a [`InstallMode::Channel`]
	/// device, the pull credential (rendered to `auth.json`) and the trust files.
	/// `key` is a private SSH key path (the `.pub` sibling is read for the public key
	/// content), or `None` to discover/prompt on a TTY.
	///
	/// # Errors
	///
	/// Returns an error if key or credential collection fails, or registry verification fails.
	pub fn collect(manifest: &Manifest, key: Option<&str>, mode: &InstallMode) -> Result<Self> {
		Ok(Self::collect_with_key(manifest, key, mode)?.0)
	}

	/// Like [`Self::collect`], but also returns the resolved admin **private** key
	/// path. `bootcher device takeover` needs it as the post-reboot `admin@host` SSH
	/// identity (the stock cloud user is gone by then), not just the public half
	/// baked into `authorized_keys`. The picker/discovery on a TTY (when `key` is
	/// `None`) runs exactly once.
	///
	/// # Errors
	///
	/// Returns an error if key or credential collection fails, or registry verification fails.
	pub fn collect_with_key(
		manifest: &Manifest,
		key: Option<&str>,
		mode: &InstallMode,
	) -> Result<(Self, PathBuf)> {
		let (authorized_keys, key_path) = collect_authorized_keys(key, "--ssh-key")?;
		let (auth_json, trust) = match mode {
			InstallMode::Direct => (None, None),
			InstallMode::Channel { anonymous, skip_pull_check, .. } => {
				let reg = manifest.require_registry("a channel install")?;
				let ns = reg.url.as_str();
				// `--anonymous`: the registry is public, so collect no pull credential
				// and bake no `auth.json`. Without it a credential is required (env or
				// TTY, else a hard error); the flag keeps that default fail-closed.
				let auth: Option<Result<String>> = if *anonymous {
					None
				} else {
					collect_pull_credential(ns)?.map(|(user, token)| {
						// Prove the credential authenticates before it's baked into a
						// disk, unless opted out (a registry unreachable from here).
						if !skip_pull_check {
							verify_pull_login(ns, &user, &token)?;
						}
						Ok(render_auth_json(ns, &user, &token))
					})
				};
				(auth.transpose()?, TrustFiles::for_manifest(manifest)?)
			}
		};
		Ok((Self { authorized_keys, auth_json, trust }, key_path))
	}

	/// The provisioning files as `(device path, octal mode, content)` triples, in a
	/// stable order (admin key, then a channel device's pull secret, then the three
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
		if let Some(t) = &self.trust {
			// The public key(s) + policy + registries.d that turn on signature enforcement.
			files.extend(t.device_files());
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

/// On-device path of a trusted cosign public key: `/etc/pki/containers/<name>-<fp8>.pub`,
/// named after the image and the key's fingerprint prefix ([`fingerprint`]) so
/// several keys can be trusted side by side during a rotation.
pub(crate) fn device_pubkey_path(name: &str, fp: &str) -> String {
	format!("{DEVICE_COSIGN_PUBKEY_DIR}/{name}-{}.pub", &fp[..8.min(fp.len())])
}

/// The SHA-256 fingerprint (lowercase hex) of a PEM public key's DER body
/// (`SubjectPublicKeyInfo`).
///
/// # Errors
///
/// Returns an error if `pem` isn't a PEM document.
pub(crate) fn fingerprint(pem: &str) -> Result<String> {
	use sha2::Digest;
	let (_, der) = pem_rfc7468::decode_vec(pem.trim().as_bytes())
		.map_err(|e| anyhow::anyhow!("not a PEM public key: {e}"))?;
	Ok(sha2::Sha256::digest(&der).iter().fold(String::new(), |mut out, b| {
		let _ = write!(out, "{b:02x}");
		out
	}))
}

/// One public key a device trusts: its on-device path and PEM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrustKey {
	pub(crate) path: String,
	pub(crate) pem: String,
}

impl TrustKey {
	/// The trust entry for `pem` in project `name`.
	///
	/// # Errors
	///
	/// Returns an error if `pem` isn't a PEM document.
	pub(crate) fn new(name: &str, pem: String) -> Result<Self> {
		Ok(Self { path: device_pubkey_path(name, &fingerprint(&pem)?), pem })
	}

	/// Read the public key at `pub_path` (a project `.pub`).
	///
	/// # Errors
	///
	/// Returns an error if the file can't be read or isn't a PEM key.
	pub(crate) fn read(name: &str, pub_path: &str) -> Result<Self> {
		let pem = fs::read_to_string(pub_path).with_context(|| {
			format!(
				"reading the signing public key {pub_path} — generate a keypair with \
				 `bootcher signing enroll`"
			)
		})?;
		Self::new(name, pem)
	}
}

/// A channel device's signature-verification files: the trusted public key(s), a
/// `policy.json` requiring a signature from one of them for the OS image, and the
/// `registries.d` drop-in fetching sigstore attachments. Installed into `/etc` at
/// provision/takeover and refreshed by every channel device step (see
/// [`Self::install_script`]).
#[derive(Clone, Debug)]
pub(crate) struct TrustFiles {
	/// Image name (the key-file prefix).
	pub(crate) name: String,
	/// The OS image repository the policy binds (`<url>/<name>`).
	pub(crate) repo: String,
	/// The registry namespace the attachments drop-in covers (`<url>`).
	pub(crate) ns: String,
	/// The keys to install.
	pub(crate) keys: Vec<TrustKey>,
}

impl TrustFiles {
	/// The trust files for the manifest's configured key, or `None` without
	/// signing.
	///
	/// # Errors
	///
	/// Returns an error if the configured public key can't be read.
	pub(crate) fn for_manifest(manifest: &Manifest) -> Result<Option<Self>> {
		let (Some(signing), Some(reg)) = (manifest.signing(), manifest.registry()) else {
			return Ok(None);
		};
		let name = manifest.general.name.clone();
		let key = TrustKey::read(
			&name,
			&manifest.root().join(signing.public_key_path()).to_string_lossy(),
		)?;
		Ok(Some(Self {
			repo: format!("{}/{name}", reg.url),
			ns: reg.url.clone(),
			name,
			keys: vec![key],
		}))
	}

	/// The static files a fresh install gets (provision blueprint, takeover inject):
	/// each key, a policy trusting exactly them, and the attachments drop-in.
	pub(crate) fn device_files(&self) -> Vec<DeviceFile> {
		let mut files: Vec<DeviceFile> =
			self.keys.iter().map(|k| DeviceFile::new(&k.path, "0644", &k.pem)).collect();
		let paths: Vec<&str> = self.keys.iter().map(|k| k.path.as_str()).collect();
		files.push(DeviceFile::new(
			DEVICE_POLICY_JSON,
			"0644",
			&render_policy_json(&self.repo, &paths),
		));
		files.push(DeviceFile::new(
			&device_registries_d_path(),
			"0644",
			&render_registries_d(&self.ns),
		));
		files
	}

	/// The device script a running device's trust files are refreshed with (piped to
	/// `sh` over SSH). Installs [`Self::keys`] and removes the legacy unsuffixed
	/// `<name>.pub`; with `prune`, also removes every other `<name>-<fp8>.pub`
	/// (`signing retire`). The policy then trusts every `<name>-<fp8>.pub` left on the
	/// device, so a routine refresh adds the current key without dropping a key a
	/// rotation added — only `retire` narrows the set.
	pub(crate) fn install_script(&self, prune: bool) -> String {
		let dir = DEVICE_COSIGN_PUBKEY_DIR;
		let name = &self.name;
		let glob = format!("{dir}/{name}-{}.pub", "[0-9a-f]".repeat(8));
		let mut s = String::from("set -eu\ntmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT\n");
		for k in &self.keys {
			let _ = write!(
				s,
				"cat > \"$tmp\" <<'BOOTCHER_PUB'\n{pem}BOOTCHER_PUB\nsudo install -m 0644 -D \"$tmp\" {path}\n",
				pem = ensure_newline(&k.pem),
				path = k.path,
			);
		}
		let _ = writeln!(s, "sudo rm -f {dir}/{name}.pub");
		if prune {
			let keep: Vec<&str> = self.keys.iter().map(|k| k.path.as_str()).collect();
			let _ = writeln!(
				s,
				"for f in {glob}; do [ -e \"$f\" ] || continue; case \"$f\" in {}) ;; *) sudo rm -f \"$f\" ;; esac; done",
				keep.join("|")
			);
		}
		let _ = write!(
			s,
			"paths=\nfor f in {glob}; do [ -e \"$f\" ] || continue; paths=\"$paths${{paths:+,}}\\\"$f\\\"\"; done\n\
			 cat > \"$tmp\" <<'BOOTCHER_POL'\n{policy}BOOTCHER_POL\n\
			 sed \"s|\\\"{PLACEHOLDER}\\\"|$paths|\" \"$tmp\" > \"$tmp.pol\"\n\
			 sudo install -m 0644 \"$tmp.pol\" {DEVICE_POLICY_JSON}; rm -f \"$tmp.pol\"\n\
			 cat > \"$tmp\" <<'BOOTCHER_REGD'\n{regd}BOOTCHER_REGD\n\
			 sudo install -m 0644 -D \"$tmp\" {regd_path}\n",
			policy = render_policy_json(&self.repo, &[PLACEHOLDER]),
			regd = render_registries_d(&self.ns),
			regd_path = device_registries_d_path(),
		);
		s
	}
}

/// The `keyPaths` placeholder [`TrustFiles::install_script`] substitutes on the device.
const PLACEHOLDER: &str = "__BOOTCHER_KEYPATHS__";

/// `s` with a trailing newline (a heredoc body must end in one).
fn ensure_newline(s: &str) -> String {
	if s.ends_with('\n') { s.to_owned() } else { format!("{s}\n") }
}

/// On-device path of bootcher's `use-sigstore-attachments` drop-in. A fixed name
/// (one project ⇒ one registry) keeps the trust-file refresh and provision in lockstep.
pub(crate) fn device_registries_d_path() -> String {
	format!("{DEVICE_REGISTRIES_D}/bootcher-signing.yaml")
}

/// Render the device `policy.json`: a `default: reject` baseline (so the
/// `ostree-image-signed` origin's `ContainerPolicy` is satisfied — ostree-ext
/// makes a blanket check on the global `default` and refuses to deploy when it's
/// permissive, regardless of any per-scope rule) plus a `sigstoreSigned`
/// requirement pinning the OS image repository `repo` to the on-device public
/// key(s), with `matchRepository` identity (so any tag/arch of that image
/// verifies against the same key). Always `keyPaths`, so a rotation can trust the
/// current and the incoming key at once. A `docker` transport-wide `""` scope of `insecureAcceptAnything`
/// (more specific than the global `default`, so it wins for everything but
/// `repo`) lets every *other* image pull unsigned — only the OS image is bound by
/// the signing policy, so sibling images under the same registry namespace (and
/// images from any other registry) keep working. `containers-storage` stays
/// permissive so the device's already-pulled local images keep working.
pub(crate) fn render_policy_json(repo: &str, keypaths: &[&str]) -> String {
	render_policy(repo, "keyPaths", serde_json::json!(keypaths))
}

/// [`render_policy_json`] trusting the single key at `keypath`, as `keyPath`. The
/// build host's one-off checks (`signing verify`, promote) use it, because an older
/// containers/image (podman 4.9, as on Ubuntu 24.04) rejects `keyPaths` as an
/// unknown key. Devices run a current containers/image and keep `keyPaths`.
pub(crate) fn render_single_key_policy_json(repo: &str, keypath: &str) -> String {
	render_policy(repo, "keyPath", serde_json::json!(keypath))
}

/// The policy document both renderers share, with `field: keys` as the key set.
fn render_policy(repo: &str, field: &str, keys: serde_json::Value) -> String {
	let mut req = serde_json::json!({
		"type": "sigstoreSigned",
		"signedIdentity": { "type": "matchRepository" }
	});
	let obj = req.as_object_mut().expect("just built an object");
	obj.insert(field.into(), keys);
	let doc = serde_json::json!({
		"default": [{ "type": "reject" }],
		"transports": {
			"docker": {
				repo: [req],
				"": [{ "type": "insecureAcceptAnything" }]
			},
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
		.prompt()
		.prompted()?;
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
				.prompt()
				.prompted()?;
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
/// `bootcher device rotate pull-token` starts its rollout — so a fat-fingered token surfaces
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
			.prompt()
			.prompted()?
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
/// `rotate key` passes to ssh as the identity). `flag` names the caller's CLI
/// option for the key path, quoted in the non-TTY error.
pub(crate) fn collect_authorized_keys(key: Option<&str>, flag: &str) -> Result<(String, PathBuf)> {
	let (keys, path) = collect_pubkeys(key, flag)?;
	Ok((keys.join("\n") + "\n", path))
}

/// Collect one or more SSH public keys from the `<arg>.pub` sibling of `arg`
/// (a private key path, or `None` to discover keys / prompt on a TTY),
/// stripping blanks and `#` comments. Returns the parsed keys and the private
/// key path.
fn collect_pubkeys(arg: Option<&str>, flag: &str) -> Result<(Vec<String>, PathBuf)> {
	let (raw, key_path) = read_input(arg, flag)?;
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

fn read_input(arg: Option<&str>, flag: &str) -> Result<(String, PathBuf)> {
	match arg {
		Some(key_path) => {
			let pub_path = format!("{key_path}.pub");
			if !Path::new(&pub_path).is_file() {
				bail!("public key not found: {pub_path} — expected the .pub sibling of {key_path}");
			}
			Ok((fs::read_to_string(&pub_path)?, PathBuf::from(key_path)))
		}
		None if io::stdin().is_terminal() => prompt_key(),
		None => bail!("no key provided and stdin is not a TTY\nPass {flag} <path>."),
	}
}

fn prompt_key() -> Result<(String, PathBuf)> {
	let candidates = discover_keys();
	if candidates.is_empty() {
		let path_str = inquire::Text::new("SSH key path:").prompt().prompted()?;
		let pub_path = format!("{path_str}.pub");
		if !Path::new(&pub_path).is_file() {
			bail!("public key not found: {pub_path} — expected the .pub sibling of {path_str}");
		}
		eprintln!("→ pass --ssh-key {path_str} to skip this prompt");
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

	let chosen = inquire::Select::new("Select admin SSH key:", options).prompt().prompted()?;
	match chosen.kind {
		OptionKind::File { key_path, pub_contents } => {
			eprintln!("→ pass --ssh-key {key_path} to skip this prompt");
			Ok((pub_contents, PathBuf::from(key_path)))
		}
		OptionKind::Path => {
			let path_str = inquire::Text::new("SSH key path:").prompt().prompted()?;
			let pub_path = format!("{path_str}.pub");
			if !Path::new(&pub_path).is_file() {
				bail!("public key not found: {pub_path} — expected the .pub sibling of {path_str}");
			}
			eprintln!("→ pass --ssh-key {path_str} to skip this prompt");
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

	/// A fresh P-256 public key PEM.
	fn pem() -> String {
		use p256::elliptic_curve::Generate;
		use p256::pkcs8::EncodePublicKey;
		p256::SecretKey::try_generate()
			.unwrap()
			.public_key()
			.to_public_key_pem(pem_rfc7468::LineEnding::LF)
			.unwrap()
	}

	fn trust(keys: Vec<TrustKey>) -> TrustFiles {
		TrustFiles {
			name: "kiosk".into(),
			repo: "reg/org/kiosk".into(),
			ns: "reg/org".into(),
			keys,
		}
	}

	#[test]
	fn blueprint_injects_only_authorized_keys_for_a_direct_install() {
		let p = Provisioning {
			authorized_keys: "ssh-ed25519 AAAA test@host\n".into(),
			auth_json: None,
			trust: None,
		};
		let cfg = p.blueprint().unwrap();
		assert!(cfg.contains("[[customizations.files]]"), "{cfg}");
		assert!(cfg.contains("/etc/ssh/authorized_keys.d/admin"), "{cfg}");
		assert!(cfg.contains("ssh-ed25519 AAAA test@host"), "{cfg}");
		// No directory customization: the image (the scaffold Containerfile) already
		// creates `/etc/ssh/authorized_keys.d`, and the osbuild non-idempotent `mkdir` would
		// fail trying to create a dir that exists.
		assert!(!cfg.contains("[[customizations.directories]]"), "{cfg}");
		assert!(!cfg.contains("/etc/ostree/auth.json"), "{cfg}");
		assert!(!cfg.contains("policy.json"), "{cfg}");
	}

	#[test]
	fn blueprint_includes_auth_json_for_a_channel_install() {
		let p = Provisioning {
			authorized_keys: "k\n".into(),
			auth_json: Some("{\"auths\":{}}\n".into()),
			trust: None,
		};
		let cfg = p.blueprint().unwrap();
		assert!(cfg.contains("/etc/ostree/auth.json"), "{cfg}");
		assert!(cfg.contains("0600"), "{cfg}");
	}

	fn manifest(signing: bool) -> Manifest {
		let key = if signing { "key = \"cosign.key\"\n" } else { "" };
		toml::from_str(&format!(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [registry]\nurl = \"reg.example.com/org\"\nchannels = [\"stable\"]\n{key}",
		))
		.unwrap()
	}

	fn key_file() -> (tempfile::TempDir, String) {
		let dir = tempfile::tempdir().unwrap();
		let key = dir.path().join("id");
		fs::write(dir.path().join("id.pub"), "ssh-ed25519 AAAA test@host\n").unwrap();
		let key = key.to_str().unwrap().to_owned();
		(dir, key)
	}

	#[test]
	fn anonymous_channel_install_collects_no_credential_and_bakes_no_auth_json() {
		// `--anonymous` must skip the pull-credential collection entirely — no env var,
		// no TTY, no error — and bake no auth.json.
		let (_dir, key) = key_file();
		let mode = InstallMode::Channel {
			channel: "stable".into(),
			anonymous: true,
			skip_pull_check: false,
		};
		let p = Provisioning::collect(&manifest(false), Some(&key), &mode).unwrap();
		assert!(p.auth_json.is_none(), "anonymous provision must collect no pull credential");
		let cfg = p.blueprint().unwrap();
		assert!(!cfg.contains("/etc/ostree/auth.json"), "no auth.json must be baked: {cfg}");
		assert!(cfg.contains("/etc/ssh/authorized_keys.d/admin"), "{cfg}");
	}

	#[test]
	fn direct_install_in_a_signing_project_bakes_no_trust_files() {
		// D4: S1 showed direct installs need no trust files, so nothing is baked —
		// not even the public key (which would be read, and fail, if it were).
		let (_dir, key) = key_file();
		let p = Provisioning::collect(&manifest(true), Some(&key), &InstallMode::Direct).unwrap();
		assert!(p.trust.is_none() && p.auth_json.is_none());
	}

	#[test]
	fn policy_json_signs_only_the_os_image_and_rejects_by_default() {
		let policy = render_policy_json(
			"reg.example.com/org/kiosk",
			&["/etc/pki/containers/kiosk-0011.pub"],
		);
		let v: serde_json::Value = serde_json::from_str(&policy).unwrap();
		// `default: reject` — ostree-image-signed's ContainerPolicy makes a blanket check
		// on the global `default` and refuses a permissive one.
		assert_eq!(v["default"][0]["type"], "reject");
		// Signing is pinned to the OS image repository, not the whole namespace.
		let req = &v["transports"]["docker"]["reg.example.com/org/kiosk"][0];
		assert_eq!(req["type"], "sigstoreSigned");
		assert_eq!(req["keyPaths"][0], "/etc/pki/containers/kiosk-0011.pub");
		assert!(req["keyPath"].is_null(), "{policy}");
		assert_eq!(req["signedIdentity"]["type"], "matchRepository");
		// Every *other* image pulls unsigned via the docker transport-wide wildcard.
		assert_eq!(v["transports"]["docker"][""][0]["type"], "insecureAcceptAnything");
		// Local already-pulled images must keep working.
		assert_eq!(v["transports"]["containers-storage"][""][0]["type"], "insecureAcceptAnything");
	}

	#[test]
	fn policy_json_trusts_several_keys_at_once() {
		let policy = render_policy_json("reg/org/name", &["/k1.pub", "/k2.pub"]);
		let v: serde_json::Value = serde_json::from_str(&policy).unwrap();
		let req = &v["transports"]["docker"]["reg/org/name"][0];
		assert_eq!(req["keyPaths"], serde_json::json!(["/k1.pub", "/k2.pub"]));
		let single: serde_json::Value =
			serde_json::from_str(&render_single_key_policy_json("reg/org/name", "/k1.pub"))
				.unwrap();
		let req = &single["transports"]["docker"]["reg/org/name"][0];
		assert_eq!(req["keyPath"], "/k1.pub", "podman 4.9 only knows keyPath");
		assert!(req.get("keyPaths").is_none());
	}

	#[test]
	fn registries_d_enables_sigstore_attachments_for_the_namespace() {
		let yaml = render_registries_d("reg.example.com/org");
		assert!(yaml.contains("use-sigstore-attachments: true"), "{yaml}");
		assert!(yaml.contains("\"reg.example.com/org\""), "{yaml}");
	}

	#[test]
	fn key_files_are_named_by_fingerprint_prefix() {
		let pem = pem();
		let fp = fingerprint(&pem).unwrap();
		assert_eq!(fp.len(), 64);
		let k = TrustKey::new("kiosk", pem.clone()).unwrap();
		assert_eq!(k.path, format!("/etc/pki/containers/kiosk-{}.pub", &fp[..8]));
		assert!(fingerprint("not a pem").is_err());
	}

	#[test]
	fn blueprint_injects_the_trust_files() {
		let pem = pem();
		let p = Provisioning {
			authorized_keys: "k\n".into(),
			auth_json: Some("{\"auths\":{}}\n".into()),
			trust: Some(trust(vec![TrustKey::new("kiosk", pem.clone()).unwrap()])),
		};
		let cfg = p.blueprint().unwrap();
		let fp8 = &fingerprint(&pem).unwrap()[..8];
		assert!(cfg.contains(&format!("/etc/pki/containers/kiosk-{fp8}.pub")), "{cfg}");
		assert!(cfg.contains("/etc/containers/policy.json"), "{cfg}");
		assert!(cfg.contains("/etc/containers/registries.d/bootcher-signing.yaml"), "{cfg}");
		assert!(cfg.contains("BEGIN PUBLIC KEY"), "{cfg}");
	}

	/// Run an install script against a scratch root: `sudo` is a no-op shim and every
	/// absolute device path is rewritten under `root`.
	fn run_script(script: &str, root: &Path) {
		let script =
			script.replace("/etc/", &format!("{}/etc/", root.display())).replace("sudo ", "");
		let out = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
		assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stderr), script);
	}

	#[test]
	fn the_refresh_script_keeps_existing_keys_and_retire_prunes_them() {
		let root = tempfile::tempdir().unwrap();
		let dir = root.path().join("etc/pki/containers");
		fs::create_dir_all(&dir).unwrap();
		fs::create_dir_all(root.path().join("etc/containers/registries.d")).unwrap();
		// A legacy unsuffixed key and a key from an earlier rotation are on the device.
		fs::write(dir.join("kiosk.pub"), "legacy").unwrap();
		fs::write(dir.join("kiosk-deadbeef.pub"), "old").unwrap();
		fs::write(dir.join("kiosk-dev-01234567.pub"), "another image's key").unwrap();
		let pem = pem();
		let t = trust(vec![TrustKey::new("kiosk", pem.clone()).unwrap()]);
		let current = t.keys[0].path.replace("/etc/", &format!("{}/etc/", root.path().display()));

		// A routine refresh adds the current key and keeps the rotation's key.
		run_script(&t.install_script(false), root.path());
		let policy: serde_json::Value = serde_json::from_str(
			&fs::read_to_string(root.path().join("etc/containers/policy.json")).unwrap(),
		)
		.unwrap();
		let paths = policy["transports"]["docker"]["reg/org/kiosk"][0]["keyPaths"].clone();
		let paths: Vec<String> = serde_json::from_value(paths).unwrap();
		assert_eq!(paths.len(), 2, "{paths:?}");
		assert!(
			paths.contains(&current) && paths.iter().any(|p| p.ends_with("kiosk-deadbeef.pub"))
		);
		assert!(!dir.join("kiosk.pub").exists(), "the legacy key file is dropped");
		assert!(dir.join("kiosk-dev-01234567.pub").exists(), "another image's key is untouched");
		assert!(root.path().join("etc/containers/registries.d/bootcher-signing.yaml").exists());

		// Retire trusts only the current key and deletes the rest.
		run_script(&t.install_script(true), root.path());
		let policy: serde_json::Value = serde_json::from_str(
			&fs::read_to_string(root.path().join("etc/containers/policy.json")).unwrap(),
		)
		.unwrap();
		assert_eq!(
			policy["transports"]["docker"]["reg/org/kiosk"][0]["keyPaths"],
			serde_json::json!([current])
		);
		assert!(!dir.join("kiosk-deadbeef.pub").exists());
		assert!(dir.join("kiosk-dev-01234567.pub").exists());
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
		let (keys, _) = collect_pubkeys(Some(key_path.to_str().unwrap()), "--ssh-key").unwrap();
		assert_eq!(keys, vec!["ssh-ed25519 AAAA a", "ssh-ed25519 BBBB b"]);
	}

	#[test]
	fn collect_pubkeys_errors_when_no_keys() {
		let dir = tempfile::tempdir().unwrap();
		let key_path = dir.path().join("id");
		fs::write(dir.path().join("id.pub"), "# only a comment\n\n").unwrap();
		assert!(collect_pubkeys(Some(key_path.to_str().unwrap()), "--ssh-key").is_err());
	}
}
