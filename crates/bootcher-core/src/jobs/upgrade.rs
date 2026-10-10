use crate::{
	context::{Arch, ImageRef, Manifest, SigningConfig, ToSsh},
	exec::{run_argv_labeled, sh_quote},
	fleet,
	hooks::{HookMetadata, Phase, Stage},
	preflight::{self, Checks},
	progress::Scope,
	registry, signals,
	ssh::Ssh,
};
use anyhow::{Context, Ok, Result, bail};
use duct::cmd;
use std::ffi::OsString;
use std::io::Write;
use std::num::NonZeroUsize;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Check the external tools the push/upgrade phase needs: `podman` always (build
/// the manifest list, push, inspect); `ssh` whenever the push reaches devices —
/// always in LAN mode (the transfer *is* an ssh tunnel), and in registry mode only
/// when listed remotes are upgraded immediately (`!skip_bootc_upgrade`). Co-located
/// with [`run`]; invoked before the push starts.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub(crate) fn preflight(manifest: &Manifest, skip_bootc_upgrade: bool) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	let ssh_needed = match manifest.registry() {
		// LAN mode: the image is shipped to each device over an SSH-tunnelled push.
		None => true,
		// Registry mode: ssh only to run `bootc upgrade` on listed remotes.
		Some(_) => !manifest.deploy_remotes().is_empty() && !skip_bootc_upgrade,
	};
	if ssh_needed {
		checks.bin("ssh", "reach the deploy targets over SSH");
	}
	checks.finish()
}

/// Push the freshly built image to the configured targets and stage a `bootc
/// upgrade` on each. `images` is one [`ImageRef`] per built arch (see
/// [`crate::context::Manifest::images`]); they share a name and registry, so the
/// first decides the backend. Dispatches on whether the manifest configures a
/// registry (see [`Manifest::registry_list_ref`]): a configured registry takes
/// the registry backend (assemble + `podman manifest push` the multi-arch list,
/// then per-device `bootc switch <ref>`), otherwise the LAN backend (identify
/// each device's arch over ssh, then an SSH-tunnelled `podman pull` of the
/// matching member from a loopback mini-registry on the builder + `bootc switch
/// --transport containers-storage`).
///
/// `remotes` is the `[deploy] remotes` list. LAN requires at least one — there's
/// no path to a device without it — but registry mode accepts an empty list:
/// the push is then the whole job (every target pulls the new revision on its
/// next auto-update); listed remotes are additionally upgraded immediately.
///
/// `hooks` carries the manifest's `[hooks.upgrade]` `pre`/`post` commands, run
/// once around the whole push + per-device rollout — so they fire for `deploy`
/// (which calls this, with or without `--skip-build`). The `pre` hook runs after
/// the target-arch check but before the push; `post` only after every device has
/// finished.
///
/// `max_workers` is the manifest's `[concurrency] upgrade` cap (`None` =
/// unbounded), bounding how many devices roll out at once.
///
/// # Errors
///
/// Returns an error if no target arches are configured, a hook fails, the push fails,
/// or any device's upgrade fails.
pub(crate) fn run(
	manifest: &Manifest,
	skip_bootc_upgrade: bool,
	channel: &str,
	job: &mut Scope,
) -> Result<()> {
	let images = manifest.images();
	let remotes = manifest.deploy_remotes().to_ssh();
	let hooks = manifest.hooks();
	if images.is_empty() {
		bail!("no target arches to deploy — set `[targets]` in bootcher.toml");
	}
	let remote_hosts: Vec<String> = remotes.iter().map(|s| s.host().to_owned()).collect();
	let provenance = crate::context::GitProvenance::detect(std::path::Path::new("."));
	// Compute the immutable CalVer tag now, while `provenance` is still whole (its
	// fields are moved into `meta` just below). The commit suffix pins the tag to source.
	let calver = crate::context::calver_now(provenance.short_sha().as_deref());
	let mut meta = HookMetadata {
		phase: Phase::Upgrade,
		stage: Stage::Pre,
		image_name: manifest.general.name.clone(),
		arches: images.iter().map(|i| i.arch).collect(),
		// The ref pushed/served and recorded as the device bootc origin: the registry
		// list ref for this run's channel in registry mode, else the LAN-served local
		// list ref (LAN has no channels).
		image_ref: manifest.registry_list_ref(channel).unwrap_or_else(|| manifest.local_list_ref()),
		revision: provenance.revision,
		version: provenance.version,
		output_dir: None,
		targets: None,
		remotes: (!remote_hosts.is_empty()).then_some(remote_hosts),
		credential: None,
	};
	crate::hooks::run(&meta, hooks.upgrade.pre.as_deref(), job)?;
	if let Some(channel_ref) = manifest.registry_list_ref(channel)
	// The immutable companion tag for this push (see `run_registry`); both refs
	// come from the manifest, not any one arch's image.
	 && let Some(version_ref) = manifest.registry_version_ref(&calver)
	{
		let signing = manifest.signing();
		run_registry(
			&manifest.local_list_ref(),
			(&channel_ref, &version_ref),
			&remotes,
			signing.as_ref(),
			manifest.concurrency().upgrade,
			skip_bootc_upgrade,
			job,
		)?;
	} else if remotes.is_empty() {
		bail!(
			"LAN deploys need at least one target in the `[deploy] remotes` array \
			 (user@host) to ship the image to; or set `registry` in bootcher.toml \
			 to push to a registry without one"
		);
	} else {
		run_lan(
			&images,
			&manifest.local_list_ref(),
			&remotes,
			manifest.concurrency().upgrade,
			job,
		)?;
	}
	meta.stage = Stage::Post;
	crate::hooks::run(&meta, hooks.upgrade.post.as_deref(), job)
}

