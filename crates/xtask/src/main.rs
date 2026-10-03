//! Development task runner for bootcher itself.
//!
//! A dev-only CLI (never published, see `publish = false`) holding the workflows
//! a contributor runs: `cargo xtask ci`, `cargo xtask run …`, `cargo xtask
//! release`. The tasks are almost all thin wrappers over `cargo`, so keeping
//! them here means the repo needs no task runner beyond cargo itself — and the
//! one task with real logic (`release`, which pushes tags) is typed Rust rather
//! than a shell heredoc.
//!
//! bootcher is a standalone CLI: end users `cargo install` it and run `bootcher
//! <cmd>` *inside their own project* (scaffolded by `bootcher init`). So the
//! build/image/deploy workflows live in a project, not here — use `cargo xtask
//! run` to point a dev build at one. Unlike the other tasks it does *not* run
//! from the repo root: bootcher reads `./bootcher.toml`, so it keeps the
//! directory it was invoked from.
//!
//! ```text
//! cargo xtask run init my-project
//! cargo xtask run build              # (from inside a project dir)
//! ```
//!
//! The `cargo xtask` alias is discovered by walking up from the working
//! directory, so this only reaches projects nested under the repo. For a project
//! elsewhere on disk, `cargo xtask install` and use the real `bootcher` binary.
//!
//! # Threading flags into cargo
//!
//! Every cargo invocation below splices in `$BOOTCHER_CARGO_FLAGS` (whitespace
//! separated). It's empty for local dev; CI sets it to `--frozen` to stay
//! offline and pin `Cargo.lock` after a prior `cargo fetch --locked`. An
//! environment variable rather than a flag because these tasks are reached
//! through the `cargo xtask` alias, which would otherwise need the flag twice —
//! once for the build of *this* binary, once for what it shells out to.
//! (`CARGO_NET_OFFLINE=true` covers the outer build; `--locked` has no config-key
//! equivalent, hence this.)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
	env,
	ffi::OsString,
	os::unix::process::ExitStatusExt,
	path::{Path, PathBuf},
	process::{Command, ExitCode, ExitStatus},
};
use xshell::{Shell, cmd};

mod release;

#[derive(Parser)]
#[command(
	about = "development task runner for bootcher itself",
	long_about = None,
	// Bare `cargo xtask` lists the tasks rather than erroring out — the task
	// list is the discovery path, the way `just`'s default recipe used to be.
	arg_required_else_help = true
)]
struct Cli {
	#[command(subcommand)]
	cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// Run the dev build of the bootcher binary with arbitrary args, against
	/// whatever project directory you're in.
	Run {
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		args: Vec<String>,
	},
	/// Install the CLI onto PATH (the real-world entry point: `cargo install`).
	Install,
	/// Build the cross-platform container image locally (what CI publishes to ghcr.io).
	Image {
		#[arg(default_value = "bootcher:dev")]
		tag: String,
	},
	/// Run the test suite.
	Test,
	/// Everything CI runs (invoked by .github/workflows/verify.yml so the two
	/// can't drift): fmt check, clippy, doc, test.
	Ci {
		/// Additionally run the slow VM end-to-end suite (see `e2e`).
		#[arg(long)]
		e2e: bool,
		/// Additionally run the cross-arch builder test (see `e2e-cross`).
		#[arg(long = "e2e-cross")]
		e2e_cross: bool,
	},
	/// Slow VM end-to-end: boot real bootc disks and exercise the LAN rotate +
	/// upgrade lifecycle (`e2e_vm`), the plain registry lifecycle + pull-token
	/// rotation + signing enrollment (`e2e_registry`), the LAN→registry origin
	/// switch (`e2e_lan_to_registry`), registry-mode image signing
	/// (`e2e_registry_sign`), release-channel isolation (`e2e_channels`), and
	/// the in-place takeover of a stock Debian host under UEFI and legacy BIOS
	/// (`e2e_takeover`, x86-64 hosts only).
	///
	/// Opt-in everywhere because it needs podman, qemu+KVM, UEFI firmware, and
	/// passwordless `sudo sh`; it hard-fails when those are missing (it asserts
	/// its prerequisites rather than skipping), so only run it on a host that can
	/// actually host a VM.
	///
	/// Each test uses its own podman store under /var/tmp/bootcher-e2e-*-store,
	/// wiped on teardown by default. Set `BOOTCHER_E2E_KEEP_STORE=1` to keep them
	/// across runs (warm base-image cache, faster reruns) at the cost of several
	/// GB of disk per store.
	E2e,
	/// Cross-arch builder end-to-end: build a *foreign-arch* bootc disk
	/// (image-builder in a TCG builder VM) and boot it under TCG. Far slower than
	/// `e2e` and with different prerequisites (foreign qemu + firmware +
	/// qemu-user binfmt; no KVM/sudo), so it's gated apart.
	///
	/// Honors `BOOTCHER_E2E_KEEP_STORE` (see `e2e`) for its podman store; the
	/// separate foreign-arch cloud-image download cache is always kept regardless.
	E2eCross,
	/// Attempt to fix formatting and linting issues.
	Fix,
	/// Build the published site into docs/site/: the landing page (docs/landing/)
	/// at the root, the mdBook under docs/site/book/, and rustdoc embedded under
	/// book/api/. The GitHub Pages workflow uploads docs/site/ as-is.
	Docs,
	/// Build the site and serve it at <http://localhost:3000>.
	SiteServe,
	/// Serve the documentation with live reload at <http://localhost:3000>.
	///
	/// Note: mdbook clears its output on every rebuild, so the rustdoc embedded
	/// by `docs` does not survive here. Use this to edit prose and `docs` when
	/// you need working API links.
	DocsWatch,
	/// Cut a release: bump the workspace version, commit, tag vX.Y.Z, and push —
	/// which triggers the release/image jobs in .github/workflows/release.yml.
	/// Must be on a clean `main`.
	Release {
		/// `patch`, `minor`, `major`, or an explicit `X.Y.Z`.
		#[arg(default_value = "patch")]
		bump: String,
	},
}

