use anyhow::Result;

use crate::context::Manifest;
use crate::jobs::secrets::Provisioning;
use crate::{jobs, progress};

/// Full takeover: build every target arch's container, then convert each live
/// `[deploy] remotes` host to bootc in place. A third device-facing pipeline,
/// sibling to `deploy` — but a *one-time* funnel: after it succeeds each
/// host is an ordinary bootc device on the same origin, so `deploy`/`rotate`
/// take over from there unchanged.
///
/// Two phases, like the others: the build fan-out (honouring `[hooks.build]`) then
/// the per-host takeover rollout (honouring `[hooks.takeover]`). `login` is the
/// fleet-wide stock cloud login (`--login`); `provisioning` is the same secret set
/// disk provisioning bakes; `ssh_key` is the resolved admin private key path.
/// `skip_build` (`--skip-build`) converts the hosts from the already-built
/// container, skipping the container build — the takeover phase alone (its hooks
/// still fire). `yes` (`-y`) trusts each host's new SSH host key without prompting.
///
/// # Errors
///
/// Returns an error if the build or takeover phase fails.
pub fn run(
	manifest: &Manifest,
	login: Option<&str>,
	provisioning: &Provisioning,
	ssh_key: &str,
	skip_build: bool,
	channel: &str,
	yes: bool,
) -> Result<()> {
	if skip_build {
		return jobs::takeover::run(
			manifest,
			login,
			provisioning,
			ssh_key,
			channel,
			yes,
			&mut progress::Scope::standalone(),
		);
	}
	let mut b = progress::Scope::root("takeover", Some(2));
	jobs::build::run(manifest, None, &mut b.child("build"))?;
	jobs::takeover::run(
		manifest,
		login,
		provisioning,
		ssh_key,
		channel,
		yes,
		&mut b.child("takeover"),
	)
}

/// Check `takeover`'s prerequisites up front, before the build: the local build
/// tools ([`jobs::build::preflight`]) and the per-host over-SSH readiness checks
/// (podman/sudo present, a supported arch and adoptable layout). Run before
/// collecting secrets so a disqualified host fails fast rather than after a
/// multi-GB build. With `skip_build` the container build is skipped, so the build
/// tools aren't checked — but the takeover phase still needs a local `podman` to
/// assemble/push the image list.
///
/// # Errors
///
/// Returns an error listing every missing local prerequisite or ineligible host.
pub fn preflight(manifest: &Manifest, login: Option<&str>, skip_build: bool) -> Result<()> {
	if skip_build {
		return jobs::takeover::preflight(manifest, login);
	}
	jobs::build::preflight(manifest, None)?;
	jobs::takeover::host_preflight(manifest, login)
}