/// Registry backend: assemble the per-arch members into one multi-arch manifest
/// list, push it to the configured registry (under the run's mutable channel tag
/// — `:latest` by default — and an immutable `CalVer` tag —
/// `:YYYYMMDD.HHMM.g<short-sha>`, or `:YYYYMMDD.HH.MM` with no git — for the same
/// digest), then point each listed device's
/// bootc origin at the (suffix-free) registry ref and upgrade it now. With no
/// remotes the push is the whole job — every target's auto-update timer fetches
/// the new revision (and resolves its own arch out of the list) on its own. The
/// first switch is idempotent against a registry-mode provision (origin already
/// the ref).
fn run_registry(
	local_list_ref: &str,
	// `(channel_ref, version_ref)`: the run's mutable channel tag (`:latest` by
	// default) and the immutable CalVer tag (see `calver_now`), both for the same
	// pushed digest.
	refs: (&str, &str),
	remotes: &[Ssh],
	signing: Option<&SigningConfig>,
	max_workers: Option<NonZeroUsize>,
	skip_bootc_upgrade: bool,
	job: &mut Scope,
) -> Result<()> {
	let (channel_ref, version_ref) = refs;
	let enforce_sig = push_multiarch_list(local_list_ref, channel_ref, version_ref, signing, job)?;

	if remotes.is_empty() || skip_bootc_upgrade {
		job.println(format!(
			"pushed {channel_ref} (also tagged {version_ref}) — targets will pull it on their next \
			 auto-update; add a target to `[deploy] remotes` to apply it immediately"
		));
		return Ok(());
	}

	// Apply to every listed device in parallel, attempting all (see `fleet`).
	fleet::for_each_remote(remotes, "upgrade", max_workers, job, |remote, scope| {
		apply_registry(channel_ref, enforce_sig, remote, scope)
	})
}

