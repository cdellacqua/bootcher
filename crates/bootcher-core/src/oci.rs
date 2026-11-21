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

use crate::context::{Arch, RELEASE_ANNOTATION};
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
	fn only_manifest_unknown_means_missing() {
		assert!(is_manifest_unknown(
			"Error: reading manifest nope in localhost:5991/probe: manifest unknown"
		));
		assert!(is_manifest_unknown("... MANIFEST_UNKNOWN: manifest unknown; map[Tag:x]"));
		assert!(!is_manifest_unknown("Error: unauthorized: authentication required"));
		assert!(!is_manifest_unknown("dial tcp: connection refused"));
	}
}
