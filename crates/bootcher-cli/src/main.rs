use anyhow::Result;
use bootcher_core::context::{Arch, DiskType, MANIFEST, Manifest};
use bootcher_core::jobs::device::{self, Mode};
use bootcher_core::progress::Scope;
use bootcher_core::{cache, jobs};
use clap::{Args, Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
	// The Cargo.toml version, plus `git describe` when built from a commit that isn't
	// exactly that release (see build.rs).
	version = env!("BOOTCHER_VERSION"),
	about = "bootc image lifecycle: build, provision, update devices, publish releases"
)]
struct Cli {
	/// Load the project manifest from this file instead of `./bootcher.toml`, for
	/// keeping per-pipeline overrides (e.g. `bootcher --manifest bootcher.ci.toml
	/// build`) beside the default. Only the manifest is redirected: the build context
	/// and every relative path it names (Containerfile, `sysroot/`, keys) stay
	/// anchored to the working directory. A refresh loads the same relative path from
	/// the release commit's own tree. Ignored by `init`, which writes a fresh
	/// `bootcher.toml`.
	#[arg(short = 'm', long = "manifest", global = true, default_value = MANIFEST)]
	manifest: String,
	#[command(subcommand)]
	cmd: Cmd,
}

/// A bootcher project is one image, described by `bootcher.toml` in the working
/// directory. Commands are grouped by what they act on: the project itself
/// (`init`, `info`, `build`, `clean`), the devices (`device`), the releases
/// published to a registry (`release`), the channels devices follow (`channel`), and
/// image signing (`signing`).
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
		/// Accept defaults without prompting (host platform, no registry unless
		/// `--registry` names one) — the non-interactive path for scripts/CI.
		#[arg(short = 'y', long = "yes")]
		yes: bool,
		/// With `-y`: publish releases to this registry namespace (`[registry] url`,
		/// e.g. `ghcr.io/acme`).
		#[arg(long = "registry", value_name = "URL", requires = "yes")]
		registry: Option<String>,
		/// With `--registry`: a channel devices can follow (repeatable; default
		/// `stable`).
		#[arg(long = "channel", value_name = "CHANNEL", requires = "registry")]
		channel: Vec<String>,
		/// With `--registry`: enroll a signing key, as `signing enroll` does (the
		/// passphrase is read from `BOOTCHER_SIGN_PASSPHRASE`, else prompted).
		#[arg(long = "sign", requires = "registry")]
		sign: bool,
		/// Scaffold into a non-empty (or already-existing) directory anyway,
		/// overwriting any files the scaffold collides with.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Fedora release to pin the Containerfile's `fedora-bootc` base to (e.g.
		/// `45`). Defaults to the current stable release, looked up on the registry.
		#[arg(long = "fedora-release", value_name = "N")]
		fedora_release: Option<u32>,
	},
	/// Print the project's resolved facts (name, registry, the `<registry>/<name>`
	/// image ref, channels, remotes, signing, targets) from `bootcher.toml`, offline.
	/// Prints the whole document by default (or with `--format json`), or extracts one
	/// value with a `--format '{{ .Field }}'` template.
	Info {
		#[command(flatten)]
		format: FormatArg,
	},
	/// Build the current checkout into `localhost/<name>:latest-<arch>` (plus the
	/// local `localhost/<name>:latest` list when unscoped). Never publishes.
	Build {
		#[command(flatten)]
		target: TargetArg,
	},
	/// Delete the bootcher cache (`~/.cache/bootcher`): the cross-arch builder VM
	/// images.
	Clean,
	/// Devices: what they run, provisioning and takeover, updates, moves, credentials.
	#[command(subcommand)]
	Device(DeviceCmd),
	/// Releases: named, signed images in the registry, built from a clean commit.
	#[command(subcommand)]
	Release(ReleaseCmd),
	/// Channels: the tags devices follow, each a copy of some release.
	#[command(subcommand)]
	Channel(ChannelCmd),
	/// Image signing: the key releases are signed with and channel devices trust.
	#[command(subcommand)]
	Signing(SigningCmd),
}

// ------------------------------------------------------------------ shared args

/// `-y/--yes`: proceed without asking. On a TTY it skips the confirmation; off a
/// TTY it is required for any command that touches devices or moves a channel.
#[derive(Args, Clone, Copy, Debug, Default)]
struct YesArg {
	/// Proceed without asking (required off a TTY).
	#[arg(short = 'y', long = "yes")]
	yes: bool,
}

