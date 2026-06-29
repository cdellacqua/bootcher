//! Post-provision credential rotation on the already-deployed devices.
//!
//! `provision` injects a device's credentials (the admin SSH key, and in
//! registry mode the read-only pull token) into its persistent `/etc` once, at
//! image-build time. This module rotates them afterwards, in place: it reuses
//! the same collection logic as provision ([`crate::jobs::secrets`]) but
//! delivers the result over the live SSH channel `deploy`/`upgrade` already use,
//! writing the file straight to the device rather than baking it into a fresh
//! image. Nothing is rebuilt or re-provisioned — only the on-device credential
//! changes. The fleet-wide rollout runs in parallel across the configured remotes.
//!
//! Three subcommands:
//!
//! - [`registry_token`] — registry pull token (`/etc/ostree/auth.json`), registry mode only.
//! - [`ssh_key`] — admin SSH authorized key (`/etc/ssh/authorized_keys.d/admin`), all modes.
//! - [`signing_key`] — image-signing public key + policy (`/etc/pki/containers/`,
//!   `/etc/containers/policy.json`, `/etc/containers/registries.d/`), registry + signing only.
//!   Also covers *enrolling* signing on a fleet provisioned before signing was enabled.
//!
//! [`registry_token`] and [`ssh_key`] are validate-then-commit, so a bad credential (or an
//! interrupted run) never bricks a device. [`registry_token`] stages the candidate
//! `auth.json` and only installs it once `podman` proves it authenticates. [`ssh_key`]
//! is the same shape but with a twist unique to SSH: the new key can only be
//! verified by a *fresh* login, so it can't be checked in the staging shell.
//! Instead it leaves the old key live *alongside* the new one during validation —
//! so even a mid-rotation crash leaves both keys working, never a lockout — and
//! retires the old key only after the new one authenticates.
//!
//! Crucially, that "authenticates" check forces the *new* key specifically: the
//! probe disables the agent and any config identity and offers only the new
//! private key (`validate_login`). If it instead used the ambient ssh identity,
//! the old key (live below the marker) would answer and the probe would pass even
//! for a typo'd new key — then the commit would retire the old key and lock the
//! device out. So [`ssh_key`] needs the new *private* key, passed directly as the
//! `--key` argument (the `.pub` sibling is derived from it for `authorized_keys`).
//!
//! [`signing_key`] writes directly without a post-write check: verifying a new
//! signing key would require pulling and staging an image signed with it, which is
//! expensive and only makes sense when a new image is actually ready to deploy. A
//! bad key only blocks future `bootc upgrade` attempts — SSH access is always
//! unaffected, so the fleet is always recoverable by rotating the signing key again.

use crate::context::{DEVICE_ADMIN_AUTHORIZED_KEYS, DEVICE_AUTH_JSON, Manifest, ToSsh};
use crate::preflight::Checks;
use crate::progress::Scope;
use crate::ssh::Ssh;
use crate::{exec, fleet, jobs::secrets};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::time::Duration;

/// Check the external tools a credential rotation needs: `ssh` always (every
/// rotation writes the new secret to each device over the live SSH channel), plus
/// `podman` when `pull_check` (the `rotate pull-token` local pre-flight, skipped by
/// `--skip-pull-check`). Co-located with the rotate entry points; invoked before
/// the rollout starts.
///
/// # Errors
///
/// Returns an error listing every missing prerequisite.
pub fn preflight(pull_check: bool) -> Result<()> {
	let mut checks = Checks::default();
	checks.bin("ssh", "reach the deployed devices over SSH");
	if pull_check {
		checks.bin("podman", "pre-flight the new pull token against the registry locally");
	}
	checks.finish()
}

/// The fleet's `[deploy] remotes` as SSH targets, or an error naming `cmd` (the
/// `rotate` subcommand) if none are configured — every rotation writes to those
/// targets, so an empty set is a hard stop.
fn deploy_remotes_or_bail(manifest: &Manifest, cmd: &str) -> Result<Vec<Ssh>> {
	let remotes = manifest.deploy_remotes().to_ssh();
	if remotes.is_empty() {
		bail!(
			"no devices to rotate — `rotate {cmd}` writes to the `[deploy] remotes` \
			 targets, and none are configured in bootcher.toml"
		);
	}
	Ok(remotes)
}