/// Assemble the per-arch members into one multi-arch manifest list and push it to
/// the registry under both the mutable `channel_ref` (`:<channel>`, `:latest` by
/// default) tag and the immutable `version_ref` (`CalVer`, see `calver_now`) tag —
/// the same digest under both. Returns whether signature enforcement is in effect
/// (`true` iff `signing` is configured), so the caller records a verifying origin on
/// each device.
///
/// Shared by `upgrade`/`deploy` (which then switch + reboot each device) and
/// [`crate::jobs::takeover`] (where each host pulls the pushed ref directly): both
/// need the image *in* the registry before a device can fetch it.
///
/// # Errors
///
/// Returns an error if assembling the list or either push fails (e.g. this host
/// isn't logged in to the registry).
pub(crate) fn push_multiarch_list(
	local_list_ref: &str,
	channel_ref: &str,
	version_ref: &str,
	signing: Option<&SigningConfig>,
	job: &mut Scope,
) -> Result<bool> {
	// Assemble + push the multi-arch image to the registry. `podman manifest push`
	// streams line-oriented progress to a pipe (not a TTY), so `run_command`'s
	// line-forwarding renders it cleanly without clobbering the live bars.
	// Push auth is the builder's own concern (this machine's `podman login`),
	// distinct from the read-only pull token baked into the image.
	job.step("push image");
	// The list object was assembled by the build phase; `local_list_ref` just names it.

	// `--all` ships every member manifest + blobs the list references, not just the
	// host-arch one, so the registry ends up with the full multi-arch image. With
	// signing configured we splice in `--sign-by-sigstore-private-key` (so the push also
	// writes a cosign signature attachment — having ensured this host is configured
	// to store one), and the caller records a signature-enforcing origin so every
	// later upgrade verifies the signature against the injected policy.
	let mut push_argv: Vec<OsString> =
		["podman", "manifest", "push", "--all"].iter().map(|&s| OsString::from(s)).collect();
	let passguard = match signing {
		Some(cfg) => {
			let ns = crate::jobs::signing::registry_namespace(channel_ref);
			crate::jobs::signing::ensure_push_attachments(ns, job)?;
			let passguard = crate::jobs::signing::sign_args(cfg)?;
			push_argv.extend(passguard.flags().iter().cloned());
			Some(passguard)
		}
		None => None,
	};
	push_argv.push(OsString::from(local_list_ref));
	push_argv.push(OsString::from(channel_ref));
	run_argv_labeled(job, &push_argv, "podman manifest push").with_context(|| {
		let host = channel_ref.split('/').next().unwrap_or(channel_ref);
		format!(
			"`podman manifest push` to {channel_ref} failed — is this machine logged in? (`podman login {host}`)"
		)
	})?;
	// The push is done; the temp passphrase file (held by `passguard`) can go now.
	let enforce_sig = passguard.is_some();
	drop(passguard);

	// Also publish an immutable CalVer tag pointing at the same manifest. `:latest`
	// is the mutable channel devices track; this version tag is a durable,
	// human-readable anchor for rollback/audit (an untagged digest can be GC'd).
	// Same digest ⇒ only a manifest PUT, no layer re-upload. No sign flags: cosign
	// sigstore attachments are keyed by manifest *digest*, so the `:latest` push's
	// attachment already covers the identical digest this tag points at — bootc
	// verifies by digest regardless of the tag the device resolved it through.
	let version_argv: Vec<OsString> =
		["podman", "manifest", "push", "--all", local_list_ref, version_ref]
			.iter()
			.map(OsString::from)
			.collect();
	run_argv_labeled(job, &version_argv, "podman manifest push").with_context(|| {
		format!("`podman manifest push` of the version tag to {version_ref} failed")
	})?;

	Ok(enforce_sig)
}