/// `--remote <name>` (repeatable): restrict the device step to these remotes, named
/// by their full destination or bare host.
#[derive(Args, Clone, Debug, Default)]
struct RemoteArgs {
	/// Restrict the device step to this remote (repeatable): its destination as
	/// written in `remotes`, or just its host.
	#[arg(long = "remote", value_name = "NAME")]
	remote: Vec<String>,
}

/// `--format`: the `info` template subset, plus `json`.
#[derive(Args, Clone, Debug, Default)]
struct FormatArg {
	/// `json`, or a Go-template subset (`{{ .Field }}`, `{{ json .Field }}`; fields
	/// match case-insensitively).
	#[arg(long = "format")]
	format: Option<String>,
}

/// `--target <arch>`: only where one arch makes sense alone.
#[derive(Args, Clone, Copy, Debug, Default)]
struct TargetArg {
	/// Restrict to one architecture from `[targets]` (`aarch64` or `x86_64`).
	#[arg(long = "target")]
	target: Option<Arch>,
}

/// `--channel ch | --direct`: how a device receives updates.
#[derive(Args, Clone, Debug, Default)]
#[group(id = "mode", multiple = false)]
struct ModeArgs {
	/// Follow this declared channel (`[registry] channels`).
	#[arg(long = "channel", value_name = "CHANNEL")]
	channel: Option<String>,
	/// Be updated over SSH from this machine (`bootcher device deploy`). Implied
	/// without a `[registry]`.
	#[arg(long = "direct")]
	direct: bool,
}

impl ModeArgs {
	fn mode(&self) -> Option<Mode> {
		match (&self.channel, self.direct) {
			(Some(ch), _) => Some(Mode::Channel(ch.clone())),
			(None, true) => Some(Mode::Direct),
			(None, false) => None,
		}
	}
}

/// The pull-credential flags, valid only with `--channel`.
#[derive(Args, Clone, Copy, Debug, Default)]
struct PullArgs {
	/// The registry is public: install no pull credential.
	#[arg(long = "anonymous", requires = "channel", conflicts_with = "direct")]
	anonymous: bool,
	/// Don't verify the pull credential from this machine (e.g. a registry only the
	/// devices can reach). Each device still verifies it before committing it.
	#[arg(long = "skip-pull-check", requires = "channel", conflicts_with = "direct")]
	skip_pull_check: bool,
}

/// `--skip-build` for the commands that build here: reuse the
/// `localhost/<name>:latest-<arch>` images already in local storage.
#[derive(Args, Clone, Copy, Debug, Default)]
struct SkipBuildArg {
	/// Use the `localhost/<name>:latest-<arch>` images already in local storage
	/// (however they got there) instead of building.
	#[arg(long = "skip-build", conflicts_with = "channel")]
	skip_build: bool,
}

/// `--skip-remotes`: skip the device step after moving a channel.
#[derive(Args, Clone, Copy, Debug, Default)]
struct SkipRemotesArg {
	/// Move the channel but don't run the device step: devices pick it up on their
	/// next auto-update.
	#[arg(long = "skip-remotes")]
	skip_remotes: bool,
}

// ------------------------------------------------------------------ device

