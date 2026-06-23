use anyhow::{Context, Result};
use bootcher_core::context::{self, Manifest, ToSsh};
use bootcher_core::progress::Scope;
use bootcher_core::{cache, jobs, pipelines};
use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "bootc image provisioning pipeline")]
struct Cli {
	#[command(subcommand)]
	cmd: Cmd,
}

/// A bootcher project is one image, described by `bootcher.toml` in the working
/// directory. Everything the image-building subcommands need — the name, the
/// target arch(es) (`platform`, one or several for a multi-arch image), the build
/// location, the layout — comes from the manifest; edit it to change the target.
#[derive(Subcommand)]
enum Cmd {
	/// Scaffold a new bootcher project: create `<name>/` with a starter
	/// Containerfile (`FROM fedora-bootc`), the `sysroot/` overlay, and a
	/// `bootcher.toml`.
	Init {
		/// Project path to create (its final component is the image name; missing
		/// parent dirs are made and `.`/`..` resolved). Omit to scaffold the
		/// current directory in place.
		name: Option<String>,
		/// Accept defaults without prompting (host platform, LAN deploys) — the
		/// non-interactive path for scripts/CI.
		#[arg(short = 'y', long = "yes")]
		yes: bool,
		/// Scaffold into a non-empty (or already-existing) directory anyway,
		/// overwriting any files the scaffold collides with.
		#[arg(short = 'f', long = "force")]
		force: bool,
	},
	/// First-time provisioning: build the container, then build its disk image under
	/// `output/` for you to write or upload to the target device. Any project-specific
	/// post-processing (e.g. embedding firmware) can be performed by custom
	/// lifecycle hooks (e.g. `[hooks.disk] post = "dd ..."`) configurable in `bootcher.toml`.
	Provision {
		/// Admin SSH private key to install on the device. The public key is read
		/// from the `<key>.pub` sibling, mirroring how `[deploy] registry` signing
		/// keys work. Omit to pick interactively on a TTY, or to auto-select when
		/// only one key is available.
		#[arg(long = "ssh-key")]
		ssh_key: Option<String>,
		/// When provisioning, if you want to specify a registry for future updates and
		/// that registry requires an authentication token, bootcher will try to validate it
		/// from the current machine. With this flag you can skip the validation entirely since it
		/// might not be possibile if, for example, the registry and devices you're provisioning
		/// are inside a VPN.
		#[arg(long = "skip-pull-check")]
		skip_pull_check: bool,
		/// The configured registry is public (no authentication): provision without a
		/// pull credential, baking no `/etc/ostree/auth.json`, so the device pulls
		/// anonymously. Without this, a registry-mode provision requires a pull
		/// credential. No effect in LAN mode.
		#[arg(long = "anonymous")]
		anonymous: bool,
		/// Build the disk from the already-built container, skipping the container
		/// build — for iterating on the disk step alone, or when an earlier `build`
		/// already produced the container.
		#[arg(long = "skip-build")]
		skip_build: bool,
	},
	/// Subsequent deployment to the configured targets in `bootcher.toml`: build the
	/// container, then push it and trigger `bootc upgrade`.
	/// For LAN deploys this uploads the image by emulating a simple registry and
	/// tunnelling via an SSH connection.
	/// If a registry is configured, this pushes the up-to-date container image to it and,
	/// if `bootcher.toml` includes remotes, a `bootc upgrade` command will be launched via SSH to start
	/// the upgrade immediately.
	Deploy {
		/// Registry mode only: push the image to the registry and return without
		/// `SSHing` into the configured remotes to run `bootc upgrade` + reboot.
		/// Devices will pick up the new revision on their next auto-update cycle.
		/// Has no effect in LAN mode.
		#[arg(long = "skip-bootc-upgrade")]
		skip_bootc_upgrade: bool,
		/// Push the already-built container, skipping the container build — for
		/// shipping an image an earlier `build` already produced, or iterating on the
		/// push/upgrade step alone.
		#[arg(long = "skip-build")]
		skip_build: bool,
	},
	/// Build the bootc container image.
	Build,
	/// Convert live, SSH-reachable hosts (stock Ubuntu/Debian/Rocky on a VPS) into
	/// bootc systems **in place** with `bootc install to-existing-root`: build + a
	/// destructive per-host rollout over the `[deploy] remotes` SSH channel. The
	/// aggressive name is deliberate — this irreversibly wipes each target's current
	/// OS. Afterwards each host is an ordinary bootc device on the same origin, so
	/// `deploy`/`rotate` work against it unchanged. Each host must already
	/// have `podman` and `sudo` installed (bootcher never mutates the foreign distro).
	Takeover {
		/// Admin SSH private key: its public half is installed on the new system, and
		/// its private half is the identity bootcher reconnects with as `admin@host`
		/// after the reboot (the stock cloud user is gone by then). Effectively
		/// required; omit only to pick interactively on a TTY. See `provision`.
		#[arg(long = "ssh-key")]
		ssh_key: Option<String>,
		/// Fleet-wide default stock cloud login for the *initial* connection
		/// (`debian`/`ubuntu`/`cloud-user`/`root`), before the image's `admin`
		/// user replaces it. Override per host with a remote's `takeover_login` in
		/// bootcher.toml. There is no safe cross-distro default, so a host with
		/// neither set is an error.
		#[arg(long = "login")]
		login: Option<String>,
		/// See `provision`.
		#[arg(long = "skip-pull-check")]
		skip_pull_check: bool,
		/// See `provision`.
		#[arg(long = "anonymous")]
		anonymous: bool,
		/// Skip the interactive destructive-action confirmation (the scriptable/CI
		/// path). Without it, a non-TTY run fails closed rather than wiping a host
		/// unprompted.
		#[arg(short = 'y', long = "yes")]
		yes: bool,
		/// Convert the hosts from the already-built container, skipping the container
		/// build — for re-running against more hosts, or when an earlier `build`
		/// already produced the image.
		#[arg(long = "skip-build")]
		skip_build: bool,
	},
	/// Image-signing helpers (registry mode). See `sign`.
	#[command(subcommand)]
	Sign(SignCmd),
	/// Rotate a credential on the already-deployed devices in `[deploy] remotes`,
	/// over the same SSH channel `deploy` uses — no rebuild or re-provision, just
	/// the on-device secret replaced. See `rotate`.
	#[command(subcommand)]
	Rotate(RotateCmd),
	/// Delete the bootcher cache (`~/.cache/bootcher`): the cross-arch builder VM
	/// images.
	Clean,
}