/// Switch one device's bootc origin to `channel_ref` and reboot it into the upgrade.
/// When `enforce_sig` (i.e. signing is configured in `[deploy] registry`), the switch carries
/// `--enforce-container-sigpolicy`, which records the origin so bootc verifies the
/// image against `/etc/containers/policy.json` and rejects an unsigned/tampered one
/// on this and every later `upgrade` (`upgrade` has no flag of its own — the origin
/// remembers). Runs on its own concurrent [`Scope`] (the device name is its
/// header), so each step renders as a live bar beneath without a per-device counter.
fn apply_registry(
	channel_ref: &str,
	enforce_sig: bool,
	remote: &Ssh,
	job: &mut Scope,
) -> Result<()> {
	// 1. Point bootc at the registry ref. Idempotent: "Image specification is
	//    unchanged" on every run after the first (incl. the switch away from a LAN
	//    containers-storage origin). The signing flag is a no-op to re-apply.
	let flag = if enforce_sig { "--enforce-container-sigpolicy " } else { "" };
	remote.run_sh(job, "bootc switch", &[], format!("sudo bootc switch {flag}{channel_ref}"))?;

	// 2. Stage a new deployment, pulling from the registry.
	remote.run_sh(job, "bootc upgrade", &[], "sudo bootc upgrade".into())?;
	// 3. Reboot into the staged deployment; blocks until the device drops off.
	reboot(remote, job)?;
	// 4. Wait for the device to answer SSH again on the new boot.
	wait_online(remote, job)?;

	Ok(())
}

/// LAN backend: ship to and reboot each listed device in turn. Each device is a
/// single arch, identified over ssh, so it gets the matching member out of
/// `images`.
fn run_lan(
	images: &[ImageRef],
	local_list_ref: &str,
	remotes: &[Ssh],
	max_workers: Option<NonZeroUsize>,
	job: &mut Scope,
) -> Result<()> {
	// Serve the multi-arch list (assembled by the build phase) once, from a single
	// loopback registry shared by the whole fleet (it handles concurrent pulls),
	// rather than standing one up per device. Each device pulls the suffix-free
	// `latest` list over its own forward and resolves its own arch member out of it.
	let reg = registry::serve_manifest_list(local_list_ref, "latest", job)?;
	let port = reg.port();

	// Apply to every device in parallel, attempting all (see `fleet`).
	let result = fleet::for_each_remote(remotes, "upgrade", max_workers, job, |remote, scope| {
		apply_lan(images, local_list_ref, remote, port, scope)
	});

	// Keep serving until every device has finished pulling, then tear it down.
	drop(reg);
	result
}

