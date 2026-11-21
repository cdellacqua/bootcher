//! `bootcher channel …`: the tags devices follow.
//!
//! A channel `:ch` is declared in `[registry] channels` and is always a
//! byte-for-byte copy of some release `:X`'s index, so it carries the
//! `bootcher.release` annotation too. Only `channel promote` and the refresh
//! commands write channels, and every channel move is followed by the channel
//! device step for the devices on that channel (unless `--skip-remotes`).

use crate::context::Manifest;
use crate::hooks::{HookMetadata, Phase, Stage};
use crate::interactive::{self, Choice, Equivalent, Prompted};
use crate::jobs::device::{self, ChannelDevices};
use crate::jobs::release::{self, Published};
use crate::oci::{self, BaseCache};
use crate::preflight::Checks;
use crate::progress::Scope;
use crate::ssh::Ssh;
use anyhow::{Context, Result, bail};

/// `bootcher channel promote`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PromoteArgs {
	pub release: Option<String>,
	pub channel: Option<String>,
	pub allow_stale_base: bool,
	pub remotes: Vec<String>,
	pub skip_remotes: bool,
	pub yes: bool,
}

/// The `promote` channel select: each declared channel, annotated with what it
/// serves now.
pub(crate) fn channel_choices(channels: &[(String, Option<String>)]) -> Vec<Choice<String>> {
	channels
		.iter()
		.map(|(ch, serves)| {
			let note = serves
				.as_deref()
				.map_or_else(|| "not published yet".to_owned(), |r| format!("serves {r}"));
			Choice::new(format!("{ch}: {note}"), ch.clone(), false)
		})
		.collect()
}

/// What a stale base does at promote time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaleChoice {
	Cancel,
	PromoteAnyway,
}

/// The stale-base choices: "cancel" pre-selected (the non-TTY default refuses).
pub(crate) fn stale_choices() -> Vec<Choice<StaleChoice>> {
	vec![
		Choice::new(
			"cancel (refresh it first: `bootcher release refresh <release>`)",
			StaleChoice::Cancel,
			true,
		),
		Choice::new("promote anyway", StaleChoice::PromoteAnyway, false),
	]
}

/// Refuse to overwrite a channel whose index claims to be a release literally named
/// like the channel (a release published under that name before the channel was
/// declared): moving it would lose that release.
///
/// # Errors
///
/// Returns an error in that case.
pub(crate) fn check_not_a_release(channel: &str, current: Option<&oci::Fetched>) -> Result<()> {
	if current.and_then(|c| c.manifest.release()) == Some(channel) {
		bail!(
			"`:{channel}` holds a release published *as* `{channel}` — republish it under another \
			 name first, so promoting doesn't overwrite it"
		);
	}
	Ok(())
}

/// The signing check: with signing on, `:X`'s index and every member must carry a
/// signature the configured key verifies.
fn check_signed(manifest: &Manifest, name: &str, release: &Published) -> Result<()> {
	let Some(signing) = manifest.signing() else { return Ok(()) };
	let repo = manifest.repo().context("a registry")?;
	let pub_path = manifest.root().join(signing.public_key_path());
	let pem = std::fs::read_to_string(&pub_path)
		.with_context(|| format!("reading the signing public key {}", pub_path.display()))?;
	let missing = oci::unsigned_digests(&repo, &release.index, &pem)?;
	if !missing.is_empty() {
		bail!(
			"{name} isn't signed by the configured key ({}): {} — a release signed only by an old \
			 key needs `bootcher release refresh {name} --force` (re-signed with the current key), \
			 or `bootcher signing rotate` (which signs the releases channels serve)",
			signing.public_key_path(),
			missing.iter().map(|d| oci::short(d)).collect::<Vec<_>>().join(", ")
		);
	}
	Ok(())
}

