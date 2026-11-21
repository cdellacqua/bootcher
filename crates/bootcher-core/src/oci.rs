//! Reading (and, for the publishing commands, writing) the registry through
//! `skopeo`.
//!
//! Every registry read goes through here: `skopeo inspect --raw` for a manifest or
//! index (its digest is the SHA-256 of those exact bytes), `skopeo inspect
//! --config` for a member's labels, and `skopeo list-tags`. One code path serves
//! remote refs (`docker://…`) and local ones (`containers-storage:…`), and nothing
//! depends on the podman version a builder runs. Plain-HTTP test registries are
//! configured the one way both skopeo and podman honour: `insecure = true` in
//! `registries.conf`.
//!
//! Only a registry's "manifest unknown" means a tag is missing; any other failure
//! (auth, network, a registry refusing to say) is an error, never "absent".

use crate::context::{
	Arch, BASE_DIGEST_ANNOTATION, BASE_NAME_ANNOTATION, RELEASE_ANNOTATION, SOURCE_LABEL,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::ffi::OsString;

/// The install hint for a missing `skopeo`.
pub(crate) const SKOPEO_HINT: &str = "read and copy registry images — install skopeo (e.g. `dnf install skopeo` / `apt install skopeo`)";

/// The `sha256:<hex>` digest of `bytes`.
pub(crate) fn digest_of(bytes: &[u8]) -> String {
	use std::fmt::Write as _;
	let hex = sha2::Sha256::digest(bytes).iter().fold(String::new(), |mut out, b| {
		let _ = write!(out, "{b:02x}");
		out
	});
	format!("sha256:{hex}")
}

/// A manifest or index as `skopeo inspect --raw` returns it, parsed loosely: the
/// fields bootcher reads.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
	#[serde(default)]
	pub(crate) media_type: Option<String>,
	/// Index entries (empty for a single-image manifest). podman 4.9's `manifest
	/// inspect` prints `null` for an empty list.
	#[serde(default, deserialize_with = "null_as_empty")]
	pub(crate) manifests: Vec<Entry>,
	#[serde(default)]
	pub(crate) annotations: BTreeMap<String, String>,
	/// Present on a single-image manifest.
	#[serde(default)]
	pub(crate) config: Option<Descriptor>,
	/// Present on a single-image manifest (and on a sigstore `.sig` manifest).
	#[serde(default)]
	pub(crate) layers: Vec<Descriptor>,
}

/// One index entry.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Entry {
	pub(crate) digest: String,
	#[serde(default)]
	pub(crate) platform: Option<Platform>,
	/// The descriptor's annotations, where a release records its name.
	#[serde(default)]
	pub(crate) annotations: BTreeMap<String, String>,
}

/// Deserialize a JSON `null` as an empty `Vec`.
fn null_as_empty<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
	D: serde::Deserializer<'de>,
	T: Deserialize<'de>,
{
	Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// An index entry's platform.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Platform {
	pub(crate) architecture: String,
	#[serde(default)]
	pub(crate) os: String,
}

/// A content descriptor (config or layer).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Descriptor {
	pub(crate) digest: String,
	#[serde(default)]
	pub(crate) annotations: BTreeMap<String, String>,
}

impl Manifest {
	/// Whether this is an index (a multi-arch list) rather than one image.
	pub(crate) fn is_index(&self) -> bool {
		self.config.is_none()
	}

	/// The release an index was published as: the [`RELEASE_ANNOTATION`] every
	/// entry's descriptor carries. It lives on the entries, not the index, because
	/// podman before 5.0 can't annotate an index (`manifest annotate --index`); the
	/// entries are part of the index's bytes all the same, so a channel copy keeps
	/// them. `None` when an entry lacks it or the entries disagree.
	pub(crate) fn release(&self) -> Option<&str> {
		let mut names = self.manifests.iter().map(|e| {
			e.annotations.get(RELEASE_ANNOTATION).map(String::as_str).filter(|r| !r.is_empty())
		});
		let first = names.next()??;
		names.all(|n| n == Some(first)).then_some(first)
	}

	/// The index entry for `arch` (`linux/<oci arch>`).
	pub(crate) fn member(&self, arch: Arch) -> Option<&Entry> {
		self.manifests.iter().find(|e| {
			e.platform
				.as_ref()
				.is_some_and(|p| p.architecture == arch.oci_arch() && p.os != "windows")
		})
	}

