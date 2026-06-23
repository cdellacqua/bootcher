//! Convert a live, SSH-reachable host into a bootc system *in place* with `bootc
//! install to-existing-root` — the engine behind `bootcher takeover`.
//!
//! Takeover is a one-time funnel into the existing deploy flow: it builds the
//! image (the pipeline's first phase) then, over the `[deploy] remotes` SSH
//! channel, replaces each host's stock OS (Ubuntu/Debian/Rocky on a VPS) with the
//! built image. Afterwards the host is an ordinary bootc device pointing at the
//! same origin, so `deploy`/`rotate` work against it unchanged.
//!
//! ## Two SSH identities
//!
//! A run spans two identities, because the stock cloud user is replaced by the
//! image's `admin` user (uid 1000) at install time:
//!
//! - the **initial connection** (image transfer, the install container, the
//!   reboot) is the stock cloud user — `debian`/`ubuntu`/`cloud-user`/`root` — which
//!   has passwordless sudo on cloud images. Resolved per host by
//!   [`RemoteConfig::takeover_ssh`] (`takeover_login` → `--login` → error).
//! - the **post-reboot** verification (`wait_online` + `bootc status`) is
//!   `admin@host` with the just-injected `--ssh-key` identity
//!   ([`RemoteConfig::admin_ssh`]) — the normal steady-state remote, so the box needs
//!   no `bootcher.toml` edit afterwards.
//!
//! ## Backends
//!
//! Mirrors the upgrade job: in LAN mode each host pulls the matching arch
//! member from a loopback registry over an `ssh -R` tunnel (reusing upgrade's
//! transfer); in registry mode the multi-arch list is pushed once and each host pulls the
//! configured ref directly. Either way the image lands in the host's
//! containers-storage, `bootc install to-existing-root` converts the disk, the
//! provisioning secrets are installed into the staged deployment's `/etc`, and the
//! host reboots into bootc.

use crate::context::{DEVICE_AUTH_JSON, ImageRef, Manifest, RemoteConfig};
use crate::jobs::secrets::{DeviceFile, Provisioning};
use crate::jobs::upgrade;
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use crate::ssh::Ssh;
use crate::{fleet, registry};
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::num::NonZeroUsize;

/// Root filesystems `bootc install to-existing-root` can adopt — the layout
/// pre-check rejects anything else before a multi-GB build (bootc runs the
/// authoritative check at install start regardless).
const SUPPORTED_ROOTFS: &[&str] = &["ext4", "xfs", "btrfs"];

/// Per-host readiness probe sent over the initial (stock-login) SSH connection. It
/// emits one `key=value` line per check so [`evaluate_probe`] can parse the lot in
/// one round-trip; the `bootc` line doubles as the idempotency signal (a host
/// already converted is skipped).
const PROBE_SCRIPT: &str = "\
echo \"podman=$(command -v podman >/dev/null 2>&1 && echo yes || echo no)\"
echo \"sudo=$(command -v sudo >/dev/null 2>&1 && echo yes || echo no)\"
echo \"arch=$(uname -m)\"
echo \"boot=$([ -d /boot ] && [ -n \"$(ls -A /boot 2>/dev/null)\" ] && echo yes || echo no)\"
echo \"fstype=$(findmnt -no FSTYPE / 2>/dev/null || true)\"
echo \"efi=$([ -d /sys/firmware/efi ] && echo yes || echo no)\"
echo \"bootc=$(command -v bootc >/dev/null 2>&1 && sudo bootc status >/dev/null 2>&1 && echo yes || echo no)\"
";

/// Verdict for one host's readiness, from [`evaluate_probe`].
#[derive(Debug, PartialEq, Eq)]
enum HostReadiness {
	/// Already a bootc system — takeover is a no-op; skip it (idempotent).
	AlreadyBootc,
	/// No disqualifiers found; ready to take over.
	Ready,
	/// Not eligible; each entry is a human-readable reason (with a fix hint).
	Unsupported(Vec<String>),
}