fn apply_lan(
	images: &[ImageRef],
	local_list_ref: &str,
	remote: &Ssh,
	port: u16,
	job: &mut Scope,
) -> Result<()> {
	// Identify the device's arch first so we ship it the matching member. A device
	// whose arch the project doesn't build is a hard error (better than silently
	// shipping the wrong arch).
	let arch = remote_arch(remote)?;
	let Some(image) = images.iter().find(|i| i.arch == arch) else {
		let built: Vec<_> = images.iter().map(|i| i.arch.to_string()).collect();
		bail!(
			"{} is {arch}, but `[targets]` only builds {} — \
			 add \"{arch}\" to deploy to this device",
			remote.host(),
			built.join(", ")
		);
	};

	// Fully rootless on the builder: `registry::serve` reads the image from the
	// user's rootless storage where `build::run` placed it, and `ssh` uses the
	// invoking user's `~/.ssh/`. Remote-side `sudo` is embedded in the ssh
	// commands below (and the tunnelled `podman pull` in `transfer`).
	//
	// The device stores and switches to the suffix-free `local_list_ref` (its single
	// pulled member is tagged as it), so a multi-arch project pins every device to
	// the same `localhost/<name>:latest` origin.

	// Pre-flight: if the device is already booted into this exact image, the whole
	// pipeline (multi-GB transfer + bootc dance + reboot) is a no-op. bootc itself
	// reports "No update available" at step 3, but only after we've shipped the
	// bytes — so check the digests first: the device's stored list tag against this
	// arch's local member (what the device would pull and re-tag).
	let local_id = local_podman_image_id(&image.tag())?;
	if let Some(remote_id) = remote_podman_image_id(remote, local_list_ref)?
		&& remote_id == local_id
	{
		job.println(format!("{}: already booted into {local_id} — nothing to do", remote.host()));
		return Ok(());
	}

	// Runs on its own concurrent `Scope` (header = the device name), so each step
	// below renders as a live bar beneath it without a per-device step counter.

	// 1. Ship the image to the target's local containers-storage. The builder
	//    serves it from a loopback-only mini-registry and the device pulls over
	//    an SSH remote forward, so podman only fetches the layers the device is
	//    missing and the secret-bearing bytes ride the trusted SSH channel, not
	//    cleartext LAN HTTP.
	transfer(job, image, local_list_ref, remote, port)?;

	// 2. Pin the bootc image spec to the containers-storage transport. Without this
	//    bootc defaults to the `registry` transport and tries to fetch
	//    https://localhost/v2/... from a non-existent local registry. Idempotent:
	//    a no-op ("Image specification is unchanged") on every run after the first.
	remote.run_sh(
		job,
		"bootc switch",
		&[],
		format!("sudo bootc switch --transport containers-storage {local_list_ref}"),
	)?;

	// 3. Stage a new deployment from the (now updated) local image.
	remote.run_sh(job, "bootc upgrade", &[], "sudo bootc upgrade".into())?;

	// 4. Remove the ephemeral pull ref and prune the now-untagged previous image.
	//    bootc has already committed the staged data into the ostree repo, so the
	//    containers-storage copies are redundant. bootc does not prune them itself.
	let pull_ref = format!("127.0.0.1:{port}/{}:latest", image.name);
	remote.run_sh(
		job,
		"cleanup images",
		&[],
		format!("sudo podman rmi --ignore {pull_ref} && sudo podman image prune -f"),
	)?;

	// 5. Reboot into the staged deployment; blocks until the device drops off.
	reboot(remote, job)?;
	// 6. Wait for the device to answer SSH again on the new boot.
	wait_online(remote, job)?;

	Ok(())
}