/// Copy `:X` (at `digest`) onto `:ch` unless the channel is already there, bracketed
/// by `[hooks.promote]`. Returns whether it moved.
///
/// # Errors
///
/// Returns an error if a hook or the copy fails.
pub(crate) fn move_channel(
	manifest: &Manifest,
	name: &str,
	release: &Published,
	channel: &str,
	previous: Option<&oci::Fetched>,
	job: &mut Scope,
) -> Result<bool> {
	let ch_ref = manifest.tag_ref(channel).context("a channel ref")?;
	if previous.is_some_and(|p| p.digest == release.index.digest) {
		eprintln!(
			":{channel} already serves {name} ({}) — not moved",
			oci::short(&release.index.digest)
		);
		return Ok(false);
	}
	let mut meta = HookMetadata::new(manifest, Phase::Promote, ch_ref.clone());
	meta.release = Some(name.to_owned());
	meta.channel = Some(channel.to_owned());
	meta.previous_release = previous.and_then(|p| p.manifest.release()).map(str::to_owned);
	meta.revision = release.source().ok().flatten();
	let hooks = manifest.hooks();
	crate::hooks::run(&meta, hooks.promote.pre.as_deref(), job)?;
	let src = manifest.tag_ref(name).context("a release ref")?;
	oci::copy(&src, &ch_ref, true, None, job)?;
	match &meta.previous_release {
		Some(prev) => eprintln!(
			":{channel} moved {prev} → {name} (roll back with `bootcher channel promote {prev} {channel}`)"
		),
		None => eprintln!(":{channel} now serves {name}"),
	}
	meta.stage = Stage::Post;
	crate::hooks::run(&meta, hooks.promote.post.as_deref(), job)?;
	Ok(true)
}

/// The channel device step for the devices on `channel` (all of `targets`), with
/// `release` in the hook metadata.
pub(crate) fn device_step(
	manifest: &Manifest,
	channel: &str,
	release: Option<&str>,
	targets: ChannelDevices,
	job: &mut Scope,
) -> Result<()> {
	let ssh: Vec<Ssh> =
		targets.devices.iter().filter(|(c, _)| c == channel).map(|(_, d)| d.ssh.clone()).collect();
	if ssh.is_empty() && targets.unreachable.is_empty() {
		return Ok(());
	}
	device::channel_rollout(
		manifest,
		channel,
		&ssh,
		targets.unreachable,
		release.map(str::to_owned),
		&|_, _| Ok(()),
		job,
	)
}