/// `bootcher sign <what>`: container-image signing helpers. Signing is opt-in via
/// the `[deploy] registry` inline-table form (registry mode only); see `bootcher.toml`.
#[derive(Subcommand)]
enum SignCmd {
	/// Generate a cosign/sigstore signing keypair — `<prefix>.key` (private,
	/// 0600, git-ignored) and `<prefix>.pub` (public, safe to commit). Set
	/// `key` in `[deploy] registry`; the passphrase is read from
	/// `BOOTCHER_SIGN_PASSPHRASE` or prompted.
	Enroll {
		/// Output filename prefix (`<prefix>.key` + `<prefix>.pub`).
		#[arg(default_value = "cosign")]
		prefix: String,
		/// Overwrite an existing `<prefix>.key`, any image signed with the old key becomes unverifiable.
		#[arg(short = 'f', long = "force")]
		force: bool,
	},
	/// Verify that a registry image carries a valid cosign/sigstore signature
	/// from a given public key — the same check a signing-enforcing device
	/// performs on `bootc upgrade`. Exits 0 on success; useful as a post-deploy
	/// sanity check in CI. Registry mode only.
	Verify {
		/// Fully-qualified image reference to verify
		/// (e.g. `reg.example.com/org/name:tag`).
		image: String,
		/// Path to the cosign/sigstore public key (`.pub` from `bootcher sign
		/// enroll`). Defaults to the public key from `key` in `[deploy] registry`
		/// in `bootcher.toml`.
		#[arg(long)]
		pubkey: Option<String>,
		/// Verify the registry's TLS certificate. Pass `--tls-verify=false` for
		/// a plain-HTTP registry (e.g. a local test registry).
		#[arg(long = "tls-verify", default_value = "true")]
		tls_verify: bool,
	},
}