	/// Each entry's (arch, digest), for the arches bootcher builds; entries of
	/// other platforms are skipped.
	pub(crate) fn members(&self) -> Vec<(Arch, String)> {
		self.manifests
			.iter()
			.filter_map(|e| {
				let p = e.platform.as_ref()?;
				let arch = match p.architecture.as_str() {
					"amd64" => Arch::X86_64,
					"arm64" => Arch::Aarch64,
					_ => return None,
				};
				Some((arch, e.digest.clone()))
			})
			.collect()
	}

	/// The base image a member was built from: its [`BASE_NAME_ANNOTATION`] and
	/// [`BASE_DIGEST_ANNOTATION`], when both are present.
	pub(crate) fn base(&self) -> Option<(&str, &str)> {
		Some((
			self.annotations.get(BASE_NAME_ANNOTATION)?.as_str(),
			self.annotations.get(BASE_DIGEST_ANNOTATION)?.as_str(),
		))
	}
}

/// A manifest or index fetched from a registry: its digest and parsed content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fetched {
	pub(crate) digest: String,
	pub(crate) manifest: Manifest,
}

/// Run skopeo with `args`, returning stdout, or `Ok(None)` when it failed with a
/// registry's "manifest unknown".
fn skopeo(args: &[OsString]) -> Result<Option<Vec<u8>>> {
	let cmdline = skopeo_argv(args.iter().cloned());
	let out = duct::cmd(&cmdline[0], &cmdline[1..])
		.env("LC_ALL", "C")
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
		.context("running skopeo")?;
	if out.status.success() {
		return Ok(Some(out.stdout));
	}
	let stderr = String::from_utf8_lossy(&out.stderr);
	if is_manifest_unknown(&stderr) {
		return Ok(None);
	}
	let line = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("no message");
	let cmd = args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
	bail!("`skopeo {cmd}` failed: {line}")
}

/// The argv running skopeo with `args`. A rootless skopeo reading the local store
/// (`containers-storage:`) creates its own user namespace, which hosts restricting
/// unprivileged user namespaces refuse (Ubuntu 24.04 does, via its `AppArmor` policy:
/// `unshare(...): Operation not permitted`), while podman is allowed to. So such a
/// call runs under `podman unshare`, inside the namespace podman sets up, where
/// skopeo needs no new one.
fn skopeo_argv(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
	let args: Vec<OsString> = args.into_iter().collect();
	let local = args.iter().any(|a| a.to_string_lossy().starts_with("containers-storage:"));
	let mut cmdline: Vec<OsString> = Vec::with_capacity(args.len() + 3);
	if local && !rustix::process::geteuid().is_root() {
		cmdline.extend(["podman", "unshare"].map(OsString::from));
	}
	cmdline.push("skopeo".into());
	cmdline.extend(args);
	cmdline
}

/// Whether skopeo's stderr reports a registry "manifest unknown" (the one answer
/// that means the tag doesn't exist).
pub(crate) fn is_manifest_unknown(stderr: &str) -> bool {
	let s = stderr.to_ascii_lowercase();
	s.contains("manifest unknown") || s.contains("manifest_unknown")
}

/// `docker://<image_ref>` unless `image_ref` already names a transport.
fn transport_ref(image_ref: &str) -> String {
	if image_ref.starts_with("docker://") || image_ref.starts_with("containers-storage:") {
		image_ref.to_owned()
	} else {
		format!("docker://{image_ref}")
	}
}

/// Fetch the raw manifest or index `image_ref` names (a tag or `@digest`), or
/// `None` when the registry reports it unknown.
///
/// # Errors
///
/// Returns an error for any other failure, or an unparseable manifest.
pub(crate) fn fetch(image_ref: &str) -> Result<Option<Fetched>> {
	let args: Vec<OsString> =
		vec!["inspect".into(), "--raw".into(), transport_ref(image_ref).into()];
	let Some(raw) = skopeo(&args)? else { return Ok(None) };
	let manifest = serde_json::from_slice(&raw)
		.with_context(|| format!("parsing the manifest of {image_ref}"))?;
	Ok(Some(Fetched { digest: digest_of(&raw), manifest }))
}