/// The takeover *job*'s prerequisites, for the `--skip-build` path (the build phase
/// is skipped, so [`crate::jobs::build::preflight`] doesn't run): a local `podman`
/// (the job assembles the multi-arch list and, in registry mode, pushes it) plus the
/// per-host over-SSH readiness checks. The build-inclusive pipeline reaches `podman`
/// through `build::preflight` instead and calls [`host_preflight`] directly.
///
/// # Errors
///
/// Returns an error if `podman`/`ssh` are missing, no remotes are configured, or any
/// host is ineligible.
pub(crate) fn preflight(manifest: &Manifest, login: Option<&str>) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	checks.finish()?;
	host_preflight(manifest, login)
}

/// Per-remote, over-SSH readiness checks, run up front (before the build) so an
/// obvious disqualifier — a missing `podman`/`sudo`, an arch the project doesn't
/// build, an unadoptable root filesystem — fails fast rather than after a multi-GB
/// build. `login` is the fleet-wide `--login` default; each host may override it
/// with its own `takeover_login`.
///
/// Accumulates failures across the whole fleet (like [`Checks`]) and reports them
/// together. A host already on bootc is noted and skipped, not failed — takeover is
/// idempotent. bootc runs the authoritative layout check at install start; this is
/// the cheap pre-filter.
///
/// # Errors
///
/// Returns an error if `ssh` is missing locally, no remotes are configured, a host
/// is unreachable, or any host is ineligible.
pub(crate) fn host_preflight(manifest: &Manifest, login: Option<&str>) -> Result<()> {
	// The whole rollout is SSH; without it locally we can't even probe.
	let mut checks = Checks::default();
	checks.bin("ssh", "reach the takeover targets over SSH");
	checks.finish()?;

	let remotes = manifest.deploy_remotes();
	if remotes.is_empty() {
		bail!(
			"takeover needs at least one target in `[deploy] remotes` (the live host to convert) \
			 in bootcher.toml"
		);
	}

	let mut failures: Vec<String> = Vec::new();
	for remote in remotes {
		let initial = remote.takeover_ssh(login)?;
		let host = initial.host().to_owned();
		let probe = initial
			.read_sh(PROBE_SCRIPT)
			.with_context(|| format!("probing {host} for takeover readiness over SSH"))?;
		match evaluate_probe(&probe) {
			HostReadiness::AlreadyBootc => {
				eprintln!("takeover: {host} is already a bootc system — it will be skipped");
			}
			HostReadiness::Ready => {}
			HostReadiness::Unsupported(reasons) => {
				for reason in reasons {
					failures.push(format!("{host}: {reason}"));
				}
			}
		}
	}

	if !failures.is_empty() {
		let mut msg = String::from("takeover pre-flight failed for some hosts:\n");
		for f in &failures {
			msg.push_str("  - ");
			msg.push_str(f);
			msg.push('\n');
		}
		msg.push_str("fix the above and retry");
		bail!(msg);
	}
	Ok(())
}