/// Rotate the registry pull token on every configured device. Collects a fresh
/// credential once up front (env or TTY prompt, like provision), verifies it
/// against the registry from this machine, then per device verifies it actually
/// pulls the project's image before committing it to [`DEVICE_AUTH_JSON`].
/// Registry mode only — a LAN device carries no pull secret.
///
/// `skip_pull_check` (`--skip-pull-check`) bypasses only the eager *local* pre-flight;
/// the per-device on-device check — the authoritative guard against committing a
/// bad token — always runs.
///
/// # Panics
///
/// Panics if `[targets]` is empty (validated non-empty by manifest loading).
///
/// # Errors
///
/// Returns an error if registry mode is not configured, no devices are set, credential
/// collection fails, or any device's rotation fails.
pub fn registry_token(manifest: &Manifest, skip_pull_check: bool, job: &mut Scope) -> Result<()> {
	let Some(ns) = manifest.registry() else {
		bail!(
			"`rotate pull-token` only applies in registry mode, but `[deploy] registry` \
			 is not set in bootcher.toml — LAN devices carry no pull secret to rotate"
		);
	};
	let remotes = deploy_remotes_or_bail(manifest, "pull-token")?;

	// Collect the new credential once, before touching any device. Verify it from
	// here up front (unless opted out) so a typo'd token fails the whole command
	// before the rollout starts, rather than device-by-device. A blank username
	// (the public-registry opt-out) has no meaning here — there's no token to roll —
	// so it's a hard error rather than silently wiping each device's credential.
	let Some((user, token)) = secrets::collect_pull_credential(ns)? else {
		bail!(
			"`rotate pull-token` needs a credential, but none was provided (a blank username means \
			 a public registry, which has no token to rotate)"
		);
	};
	if !skip_pull_check {
		secrets::verify_pull_login(ns, &user, &token)?;
	}
	let auth_json = secrets::render_auth_json(ns, &user, &token);
	// A suffix-free, multi-arch registry ref the new credential is verified against on
	// the device before it's committed. Channel-agnostic: pull auth is scoped to the
	// registry namespace, not the tag, and the default `latest` channel is the ref
	// most likely to exist — so a credential probe uses it regardless of which channel
	// a given device tracks. Project-level (arch-independent), straight off the manifest.
	let registry_ref = manifest
		.registry_list_ref(crate::context::DEFAULT_CHANNEL)
		.expect("registry set ⇒ a registry ref exists");

	fleet::for_each_remote(
		&remotes,
		"pull-token rotation",
		manifest.concurrency().rotate,
		job,
		|remote, scope| {
			verify_and_commit_token(remote, scope, &registry_ref, DEVICE_AUTH_JSON, &auth_json)
		},
	)
}

/// Verify the candidate `auth.json` on `remote`, then commit it to `path` — all
/// in one remote shell so the verification *is* the transaction guard. The
/// candidate is staged to a device temp file, `podman manifest inspect` proves it
/// authenticates and can see `registry_ref` (a manifest-only fetch, no blobs), and only
/// on success is it `install`ed over the live credential — so a failed probe (or an
/// unreachable device) aborts before the commit and leaves the credential untouched,
/// with no old file to restore. The script rides ssh stdin with the candidate in a
/// quoted heredoc, `set -e` skipping the commit on a failed probe, and a `trap`
/// cleaning the temp (see [`Ssh`] for why stdin, not argv).
fn verify_and_commit_token(
	remote: &Ssh,
	scope: &Scope,
	registry_ref: &str,
	auth_json_path: &str,
	auth_json: &str,
) -> Result<()> {
	let script = format!(
		"set -eu\n\
		 tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT\n\
		 cat > \"$tmp\" <<'BOOTCHER_EOF'\n\
		 {auth_json}\
		 BOOTCHER_EOF\n\
		 chmod 600 \"$tmp\"\n\
		 sudo podman manifest inspect --authfile \"$tmp\" {registry_ref} >/dev/null\n\
		 sudo install -m 0600 -D \"$tmp\" {auth_json_path}\n"
	);
	remote.run_sh(
		scope,
		format!("verify + write {auth_json_path} on {}", remote.host()),
		&[],
		script,
	)
}

