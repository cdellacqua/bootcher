//! What each configured device is, read live from the device itself.
//!
//! Every `remotes` entry is classified from `bootc status --format json`
//! `.spec.image` — the image the device's next upgrade follows — never from config
//! (there is no per-device mode). The booted image is the wrong thing to read for a
//! device with a staged switch. See [`classify`] for the classes.
//!
//! [`probe`] reads one device in a single SSH round trip (its arch, whether it holds
//! a pull credential, and its bootc status) through [`Ssh::read_sh`], which pipes
//! the script to `sh`: a device's admin login shell may not be POSIX (the scaffold
//! sets fish), so nothing here is ever sent as a bare command string. Parsing and
//! classification are pure functions over the probe output.

use crate::context::{Arch, DEVICE_AUTH_JSON, Manifest, RemoteConfig};
use crate::ssh::Ssh;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fmt;

/// A bootc image reference as `bootc status` reports it (`spec.image`, or a
/// deployment's `image.image`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageSpec {
	/// The image reference (`quay.io/org/name:tag`, `localhost/name:latest`).
	pub(crate) image: String,
	/// `registry`, `containers-storage`, … (bootc's spelling).
	pub(crate) transport: String,
	/// The signature mode (`insecure`, `containerPolicy`, `ostreeRemote:<r>`), when
	/// reported.
	pub(crate) signature: Option<String>,
}

impl ImageSpec {
	/// The registry transport (the one channel devices follow).
	pub(crate) fn is_registry(&self) -> bool {
		self.transport == "registry"
	}
}

impl fmt::Display for ImageSpec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{} ({}", self.image, self.transport)?;
		if let Some(sig) = &self.signature {
			write!(f, ", {sig}")?;
		}
		f.write_str(")")
	}
}

/// One bootc deployment's image: the reference it came from, its per-arch manifest
/// digest, and its `version` label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Deployed {
	pub(crate) image: Option<ImageSpec>,
	/// The per-arch **member** manifest digest (never the index's, S2).
	pub(crate) digest: String,
	pub(crate) version: Option<String>,
}

impl fmt::Display for Deployed {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.version {
			Some(v) => write!(f, "version {v} ({})", self.digest),
			None => f.write_str(&self.digest),
		}
	}
}

/// A device's bootc state plus the facts the device steps need, from one probe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostStatus {
	/// The device's arch, when bootcher builds it.
	pub(crate) arch: Option<Arch>,
	/// Whether `/etc/ostree/auth.json` (the pull credential) is present.
	pub(crate) has_auth: bool,
	/// `spec.image`: what the next upgrade follows.
	pub(crate) spec: Option<ImageSpec>,
	pub(crate) booted: Option<Deployed>,
	pub(crate) staged: Option<Deployed>,
}

/// The separator between the probe's `key=value` facts and the JSON status.
const PROBE_SEPARATOR: &str = "---bootcher-status---";

/// The probe [`probe`] runs: arch, pull-credential presence, then the status JSON.
fn probe_script() -> String {
	format!(
		"echo \"arch=$(uname -m)\"\n\
		 if sudo test -f {DEVICE_AUTH_JSON}; then echo auth=yes; else echo auth=no; fi\n\
		 echo {PROBE_SEPARATOR}\n\
		 sudo bootc status --format json\n"
	)
}

/// Parse a [`probe_script`] output.
pub(crate) fn parse_probe(out: &str) -> Result<HostStatus> {
	let (facts, json) =
		out.split_once(PROBE_SEPARATOR).context("the device probe printed no bootc status")?;
	let get = |key: &str| {
		facts.lines().find_map(|l| l.trim().strip_prefix(key).and_then(|r| r.strip_prefix('=')))
	};
	let mut status = parse_status(json.trim())?;
	status.arch = get("arch").and_then(Arch::from_uname);
	status.has_auth = get("auth") == Some("yes");
	Ok(status)
}

/// Parse `bootc status --format json` into the spec + booted/staged deployments.
pub(crate) fn parse_status(json: &str) -> Result<HostStatus> {
	let v: Value = serde_json::from_str(json).context("parsing `bootc status --format json`")?;
	let deployed = |slot: &str| -> Option<Deployed> {
		let img = v.pointer(&format!("/status/{slot}/image"))?;
		Some(Deployed {
			image: img.get("image").and_then(image_spec),
			digest: img.get("imageDigest")?.as_str()?.to_owned(),
			version: img.get("version").and_then(Value::as_str).map(str::to_owned),
		})
	};
	Ok(HostStatus {
		arch: None,
		has_auth: false,
		spec: v.pointer("/spec/image").and_then(image_spec),
		booted: deployed("booted"),
		staged: deployed("staged"),
	})
}