/// Parse the [`PROBE_SCRIPT`] output (`key=value` lines) into a [`HostReadiness`].
/// Missing keys are treated as failures (a truncated probe is not "ready").
fn evaluate_probe(out: &str) -> HostReadiness {
	let get = |key: &str| {
		out.lines()
			.find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
			.unwrap_or("")
	};

	// `bootc status` succeeding ⇒ already converted; nothing else matters.
	if get("bootc") == "yes" {
		return HostReadiness::AlreadyBootc;
	}

	let mut reasons = Vec::new();
	if get("podman") != "yes" {
		reasons.push(
			"podman not installed — install it on the host first (e.g. `apt install -y podman`, \
			 `dnf install -y podman`, or `zypper install -y podman`); bootcher never mutates the \
			 foreign distro"
				.to_owned(),
		);
	}
	if get("sudo") != "yes" {
		reasons.push(
			"sudo not installed — the rollout scripts embed `sudo` (one path for stock-user and \
			 root@ targets alike); install it on the host first"
				.to_owned(),
		);
	}
	let arch = get("arch");
	if crate::context::Arch::from_uname(arch).is_none() {
		reasons.push(format!(
			"unsupported architecture {arch:?} — bootcher only builds x86_64/aarch64"
		));
	}
	let fstype = get("fstype");
	if !SUPPORTED_ROOTFS.contains(&fstype) {
		reasons.push(format!(
			"root filesystem {fstype:?} is not adoptable by `bootc install to-existing-root` \
			 (need one of {})",
			SUPPORTED_ROOTFS.join("/")
		));
	}
	if get("boot") != "yes" {
		reasons.push("/boot is missing or empty — no bootloader partition to adopt".to_owned());
	}
	if get("efi") != "yes" {
		reasons.push(
			"no /sys/firmware/efi — the host appears to be booted in legacy BIOS mode, which \
			 bootc install does not support"
				.to_owned(),
		);
	}

	if reasons.is_empty() { HostReadiness::Ready } else { HostReadiness::Unsupported(reasons) }
}

/// Run the takeover across the configured fleet: get the image onto each live
/// host, convert it in place, inject the provisioning secrets, and reboot into
/// bootc — verifying over the new `admin@` identity.
///
/// `login` is the fleet-wide stock login (`--login`); `provisioning` is the same
/// secret set disk provisioning bakes (its [`Provisioning::files`] are installed
/// into the staged `/etc`); `ssh_key` is the resolved admin **private** key path —
/// its public half is injected, its private half is the post-reboot identity.
///
/// Dispatches on registry vs LAN exactly like [`upgrade::run`]: a configured
/// registry pushes the multi-arch list and has each host pull the ref; otherwise
/// each host pulls its arch member from a shared loopback registry over `ssh -R`.
///
/// # Errors
///
/// Returns an error if no arches/remotes are configured, the push fails, or any
/// host's takeover fails.
pub(crate) fn run(
	manifest: &Manifest,
	login: Option<&str>,
	provisioning: &Provisioning,
	ssh_key: &str,
	job: &mut Scope,
) -> Result<()> {
	let images = manifest.images();
	if images.is_empty() {
		bail!("no target arches to take over — set `[general.disk_types]` in bootcher.toml");
	}
	let remotes = manifest.deploy_remotes();
	if remotes.is_empty() {
		bail!("takeover needs at least one target in `[deploy] remotes` in bootcher.toml");
	}

	let targets = resolve_targets(remotes, login, ssh_key)?;
	let files = provisioning.files();
	let max_workers = manifest.concurrency().takeover;

	if let Some(latest_ref) = manifest.registry_list_ref()
		&& let Some(version_ref) = manifest.registry_version_ref(&crate::context::calver_now())
	{
		// Registry mode: the host pulls the ref directly, so it must be in the registry
		// first. Push the multi-arch list (signing it if configured), then hand each
		// host the ref + the pull credential (the device `auth.json`, when present).
		upgrade::push_multiarch_list(
			&manifest.local_list_ref(),
			&latest_ref,
			&version_ref,
			manifest.signing().as_ref(),
			job,
		)?;
		let auth_json = files.iter().find(|f| f.path == DEVICE_AUTH_JSON).map(|f| f.data.as_str());
		let backend = Backend::Registry { reference: &latest_ref, auth_json };
		run_fleet(&targets, &backend, &files, max_workers, job)
	} else {
		// LAN mode: stand up one loopback registry for the whole fleet (it handles
		// concurrent pulls) and tear it down once every host has finished. The list
		// object was assembled by the build phase; just name it.
		let local_list_ref = manifest.local_list_ref();
		let reg = registry::serve_manifest_list(&local_list_ref, "latest", job)?;
		let backend =
			Backend::Lan { images: &images, local_list_ref: &local_list_ref, port: reg.port() };
		let result = run_fleet(&targets, &backend, &files, max_workers, job);
		drop(reg);
		result
	}
}