fn main() -> ExitCode {
	match run() {
		Ok(code) => code,
		Err(e) => {
			eprintln!("xtask: {e:#}");
			ExitCode::FAILURE
		}
	}
}

fn run() -> Result<ExitCode> {
	let cli = Cli::parse();
	let sh = Shell::new()?;
	// Every task is written relative to the repo root; `cargo run` leaves the
	// child in the caller's directory, so anchor once here (this is what running
	// under `just` used to provide for free). The exception is `run`, which
	// deliberately restores the caller's directory below.
	let invoked_from = sh.current_dir();
	sh.change_dir(workspace_root());

	let cargo = cargo();
	// Bound by reference so the `{flags...}` splats below borrow rather than
	// consume it — several tasks issue more than one cargo invocation.
	let cargo_flags = cargo_flags();
	let flags = &cargo_flags;

	match cli.cmd {
		Cmd::Run { args } => {
			// A dev build pointed at the *user's* project: bootcher reads
			// ./bootcher.toml, so it has to run where the developer invoked it.
			sh.change_dir(invoked_from);
			return passthrough(&sh, &cargo, flags, &args);
		}
		Cmd::Install => {
			cmd!(sh, "{cargo} install --path crates/bootcher-cli").run()?;
		}
		Cmd::Image { tag } => {
			cmd!(sh, "podman build -f Containerfile -t {tag} .").run()?;
		}
		Cmd::Test => test(&sh, &cargo, flags)?,
		Cmd::Ci { e2e: run_e2e, e2e_cross: run_e2e_cross } => {
			cmd!(sh, "{cargo} fmt --check").run()?;
			cmd!(sh, "{cargo} clippy --all-targets {flags...} -- -D warnings").run()?;
			// --document-private-items so intra-doc links in private items' docs
			// get checked too. This is a lint pass, output is discarded; the
			// published `docs` task stays public-only.
			cmd!(sh, "{cargo} doc --workspace --no-deps --document-private-items {flags...}")
				.env("RUSTDOCFLAGS", "-D warnings")
				.run()?;
			test(&sh, &cargo, flags)?;
			if run_e2e {
				e2e(&sh, &cargo, flags)?;
			}
			if run_e2e_cross {
				e2e_cross(&sh, &cargo, flags)?;
			}
		}
		Cmd::E2e => e2e(&sh, &cargo, flags)?,
		Cmd::E2eCross => e2e_cross(&sh, &cargo, flags)?,
		Cmd::Fix => {
			cmd!(sh, "{cargo} fmt").run()?;
			cmd!(sh, "{cargo} clippy --fix --allow-dirty --all-targets {flags...} -- -D warnings")
				.run()?;
		}
		Cmd::Docs => docs(&sh, &cargo, flags)?,
		Cmd::SiteServe => {
			docs(&sh, &cargo, flags)?;
			cmd!(sh, "python3 -m http.server 3000 --directory docs/site").run()?;
		}
		Cmd::DocsWatch => {
			cmd!(sh, "mdbook serve docs/").run()?;
		}
		Cmd::Release { bump } => release::release(&sh, &bump)?,
	}
	Ok(ExitCode::SUCCESS)
}

