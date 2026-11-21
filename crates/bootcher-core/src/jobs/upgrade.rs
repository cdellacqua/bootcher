//! The two device steps every device-touching command is built from, plus the
//! reboot/online plumbing they share.
//!
//! - **Direct device step** ([`run_direct`] / [`apply_direct`]): serve the local
//!   manifest list from a loopback registry, have each device pull its arch member
//!   over an `ssh -R` tunnel into its containers-storage as
//!   `localhost/<name>:latest`, `bootc switch --transport containers-storage` to
//!   it, upgrade, and reboot into it.
//! - **Channel device step** ([`apply_channel`]): with signing on, make sure the
//!   trust files are current, then `bootc switch [--enforce-container-sigpolicy]
//!   <channel ref>`, `bootc upgrade`, and reboot into the staged deployment.
//!
//! Both are idempotent: running them on a device that already runs the target is
//! cheap (nothing staged ⇒ no reboot), which is what lets a re-run retry exactly
//! the devices a failed run missed.

use crate::{
	context::{Arch, ImageRef},
	exec::sh_quote,
	fleet,
	preflight::Checks,
	progress::Scope,
	registry, signals,
	ssh::Ssh,
};
use anyhow::{Context, Ok, Result, bail};
use duct::cmd;
use std::io::Write;
use std::num::NonZeroUsize;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Check the external tools the direct device step needs: `podman` (serve the
/// local list) and `ssh` (the tunnelled transfer).
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub(crate) fn preflight_direct() -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("podman", crate::preflight::PODMAN_HINT);
	checks.bin("ssh", "reach the devices over SSH");
	checks.finish()
}

/// Direct device step across `targets`: serve the local manifest list `list_ref`
/// once from a single loopback registry shared by the whole fleet (it handles
/// concurrent pulls), and ship each device the member of `images` matching its
/// arch, stored on the device as `device_ref` (the project's
/// `localhost/<name>:latest`). Returns the per-device [`fleet::Report`], failures
/// included, for the caller to raise after its `post` hook.
///
/// # Errors
///
/// Returns an error if the list can't be served, or a signal cut the rollout short.
pub(crate) fn run_direct(
	images: &[ImageRef],
	list_ref: &str,
	device_ref: &str,
	targets: &[Ssh],
	max_workers: Option<NonZeroUsize>,
	job: &mut Scope,
) -> Result<fleet::Report> {
	let reg = registry::serve_manifest_list(list_ref, "latest", job)?;
	let port = reg.port();
	let result = fleet::for_each_remote(targets, "upgrade", max_workers, job, |remote, scope| {
		apply_direct(images, device_ref, remote, port, scope)
	});
	// Keep serving until every device has finished pulling, then tear it down.
	drop(reg);
	result
}

/// The direct device step on one device; see [`run_direct`].
pub(crate) fn apply_direct(
	images: &[ImageRef],
	device_ref: &str,
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
			"{} is {arch}, but this run built only {} — add \"{arch}\" to `[targets]` to \
			 update this device",
			remote.host(),
			built.join(", ")
		);
	};

	// Pre-flight: if the device already stores this exact image (the arch member's
	// local ID against the device's stored list tag), skip the multi-GB transfer.
	// That alone doesn't mean it *booted* it — a staged deployment can be dropped at
	// shutdown — so the switch + upgrade below still run, and bootc stages nothing
	// when the booted deployment already matches.
	let local_id = local_podman_image_id(&image.tag())?;
	let stored = remote_podman_image_id(remote, device_ref)?.is_some_and(|id| id == local_id);

	// 1. Ship the image to the target's local containers-storage over the SSH
	//    tunnel (see `transfer`).
	if !stored {
		transfer(job, image, device_ref, remote, port)?;
	}

	// 2. Pin the bootc image spec to the containers-storage transport. Without this
	//    bootc defaults to the `registry` transport and tries to fetch
	//    https://localhost/v2/... from a non-existent local registry. Idempotent:
	//    a no-op ("Image specification is unchanged") on every run after the first.
	//    This also re-pins a fresh direct device from the signature-enforcing origin
	//    its install recorded to an unverified one: a direct device's trust is the
	//    SSH channel, not a signature.
	remote.run_sh(
		job,
		"bootc switch",
		&[],
		format!("sudo bootc switch --transport containers-storage {device_ref}"),
	)?;

	// 3. Stage a new deployment from the (now updated) local image.
	remote.run_sh(job, "bootc upgrade", &[], "sudo bootc upgrade".into())?;

	// 4. Remove the ephemeral pull ref and prune the now-untagged previous image.
	//    bootc has already committed the staged data into the ostree repo, so the
	//    containers-storage copies are redundant. bootc does not prune them itself.
	if !stored {
		let pull_ref = format!("127.0.0.1:{port}/{}:latest", image.name);
		remote.run_sh(
			job,
			"cleanup images",
			&[],
			format!("sudo podman rmi --ignore {pull_ref} && sudo podman image prune -f"),
		)?;
	}

	// 5. Reboot into the staged deployment and check it's what booted.
	reboot_into_staged(remote, job)
}