/// `bootcher rotate <what>`: which on-device credential to roll. The targets are
/// the `[deploy] remotes` devices; the new value is collected the same way
/// `provision` collects it (TTY prompt, or env on a non-TTY run).
#[derive(Subcommand)]
enum RotateCmd {
	/// Replace the registry pull token (`/etc/ostree/auth.json`) on each deployed
	/// device with a freshly collected one — for when the registry deploy token
	/// has expired or been revoked, without re-provisioning. Registry mode only; the
	/// new credential is prompted on a TTY, or read from `BOOTCHER_PULL_USER` /
	/// `BOOTCHER_PULL_TOKEN` on a non-TTY run, exactly like `provision`.
	PullToken {
		/// Skip the eager local pre-flight that the new token authenticates before
		/// the rollout starts. Each device still verifies the token before committing it.
		#[arg(long = "skip-pull-check")]
		skip_pull_check: bool,
	},
	/// Replace the admin SSH key (`/etc/ssh/authorized_keys.d/admin`) on each
	/// deployed device with a freshly collected one — for when the admin keypair is
	/// being rolled, without re-provisioning. Lockout-safe: the new key is added
	/// *alongside* the old one and the old one is retired only after a fresh login
	/// proves the new key works, so an interrupted run leaves both keys valid. The
	/// new key is picked with the same mechanism as `provision`.
	SshKey {
		/// Admin SSH private key to roll to, or omit to pick from `~/.ssh/` on a
		/// TTY (same as `provision`). The public key is read from the `<path>.pub`
		/// sibling and injected into the device's `authorized_keys`.
		#[arg(long)]
		path: Option<String>,
	},
	/// Replace the image-**signing** key trusted on each device (registry mode) —
	/// for a leaked or expiring cosign key, without re-provisioning. `SSHes` into every
	/// `[deploy] remotes` device and atomically replaces its trusted signing key.
	/// Defaults to the public key from `[deploy] registry` in bootcher.toml; pass
	/// `--pubkey` to rotate to a different key. The next `bootcher deploy`
	/// re-signs with the new key; any upgrade attempt in the interim will fail and retry cleanly.
	SignKey {
		/// The new signing public key (a `.pub` path, e.g. from `bootcher sign
		/// enroll`). Defaults to the key from `[deploy] registry` in bootcher.toml.
		#[arg(long)]
		pubkey: Option<String>,
	},
}

fn main() -> ExitCode {
	let result = run();
	// `podman build` never reaps its working container on a signal, so an
	// interrupted (or aborted) build strands it. Reclaim those on any abnormal
	// exit — here at the top level, once the stack has fully unwound and every
	// build child is dead. A clean success leaves nothing to sweep.
	if result.is_err() {
		bootcher_core::podman::sweep_working_containers();
	}
	match result {
		Ok(()) => ExitCode::SUCCESS,
		// An interrupt unwinds as an ordinary error, but it isn't a failure to
		// report as one: print a terse note and exit 130 (128 + SIGINT), the
		// conventional status for a signal-terminated process. Keyed off the
		// signal flag rather than the error text so any error raised mid-unwind
		// still reports as the interruption it was.
		Err(_) if bootcher_core::signals::interrupted() => {
			eprintln!("interrupted");
			ExitCode::from(130)
		}
		Err(e) => {
			eprintln!("Error: {e:?}");
			ExitCode::FAILURE
		}
	}
}