/// Run the dev build of the CLI as a *transparent* passthrough, the one task
/// whose job is to stand in for the real `bootcher` binary.
///
/// Driven with [`Command`] rather than `xshell` so the child keeps this
/// process's stdin (xshell hands its children `Stdio::null()`, which would kill
/// any prompt or TTY-driven progress rendering) and so its exit status is
/// forwarded verbatim — a plain `?` would flatten every failure to 1, hiding
/// bootcher's 130-on-interrupt. Nothing is echoed either: a stand-in shouldn't
/// print a command line the real binary wouldn't.
fn passthrough(sh: &Shell, cargo: &Path, flags: &[OsString], args: &[String]) -> Result<ExitCode> {
	let status = Command::new(cargo)
		.current_dir(sh.current_dir())
		.args(["run", "--quiet"])
		.args(flags)
		.args(["-p", "bootcher-cli", "--"])
		.args(args)
		.status()
		.context("running the bootcher dev build")?;
	Ok(ExitCode::from(exit_code(status)))
}

/// Map a child's exit status onto our own, following the shell convention of
/// `128 + signal` when it was killed by one ([`ExitStatus::code`] is `None`
/// there, and collapsing that to a generic failure would hide exactly the
/// signal handling bootcher implements).
fn exit_code(status: ExitStatus) -> u8 {
	let code = status.code().or_else(|| status.signal().map(|sig| 128 + sig)).unwrap_or(1);
	u8::try_from(code).unwrap_or(1)
}

fn test(sh: &Shell, cargo: &Path, flags: &[OsString]) -> Result<()> {
	cmd!(sh, "{cargo} test {flags...}").run()?;
	Ok(())
}

fn e2e(sh: &Shell, cargo: &Path, flags: &[OsString]) -> Result<()> {
	let mut suites =
		vec!["e2e_vm", "e2e_registry", "e2e_lan_to_registry", "e2e_registry_sign", "e2e_channels"];
	// The takeover guest is a pinned amd64 Debian image, booted under KVM.
	if cfg!(target_arch = "x86_64") {
		suites.push("e2e_takeover");
	}
	for suite in suites {
		cmd!(
			sh,
			"{cargo} test {flags...} -p bootcher-cli --features=e2e --test {suite} -- --nocapture"
		)
		.run()?;
	}
	Ok(())
}

fn e2e_cross(sh: &Shell, cargo: &Path, flags: &[OsString]) -> Result<()> {
	cmd!(
		sh,
		"{cargo} test {flags...} -p bootcher-cli --features=e2e_cross --test e2e_cross_arch -- --nocapture"
	)
	.run()?;
	Ok(())
}

fn docs(sh: &Shell, cargo: &Path, flags: &[OsString]) -> Result<()> {
	cmd!(sh, "mdbook build docs/").run()?;
	cmd!(sh, "{cargo} doc --workspace --no-deps {flags...}").run()?;
	sh.remove_path("docs/book/api")?;
	cmd!(sh, "cp -r target/doc docs/book/api").run()?;
	sh.remove_path("docs/site")?;
	sh.create_dir("docs/site")?;
	// The trailing `/.` copies the *contents* of landing/ to the site root.
	cmd!(sh, "cp -r docs/landing/. docs/site/").run()?;
	cmd!(sh, "cp -r docs/book docs/site/book").run()?;
	Ok(())
}

/// The repo root, derived at compile time from this crate's location
/// (`crates/xtask/` → `crates/` → root). Baked in rather than discovered so the
/// tasks resolve identically under `cargo xtask` (which inherits the caller's
/// directory) and `cargo test` (which uses the package root).
fn workspace_root() -> &'static Path {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.parent()
		.and_then(Path::parent)
		.expect("CARGO_MANIFEST_DIR should be crates/xtask inside the workspace")
}

/// The cargo that invoked us, so a `cargo +nightly xtask` or a non-default
/// toolchain propagates to the nested invocations instead of silently falling
/// back to whatever `cargo` is first on PATH.
fn cargo() -> PathBuf {
	env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// Extra flags spliced into every nested cargo invocation, from
/// `$BOOTCHER_CARGO_FLAGS`. See the module docs for why this is an env var.
fn cargo_flags() -> Vec<OsString> {
	env::var("BOOTCHER_CARGO_FLAGS")
		.unwrap_or_default()
		.split_whitespace()
		.map(OsString::from)
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A raw `wait(2)` status: the exit code lives in the high byte, a
	/// terminating signal in the low one.
	fn exited(code: i32) -> ExitStatus {
		ExitStatus::from_raw(code << 8)
	}

	#[test]
	fn exit_codes_are_forwarded_verbatim() {
		assert_eq!(exit_code(exited(0)), 0);
		assert_eq!(exit_code(exited(1)), 1);
		// 130 is what bootcher itself reports on interrupt — the case a plain
		// `?` would have flattened to 1.
		assert_eq!(exit_code(exited(130)), 130);
	}

	#[test]
	fn a_signalled_child_becomes_128_plus_the_signal() {
		// SIGTERM (15), the signal bootcher's graceful-shutdown path handles.
		assert_eq!(exit_code(ExitStatus::from_raw(15)), 143);
	}
}