/// [`fetch`], requiring the ref to exist.
///
/// # Errors
///
/// Returns an error if it doesn't, or the fetch fails.
pub(crate) fn fetch_existing(image_ref: &str) -> Result<Fetched> {
	fetch(image_ref)?.with_context(|| format!("{image_ref} doesn't exist (manifest unknown)"))
}

/// The config labels of the single image `image_ref` names (`skopeo inspect
/// --config`). Read from a member by digest, never from an index (which resolves
/// to whatever arch the runner is).
///
/// # Errors
///
/// Returns an error if the image doesn't exist or the read fails.
pub(crate) fn config_labels(image_ref: &str) -> Result<BTreeMap<String, String>> {
	let args: Vec<OsString> =
		vec!["inspect".into(), "--config".into(), transport_ref(image_ref).into()];
	let raw = skopeo(&args)?.with_context(|| format!("{image_ref} doesn't exist"))?;
	let v: serde_json::Value = serde_json::from_slice(&raw)
		.with_context(|| format!("parsing the config of {image_ref}"))?;
	Ok(v.pointer("/config/Labels")
		.and_then(serde_json::Value::as_object)
		.map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
		.unwrap_or_default())
}

/// The [`SOURCE_LABEL`] commit a member image records (`None` when absent or
/// empty: a dirty build, or one from outside git).
///
/// # Errors
///
/// Returns an error if the config can't be read.
pub(crate) fn source_of(image_ref: &str) -> Result<Option<String>> {
	Ok(config_labels(image_ref)?.remove(SOURCE_LABEL).filter(|s| !s.is_empty()))
}

/// The tags of `repo` (`<url>/<name>`); empty when the repository doesn't exist
/// yet.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn list_tags(repo: &str) -> Result<Vec<String>> {
	let args: Vec<OsString> = vec!["list-tags".into(), format!("docker://{repo}").into()];
	let out = duct::cmd("skopeo", &args)
		.env("LC_ALL", "C")
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
		.context("running skopeo")?;
	if !out.status.success() {
		let stderr = String::from_utf8_lossy(&out.stderr);
		if stderr.contains("repository name not known") || stderr.contains("NAME_UNKNOWN") {
			return Ok(Vec::new());
		}
		bail!("`skopeo list-tags docker://{repo}` failed: {}", stderr.trim());
	}
	let v: serde_json::Value =
		serde_json::from_slice(&out.stdout).context("parsing `skopeo list-tags` output")?;
	Ok(v["Tags"]
		.as_array()
		.map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
		.unwrap_or_default())
}

/// `<repo>@<digest>`.
pub(crate) fn by_digest(repo: &str, digest: &str) -> String {
	format!("{repo}@{digest}")
}

/// The repository part of an image ref (tag or digest stripped; a `host:port`
/// colon is kept).
pub(crate) fn repo_of(image_ref: &str) -> &str {
	let s = image_ref.split('@').next().unwrap_or(image_ref);
	match s.rfind(':') {
		Some(i) if s[..i].contains('/') && !s[i..].contains('/') => &s[..i],
		_ => s,
	}
}

/// `skopeo copy --all --preserve-digests` of `src` onto `dst` (both full refs) —
/// a channel promote (`--remove-signatures`: signatures are per-digest attachments,
/// so they still apply to the copy) or an in-place re-sign (`sign`, adding a
/// signature without changing a digest, S4).
///
/// # Errors
///
/// Returns an error if the copy fails.
pub(crate) fn copy(
	src: &str,
	dst: &str,
	remove_signatures: bool,
	sign: Option<&crate::jobs::signing::SignArgs>,
	job: &crate::progress::Scope,
) -> Result<()> {
	let mut args: Vec<OsString> =
		["copy", "--all", "--preserve-digests"].iter().map(OsString::from).collect();
	if remove_signatures {
		args.push("--remove-signatures".into());
	}
	if let Some(sign) = sign {
		args.extend(sign.flags().iter().cloned());
	}
	args.push(transport_ref(src).into());
	args.push(transport_ref(dst).into());
	crate::exec::run_argv_labeled(job, &skopeo_argv(args), format!("skopeo copy {src} → {dst}"))
}

/// The annotation a cosign/sigstore signature layer carries its base64 signature in.
const COSIGN_SIGNATURE_ANNOTATION: &str = "dev.cosignproject.cosign/signature";

