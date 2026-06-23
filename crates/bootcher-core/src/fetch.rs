//! Streaming downloads and the shared read-pump that drives a bytes bar.
//!
//! Like [`exec`](crate::exec), this is an I/O concern rather than a progress
//! concern; it reports under a [`Scope`] passed in.

use crate::progress::Scope;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256, Sha512};
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A checksum to verify a [`download`] against, tagged by algorithm. Different
/// publishers ship different digests — the Fedora Cloud base names a sha256, the
/// Debian cloud images publish a `SHA512SUMS` — so the caller picks the algorithm
/// and the streaming pump selects the matching hasher. The wrapped `&str` is the
/// expected lowercase-hex digest.
#[derive(Clone, Copy)]
pub enum Checksum<'a> {
	Sha256(&'a str),
	Sha512(&'a str),
}

impl Checksum<'_> {
	/// The expected lowercase-hex digest string, regardless of algorithm.
	fn expected(&self) -> &str {
		match self {
			Checksum::Sha256(s) | Checksum::Sha512(s) => s,
		}
	}

	/// A fresh hasher for this checksum's algorithm.
	fn hasher(&self) -> Hasher {
		match self {
			Checksum::Sha256(_) => Hasher::Sha256(Sha256::new()),
			Checksum::Sha512(_) => Hasher::Sha512(Sha512::new()),
		}
	}
}

/// The streaming hasher behind a [`Checksum`] — one variant per supported
/// algorithm, fed chunk-by-chunk during the download pass and finalized to a raw
/// digest. A thin enum (rather than `Box<dyn DynDigest>`) keeps the two concrete
/// `sha2` hashers monomorphized and the dependency surface unchanged.
enum Hasher {
	Sha256(Sha256),
	Sha512(Sha512),
}

impl Hasher {
	fn update(&mut self, data: &[u8]) {
		match self {
			Hasher::Sha256(h) => h.update(data),
			Hasher::Sha512(h) => h.update(data),
		}
	}

	fn finalize(self) -> Vec<u8> {
		match self {
			Hasher::Sha256(h) => h.finalize().to_vec(),
			Hasher::Sha512(h) => h.finalize().to_vec(),
		}
	}
}

/// Read `reader` to EOF in fixed-size chunks, handing each filled slice to
/// `on_chunk` while advancing a bytes bar under `scope`. The one read-loop behind
/// every streaming pump (raw disk write, download, hashing) so they stay
/// consistent and `read`'s partial-fill / short-read semantics are handled in
/// exactly one place. `on_chunk` may fail (e.g. a write error) — its error
/// short-circuits the pump.
///
/// # Errors
///
/// Returns an error if a read fails, a signal interrupts, or `on_chunk` returns an error.
pub(crate) fn pump(
	scope: &Scope,
	bytes: Option<u64>,
	label: impl Into<Cow<'static, str>>,
	mut reader: impl Read,
	mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
	const BUF_SIZE: usize = 1024 * 1024;
	let pb = scope.bytes(bytes);
	// The bar's own label, so a transfer reads as e.g. "fetch artifacts" beside its
	// byte counter — the more so when several render at once under per-arch headers.
	pb.set_message(label.into());

	let mut buf = vec![0u8; BUF_SIZE];
	while let n @ 1.. = reader.read(&mut buf)? {
		crate::signals::check()?;
		on_chunk(&buf[..n])?;
		pb.inc(n as u64);
	}
	Ok(())
}

/// Atomically download `url` to `dest`: stream into a `<dest>.part` sibling (so
/// an interrupted transfer never leaves a corrupt file at `dest`), optionally
/// verify a [`Checksum`] (sha256 or sha512, per what the publisher ships), then
/// rename into place. The hash is computed during the same streaming pass — no
/// second read — and a mismatch deletes the partial file and errors. The single
/// source of truth for "fetch a cached artifact", shared by the Fedora Cloud base
/// image and the takeover e2e's Debian base.
///
/// # Errors
///
/// Returns an error if the HTTP request fails, the checksum mismatches, or the file
/// can't be written or renamed into place.
pub fn download(scope: &Scope, url: &str, dest: &Path, expected: Option<Checksum>) -> Result<()> {
	let tmp = part_path(dest);
	let mut file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

	let resp = ureq::get(url).call()?;
	let total = resp
		.headers()
		.get("content-length")
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.parse::<u64>().ok());
	let reader = resp.into_body().into_reader();
	let mut hasher = expected.map(|c| c.hasher());
	pump(scope, total, "downloading", reader, |chunk| {
		file.write_all(chunk)?;
		if let Some(h) = hasher.as_mut() {
			h.update(chunk);
		}
		Ok(())
	})?;

	let pb = scope.spinner("flushing...");
	file.flush()?;
	drop(file);
	pb.finish();

	if let (Some(expected), Some(hasher)) = (expected, hasher) {
		let digest = hex(&hasher.finalize());
		if digest != expected.expected() {
			let _ = std::fs::remove_file(&tmp);
			bail!("checksum mismatch for {url}: expected {}, got {digest}", expected.expected());
		}
	}

	std::fs::rename(&tmp, dest)
		.with_context(|| format!("moving downloaded file into {}", dest.display()))?;
	Ok(())
}

/// The `<dest>.part` sibling a download streams into before its atomic rename.
fn part_path(dest: &Path) -> PathBuf {
	let mut name = dest.file_name().unwrap_or_default().to_os_string();
	name.push(".part");
	dest.with_file_name(name)
}

/// Lowercase hex encoding of `bytes`, two chars per byte. Used to render sha256
/// digests for the download checksum check.
fn hex(bytes: &[u8]) -> String {
	use std::fmt::Write;
	let mut s = String::with_capacity(bytes.len() * 2);
	for b in bytes {
		let _ = write!(s, "{b:02x}");
	}
	s
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hex_encodes_known_vectors() {
		assert_eq!(hex(&[]), "");
		assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
		assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
		// Always lowercase, always two chars per byte.
		assert_eq!(hex(&[0xa5; 4]), "a5a5a5a5");
	}

	#[test]
	fn part_path_appends_suffix_beside_dest() {
		// The temp sibling sits next to the destination (same dir) so the final
		// rename stays within one filesystem and is therefore atomic.
		assert_eq!(part_path(Path::new("/cache/base.qcow2")), Path::new("/cache/base.qcow2.part"));
		assert_eq!(part_path(Path::new("fw.zip")), Path::new("fw.zip.part"));
	}
}