/// One `{image, transport, signature}` object.
fn image_spec(v: &Value) -> Option<ImageSpec> {
	let signature = match v.get("signature") {
		Some(Value::String(s)) => Some(s.clone()),
		Some(Value::Object(o)) => {
			o.iter().next().map(|(k, val)| format!("{k}:{}", val.as_str().unwrap_or_default()))
		}
		_ => None,
	};
	Some(ImageSpec {
		image: v.get("image")?.as_str()?.to_owned(),
		transport: v.get("transport").and_then(Value::as_str).unwrap_or("registry").to_owned(),
		signature,
	})
}

/// What a device is, from its `spec.image`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Class {
	/// Follows the declared channel `ch` (`<registry>/<name>:<ch>`, registry
	/// transport, signed or not).
	Channel(String),
	/// Runs what `device deploy` ships: `localhost/<name>:latest`, any transport and
	/// signature mode (a fresh disk is registry transport, a fresh takeover and every
	/// deployed device `containers-storage`).
	Direct,
	/// Anything else: another image or name, an undeclared tag, a digest pin, or no
	/// image spec at all. The string says what it follows.
	Foreign(String),
	/// No SSH, or an unreadable status. The string is the reason.
	Unreachable(String),
}

impl Class {
	/// A short annotation (`on :next`, `direct`, `foreign: quay.io/x:y`,
	/// `unreachable`).
	pub(crate) fn describe(&self) -> String {
		match self {
			Class::Channel(ch) => format!("on :{ch}"),
			Class::Direct => "direct".to_owned(),
			Class::Foreign(what) => format!("foreign ({what})"),
			Class::Unreachable(_) => "unreachable".to_owned(),
		}
	}
}

/// Classify a device's `spec.image` for the project named `name` with registry
/// repository `repo` (`<url>/<name>`, `None` without a registry) and declared
/// `channels`. Pure, so it's unit-tested on canned status JSON.
pub(crate) fn classify(
	spec: Option<&ImageSpec>,
	name: &str,
	repo: Option<&str>,
	channels: &[String],
) -> Class {
	let Some(spec) = spec else {
		return Class::Foreign("no image spec".to_owned());
	};
	let image = normalize_ref(&spec.image);
	if image == normalize_ref(&format!("localhost/{name}:latest")) {
		return Class::Direct;
	}
	if spec.is_registry()
		&& let Some(repo) = repo
		&& let Some(ch) = channels.iter().find(|c| normalize_ref(&format!("{repo}:{c}")) == image)
	{
		return Class::Channel(ch.clone());
	}
	Class::Foreign(spec.image.clone())
}

/// Normalise an image reference so equivalent spellings compare equal: the
/// implicit Docker Hub domain and its aliases (`index.docker.io`,
/// `registry-1.docker.io`) become `docker.io`, a single-component Hub repository
/// gains `library/`, and a missing tag becomes `:latest`. A digest reference
/// (`@sha256:…`) is kept verbatim apart from its domain.
pub(crate) fn normalize_ref(r: &str) -> String {
	let (first, rest) = r.split_once('/').unwrap_or(("", r));
	let explicit_domain =
		!first.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost");
	let (domain, path) = if explicit_domain { (first, rest) } else { ("docker.io", r) };
	let domain = match domain {
		"index.docker.io" | "registry-1.docker.io" => "docker.io",
		d => d,
	};
	let mut path = path.to_owned();
	if domain == "docker.io" && !path.contains('/') {
		path = format!("library/{path}");
	}
	// A tag is a `:` after the last `/`; a digest is an `@`.
	let last = path.rsplit('/').next().unwrap_or(&path);
	if !last.contains(':') && !last.contains('@') {
		path.push_str(":latest");
	}
	format!("{domain}/{path}")
}

/// One configured device and what [`probe`] found on it.
#[derive(Clone, Debug)]
pub(crate) struct Device {
	pub(crate) remote: RemoteConfig,
	pub(crate) ssh: Ssh,
	/// The probe result, or why it failed.
	pub(crate) status: Result<HostStatus, String>,
	pub(crate) class: Class,
}

impl Device {
	/// The device's display name (its configured destination).
	pub(crate) fn name(&self) -> &str {
		self.remote.dest()
	}

	/// The probed status (`None` when unreachable).
	pub(crate) fn status(&self) -> Option<&HostStatus> {
		self.status.as_ref().ok()
	}