/// The sigstore attachment tag for `digest` (`sha256-<hex>.sig`).
pub(crate) fn sig_tag(digest: &str) -> String {
	format!("{}.sig", digest.replace(':', "-"))
}

/// Whether `digest` in `repo` carries a sigstore signature that `pubkey_pem`
/// verifies: some layer of its `.sig` attachment whose payload names this digest
/// and repository (the `matchRepository` identity devices use) and whose ECDSA
/// signature checks out. Reads only the tiny `.sig` manifest and payload blobs —
/// never the image's layers.
///
/// # Errors
///
/// Returns an error if the registry can't be read or the key can't be parsed.
pub(crate) fn signed_by(repo: &str, digest: &str, pubkey_pem: &str) -> Result<bool> {
	let key: p256::ecdsa::VerifyingKey = pubkey_pem
		.trim()
		.parse()
		.map_err(|_| anyhow::anyhow!("the signing public key isn't a P-256 PEM key"))?;
	let sig_ref = format!("{repo}:{}", sig_tag(digest));
	let Some(sig) = fetch(&sig_ref)? else { return Ok(false) };
	let dir = tempfile::tempdir().context("creating a temp dir for the signature payloads")?;
	let dest = format!("dir:{}", dir.path().display());
	let args: Vec<OsString> =
		vec!["copy".into(), "--quiet".into(), transport_ref(&sig_ref).into(), dest.into()];
	skopeo(&args)?.with_context(|| format!("{sig_ref} vanished while reading it"))?;
	for layer in &sig.manifest.layers {
		let Some(sig_b64) = layer.annotations.get(COSIGN_SIGNATURE_ANNOTATION) else { continue };
		let blob = dir.path().join(layer.digest.trim_start_matches("sha256:"));
		let Ok(payload) = std::fs::read(&blob) else { continue };
		if payload_verifies(&payload, sig_b64, &key, repo, digest) {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Check one simple-signing payload: its claimed digest and repository, then its
/// ECDSA signature (`sig_b64`, base64 DER) under `key`.
pub(crate) fn payload_verifies(
	payload: &[u8],
	sig_b64: &str,
	key: &p256::ecdsa::VerifyingKey,
	repo: &str,
	digest: &str,
) -> bool {
	use base64::Engine;
	use p256::ecdsa::signature::Verifier;
	let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else { return false };
	let claims_digest =
		v.pointer("/critical/image/docker-manifest-digest").and_then(|d| d.as_str())
			== Some(digest);
	let identity = v
		.pointer("/critical/identity/docker-reference")
		.and_then(|d| d.as_str())
		.unwrap_or_default();
	let claims_repo = crate::device::normalize_ref(&format!("{}:x", repo_of(identity)))
		== crate::device::normalize_ref(&format!("{repo}:x"));
	let Ok(der) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else {
		return false;
	};
	let Ok(sig) = p256::ecdsa::Signature::from_der(&der) else { return false };
	claims_digest && claims_repo && key.verify(payload, &sig).is_ok()
}

/// Whether the release index `index` (at `digest` in `repo`) and every member are
/// signed by `pubkey_pem`. Returns the digests that aren't.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn unsigned_digests(
	repo: &str,
	index: &Fetched,
	pubkey_pem: &str,
) -> Result<Vec<String>> {
	let mut digests = vec![index.digest.clone()];
	digests.extend(index.manifest.members().into_iter().map(|(_, d)| d));
	let mut missing = Vec::new();
	for d in digests {
		if !signed_by(repo, &d, pubkey_pem)? {
			missing.push(d);
		}
	}
	Ok(missing)
}

/// A member's base, judged against the base tag's current digest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub(crate) enum BaseStatus {
	/// `base.digest` is the base's current per-arch digest.
	Current { name: String, digest: String },
	/// The base moved: a newer base has been published.
	Stale { name: String, built: String, current: String },
	/// Unverifiable (no base annotations, an unreadable base): counts as moved.
	Unknown { reason: String },
}

impl BaseStatus {
	/// Whether this member needs a rebuild (stale or unverifiable).
	pub(crate) fn is_stale(&self) -> bool {
		!matches!(self, BaseStatus::Current { .. })
	}

	/// `current`, `stale (abc → def)`, `unknown (why)`.
	pub(crate) fn describe(&self) -> String {
		match self {
			BaseStatus::Current { .. } => "base current".to_owned(),
			BaseStatus::Stale { built, current, .. } => {
				format!("base stale ({} → {})", short(built), short(current))
			}
			BaseStatus::Unknown { reason } => format!("base unknown ({reason})"),
		}
	}
}

/// A digest's first 12 hex characters.
pub(crate) fn short(digest: &str) -> &str {
	let hex = digest.trim_start_matches("sha256:");
	&hex[..12.min(hex.len())]
}

/// Current per-arch digests of base images, cached per `(base ref, arch)` across
/// one run (several members and releases usually share a base).
#[derive(Default)]
pub(crate) struct BaseCache(std::collections::HashMap<(String, Arch), Result<String, String>>);

impl BaseCache {
	/// The current digest of base `name` for `arch`: the matching platform entry
	/// when the base is an index (`base.digest` is the per-arch member digest, S4),
	/// else the base manifest's own digest.
	fn current(&mut self, name: &str, arch: Arch) -> Result<String, String> {
		self.0
			.entry((name.to_owned(), arch))
			.or_insert_with(|| match fetch(name) {
				Ok(Some(f)) if f.manifest.is_index() => f
					.manifest
					.member(arch)
					.map(|e| e.digest.clone())
					.ok_or_else(|| format!("{name} has no {arch} image")),
				Ok(Some(f)) => Ok(f.digest),
				Ok(None) => Err(format!("{name} no longer exists")),
				Err(e) => Err(format!("{e:#}")),
			})
			.clone()
	}

	/// Judge a member manifest (its base annotations) for `arch`.
	pub(crate) fn status(&mut self, member: &Manifest, arch: Arch) -> BaseStatus {
		let Some((name, built)) = member.base() else {
			return BaseStatus::Unknown { reason: "no base annotations".to_owned() };
		};
		match self.current(name, arch) {
			Ok(current) if current == built => {
				BaseStatus::Current { name: name.to_owned(), digest: current }
			}
			Ok(current) => {
				BaseStatus::Stale { name: name.to_owned(), built: built.to_owned(), current }
			}
			Err(reason) => BaseStatus::Unknown { reason },
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn index_parsing_reads_members_release_and_base() {
		let raw = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
			"manifests":[
				{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:a","size":1,"platform":{"architecture":"amd64","os":"linux"},"annotations":{"bootcher.release":"v1.4"}},
				{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:b","size":1,"platform":{"architecture":"arm64","os":"linux","variant":"v8"},"annotations":{"bootcher.release":"v1.4"}},
				{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:c","size":1,"platform":{"architecture":"s390x","os":"linux"},"annotations":{"bootcher.release":"v1.4"}}]}"#;
		let m: Manifest = serde_json::from_slice(raw).unwrap();
		assert!(m.is_index());
		assert_eq!(m.release(), Some("v1.4"));
		assert_eq!(m.member(Arch::Aarch64).unwrap().digest, "sha256:b");
		assert_eq!(
			m.members(),
			[(Arch::X86_64, "sha256:a".to_owned()), (Arch::Aarch64, "sha256:b".to_owned())]
		);
		let member = br#"{"schemaVersion":2,"config":{"digest":"sha256:cfg"},"layers":[],
			"annotations":{"org.opencontainers.image.base.name":"q.io/f/b:44","org.opencontainers.image.base.digest":"sha256:base"}}"#;
		let m: Manifest = serde_json::from_slice(member).unwrap();
		assert!(!m.is_index());
		assert_eq!(m.base(), Some(("q.io/f/b:44", "sha256:base")));
		assert_eq!(m.release(), None);
	}

	#[test]
	fn local_store_reads_run_under_podman_unshare_when_rootless() {
		let remote = skopeo_argv(["inspect", "--raw", "docker://r.io/o/a:v1"].map(OsString::from));
		assert_eq!(remote[0], "skopeo");
		let local = skopeo_argv(
			["inspect", "--raw", "containers-storage:localhost/a:x"].map(OsString::from),
		);
		if rustix::process::geteuid().is_root() {
			assert_eq!(local[0], "skopeo");
		} else {
			assert_eq!(&local[..3], ["podman", "unshare", "skopeo"].map(OsString::from));
		}
	}

	#[test]
	fn release_needs_every_entry_to_agree() {
		let index = |a: &str, b: &str| -> Manifest {
			let raw = format!(
				r#"{{"manifests":[{{"digest":"sha256:a","annotations":{a}}},{{"digest":"sha256:b","annotations":{b}}}]}}"#
			);
			serde_json::from_str(&raw).unwrap()
		};
		let v1 = r#"{"bootcher.release":"v1"}"#;
		assert_eq!(index(v1, v1).release(), Some("v1"));
		assert_eq!(index(v1, "{}").release(), None, "an entry without it");
		assert_eq!(
			index(v1, r#"{"bootcher.release":"v2"}"#).release(),
			None,
			"disagreeing entries"
		);
		assert_eq!(Manifest::default().release(), None, "no entries");
		let null: Manifest = serde_json::from_str(r#"{"manifests":null}"#).unwrap();
		assert!(null.manifests.is_empty(), "podman 4.9 prints an empty list as null");
	}

	#[test]
	fn digest_is_the_sha256_of_the_raw_bytes() {
		assert_eq!(
			digest_of(b""),
			"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
		);
	}

	#[test]
	fn simple_signing_payloads_verify_only_for_their_digest_repo_and_key() {
		use base64::Engine;
		use p256::ecdsa::signature::Signer;
		use p256::elliptic_curve::Generate;
		let sk = p256::ecdsa::SigningKey::try_generate().unwrap();
		let other = p256::ecdsa::SigningKey::try_generate().unwrap();
		let payload = br#"{"critical":{"identity":{"docker-reference":"reg.io/org/app:v1"},"image":{"docker-manifest-digest":"sha256:abc"},"type":"cosign container image signature"},"optional":{}}"#;
		let sign = |k: &p256::ecdsa::SigningKey| {
			let sig: p256::ecdsa::Signature = k.sign(payload);
			base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes())
		};
		let vk = *sk.verifying_key();
		let good = sign(&sk);
		assert!(payload_verifies(payload, &good, &vk, "reg.io/org/app", "sha256:abc"));
		// Any tag of the same repository (matchRepository), but not another digest,
		// repository or key.
		assert!(!payload_verifies(payload, &good, &vk, "reg.io/org/app", "sha256:def"));
		assert!(!payload_verifies(payload, &good, &vk, "reg.io/org/other", "sha256:abc"));
		assert!(!payload_verifies(payload, &sign(&other), &vk, "reg.io/org/app", "sha256:abc"));
		assert!(!payload_verifies(payload, "not base64!", &vk, "reg.io/org/app", "sha256:abc"));
		assert_eq!(sig_tag("sha256:abc"), "sha256-abc.sig");
	}

	#[test]
	fn base_status_counts_anything_unverifiable_as_stale() {
		assert!(!BaseStatus::Current { name: "b".into(), digest: "d".into() }.is_stale());
		assert!(
			BaseStatus::Stale {
				name: "b".into(),
				built: "sha256:1".into(),
				current: "sha256:2".into()
			}
			.is_stale()
		);
		assert!(BaseStatus::Unknown { reason: "gone".into() }.is_stale());
		let mut cache = BaseCache::default();
		assert!(matches!(
			cache.status(&Manifest::default(), Arch::X86_64),
			BaseStatus::Unknown { .. }
		));
	}

	#[test]
	fn only_manifest_unknown_means_missing() {
		assert!(is_manifest_unknown(
			"Error: reading manifest nope in localhost:5991/probe: manifest unknown"
		));
		assert!(is_manifest_unknown("... MANIFEST_UNKNOWN: manifest unknown; map[Tag:x]"));
		assert!(!is_manifest_unknown("Error: unauthorized: authentication required"));
		assert!(!is_manifest_unknown("dial tcp: connection refused"));
	}

	#[test]
	fn repo_of_strips_only_the_tag_or_digest() {
		assert_eq!(repo_of("localhost:5000/a/b:v1"), "localhost:5000/a/b");
		assert_eq!(repo_of("localhost:5000/a/b"), "localhost:5000/a/b");
		assert_eq!(repo_of("q.io/a/b@sha256:1"), "q.io/a/b");
		assert_eq!(repo_of("localhost:5000"), "localhost:5000");
	}
}