fn run() -> Result<()> {
	// Cooperative cancellation: turn SIGINT/SIGTERM into a flag the long loops
	// poll, so an interrupt unwinds through the RAII guards (the qemu kill in
	// builder::vm, the cleared progress bars) instead of killing the process and
	// leaking them.
	bootcher_core::signals::install()?;
	match Cli::parse().cmd {
		Cmd::Init { name, yes, force } => jobs::init::run(name, yes, force),
		Cmd::Provision { ssh_key, skip_pull_check, anonymous, skip_build } => {
			let manifest = Manifest::load()?;
			pipelines::provision::preflight(&manifest, skip_build)?;
			let config = jobs::secrets::Provisioning::collect(
				&manifest,
				ssh_key.as_deref(),
				skip_pull_check,
				anonymous,
			)?
			.blueprint()?;
			pipelines::provision::run(&manifest, Some(&config), skip_build)
		}
		Cmd::Deploy { skip_bootc_upgrade, skip_build } => {
			let manifest = Manifest::load()?;
			pipelines::deploy::preflight(&manifest, skip_bootc_upgrade, skip_build)?;
			pipelines::deploy::run(&manifest, skip_bootc_upgrade, skip_build)
		}
		Cmd::Build => {
			let manifest = Manifest::load()?;
			jobs::build::preflight(&manifest)?;
			// `build::run` assembles the multi-arch manifest list as its final step, so a
			// bare `build` leaves a usable `localhost/<name>:latest` like the pipelines do.
			jobs::build::run(&manifest, &mut Scope::standalone())
		}
		Cmd::Takeover { ssh_key, login, skip_pull_check, anonymous, yes, skip_build } => {
			let manifest = Manifest::load()?;
			// Pre-flight before collecting secrets or building: local build tools, plus
			// the per-host over-SSH readiness checks (podman/sudo, arch, layout) — so a
			// disqualified host fails fast rather than after a multi-GB build.
			pipelines::takeover::preflight(&manifest, login.as_deref(), skip_build)?;
			// Collect the same secret set provision bakes, plus the resolved admin
			// private key path — takeover needs it as the post-reboot `admin@host`
			// identity, not just the public half baked into authorized_keys.
			let (provisioning, key_path) = jobs::secrets::Provisioning::collect_with_key(
				&manifest,
				ssh_key.as_deref(),
				skip_pull_check,
				anonymous,
			)?;
			let key_path = key_path.to_str().context("admin SSH key path is not valid UTF-8")?;
			// Name every target and gate on the destructive confirmation before the run.
			let hosts: Vec<String> =
				manifest.deploy_remotes().to_ssh().iter().map(|s| s.host().to_owned()).collect();
			jobs::takeover::confirm(&hosts, yes)?;
			pipelines::takeover::run(
				&manifest,
				login.as_deref(),
				&provisioning,
				key_path,
				skip_build,
			)
		}
		Cmd::Sign(cmd) => run_sign(cmd),
		Cmd::Rotate(cmd) => run_rotate(cmd),
		Cmd::Clean => cache::clean(),
	}
}

/// `bootcher sign <cmd>`. `enroll` generates a keypair in-process (no external
/// tools); `verify` shells out to `podman`, so it preflights that.
fn run_sign(cmd: SignCmd) -> Result<()> {
	match cmd {
		SignCmd::Enroll { prefix, force } => {
			let key_path = format!("{prefix}.key");
			let pub_path = format!("{prefix}.pub");
			jobs::signing::enroll(
				std::path::Path::new(&key_path),
				std::path::Path::new(&pub_path),
				force,
			)?;
			let toml_patched = jobs::signing::patch_registry_key(
				std::path::Path::new(context::MANIFEST),
				&key_path,
			)?;
			if toml_patched {
				eprintln!(
					"sign: wrote {key_path} (private — keep it secret; the scaffold .gitignore \
					 covers *.key), {pub_path}, and updated bootcher.toml."
				);
			} else {
				anyhow::bail!(
					"keypair written ({key_path}, {pub_path}) but no `registry` entry found in \
					 bootcher.toml — add `[deploy] registry = {{ url = \"<url>\", key = \"{key_path}\" }}`, \
					 commit {pub_path}, then run `bootcher provision`/`deploy`"
				);
			}
			Ok(())
		}
		SignCmd::Verify { image, pubkey, tls_verify } => {
			jobs::signing::preflight_verify()?;
			let pubkey_path = if let Some(p) = pubkey {
				std::path::PathBuf::from(p)
			} else {
				let manifest = Manifest::load()?;
				let signing = manifest.signing().ok_or_else(|| {
					anyhow::anyhow!(
						"no signing key configured in `[deploy] registry`; pass `--pubkey` explicitly"
					)
				})?;
				std::path::PathBuf::from(signing.public_key_path())
			};
			jobs::signing::verify(&image, &pubkey_path, tls_verify, &Scope::standalone())
		}
	}
}

/// `bootcher rotate <cmd>`: each reaches the deployed devices over ssh; the
/// pull-token path additionally pre-flights the new token locally via podman
/// (unless `--skip-pull-check`).
fn run_rotate(cmd: RotateCmd) -> Result<()> {
	let manifest = Manifest::load()?;
	let pull_check = matches!(&cmd, RotateCmd::PullToken { skip_pull_check } if !skip_pull_check);
	jobs::rotate::preflight(pull_check)?;
	match cmd {
		RotateCmd::PullToken { skip_pull_check } => {
			jobs::rotate::registry_token(&manifest, skip_pull_check, &mut Scope::standalone())
		}
		RotateCmd::SshKey { path } => {
			jobs::rotate::ssh_key(&manifest, path.as_deref(), &mut Scope::standalone())
		}
		RotateCmd::SignKey { pubkey } => {
			jobs::rotate::signing_key(&manifest, pubkey.as_deref(), &mut Scope::standalone())
		}
	}
}