/// The channel device step on one device: run `trust_script` first when signing is
/// on (it installs or refreshes the trust files — they must land before
/// `--enforce-container-sigpolicy`, which a permissive default policy makes bootc
/// refuse), then point the origin at `channel_ref` (verifying when `enforce`),
/// upgrade, and reboot into it. Idempotent: "Image specification is unchanged" and
/// nothing staged on a device already there.
///
/// # Errors
///
/// Returns an error if any remote step fails, or the device boots something other
/// than what was staged.
pub(crate) fn apply_channel(
	channel_ref: &str,
	trust_script: Option<&str>,
	remote: &Ssh,
	job: &mut Scope,
) -> Result<()> {
	if let Some(script) = trust_script {
		remote.run_sh(job, "install trust files", &[], script.to_owned())?;
	}
	let flag = if trust_script.is_some() { "--enforce-container-sigpolicy " } else { "" };
	remote.run_sh(job, "bootc switch", &[], format!("sudo bootc switch {flag}{channel_ref}"))?;
	remote.run_sh(job, "bootc upgrade", &[], "sudo bootc upgrade".into())?;
	reboot_into_staged(remote, job)
}

/// Reboot `remote` into its staged bootc deployment and check that it booted it.
/// Nothing staged means the booted deployment already is the image `bootc upgrade`
/// was pointed at, so there's nothing to reboot for.
///
/// The check matters because ostree only writes a staged deployment out at shutdown
/// (`ostree-finalize-staged.service`); when that fails the device comes back on the
/// old deployment and the staged one is gone, with nothing else reporting it.
fn reboot_into_staged(remote: &Ssh, job: &Scope) -> Result<()> {
	let Some(staged) = bootc_status(remote)?.staged else {
		job.println(format!("{}: already booted into this image — nothing to do", remote.host()));
		return Ok(());
	};
	// Blocks until the device drops off, then until it answers SSH on the new boot.
	reboot(remote, job)?;
	wait_online(remote, job)?;

	let booted = bootc_status_after_reboot(remote)?.booted;
	if booted.as_ref().map(|b| &b.digest) != Some(&staged.digest) {
		let booted = booted.map_or_else(|| "no bootc deployment".to_owned(), |b| b.to_string());
		bail!(
			"{} rebooted into {booted} instead of the staged {staged}: the staged deployment \
			 wasn't applied — check `journalctl -b -1 -u ostree-finalize-staged` on the device",
			remote.host()
		);
	}
	Ok(())
}

/// [`bootc_status`], retried for a while: right after a reboot one ssh login can get
/// in and the next still be reset while the device settles (sshd restarting,
/// qemu user-net), and that blip says nothing about what booted.
fn bootc_status_after_reboot(remote: &Ssh) -> Result<crate::device::HostStatus> {
	const ATTEMPTS: u32 = 15;
	const RETRY: Duration = Duration::from_secs(2);
	let mut attempt = 1;
	loop {
		let status = bootc_status(remote);
		if status.is_ok() || attempt >= ATTEMPTS {
			return status;
		}
		signals::check()?;
		thread::sleep(RETRY);
		attempt += 1;
	}
}

/// Read `remote`'s bootc status (spec, booted and staged deployments).
fn bootc_status(remote: &Ssh) -> Result<crate::device::HostStatus> {
	let json = remote
		.read_sh("sudo bootc status --format json")
		.with_context(|| format!("reading `bootc status` on {}", remote.host()))?;
	crate::device::parse_status(&json)
		.with_context(|| format!("parsing `bootc status --format json` from {}", remote.host()))
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
/// fleet's shared loopback registry on `port` ([`run_direct`] stands up one for the
/// whole rollout). The device `podman pull`s the suffix-free `latest` list over an
/// `ssh -R` remote forward — resolving its own arch member — so podman only
/// fetches the layers the device lacks, and the transfer inherits SSH's encryption +
/// authentication: the image holds SSH host keys and the `auth.json` credential,
/// so its bytes must never cross the LAN in the clear.
///
/// The pull lands under the ephemeral `127.0.0.1:<port>/…` ref, so it's re-tagged
/// to the stable `local_list_ref` (the project's `localhost/<name>:latest`); the following `bootc switch --transport
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

/// Identify a device's CPU arch over ssh (`uname -m`), mapped to an
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
	// is the expected "not present" case here (e.g. a device provisioned from a
	// channel device has no `localhost/…` ref). Without it, duct's `read()`
	// turns that non-zero exit into an error before we ever get to inspect the
	// captured "Error: …" text below, breaking the direct pre-flight entirely.
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