/// How a host obtains the image into its containers-storage. The two backends from
/// [`upgrade`], specialised for takeover's stock-login connection.
enum Backend<'a> {
	/// Pull the matching arch member from the shared loopback registry on `port` over
	/// an `ssh -R` tunnel; install from the local `containers-storage` ref
	/// (`local_list_ref`, the project's [`Manifest::local_list_ref`]).
	Lan { images: &'a [ImageRef], local_list_ref: &'a str, port: u16 },
	/// Pull `reference` straight from the configured registry, authenticating with
	/// `auth_json` (the device pull secret) when present; install from that ref.
	Registry { reference: &'a str, auth_json: Option<&'a str> },
}

/// One takeover target: its display name plus both SSH identities.
struct Target {
	/// The steady-state `admin@host`, used as the label and for the post-reboot verify.
	host: String,
	/// Stock cloud login, for the pre-reboot steps (transfer, install, reboot).
	initial: Ssh,
	/// `admin@host` + the injected `--ssh-key` identity, for the post-reboot verify.
	admin: Ssh,
}

/// Resolve every remote into both SSH identities up front, surfacing a missing
/// stock login (no `takeover_login`, no `--login`) before any work starts.
fn resolve_targets(
	remotes: &[RemoteConfig],
	login: Option<&str>,
	ssh_key: &str,
) -> Result<Vec<Target>> {
	remotes
		.iter()
		.map(|rc| {
			let initial = rc.takeover_ssh(login)?;
			let admin = rc.admin_ssh(ssh_key);
			Ok(Target { host: admin.host().to_owned(), initial, admin })
		})
		.collect()
}

/// Apply [`takeover_host`] to every target in parallel (its own `[concurrency]
/// takeover` cap), attempting all and naming any laggards (see [`fleet::for_each`]).
fn run_fleet(
	targets: &[Target],
	backend: &Backend,
	files: &[DeviceFile],
	max_workers: Option<NonZeroUsize>,
	job: &mut Scope,
) -> Result<()> {
	fleet::for_each(
		targets,
		"host",
		"takeover",
		|t| t.host.clone(),
		max_workers,
		job,
		|target, scope| takeover_host(target, backend, files, scope),
	)
}

/// Convert one live host to bootc. Steps 1–3 ride the stock-login connection; the
/// final verify switches to `admin@`, since the stock user is gone after the
/// install. Idempotent: a host already on bootc is skipped.
fn takeover_host(
	target: &Target,
	backend: &Backend,
	files: &[DeviceFile],
	job: &mut Scope,
) -> Result<()> {
	let initial = &target.initial;

	// Idempotency: a host already converted (e.g. a re-run after a partial failure
	// past the reboot) is a no-op. Cheaper than re-shipping the image to discover it.
	if already_bootc(initial)? {
		job.println(format!("{}: already a bootc system — nothing to do", target.host));
		return Ok(());
	}

	// 1. Get the arch-matched image into the host's containers-storage; the ref to
	//    install from (and record as the bootc origin) depends on the backend.
	let install_ref = match backend {
		Backend::Lan { images, local_list_ref, port } => {
			stage_lan_image(initial, images, local_list_ref, *port, job)?
		}
		Backend::Registry { reference, auth_json } => {
			stage_registry_image(initial, reference, *auth_json, job)?;
			(*reference).to_owned()
		}
	};

	// 2. Convert the disk in place.
	install_to_existing_root(initial, &install_ref, backend, job)?;

	// 3. Inject the provisioning secrets into the staged deployment's /etc — the
	//    load-bearing step (admin key, pull token, signing policy). Signing
	//    enforcement otherwise rides the image's baked install config, exactly as in
	//    disk provisioning.
	inject_secrets(initial, files, job)?;

	// 4. Reboot over the stock login (it blocks until the host drops off), then
	//    switch to admin@ to wait for it back and assert it's now bootc — which also
	//    proves the injected admin key authenticates.
	upgrade::reboot(initial, job)?;
	upgrade::wait_online(&target.admin, job)?;
	verify_bootc(&target.admin, job)
}