#[derive(Subcommand)]
enum DeviceCmd {
	/// What each remote runs: its class (channel, direct, foreign, unreachable),
	/// origin, booted/staged digest and version, and for channel devices the release
	/// and whether it lags its channel. Read-only.
	Status {
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		format: FormatArg,
	},
	/// Build disk images for new devices under `output/<arch>/<disk_type>/`. A
	/// `--channel` device follows that channel (the images are pulled from it, never
	/// built); a `--direct` one runs what's built here and is updated with `device
	/// deploy`.
	Provision {
		#[command(flatten)]
		mode: ModeArgs,
		#[command(flatten)]
		target: TargetArg,
		/// Restrict the disks built for `--target` to these `image-builder` type(s)
		/// (repeatable). Requires `--target`.
		#[arg(long = "disk", requires = "target")]
		disk: Vec<DiskType>,
		/// Admin SSH private key to install (its `.pub` sibling is baked). Omit to
		/// pick interactively on a TTY.
		#[arg(long = "ssh-key")]
		ssh_key: Option<String>,
		#[command(flatten)]
		skip_build: SkipBuildArg,
		#[command(flatten)]
		pull: PullArgs,
	},
	/// Convert live, SSH-reachable hosts (stock Ubuntu/Debian/Rocky on a VPS) into
	/// bootc systems **in place**, irreversibly wiping their current OS. Each host
	/// must already have `podman` and `sudo`.
	Takeover {
		#[command(flatten)]
		mode: ModeArgs,
		/// The hosts to convert (repeatable). Required off a TTY; never all remotes by
		/// default.
		#[command(flatten)]
		remotes: RemoteArgs,
		/// Fleet-wide stock cloud login for the initial connection (`debian`,
		/// `ubuntu`, `cloud-user`, `root`); a remote's `takeover_login` overrides it.
		#[arg(long = "login")]
		login: Option<String>,
		/// Admin SSH private key: installed on the new system and used to reconnect as
		/// `admin@host` after the reboot.
		#[arg(long = "ssh-key")]
		ssh_key: Option<String>,
		#[command(flatten)]
		skip_build: SkipBuildArg,
		#[command(flatten)]
		pull: PullArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Build this checkout here (dirty trees welcome) and push it to the direct
	/// devices. Never moves a device.
	Deploy {
		/// Push the images already in local storage instead of building.
		#[arg(long = "skip-build")]
		skip_build: bool,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Base patches for direct devices: rebuild the commit each one runs on the
	/// current base and ship it back to just those devices.
	Refresh {
		#[command(flatten)]
		remotes: RemoteArgs,
		/// Rebuild (with `--no-cache`) even when the base hasn't moved.
		#[arg(long = "force")]
		force: bool,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Move devices onto a channel or to direct — the only verb that moves a device.
	Switch {
		#[command(flatten)]
		mode: ModeArgs,
		/// The devices to move (repeatable). Required off a TTY.
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		skip_build: SkipBuildArg,
		#[command(flatten)]
		pull: PullArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Rotate a credential on the devices, over SSH, without re-provisioning.
	#[command(subcommand)]
	Rotate(RotateCmd),
}

#[derive(Subcommand)]
enum RotateCmd {
	/// Replace the admin SSH key on every reachable remote, lockout-safe: the new key
	/// is added beside the old one and the old one retired only once a fresh login
	/// proves the new key works.
	SshKey {
		/// The new admin SSH private key (its `.pub` sibling is installed). Omit to
		/// pick interactively on a TTY.
		#[arg(long = "ssh-key")]
		ssh_key: Option<String>,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Replace the registry pull credential on the channel devices, each verified
	/// against its own channel before it's committed. From `BOOTCHER_PULL_USER` /
	/// `BOOTCHER_PULL_TOKEN`, or prompted on a TTY.
	PullToken {
		/// Skip the local check that the new token authenticates.
		#[arg(long = "skip-pull-check")]
		skip_pull_check: bool,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
}

// ------------------------------------------------------------------ release

/// `--git-tag`: what to do with a git tag named like the release.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum GitTag {
	/// Create an annotated tag at HEAD and push it upstream.
	Push,
	/// Create the annotated tag locally only.
	Local,
	/// Don't create one.
	No,
}

#[derive(Subcommand)]
enum ReleaseCmd {
	/// Publish release `X` from a clean checkout: build every arch, push the signed
	/// multi-arch index as `<registry>/<name>:X`. Idempotent: an `X` already built
	/// from HEAD is left alone.
	Publish {
		/// The release name (any valid OCI tag that isn't a channel). Omit to be
		/// asked on a TTY.
		name: Option<String>,
		/// Publish the `latest-<arch>` images already in local storage (each must
		/// record HEAD as its `bootcher.source`) instead of building.
		#[arg(long = "skip-build")]
		skip_build: bool,
		/// Rebuild and re-push `X` even though it already records HEAD.
		#[arg(long = "rebuild", conflicts_with = "skip_build")]
		rebuild: bool,
		/// Afterwards, create a git tag named `X` at HEAD: `push` it upstream,
		/// `local` only, or `no`. Off a TTY the default is `no`.
		#[arg(long = "git-tag", value_enum)]
		git_tag: Option<GitTag>,
		/// Afterwards, promote `X` to this channel (`bootcher channel promote X CH`,
		/// device step included).
		#[arg(long = "promote", value_name = "CHANNEL")]
		promote: Option<String>,
		#[command(flatten)]
		remotes: PromoteRemotes,
	},
	/// Rebuild release `X` on the current base (its stale arches, or all with
	/// `--force`) from its recorded commit and re-push it. Moves no channel.
	Refresh {
		/// The release to refresh.
		name: String,
		/// Rebuild every arch (with `--no-cache`) even when the base hasn't moved.
		#[arg(long = "force")]
		force: bool,
		/// Publish the `latest-<arch>` images already in local storage (each must
		/// record the release's commit) instead of building.
		#[arg(long = "skip-build")]
		skip_build: bool,
	},
	/// The published releases: source commit, per-arch base status, and the channels
	/// serving each. Read-only.
	List {
		#[command(flatten)]
		format: FormatArg,
	},
}

/// The device-step flags `release publish` takes only with `--promote`.
#[derive(Args, Clone, Debug, Default)]
struct PromoteRemotes {
	/// Restrict the promote's device step to this remote (repeatable).
	#[arg(long = "remote", value_name = "NAME", requires = "promote")]
	remote: Vec<String>,
	/// Promote but skip the device step.
	#[arg(long = "skip-remotes", requires = "promote")]
	skip_remotes: bool,
	/// Proceed without asking (required off a TTY with `--promote`).
	#[arg(short = 'y', long = "yes", requires = "promote")]
	yes: bool,
}

// ------------------------------------------------------------------ channel

#[derive(Subcommand)]
enum ChannelCmd {
	/// Per channel: the release it serves, its source commit, per-arch base
	/// current/stale, and whether it lags its release. Read-only.
	Status {
		/// The channels to show (default: every declared one).
		channels: Vec<String>,
		/// `json`, or a template (`{{ .Release }}`, `{{ .Commit }}`, `{{ .Stale }}`)
		/// when exactly one channel is selected.
		#[arg(long = "format")]
		format: Option<String>,
	},
	/// Point channel `CH` at release `X` (a byte-for-byte copy), then update the
	/// devices following it.
	Promote {
		/// The release to promote. Omit to pick on a TTY.
		release: Option<String>,
		/// The channel to move. Omit to pick on a TTY.
		channel: Option<String>,
		/// Promote even though a member's base isn't the base's current digest.
		#[arg(long = "allow-stale-base")]
		allow_stale_base: bool,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		skip_remotes: SkipRemotesArg,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Base patches for channels: rebuild the release each channel serves on the
	/// current base (from its own commit), re-push it, re-copy the channels, then
	/// update their devices.
	Refresh {
		/// The channels to refresh (default: every declared one).
		channels: Vec<String>,
		/// Leave this channel alone (repeatable).
		#[arg(long = "exclude", value_name = "CHANNEL")]
		exclude: Vec<String>,
		/// Rebuild every arch (with `--no-cache`) even when the base hasn't moved.
		#[arg(long = "force")]
		force: bool,
		/// Publish the `latest-<arch>` images already in local storage (each must
		/// record the served release's commit) instead of building. The selected
		/// channels must serve a single release.
		#[arg(long = "skip-build")]
		skip_build: bool,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		skip_remotes: SkipRemotesArg,
		#[command(flatten)]
		yes: YesArg,
	},
}

// ------------------------------------------------------------------ signing

#[derive(Subcommand)]
enum SigningCmd {
	/// Turn signing on: write a cosign keypair (`<prefix>.key` private, `<prefix>.pub`
	/// public), set `[registry] key`, and bake the install drop-in. On a running
	/// fleet, also sign every release a channel serves and install the trust files on
	/// the channel devices.
	Enroll {
		/// Output filename prefix (`<prefix>.key` + `<prefix>.pub`).
		#[arg(default_value = "cosign")]
		prefix: String,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Rotate the signing key with no window in which a device rejects what its
	/// channel serves: add a new key, sign every served release with it, trust old +
	/// new on the channel devices, then point `[registry] key` at the new key.
	Rotate {
		/// Output filename prefix for the new key (default: `cosign-<fingerprint>`).
		prefix: Option<String>,
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Trust only the current key on the channel devices and delete the others. Run
	/// it once every device — including ones not in `remotes` — has the new key.
	Retire {
		#[command(flatten)]
		remotes: RemoteArgs,
		#[command(flatten)]
		yes: YesArg,
	},
	/// Verify that an image carries a valid signature from the configured (or given)
	/// public key — the same check a channel device performs on `bootc upgrade`.
	Verify {
		/// A full image ref, or a bare release or channel name resolved against the
		/// project image (`<registry>/<name>:<tag>`).
		image: String,
		/// Path to the public key (default: the `.pub` of `[registry] key`).
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
		// An interrupt (a signal, or Esc/Ctrl-C in a prompt) unwinds as an ordinary
		// error, but it isn't a failure to report as one: print a terse note and exit
		// 130 (128 + SIGINT). Keyed off the signal flag rather than the error text so
		// any error raised mid-unwind still reports as the interruption it was.
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
	let cli = Cli::parse();
	let manifest_path = cli.manifest.as_str();
	let load = || Manifest::load(manifest_path);
	match cli.cmd {
		Cmd::Init { name, yes, force, fedora_release, registry, channel, sign } => {
			let registry = jobs::init::RegistryArgs { url: registry, channels: channel, sign };
			jobs::init::run(name, yes, force, fedora_release, registry)
		}
		Cmd::Info { format } => {
			let info = jobs::info::Info::from_manifest(&load()?);
			println!("{}", jobs::info::render(&info, format.format.as_deref())?);
			Ok(())
		}
		Cmd::Build { target } => {
			let manifest = load()?;
			// `--target <arch>` scopes the build to one arch and stops at the per-arch
			// member (the producer half of a split CI pipeline); a bare `build` builds
			// every arch and assembles `localhost/<name>:latest`.
			manifest.check_selection(target.target, &[])?;
			jobs::build::preflight(&manifest, target.target)?;
			jobs::build::run(&manifest, target.target, &mut Scope::standalone())
		}
		Cmd::Clean => cache::clean(),
		Cmd::Device(cmd) => run_device(cmd, &load()?),
		Cmd::Release(cmd) => run_release(cmd, &load()?),
		Cmd::Channel(cmd) => run_channel(cmd, &load()?),
		Cmd::Signing(cmd) => run_signing(cmd, manifest_path),
	}
}

fn run_device(cmd: DeviceCmd, manifest: &Manifest) -> Result<()> {
	match cmd {
		DeviceCmd::Status { remotes, format } => {
			device::status(manifest, &remotes.remote, format.format.as_deref())
		}
		DeviceCmd::Provision { mode, target, disk, ssh_key, skip_build, pull } => {
			device::provision(
				manifest,
				&device::ProvisionArgs {
					mode: mode.mode(),
					target: target.target,
					disks: disk,
					ssh_key,
					skip_build: skip_build.skip_build,
					skip_pull_check: pull.skip_pull_check,
					anonymous: pull.anonymous,
				},
			)
		}
		DeviceCmd::Takeover { mode, remotes, login, ssh_key, skip_build, pull, yes } => {
			device::takeover(
				manifest,
				&device::TakeoverArgs {
					mode: mode.mode(),
					remotes: remotes.remote,
					login,
					ssh_key,
					skip_build: skip_build.skip_build,
					skip_pull_check: pull.skip_pull_check,
					anonymous: pull.anonymous,
					yes: yes.yes,
				},
			)
		}
		DeviceCmd::Deploy { skip_build, remotes, yes } => {
			device::deploy(manifest, skip_build, &remotes.remote, yes.yes)
		}
		DeviceCmd::Refresh { remotes, force, yes } => {
			device::refresh(manifest, &remotes.remote, force, yes.yes)
		}
		DeviceCmd::Switch { mode, remotes, skip_build, pull, yes } => device::switch(
			manifest,
			&device::SwitchArgs {
				mode: mode.mode(),
				remotes: remotes.remote,
				skip_build: skip_build.skip_build,
				anonymous: pull.anonymous,
				skip_pull_check: pull.skip_pull_check,
				yes: yes.yes,
			},
		),
		DeviceCmd::Rotate(RotateCmd::SshKey { ssh_key, remotes, yes }) => {
			device::rotate_ssh_key(manifest, ssh_key.as_deref(), &remotes.remote, yes.yes)
		}
		DeviceCmd::Rotate(RotateCmd::PullToken { skip_pull_check, remotes, yes }) => {
			device::rotate_pull_token(manifest, skip_pull_check, &remotes.remote, yes.yes)
		}
	}
}

fn run_release(cmd: ReleaseCmd, manifest: &Manifest) -> Result<()> {
	use jobs::release;
	match cmd {
		ReleaseCmd::Publish { name, skip_build, rebuild, git_tag, promote, remotes } => {
			release::publish(
				manifest,
				&release::PublishArgs {
					name,
					skip_build,
					rebuild,
					git_tag: git_tag.map(|g| match g {
						GitTag::Push => release::GitTag::Push,
						GitTag::Local => release::GitTag::Local,
						GitTag::No => release::GitTag::No,
					}),
					promote,
					remotes: remotes.remote,
					skip_remotes: remotes.skip_remotes,
					yes: remotes.yes,
				},
			)
		}
		ReleaseCmd::Refresh { name, force, skip_build } => {
			release::refresh(manifest, &name, force, skip_build)
		}
		ReleaseCmd::List { format } => release::list(manifest, format.format.as_deref()),
	}
}

fn run_channel(cmd: ChannelCmd, manifest: &Manifest) -> Result<()> {
	use jobs::channel;
	match cmd {
		ChannelCmd::Promote { release, channel, allow_stale_base, remotes, skip_remotes, yes } => {
			channel::promote(
				manifest,
				&channel::PromoteArgs {
					release,
					channel,
					allow_stale_base,
					remotes: remotes.remote,
					skip_remotes: skip_remotes.skip_remotes,
					yes: yes.yes,
				},
			)
		}
		ChannelCmd::Status { channels, format } => {
			channel::status(manifest, &channels, format.as_deref())
		}
		ChannelCmd::Refresh {
			channels,
			exclude,
			force,
			skip_build,
			remotes,
			skip_remotes,
			yes,
		} => channel::refresh(
			manifest,
			&channel::RefreshArgs {
				channels,
				exclude,
				force,
				skip_build,
				remotes: remotes.remote,
				skip_remotes: skip_remotes.skip_remotes,
				yes: yes.yes,
			},
		),
	}
}

fn run_signing(cmd: SigningCmd, manifest_path: &str) -> Result<()> {
	match cmd {
		SigningCmd::Enroll { prefix, remotes, yes } => {
			let manifest = Manifest::load(manifest_path)?;
			jobs::signing::enroll_fleet(&manifest, &prefix, &remotes.remote, yes.yes).map(drop)
		}
		SigningCmd::Rotate { prefix, remotes, yes } => {
			let manifest = Manifest::load(manifest_path)?;
			jobs::signing::rotate(&manifest, prefix.as_deref(), &remotes.remote, yes.yes).map(drop)
		}
		SigningCmd::Retire { remotes, yes } => {
			let manifest = Manifest::load(manifest_path)?;
			jobs::signing::retire(&manifest, &remotes.remote, yes.yes)
		}
		SigningCmd::Verify { image, pubkey } => {
			jobs::signing::preflight_verify()?;
			let manifest = Manifest::load(manifest_path).ok();
			let image_ref = resolve_image(manifest.as_ref(), &image)?;
			let pubkey = if let Some(p) = pubkey {
				std::path::PathBuf::from(p)
			} else {
				let signing = manifest.as_ref().and_then(Manifest::signing).ok_or_else(|| {
					anyhow::anyhow!("no signing key configured in `[registry]`; pass `--pubkey`")
				})?;
				std::path::PathBuf::from(signing.public_key_path())
			};
			jobs::signing::verify(&image_ref, &pubkey, &Scope::standalone())
		}
	}
}

/// A full image ref as given, or a bare release/channel name (no `/`) resolved
/// against the project image.
fn resolve_image(manifest: Option<&Manifest>, image: &str) -> Result<String> {
	if image.contains('/') {
		return Ok(image.to_owned());
	}
	manifest.and_then(|m| m.tag_ref(image)).ok_or_else(|| {
		anyhow::anyhow!(
			"`{image}` isn't a full image ref, and there's no project `[registry]` to resolve it \
			 against — pass `<registry>/<name>:<tag>`"
		)
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
		Cli::try_parse_from(std::iter::once("bootcher").chain(args.iter().copied()))
	}

	#[test]
	fn the_tree_is_grouped_by_domain() {
		for args in [
			&["device", "status"][..],
			&["device", "deploy", "-y", "--remote", "a", "--remote", "b"],
			&["device", "provision", "--channel", "stable", "--target", "x86_64", "--disk", "raw"],
			&["device", "switch", "--direct", "--remote", "k", "--skip-build"],
			&["device", "rotate", "ssh-key", "--ssh-key", "k"],
			&["device", "rotate", "pull-token", "--skip-pull-check"],
			&["release", "publish", "v1.4", "--promote", "next", "-y", "--skip-remotes"],
			&["release", "refresh", "v1.3", "--force"],
			&["release", "list", "--format", "json"],
			&["channel", "status", "stable", "--format", "{{ .Stale }}"],
			&["channel", "promote", "v1.4", "stable", "--allow-stale-base"],
			&["channel", "refresh", "--exclude", "stable", "--skip-build", "-y"],
			&["signing", "enroll"],
			&["signing", "verify", "v1.4"],
		] {
			parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
		}
		// No top-level aliases for the old flat commands.
		for old in ["deploy", "provision", "takeover", "rotate", "sign"] {
			assert!(parse(&[old]).is_err(), "{old}");
		}
	}

	#[test]
	fn mode_flags_are_exclusive_and_gate_their_options() {
		assert!(parse(&["device", "provision", "--channel", "a", "--direct"]).is_err());
		// The pull flags need `--channel`; `--skip-build` conflicts with it.
		assert!(parse(&["device", "provision", "--direct", "--anonymous"]).is_err());
		assert!(parse(&["device", "provision", "--skip-pull-check"]).is_err());
		assert!(
			parse(&["device", "provision", "--channel", "a", "--anonymous", "--skip-pull-check"])
				.is_ok()
		);
		assert!(parse(&["device", "provision", "--channel", "a", "--skip-build"]).is_err());
		assert!(parse(&["device", "switch", "--channel", "a", "--skip-build"]).is_err());
		assert!(parse(&["device", "takeover", "--direct", "--anonymous"]).is_err());
		// `--disk` needs `--target`.
		assert!(parse(&["device", "provision", "--disk", "raw"]).is_err());
		let cli = parse(&["device", "switch", "--channel", "next"]).unwrap();
		let Cmd::Device(DeviceCmd::Switch { mode, .. }) = cli.cmd else { unreachable!() };
		assert_eq!(mode.mode(), Some(Mode::Channel("next".into())));
	}

	#[test]
	fn publish_device_flags_need_promote() {
		assert!(parse(&["release", "publish", "v1", "-y"]).is_err());
		assert!(parse(&["release", "publish", "v1", "--remote", "a"]).is_err());
		assert!(parse(&["release", "publish", "v1", "--skip-remotes"]).is_err());
		assert!(parse(&["release", "publish", "v1", "--rebuild", "--skip-build"]).is_err());
		let cli = parse(&["release", "publish", "v1", "--git-tag", "local"]).unwrap();
		let Cmd::Release(ReleaseCmd::Publish { git_tag, .. }) = cli.cmd else { unreachable!() };
		assert_eq!(git_tag, Some(GitTag::Local));
	}

	#[test]
	fn read_only_commands_take_no_yes() {
		assert!(parse(&["device", "status", "-y"]).is_err());
		assert!(parse(&["release", "list", "-y"]).is_err());
		assert!(parse(&["channel", "status", "-y"]).is_err());
		assert!(parse(&["signing", "verify", "x", "-y"]).is_err());
	}

	#[test]
	fn verify_drops_tls_verify() {
		assert!(parse(&["signing", "verify", "r/o/x:1", "--tls-verify=false"]).is_err());
	}

	#[test]
	fn bare_names_resolve_against_the_project_image() {
		let m: Manifest = toml::from_str(
			"[general]\nname = \"kiosk\"\n[registry]\nurl = \"r.io/o\"\nchannels = [\"stable\"]\n",
		)
		.unwrap();
		assert_eq!(resolve_image(Some(&m), "v1.4").unwrap(), "r.io/o/kiosk:v1.4");
		assert_eq!(resolve_image(Some(&m), "q.io/a/b:c").unwrap(), "q.io/a/b:c");
		assert!(resolve_image(None, "v1.4").is_err());
	}
}
