//! `bootcher release …`: named releases in the registry.
//!
//! A release `:X` is any valid OCI tag the user picks. Each entry of its multi-arch
//! index carries the `bootcher.release=X` annotation and each member the `bootcher.source=<sha>`
//! label of the clean commit it was built from. The one rule: `:X` may only be
//! written by a clean build of the commit its members record, which keeps
//! publishing idempotent, makes refreshes legal, and stops `:X` from ever changing
//! source.

use crate::context::{Manifest, RESERVED_TAG_PREFIXES};
use crate::oci;
use anyhow::Result;

/// Whether `tag` could name a release: not a declared channel and not reserved.
pub(crate) fn is_release_candidate(manifest: &Manifest, tag: &str) -> bool {
	!manifest.channels().iter().any(|c| c == tag)
		&& !RESERVED_TAG_PREFIXES.iter().any(|p| tag.starts_with(p))
}

/// Every published release: each tag (filtered by the reserved names) whose index
/// carries a release annotation, with its fetched index.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn releases(manifest: &Manifest) -> Result<Vec<(String, oci::Fetched)>> {
	let Some(repo) = manifest.repo() else { return Ok(Vec::new()) };
	let mut out = Vec::new();
	for tag in oci::list_tags(&repo)? {
		if !is_release_candidate(manifest, &tag) {
			continue;
		}
		if let Some(f) = oci::fetch(&format!("{repo}:{tag}"))?
			&& f.manifest.release().is_some()
		{
			out.push((tag, f));
		}
	}
	Ok(out)
}

/// Every release with its member digests — for mapping a device's booted digest
/// (a per-arch member digest, S2) to the release it belongs to.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn member_digests(manifest: &Manifest) -> Result<Vec<(String, Vec<String>)>> {
	Ok(releases(manifest)?
		.into_iter()
		.map(|(tag, f)| (tag, f.manifest.members().into_iter().map(|(_, d)| d).collect()))
		.collect())
}