	/// `rpi-1: direct, aarch64`-style annotation for prompts and logs.
	pub(crate) fn describe(&self) -> String {
		let arch = self.status().and_then(|s| s.arch).map(|a| format!(", {a}")).unwrap_or_default();
		format!("{}{arch}", self.class.describe())
	}
}

/// Probe `remote` once: arch, pull-credential presence and bootc status, all in one
/// `sh` script over SSH.
///
/// # Errors
///
/// Returns an error if SSH fails or the output can't be parsed.
pub(crate) fn probe(ssh: &Ssh) -> Result<HostStatus> {
	let out = ssh
		.read_sh(&probe_script())
		.with_context(|| format!("reading `bootc status` on {}", ssh.host()))?;
	parse_probe(&out).with_context(|| format!("parsing the device probe from {}", ssh.host()))
}

/// Probe and classify every remote in `remotes` in parallel (one SSH round trip
/// each). Unreachable devices come back as [`Class::Unreachable`], never an error.
pub(crate) fn probe_all(manifest: &Manifest, remotes: &[RemoteConfig]) -> Vec<Device> {
	let repo = manifest.repo();
	std::thread::scope(|scope| {
		let handles: Vec<_> = remotes
			.iter()
			.map(|rc| {
				let repo = repo.as_deref();
				scope.spawn(move || {
					let ssh = Ssh::from(rc);
					let status = probe(&ssh).map_err(|e| format!("{e:#}"));
					let class = match &status {
						Ok(s) => classify(
							s.spec.as_ref(),
							&manifest.general.name,
							repo,
							manifest.channels(),
						),
						Err(e) => Class::Unreachable(e.clone()),
					};
					Device { remote: rc.clone(), ssh, status, class }
				})
			})
			.collect();
		handles.into_iter().map(|h| h.join().expect("device probe thread panicked")).collect()
	})
}

