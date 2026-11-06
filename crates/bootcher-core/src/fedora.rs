//! The Fedora release a new project's `fedora-bootc` base is pinned to.
//!
//! The scaffold `Containerfile` builds from one Fedora release
//! (`ARG FEDORA_VERSION=<N>` / `FROM quay.io/fedora/fedora-bootc:${FEDORA_VERSION}`)
//! instead of following `:latest`, which jumps to a new major release on Fedora's
//! release day and would carry an unattended fleet with it. `init` sets the pin to
//! the current stable release, read here from the registry. Noticing when a pin has
//! fallen behind is left to the CI recipes, which users adapt to their own setup.

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

/// The global `ARG` the scaffold `Containerfile` sets the release through.
pub const RELEASE_ARG: &str = "FEDORA_VERSION";

/// The manifest of the tag that follows the current stable Fedora release, served
/// anonymously by quay.io.
const STABLE_MANIFEST: &str = "https://quay.io/v2/fedora/fedora-bootc/manifests/latest";

/// The manifest annotation carrying the build's version, e.g. `45.20261105.0`. The
/// leading component is the Fedora release.
const VERSION_KEY: &str = "org.opencontainers.image.version";

/// Media types accepted for a manifest: the multi-arch index `:latest` points at,
/// and the per-arch manifest it lists.
const MANIFEST_ACCEPT: &str =
	"application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json";

/// The scaffold `Containerfile`, embedded so the pinned release can be read back.
const SCAFFOLD_CONTAINERFILE: &str = include_str!("../scaffold/Containerfile");

/// The release a Containerfile pins through its `ARG FEDORA_VERSION=<N>` line.
fn pinned(containerfile: &str) -> Option<u32> {
	let prefix = format!("ARG {RELEASE_ARG}=");
	containerfile.lines().find_map(|line| line.trim().strip_prefix(&prefix)?.trim().parse().ok())
}

/// The release the embedded scaffold `Containerfile` pins: the stable release when
/// this bootcher version was published.
///
/// # Errors
///
/// Returns an error if the scaffold has no `ARG FEDORA_VERSION=<N>` line.
pub fn scaffold_release() -> Result<u32> {
	pinned(SCAFFOLD_CONTAINERFILE)
		.with_context(|| format!("the scaffold Containerfile has no `ARG {RELEASE_ARG}=<N>`"))
}

/// The current stable Fedora release: the leading component of the version
/// `fedora-bootc:latest` carries.
///
/// # Errors
///
/// Returns an error if quay.io can't be reached or the image carries no parseable
/// version.
pub fn current_release() -> Result<u32> {
	let agent: ureq::Agent =
		ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into();
	let get = |url: &str| -> Result<Value> {
		let body = agent
			.get(url)
			.header("Accept", MANIFEST_ACCEPT)
			.call()
			.and_then(|mut r| r.body_mut().read_to_string())
			.with_context(|| format!("fetching {url}"))?;
		serde_json::from_str(&body).with_context(|| format!("parsing {url}"))
	};
	// Every arch in the index is the same release, so the first one will do.
	let index = get(STABLE_MANIFEST)?;
	let digest =
		index["manifests"][0]["digest"].as_str().context("fedora-bootc:latest lists no image")?;
	let manifest = get(&STABLE_MANIFEST.replace("latest", digest))?;
	let version = manifest["annotations"][VERSION_KEY]
		.as_str()
		.context("fedora-bootc:latest carries no version")?;
	release_of(version).with_context(|| format!("unexpected fedora-bootc version {version:?}"))
}

/// The Fedora release in a version string like `45.20261105.0`.
fn release_of(version: &str) -> Option<u32> {
	version.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pin_from_the_arg_line() {
		let file = "# comment\nARG FEDORA_VERSION=45\nFROM quay.io/fedora/fedora-bootc:${FEDORA_VERSION}\n";
		assert_eq!(pinned(file), Some(45));
		assert_eq!(pinned("FROM quay.io/fedora/fedora-bootc:latest\n"), None);
		assert_eq!(pinned("ARG FEDORA_VERSION=latest\n"), None);
	}

	#[test]
	fn release_from_version() {
		assert_eq!(release_of("45.20261105.0"), Some(45));
		assert_eq!(release_of("rawhide"), None);
	}

	#[test]
	fn the_scaffold_pins_a_release() {
		assert!(scaffold_release().is_ok());
	}
}