/// LAN backend: identify the host's arch, ship it the matching member from the
/// shared loopback registry, and return the local `containers-storage` ref to
/// install from (the suffix-free `localhost/<name>:latest` list tag).
fn stage_lan_image(
	initial: &Ssh,
	images: &[ImageRef],
	local_list_ref: &str,
	port: u16,
	job: &Scope,
) -> Result<String> {
	let arch = upgrade::remote_arch(initial)?;
	let Some(image) = images.iter().find(|i| i.arch == arch) else {
		let built: Vec<_> = images.iter().map(|i| i.arch.to_string()).collect();
		bail!(
			"{} is {arch}, but `[general.disk_types]` only builds {} — add \"{arch}\" to take over \
			 this host",
			initial.host(),
			built.join(", ")
		);
	};
	upgrade::transfer(job, image, local_list_ref, initial, port)?;
	Ok(local_list_ref.to_owned())
}

/// Registry backend: write the device pull credential (if any) to a host temp file
/// and `sudo podman pull` the ref into containers-storage. Install then runs from
/// the same registry ref (so the recorded origin is the registry, matching the
/// device's steady state).
fn stage_registry_image(
	initial: &Ssh,
	reference: &str,
	auth_json: Option<&str>,
	job: &Scope,
) -> Result<()> {
	let script = match auth_json {
		Some(auth) => format!(
			"set -eu\n\
			 tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT\n\
			 cat > \"$tmp\" <<'BOOTCHER_AUTH'\n\
			 {auth}\
			 BOOTCHER_AUTH\n\
			 sudo podman pull --authfile \"$tmp\" {reference}\n"
		),
		None => format!("set -eu\nsudo podman pull {reference}\n"),
	};
	initial.run_sh(job, "pull image", &[], script)
}

/// `podman run … bootc install to-existing-root` over the stock login — the
/// destructive conversion. The privileged container mounts the host root, dev, and
/// container store; `--target-imgref` pins the recorded bootc origin to the ref the
/// host will track afterwards (a local `containers-storage` ref in LAN mode, the
/// registry ref otherwise).
///
/// The exact flags here are the ones validated on the e2e VM; signing enforcement
/// is not set explicitly — it rides the image's baked install config, exactly as in
/// disk provisioning.
fn install_to_existing_root(
	initial: &Ssh,
	install_ref: &str,
	backend: &Backend,
	job: &Scope,
) -> Result<()> {
	// LAN installs from a local containers-storage image; registry from the registry.
	let target_flags = match backend {
		Backend::Lan { .. } => {
			format!("--target-transport containers-storage --target-imgref {install_ref}")
		}
		Backend::Registry { .. } => format!("--target-imgref {install_ref}"),
	};
	let script = format!(
		"set -eu\n\
		 sudo podman run --rm --privileged --pid=host --security-opt label=disable \
		 -v /:/target -v /var/lib/containers:/var/lib/containers -v /dev:/dev \
		 {install_ref} bootc install to-existing-root --acknowledge-destructive {target_flags}\n"
	);
	initial.run_sh(job, "bootc install to-existing-root", &[], script)
}