/// Sentinel comment delimiting the soon-to-be-retired keys in a device's
/// `authorized_keys` during a [`ssh_key`] rotation. The new key is written *above* it,
/// the current keys live on *below* it (a `#` comment is inert to sshd, so the
/// keys under it still authenticate). [`commit_key_script`] drops the marker and
/// everything below it; the next [`stage_key`] sweeps any stragglers a crashed
/// run left behind. Deliberately punctuation-free so it needs no shell quoting.
const RETIRING_MARKER: &str = "# bootcher-retiring: removed on the next successful rotate key";

/// How many times to attempt the post-stage validation login before giving up and
/// rolling back. The new key is live the instant it's staged (sshd reads
/// `authorized_keys` per connection), so one try almost always suffices — the
/// retries only absorb a transient network blip, so we don't retire the old key
/// over a momentary hiccup.
const KEY_VALIDATE_ATTEMPTS: usize = 3;

/// Rotate the admin SSH key on every configured device. Collects the new key once
/// up front (a private key path or a TTY picker), then rolls the fleet in parallel.
/// Each device is lockout-safe: the new key is staged and proven with a real login
/// before the old key is retired, so a failed or interrupted rotation always leaves
/// a working key in place. Applies regardless of LAN/registry mode.
///
/// # Errors
///
/// Returns an error if no devices are configured, key collection fails, or any
/// device's rotation fails.
pub fn ssh_key(manifest: &Manifest, key: Option<&str>, job: &mut Scope) -> Result<()> {
	let remotes = deploy_remotes_or_bail(manifest, "ssh-key")?;

	// Collect the new key; may run the interactive picker on a TTY.
	let (authorized_keys, key_path) = secrets::collect_authorized_keys(key)?;

	fleet::for_each_remote(
		&remotes,
		"admin-key rotation",
		manifest.concurrency().rotate,
		job,
		|remote, scope| {
			verify_and_commit_key(
				remote,
				scope,
				DEVICE_ADMIN_AUTHORIZED_KEYS,
				&authorized_keys,
				&key_path,
			)
		},
	)
}

/// Push the image-signing config to every configured device. Registry mode only;
/// targets `[deploy] remotes`. Two use cases:
///
/// - **Enrolling signing** on a fleet provisioned before signing was enabled — run
///   this, then `bootcher deploy` to switch the bootc origin to the verifying
///   transport.
/// - **Rotating the signing key** when the cosign key has leaked or is expiring —
///   run this, then `bootcher deploy`; the window between the two steps leaves
///   `bootc upgrade` failing verification (the registry still carries the old
///   signature), but SSH access is unaffected and the fleet is always recoverable.
///
/// Connects via SSH to each device and atomically overwrites its trusted key
/// (`/etc/pki/containers/<name>.pub`), `policy.json`, and the `registries.d`
/// sigstore-attachment drop-in. Defaults to the public key from `[deploy] registry`
/// in bootcher.toml; pass `--pubkey` to use a different one.
///
/// # Errors
///
/// Returns an error if registry/signing mode is not configured, key collection fails,
/// or any device's update fails.
pub fn signing_key(manifest: &Manifest, pubkey: Option<&str>, job: &mut Scope) -> Result<()> {
	let Some(ns) = manifest.registry() else {
		bail!(
			"`rotate sign-key` only applies in registry mode, but `[deploy] registry` is not set \
			 in bootcher.toml — image signing is a registry-mode feature"
		);
	};
	let Some(signing) = manifest.signing() else {
		bail!(
			"`[deploy] registry` is a plain URL with no signing config — there's no signing key to \
			 rotate. Expand it to the signing form: \
			 `registry = {{ url = \"...\", key = \"cosign.key\" }}`"
		);
	};
	let remotes = deploy_remotes_or_bail(manifest, "sign-key")?;

	let name = &manifest.general.name;
	let device_pubkey_path = secrets::device_pubkey_path(name);
	let policy_json = secrets::render_policy_json(ns, &[&device_pubkey_path]);
	let registries_d_path = secrets::device_registries_d_path();
	let registries_d = secrets::render_registries_d(ns);

	let pubkey_path = pubkey.map_or_else(|| signing.public_key_path(), str::to_owned);
	let pubkey_pem = read_pubkey(&pubkey_path)?;
	let script = signing_key_script(
		&pubkey_pem,
		&device_pubkey_path,
		&policy_json,
		&registries_d_path,
		&registries_d,
	);

	fleet::for_each_remote(
		&remotes,
		"sign-key rotation",
		manifest.concurrency().rotate,
		job,
		|remote, scope| {
			remote.run_sh(
				scope,
				format!("replace signing key on {}", remote.host()),
				&[],
				script.clone(),
			)
		},
	)
}