/// `bootcher channel promote X ch`: point `:ch` at `:X` (a digest-preserving copy;
/// no write when it's already there), then run the channel device step for the
/// devices on `ch` — always, even when the channel didn't move, so a re-run retries
/// the devices a failed one missed. Refuses an unsigned `:X` (with signing on) and a
/// stale base (unless `--allow-stale-base`). Needs neither the signing key nor a
/// build environment.
///
/// # Errors
///
/// Returns an error if a check refuses, the user declines, the copy fails, or any
/// device fails or is unreachable.
#[allow(clippy::too_many_lines)]
pub fn promote(manifest: &Manifest, args: &PromoteArgs) -> Result<()> {
	manifest.require_registry("`channel promote`")?;
	let mut checks = Checks::default();
	checks.bin("skopeo", oci::SKOPEO_HINT);
	if !args.skip_remotes && !manifest.remotes().is_empty() {
		checks.bin("ssh", "reach the devices over SSH");
	}
	checks.finish()?;
	let mut prompted = false;
	let name = match &args.release {
		Some(r) => r.clone(),
		None if interactive::is_tty() => {
			prompted = true;
			let names: Vec<String> =
				release::releases(manifest)?.into_iter().map(|(t, _)| t).collect();
			let suggest = move |input: &str| -> Result<Vec<String>, inquire::CustomUserError> {
				Ok(names.iter().filter(|n| n.contains(input)).cloned().collect())
			};
			inquire::Text::new("Release to promote:")
				.with_autocomplete(suggest)
				.prompt()
				.prompted()?
				.trim()
				.to_owned()
		}
		None => bail!(
			"`channel promote` needs the release and the channel (`bootcher channel promote v1.4 stable`)"
		),
	};
	let channel = match &args.channel {
		Some(c) => c.clone(),
		None if interactive::is_tty() => {
			prompted = true;
			let serving: Vec<(String, Option<String>)> = manifest
				.channels()
				.iter()
				.map(|c| {
					let serves = manifest
						.tag_ref(c)
						.and_then(|r| oci::fetch(&r).ok().flatten())
						.and_then(|f| f.manifest.release().map(str::to_owned));
					(c.clone(), serves)
				})
				.collect();
			interactive::select(
				&format!("Promote {name} to which channel?"),
				&channel_choices(&serving),
			)?
		}
		None => bail!(
			"`channel promote` needs the channel (`bootcher channel promote {name} <channel>`)"
		),
	};
	manifest.check_channel(&channel)?;
	let release = release::read(manifest, &name)?.with_context(|| {
		format!("{name} isn't published — publish it first (`bootcher release publish {name}`)")
	})?;
	let ch_ref = manifest.tag_ref(&channel).context("a channel ref")?;
	let current = oci::fetch(&ch_ref)?;
	check_not_a_release(&channel, current.as_ref())?;
	check_signed(manifest, &name, &release)?;

	// The strict base check: every member on the base's current digest.
	let mut cache = BaseCache::default();
	let stale: Vec<String> = release
		.bases(&mut cache)
		.into_iter()
		.filter(|(_, s)| s.is_stale())
		.map(|(a, s)| format!("{a}: {}", s.describe()))
		.collect();
	let mut allow_stale = args.allow_stale_base;
	if !stale.is_empty() && !allow_stale {
		eprintln!("{name} isn't on the current base — {}", stale.join("; "));
		if interactive::is_tty() && !args.yes {
			prompted = true;
			if interactive::select("Promote it anyway?", &stale_choices())? == StaleChoice::Cancel {
				bail!("not promoted — refresh it first: `bootcher release refresh {name}`");
			}
			allow_stale = true;
		} else {
			bail!(
				"refusing to promote {name}: its base moved (or can't be read) — refresh it first \
				 (`bootcher release refresh {name}`), or pass `--allow-stale-base`"
			);
		}
	}

	let targets = if args.skip_remotes {
		ChannelDevices::default()
	} else {
		device::channel_devices(manifest, std::slice::from_ref(&channel), &args.remotes)?
	};
	let from = current.as_ref().and_then(|c| c.manifest.release()).unwrap_or("nothing");
	let mut lines =
		vec![format!("channel promote will point :{channel} at {name} (it serves {from} now)")];
	if args.skip_remotes {
		lines.push("  devices following it pick it up on their next auto-update".to_owned());
	} else {
		lines.extend(targets.devices.iter().map(|(_, d)| format!("  then update {}", d.name())));
	}
	if prompted {
		Equivalent::new(&["channel", "promote", &name, &channel])
			.flag_if(allow_stale, "--allow-stale-base")
			.opts("--remote", &args.remotes)
			.flag_if(args.skip_remotes, "--skip-remotes")
			.arg("-y")
			.print();
	}
	interactive::confirm(args.yes, "`channel promote` moves a channel", &lines, "Proceed?", true)?;

	let mut job = Scope::standalone();
	move_channel(manifest, &name, &release, &channel, current.as_ref(), &mut job)?;
	if args.skip_remotes {
		return Ok(());
	}
	device_step(manifest, &channel, Some(&name), targets, &mut job)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn channel_choices_annotate_what_each_serves() {
		let c = channel_choices(&[("stable".into(), Some("v1.3".into())), ("next".into(), None)]);
		assert_eq!(c[0].label, "stable: serves v1.3");
		assert_eq!(c[1].label, "next: not published yet");
		assert!(c.iter().all(|c| !c.checked));
	}

	#[test]
	fn a_stale_base_preselects_cancel() {
		let c = stale_choices();
		assert_eq!(c[interactive::start_cursor(&c).unwrap()].value, StaleChoice::Cancel);
	}

	#[test]
	fn a_channel_claiming_to_be_its_own_release_is_refused() {
		let mut m = oci::Manifest::default();
		m.annotations.insert(crate::context::RELEASE_ANNOTATION.into(), "stable".into());
		let f = oci::Fetched { digest: "sha256:x".into(), manifest: m };
		assert!(check_not_a_release("stable", Some(&f)).is_err());
		assert!(check_not_a_release("next", Some(&f)).is_ok());
		assert!(check_not_a_release("stable", None).is_ok());
	}
}