/// Install each [`DeviceFile`] into the freshly staged bootc deployment's `/etc`
/// (`/ostree/deploy/*/deploy/*.0/etc`), reusing rotate's heredoc-over-stdin shape.
/// Every provisioning path is under `/etc`, so its tail maps straight onto the
/// staged etc root — landing the secrets exactly where disk provisioning's
/// blueprint would, so the ostree 3-way merge preserves them across upgrades.
fn inject_secrets(initial: &Ssh, files: &[DeviceFile], job: &Scope) -> Result<()> {
	let mut script = String::from(
		"set -eu\n\
		 etc=$(set -- /ostree/deploy/*/deploy/*.0/etc; echo \"$1\")\n\
		 [ -d \"$etc\" ] || { echo \"no staged bootc deployment /etc at $etc\" >&2; exit 1; }\n",
	);
	for (i, f) in files.iter().enumerate() {
		let rel = etc_relative(&f.path)?;
		let marker = format!("BOOTCHER_F{i}");
		let _ = write!(
			script,
			"tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT\n\
			 cat > \"$tmp\" <<'{marker}'\n\
			 {data}\
			 {marker}\n\
			 sudo install -D -m {mode} \"$tmp\" \"$etc{rel}\"\n",
			data = f.data,
			mode = f.mode,
		);
	}
	initial.run_sh(job, "inject secrets into staged /etc", &[], script)
}

/// The path under `/etc` (with the leading `/etc` stripped) — what maps onto the
/// staged deployment's etc root. Errors on a path not under `/etc`, which would
/// otherwise land outside the deployment.
fn etc_relative(path: &str) -> Result<&str> {
	path.strip_prefix("/etc")
		.filter(|rel| rel.starts_with('/'))
		.with_context(|| format!("provisioning path {path} is not under /etc"))
}

/// Whether the host is already a bootc system. A `command -v bootc` guard keeps the
/// probe from erroring on a stock distro that lacks bootc entirely; `unchecked` so a
/// non-bootc host's failing `bootc status` reports `no` rather than erroring.
fn already_bootc(initial: &Ssh) -> Result<bool> {
	let out = initial
		.sh_expr(
			"command -v bootc >/dev/null 2>&1 && sudo bootc status >/dev/null 2>&1 \
			 && echo yes || echo no",
		)
		.stderr_to_stdout()
		.unchecked()
		.read()
		.with_context(|| format!("checking whether {} is already bootc", initial.host()))?;
	Ok(out.trim() == "yes")
}

/// Assert the host is bootc after the reboot, over the `admin@` identity — which
/// also proves the injected admin key authenticates (the stock user is gone).
fn verify_bootc(admin: &Ssh, job: &Scope) -> Result<()> {
	admin.run_sh(job, "verify bootc status", &[], "sudo bootc status".into()).context(
		"`bootc status` failed after takeover — the host may not have converted, or the injected \
		 admin key does not authenticate",
	)
}

/// The destructive-confirmation gate for `bootcher takeover`. `-y/--yes` proceeds;
/// otherwise it's an interactive y/N naming every target, and a non-TTY run without
/// `-y` fails closed (no one to ask ⇒ don't wipe a host on a guess).
///
/// # Errors
///
/// Returns an error when the user declines, or on a non-TTY run without `-y`.
pub fn confirm(hosts: &[String], yes: bool) -> Result<()> {
	match gate(yes, std::io::stdin().is_terminal()) {
		Gate::Proceed => Ok(()),
		Gate::Bail => bail!(
			"takeover irreversibly wipes each target's current OS, and stdin is not a TTY to \
			 confirm — re-run with -y/--yes to proceed"
		),
		Gate::Prompt => {
			eprintln!(
				"takeover will IRREVERSIBLY convert these hosts to bootc, wiping their current OS:"
			);
			for h in hosts {
				eprintln!("  - {h}");
			}
			eprintln!("Back up anything you need first.");
			if inquire::Confirm::new("Proceed with takeover?").with_default(false).prompt()? {
				Ok(())
			} else {
				bail!("aborted: takeover not confirmed");
			}
		}
	}
}

/// The three outcomes of the confirmation gate, factored out of [`confirm`] for
/// testability (the actual prompt needs a TTY).
#[derive(Debug, PartialEq, Eq)]
enum Gate {
	/// `-y` given — go ahead with no prompt.
	Proceed,
	/// Interactive TTY — ask y/N.
	Prompt,
	/// Non-TTY without `-y` — fail closed.
	Bail,
}