/// Trigger a reboot and block until the device has actually left the network.
///
/// Rather than fire `systemctl reboot` and then poll for the box to disappear,
/// we open an SSH session running a remote `sh`, feed it the reboot command, and
/// *keep our stdin open*: the remote shell issues the reboot and then blocks
/// reading its next line, so the session stays up until the shutdown tears the
/// connection down — at which point ssh exits. That exit is a precise "the
/// device has left" signal, no down-polling or timeout guesswork needed.
/// `ServerAlive*` bounds the wait if the drop is unclean (network yanked rather
/// than a graceful sshd teardown).
pub(crate) fn reboot(remote: &Ssh, job: &Scope) -> Result<()> {
	let pb = job.spinner("rebooting device");

	let argv = remote.argv(&["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"], "sh");
	let (program, rest) = argv.split_first().expect("non-empty argv");
	let mut ssh = KillOnDrop(
		Command::new(program)
			.args(rest)
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.context("spawning ssh for reboot")?,
	);

	let mut stdin = ssh.0.stdin.take().expect("stdin piped");
	stdin.write_all(b"sudo systemctl reboot\n").context("sending reboot command")?;
	stdin.flush().ok();
	// Hold `stdin` open across the wait so the remote shell stays blocked on its
	// next read; the reboot, not an stdin EOF, is what ends the session.
	let status = ssh.0.wait().context("waiting for reboot to drop the SSH connection")?;
	drop(stdin);
	pb.finish();

	// ssh reports 255 when the connection is closed under it — exactly what the
	// reboot does, so that's the success path. Any other non-zero means we never
	// got that far (host unreachable, shell wouldn't start, …).
	if !status.success() && status.code() != Some(255) {
		bail!("reboot over ssh failed with {status}");
	}
	Ok(())
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Poll SSH connectivity until the device answers again on the new boot. The
/// preceding [`reboot`] already blocked until the device left the network, so
/// there's no lingering pre-reboot connection to falsely match — we only wait
/// for it to come back. Each probe is a throwaway `ssh true` with a short
/// connect timeout and `BatchMode` so a still-down host fails fast instead of
/// hanging or prompting.
///
/// A host-key mismatch ends the wait at once: it won't resolve itself, and on a
/// routine reboot a changed key is exactly what `known_hosts` exists to catch.
pub(crate) fn wait_online(remote: &Ssh, job: &Scope) -> Result<()> {
	poll_online(remote, job, || {})
}

/// [`wait_online`], calling `on_down` after every probe that didn't get in.
pub(crate) fn poll_online(remote: &Ssh, job: &Scope, mut on_down: impl FnMut()) -> Result<()> {
	const POLL: Duration = Duration::from_secs(2);
	const UP_TIMEOUT: Duration = Duration::from_mins(10);

	let pb = job.spinner("waiting for device to come back online");
	let start = Instant::now();
	loop {
		match probe(remote) {
			Probe::Up => break,
			Probe::HostKeyChanged => {
				pb.finish();
				bail!(
					"{host} is back online but its SSH host key no longer matches known_hosts — \
					 refusing to connect. If the change is expected, remove the stale entry \
					 (`ssh-keygen -R <host>`), verify the new key, and re-run",
					host = remote.host()
				);
			}
			Probe::Down => on_down(),
		}
		// Poll the cancellation flag so a Ctrl-C ends the (up-to-10-min) wait
		// promptly instead of pinning a worker until the device returns.
		signals::check()?;
		if start.elapsed() > UP_TIMEOUT {
			pb.finish();
			bail!("device did not come back online within {}s after reboot", UP_TIMEOUT.as_secs());
		}
		thread::sleep(POLL);
	}
	pb.finish();
	Ok(())
}

/// Outcome of one reachability probe, from [`classify_probe`].
#[derive(Debug, PartialEq, Eq)]
enum Probe {
	/// The device accepted a session and ran the command.
	Up,
	/// Not reachable yet (connect/auth/exec failure) — keep polling.
	Down,
	/// ssh refused the host key: the device answers, but not with the key on record.
	HostKeyChanged,
}

/// One throwaway `ssh true`, classified by [`classify_probe`]. stderr is captured
/// (not shown) only to tell a host-key refusal apart from "not up yet".
fn probe(remote: &Ssh) -> Probe {
	// `accept-new` host-key policy is already in `Ssh`'s base opts.
	let argv = remote.argv(&["-o", "ConnectTimeout=5"], "true");
	let (program, rest) = argv.split_first().expect("non-empty argv");
	duct::cmd(program, rest)
		.stdin_null()
		.stdout_null()
		.stderr_capture()
		.unchecked()
		.run()
		.map_or(Probe::Down, |o| {
			classify_probe(o.status.success(), &String::from_utf8_lossy(&o.stderr))
		})
}

/// Classify an `ssh true` probe from its exit status and stderr. ssh prints
/// `Host key verification failed.` whenever it rejects the host key (a changed key
/// under `accept-new`, or an unknown one under `yes`).
fn classify_probe(success: bool, stderr: &str) -> Probe {
	if success {
		Probe::Up
	} else if stderr.contains("Host key verification failed")
		|| stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
	{
		Probe::HostKeyChanged
	} else {
		Probe::Down
	}
}

/// Ship `image` to the target's local containers-storage incrementally, from the
/// fleet's shared loopback registry on `port` ([`run_lan`] stands up one for the
/// whole rollout). The device `podman pull`s the suffix-free `latest` list over an
/// `ssh -R` remote forward — resolving its own arch member — so podman only
/// fetches the layers the device lacks, and the transfer inherits SSH's encryption +
/// authentication: the image holds SSH host keys and the `auth.json` credential,
/// so its bytes must never cross the LAN in the clear.
///
/// The pull lands under the ephemeral `127.0.0.1:<port>/…` ref, so it's re-tagged
/// to the stable `local_list_ref` (the project's [`Manifest::local_list_ref`],
/// `localhost/…:latest`); the following `bootc switch --transport
/// containers-storage <tag>` then records a port-independent origin.
pub(crate) fn transfer(
	job: &Scope,
	image: &ImageRef,
	local_list_ref: &str,
	remote: &Ssh,
	port: u16,
) -> Result<()> {
	let pull_ref = format!("127.0.0.1:{port}/{}:latest", image.name);
	// `-R 127.0.0.1:<port>:127.0.0.1:<port>` tunnels the device's loopback
	// <port> back to the builder's shared registry. Loopback binds need no
	// `GatewayPorts` (no sshd change); `--tls-verify=false` is harmless over
	// localhost; passwordless `wheel` sudo (base image) keeps it non-interactive.
	// `run_sh` forwards podman's line-oriented pull progress without clobbering the
	// live bars.
	let forward = format!("127.0.0.1:{port}:127.0.0.1:{port}");
	let remote_cmd = format!(
		"sudo podman pull --tls-verify=false {pull_ref} && sudo podman tag {pull_ref} {local_list_ref}"
	);
	remote.run_sh(job, "transfer image", &["-R", &forward], remote_cmd)?;
	Ok(())
}

/// Identify a LAN deploy target's CPU arch over ssh (`uname -m`), mapped to an
/// [`Arch`] bootcher builds for, so it's shipped the matching member of a
/// multi-arch project. A machine string bootcher doesn't build is an error.
pub(crate) fn remote_arch(remote: &Ssh) -> Result<Arch> {
	let machine = remote
		.read_sh("uname -m")
		.with_context(|| format!("querying {} architecture over ssh", remote.host()))?;
	Arch::from_uname(&machine)
		.with_context(|| format!("{} reports unsupported architecture {machine:?}", remote.host()))
}

fn local_podman_image_id(tag: &str) -> Result<String> {
	let id = cmd!("podman", "image", "inspect", "-f", "{{.Id}}", tag)
		.read()
		.context("`podman image inspect` for id")?
		.trim()
		.to_owned();
	if id.is_empty() {
		bail!("`podman image inspect` returned empty id for {tag}");
	}
	Ok(id)
}

/// Returns `None` if the host has no currently-booted bootc deployment
/// (fresh provision, mid-stage, …) — in that case the caller proceeds
/// with the full upgrade.
fn remote_podman_image_id(remote: &Ssh, tag: &str) -> Result<Option<String>> {
	// `unchecked()`: a missing image makes `podman image inspect` exit 125, which
	// is the expected "not present" case here (e.g. a device provisioned via the
	// registry backend has no `localhost/…` ref). Without it, duct's `read()`
	// turns that non-zero exit into an error before we ever get to inspect the
	// captured "Error: …" text below, breaking the LAN pre-flight entirely.
	let id = remote
		.sh_expr(&format!("sudo podman image inspect -f '{{{{.Id}}}}' {}", sh_quote(tag)))
		.stderr_to_stdout()
		.unchecked()
		.read()
		.context("`podman image inspect` for id")?
		.trim()
		.to_owned();

	Ok(if id.is_empty() || id.starts_with("Error: ") { None } else { Some(id) })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn probe_success_is_up() {
		assert_eq!(classify_probe(true, ""), Probe::Up);
	}

	#[test]
	fn probe_connect_failure_is_down() {
		let stderr = "ssh: connect to host h port 22: Connection refused\n";
		assert_eq!(classify_probe(false, stderr), Probe::Down);
		assert_eq!(classify_probe(false, "admin@h: Permission denied (publickey).\n"), Probe::Down);
	}

	#[test]
	fn probe_changed_host_key_is_detected() {
		let stderr = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
			@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
			Host key for h has changed and you have requested strict checking.\n\
			Host key verification failed.\n";
		assert_eq!(classify_probe(false, stderr), Probe::HostKeyChanged);
	}
}