/// Read a public-key PEM from `path`, with a pointer to enroll on a miss.
fn read_pubkey(path: &str) -> Result<String> {
	fs::read_to_string(path).with_context(|| {
		format!("reading the signing public key {path} — generate one with `bootcher sign enroll`")
	})
}

/// Device script: overwrite `pubkey_path` with the new key's PEM, install
/// `policy.json` trusting it as the sole signing key, and ensure the
/// `registries.d` drop-in enabling sigstore-attachment lookups is present.
/// Writing the drop-in is idempotent for devices already provisioned with
/// signing; it's what makes this command also cover enrolling signing on a
/// device that was provisioned unsigned.
fn signing_key_script(
	pubkey_pem: &str,
	pubkey_path: &str,
	policy_json: &str,
	registries_d_path: &str,
	registries_d: &str,
) -> String {
	format!(
		"set -eu\n\
		 ptmp=$(mktemp); poltmp=$(mktemp); rdtmp=$(mktemp)\n\
		 trap 'rm -f \"$ptmp\" \"$poltmp\" \"$rdtmp\"' EXIT\n\
		 cat > \"$ptmp\" <<'BOOTCHER_PUB'\n\
		 {pubkey_pem}\
		 BOOTCHER_PUB\n\
		 sudo install -m 0644 -D \"$ptmp\" {pubkey_path}\n\
		 cat > \"$poltmp\" <<'BOOTCHER_POL'\n\
		 {policy_json}\
		 BOOTCHER_POL\n\
		 sudo install -m 0644 \"$poltmp\" /etc/containers/policy.json\n\
		 cat > \"$rdtmp\" <<'BOOTCHER_REGD'\n\
		 {registries_d}\
		 BOOTCHER_REGD\n\
		 sudo install -m 0644 -D \"$rdtmp\" {registries_d_path}\n"
	)
}

/// Rotate the admin SSH key on one `remote`, lockout-safe. An SSH key can only be
/// proven by a *fresh* login, so unlike the pull token this can't be one atomic
/// command; instead the old key stays live until the new one is proven:
///
/// 1. **stage** ([`stage_key`]): new key, then a [`RETIRING_MARKER`], then the
///    current keys below it — both authenticate now, so a crash here can't lock out.
/// 2. **validate** ([`validate_login`]): a *fresh* login that succeeds only if the
///    new key works. On failure, roll back to exactly the prior keys
///    ([`rollback_key_script`]) over the still-live old key and abort, retiring nothing.
/// 3. **commit** ([`commit_key_script`]): drop the marker and everything below it, leaving
///    only the proven new key.
///
/// `identity` is the new key's private half, used to prove that key specifically in
/// step 2.
fn verify_and_commit_key(
	remote: &Ssh,
	scope: &Scope,
	auth_keys_path: &str,
	authorized_keys: &str,
	identity: &Path,
) -> Result<()> {
	stage_key(remote, scope, auth_keys_path, authorized_keys)?;

	if let Err(e) = validate_login(remote, scope, identity) {
		// The new key never proved out. The old key is still live (staged
		// alongside), so reach back over it and restore exactly the prior file —
		// best-effort: even if this fails, both keys still work, so it's safe.
		let _ = remote.run_sh(
			scope,
			format!("roll back admin key on {}", remote.host()),
			&[],
			rollback_key_script(auth_keys_path),
		);
		return Err(e).with_context(|| {
			format!(
				"the new admin key did not authenticate to {} — rolled back to the \
				 previous key(s); nothing was retired and no lockout occurred",
				remote.host()
			)
		});
	}

	// Proven. Retire the old key over the now-trusted new key.
	remote.run_sh(
		scope,
		format!("retire old admin key on {}", remote.host()),
		&[],
		commit_key_script(auth_keys_path),
	)
}

