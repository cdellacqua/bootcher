//! The shared on-disk cache root, `~/.cache/bootcher`.
//!
//! Bootcher caches large, re-downloadable artifacts between runs — currently the
//! cross-arch builder VM images (under `builder/`). They live beneath one root so
//! a single `bootcher clean` wipes the lot, while each consumer owns its own
//! named subdirectory beneath it.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `~/.cache/bootcher` — the location, without touching the filesystem. Honours
/// `XDG_CACHE_HOME`, falling back to `~/.cache`.
///
/// # Errors
///
/// Returns an error if neither `XDG_CACHE_HOME` nor `HOME` is set.
pub(crate) fn root() -> Result<PathBuf> {
	let base = std::env::var_os("XDG_CACHE_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
		.context("neither XDG_CACHE_HOME nor HOME is set")?;
	Ok(base.join("bootcher"))
}

/// `~/.cache/bootcher/<name>`, created if missing. Each subsystem (the builder
/// VM, …) owns one named subdirectory beneath the shared root.
///
/// # Errors
///
/// Returns an error if the cache root can't be determined or the directory can't be created.
pub(crate) fn subdir(name: &str) -> Result<PathBuf> {
	let dir = root()?.join(name);
	std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	Ok(dir)
}

/// Delete the entire cache root. Safe anytime — everything beneath it is
/// re-downloadable, so the next build just refetches. Idempotent: a
/// missing cache is reported, not an error.
///
/// # Errors
///
/// Returns an error if the cache root can't be determined or the directory can't be removed.
pub fn clean() -> Result<()> {
	let dir = root()?;
	if !dir.exists() {
		eprintln!("clean: cache is already empty ({})", dir.display());
		return Ok(());
	}
	std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
	eprintln!("clean: removed cache at {}", dir.display());
	Ok(())
}