/// Resolve `--remote` names against the configured remotes, in configuration
/// order. An empty `names` means "no restriction" and yields `None`.
///
/// # Errors
///
/// Returns an error naming any `--remote` that matches no configured remote, or
/// when there are no remotes at all.
pub(crate) fn select_remotes<'m>(
	manifest: &'m Manifest,
	names: &[String],
) -> Result<Option<Vec<&'m RemoteConfig>>> {
	if names.is_empty() {
		return Ok(None);
	}
	let remotes = manifest.remotes();
	if remotes.is_empty() {
		bail!("`--remote` names a device, but bootcher.toml lists no `remotes`");
	}
	for name in names {
		if !remotes.iter().any(|r| r.matches(name)) {
			let known: Vec<&str> = remotes.iter().map(RemoteConfig::dest).collect();
			bail!(
				"`--remote {name}` matches no configured remote; `remotes` lists: {}",
				known.join(", ")
			);
		}
	}
	Ok(Some(remotes.iter().filter(|r| names.iter().any(|n| r.matches(n))).collect()))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `bootc status --format json` for a device whose spec is `spec` (an
	/// `{image, transport, signature}` JSON object) and which booted `booted_digest`.
	fn status_json(spec: &str, booted_digest: &str) -> String {
		format!(
			r#"{{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
			"spec":{{"image":{spec},"bootOrder":"default"}},
			"status":{{"staged":null,"booted":{{"image":{{"image":{spec},
				"version":"44.20261120.0","timestamp":null,"imageDigest":"{booted_digest}"}},
				"pinned":false}},"rollback":null}}}}"#
		)
	}

	fn classify_json(json: &str) -> Class {
		let s = parse_status(json).unwrap();
		let channels = vec!["stable".to_owned(), "next".to_owned()];
		classify(s.spec.as_ref(), "kiosk", Some("ghcr.io/acme/kiosk"), &channels)
	}

	#[test]
	fn a_registry_origin_on_a_declared_channel_is_a_channel_device() {
		let json = status_json(
			r#"{"image":"ghcr.io/acme/kiosk:stable","transport":"registry","signature":"containerPolicy"}"#,
			"sha256:aaa",
		);
		assert_eq!(classify_json(&json), Class::Channel("stable".into()));
		// Unsigned origins are channel devices too.
		let json = status_json(
			r#"{"image":"ghcr.io/acme/kiosk:next","transport":"registry","signature":"insecure"}"#,
			"sha256:aaa",
		);
		assert_eq!(classify_json(&json), Class::Channel("next".into()));
	}

	#[test]
	fn localhost_latest_is_direct_under_any_transport_and_signature() {
		// S1: a fresh disk records the registry transport, a fresh takeover
		// containers-storage, both `containerPolicy` in a signing project; after the
		// first deploy it's unverified containers-storage.
		for (transport, sig) in [
			("registry", "containerPolicy"),
			("containers-storage", "containerPolicy"),
			("containers-storage", "insecure"),
		] {
			let spec = format!(
				r#"{{"image":"localhost/kiosk:latest","transport":"{transport}","signature":"{sig}"}}"#
			);
			assert_eq!(classify_json(&status_json(&spec, "sha256:b")), Class::Direct, "{spec}");
		}
	}

	#[test]
	fn everything_else_is_foreign() {
		for image in [
			"ghcr.io/acme/kiosk:latest",        // undeclared tag
			"ghcr.io/acme/other:stable",        // another image
			"ghcr.io/acme/kiosk@sha256:abcdef", // digest pin
			"localhost/other:latest",           // another local name
		] {
			let spec = format!(r#"{{"image":"{image}","transport":"registry"}}"#);
			assert!(
				matches!(classify_json(&status_json(&spec, "sha256:b")), Class::Foreign(_)),
				"{image}"
			);
		}
		// A declared channel ref under containers-storage isn't a channel device.
		let spec = r#"{"image":"ghcr.io/acme/kiosk:stable","transport":"containers-storage"}"#;
		assert!(matches!(classify_json(&status_json(spec, "sha256:b")), Class::Foreign(_)));
		// No spec at all.
		let s = parse_status(r#"{"spec":{"image":null},"status":{}}"#).unwrap();
		assert!(matches!(classify(s.spec.as_ref(), "kiosk", None, &[]), Class::Foreign(_)));
	}

	#[test]
	fn docker_hub_aliases_normalise() {
		let channels = vec!["stable".to_owned()];
		for image in [
			"docker.io/acme/kiosk:stable",
			"index.docker.io/acme/kiosk:stable",
			"registry-1.docker.io/acme/kiosk:stable",
			"acme/kiosk:stable",
		] {
			let spec =
				ImageSpec { image: image.into(), transport: "registry".into(), signature: None };
			assert_eq!(
				classify(Some(&spec), "kiosk", Some("docker.io/acme/kiosk"), &channels),
				Class::Channel("stable".into()),
				"{image}"
			);
		}
		assert_eq!(normalize_ref("busybox"), "docker.io/library/busybox:latest");
		assert_eq!(normalize_ref("localhost:5000/x"), "localhost:5000/x:latest");
		assert_eq!(normalize_ref("q.io/a/b@sha256:1"), "q.io/a/b@sha256:1");
	}

	#[test]
	fn renaming_the_project_or_a_channel_makes_devices_foreign() {
		let spec = ImageSpec {
			image: "ghcr.io/acme/kiosk:stable".into(),
			transport: "registry".into(),
			signature: None,
		};
		let renamed =
			classify(Some(&spec), "kiosk2", Some("ghcr.io/acme/kiosk2"), &["stable".into()]);
		assert!(matches!(renamed, Class::Foreign(_)));
		let dropped = classify(Some(&spec), "kiosk", Some("ghcr.io/acme/kiosk"), &["beta".into()]);
		assert!(matches!(dropped, Class::Foreign(_)));
	}

	#[test]
	fn probe_output_parses_facts_and_status() {
		let out = format!(
			"arch=aarch64\nauth=yes\n{PROBE_SEPARATOR}\n{}",
			status_json(
				r#"{"image":"localhost/kiosk:latest","transport":"containers-storage"}"#,
				"sha256:b"
			)
		);
		let s = parse_probe(&out).unwrap();
		assert_eq!(s.arch, Some(Arch::Aarch64));
		assert!(s.has_auth);
		assert_eq!(s.booted.as_ref().unwrap().digest, "sha256:b");
		assert_eq!(s.booted.unwrap().version.as_deref(), Some("44.20261120.0"));
		assert!(s.staged.is_none());
		assert!(parse_probe("arch=x86_64\n").is_err());
	}

	#[test]
	fn select_remotes_matches_names_and_rejects_unknown_ones() {
		let m: Manifest =
			toml::from_str("remotes = [\"admin@a\", \"admin@b\"]\n[general]\nname = \"x\"\n")
				.unwrap();
		assert!(select_remotes(&m, &[]).unwrap().is_none());
		let picked = select_remotes(&m, &["b".into()]).unwrap().unwrap();
		assert_eq!(picked.iter().map(|r| r.dest()).collect::<Vec<_>>(), ["admin@b"]);
		let err = select_remotes(&m, &["c".into()]).unwrap_err().to_string();
		assert!(err.contains("matches no configured remote") && err.contains("admin@a"), "{err}");
	}
}