/// Stage the candidate keys on `remote`: write them above a [`RETIRING_MARKER`]
/// with the current keys carried live below it, then install atomically. Stripping
/// blank lines and any *prior* marker as it carries the old keys down collapses a
/// previous crashed rotation's stragglers into this one's retiring set — self-healing
/// cleanup.
fn stage_key(
	remote: &Ssh,
	scope: &Scope,
	auth_keys_path: &str,
	authorized_keys: &str,
) -> Result<()> {
	let script = format!(
		"set -eu\n\
		 tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT\n\
		 cat > \"$tmp\" <<'BOOTCHER_EOF'\n\
		 {authorized_keys}\
		 BOOTCHER_EOF\n\
		 printf '%s\\n' '{RETIRING_MARKER}' >> \"$tmp\"\n\
		 if [ -f {auth_keys_path} ]; then \
		   grep -v -e '^[[:space:]]*$' -e '^# bootcher-retiring' {auth_keys_path} >> \"$tmp\" || true; \
		 fi\n\
		 sudo install -m 0644 \"$tmp\" {auth_keys_path}\n\
		 sudo restorecon {auth_keys_path} 2>/dev/null || true\n"
	);
	remote.run_sh(scope, format!("stage new admin key on {}", remote.host()), &[], script)
}

/// Open a *fresh*, non-interactive login to `remote` offering *only* the new key
/// (`identity`) and prove it authenticates. `IdentityAgent=none` +
/// `IdentitiesOnly=yes` + a single `-i` keep the still-live old key from answering
/// in the new key's place (see the module header for why that matters). `BatchMode`
/// (from [`Ssh`]) fails fast; a short timeout with a few retries rides out a
/// transient blip. A bare `true`: a successful session *is* the proof.
fn validate_login(remote: &Ssh, scope: &Scope, identity: &Path) -> Result<()> {
	let identity = identity.to_str().context("the validation identity path is not UTF-8")?;
	let pre = [
		"-o",
		"ConnectTimeout=10",
		"-o",
		"IdentitiesOnly=yes",
		"-o",
		"IdentityAgent=none",
		"-i",
		identity,
	];
	let mut last_err = None;
	for attempt in 1..=KEY_VALIDATE_ATTEMPTS {
		crate::signals::check()?;
		let argv = remote.argv(&pre, "true");
		let label = format!("verify new admin key on {}", remote.host());
		match exec::run_argv_labeled(scope, &argv, label) {
			Ok(()) => return Ok(()),
			Err(e) => {
				last_err = Some(e);
				if attempt < KEY_VALIDATE_ATTEMPTS {
					std::thread::sleep(Duration::from_secs(2));
				}
			}
		}
	}
	Err(last_err.expect("the loop runs at least once"))
}

/// Remote script that retires the old key: delete the [`RETIRING_MARKER`] and
/// everything below it, leaving only the freshly-proven key above it. Run over the
/// new key, after [`validate_login`] succeeds.
fn commit_key_script(auth_keys_path: &str) -> String {
	format!(
		"set -eu; \
		 tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT; \
		 sed '/^# bootcher-retiring/,$d' {auth_keys_path} > \"$tmp\"; \
		 sudo install -m 0644 \"$tmp\" {auth_keys_path}; \
		 sudo restorecon {auth_keys_path} 2>/dev/null || true"
	)
}

/// Remote script that undoes a [`stage_key`]: delete everything from the file head
/// through the [`RETIRING_MARKER`], leaving exactly the keys that were live before
/// the rotation. Guarded by a marker check first — without it, the `1,/marker/`
/// range would run to EOF and wipe the file, so a missing marker (nothing staged)
/// is a no-op rather than a self-inflicted lockout.
fn rollback_key_script(auth_keys_path: &str) -> String {
	format!(
		"set -eu; \
		 if grep -q '^# bootcher-retiring' {auth_keys_path}; then \
		   tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT; \
		   sed '1,/^# bootcher-retiring/d' {auth_keys_path} > \"$tmp\"; \
		   sudo install -m 0644 \"$tmp\" {auth_keys_path}; \
		   sudo restorecon {auth_keys_path} 2>/dev/null || true; \
		 fi"
	)
}