fn gate(yes: bool, is_tty: bool) -> Gate {
	if yes {
		Gate::Proceed
	} else if is_tty {
		Gate::Prompt
	} else {
		Gate::Bail
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn probe_ready_host_passes() {
		let out = "podman=yes\nsudo=yes\narch=x86_64\nboot=yes\nfstype=ext4\nefi=yes\nbootc=no\n";
		assert_eq!(evaluate_probe(out), HostReadiness::Ready);
	}

	#[test]
	fn probe_already_bootc_short_circuits() {
		// A bootc host is skipped regardless of the other lines.
		let out = "podman=no\nsudo=no\narch=x86_64\nboot=yes\nfstype=ext4\nefi=yes\nbootc=yes\n";
		assert_eq!(evaluate_probe(out), HostReadiness::AlreadyBootc);
	}

	#[test]
	fn probe_missing_podman_is_unsupported_with_install_hint() {
		let out = "podman=no\nsudo=yes\narch=x86_64\nboot=yes\nfstype=ext4\nefi=yes\nbootc=no\n";
		let HostReadiness::Unsupported(reasons) = evaluate_probe(out) else {
			panic!("expected Unsupported, got {:?}", evaluate_probe(out));
		};
		assert!(
			reasons.iter().any(|r| r.contains("podman") && r.contains("install")),
			"{reasons:?}"
		);
	}

	#[test]
	fn probe_bad_arch_is_unsupported() {
		let out = "podman=yes\nsudo=yes\narch=riscv64\nboot=yes\nfstype=ext4\nefi=yes\nbootc=no\n";
		let HostReadiness::Unsupported(reasons) = evaluate_probe(out) else {
			panic!("expected Unsupported");
		};
		assert!(reasons.iter().any(|r| r.contains("architecture")), "{reasons:?}");
	}

	#[test]
	fn probe_bad_fstype_is_unsupported() {
		let out = "podman=yes\nsudo=yes\narch=x86_64\nboot=yes\nfstype=zfs\nefi=yes\nbootc=no\n";
		let HostReadiness::Unsupported(reasons) = evaluate_probe(out) else {
			panic!("expected Unsupported");
		};
		assert!(reasons.iter().any(|r| r.contains("filesystem")), "{reasons:?}");
	}

	#[test]
	fn probe_legacy_bios_is_unsupported() {
		let out = "podman=yes\nsudo=yes\narch=x86_64\nboot=yes\nfstype=ext4\nefi=no\nbootc=no\n";
		let HostReadiness::Unsupported(reasons) = evaluate_probe(out) else {
			panic!("expected Unsupported");
		};
		assert!(reasons.iter().any(|r| r.contains("BIOS") || r.contains("efi")), "{reasons:?}");
	}

	#[test]
	fn probe_truncated_output_is_not_ready() {
		// A short read (connection cut mid-probe) must never read as Ready.
		assert!(matches!(evaluate_probe("podman=yes\n"), HostReadiness::Unsupported(_)));
		assert!(matches!(evaluate_probe(""), HostReadiness::Unsupported(_)));
	}

	#[test]
	fn etc_relative_strips_the_etc_prefix() {
		assert_eq!(
			etc_relative("/etc/ssh/authorized_keys.d/admin").unwrap(),
			"/ssh/authorized_keys.d/admin"
		);
		assert_eq!(etc_relative("/etc/ostree/auth.json").unwrap(), "/ostree/auth.json");
		// A path outside /etc (or the deceptive `/etcfoo`) is rejected.
		assert!(etc_relative("/var/lib/x").is_err());
		assert!(etc_relative("/etcfoo/x").is_err());
	}

	#[test]
	fn confirmation_gate_fails_closed_off_a_tty() {
		// -y always proceeds; a TTY prompts; a non-TTY without -y bails (never wipes
		// a host on a guess).
		assert_eq!(gate(true, false), Gate::Proceed);
		assert_eq!(gate(true, true), Gate::Proceed);
		assert_eq!(gate(false, true), Gate::Prompt);
		assert_eq!(gate(false, false), Gate::Bail);
	}
}
