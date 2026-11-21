//! `bootcher channel …`: the tags devices follow.
//!
//! A channel `:ch` is declared in `[registry] channels` and is always a
//! byte-for-byte copy of some release `:X`'s index, so its entries carry the
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
	/// `--from <channel>`: promote the release that channel serves, in place of
	/// [`Self::release`].
	pub from: Option<String>,
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

/// `--from src` onto `target`: a channel can't be promoted from itself.
///
/// # Errors
///
/// Returns an error when `src == target`.
pub(crate) fn check_not_source(src: &str, target: &str) -> Result<()> {
	if src == target {
		bail!("`--from {src}` names the channel being moved — promote from another channel");
	}
	Ok(())
}

/// `--from src`: `:src` must still be the build of `name` it was promoted as —
/// `channel_digest` equal to the release's current `release_digest`. A source that
/// lags (the release was refreshed since) would hand the target a build none of
/// `:src`'s devices ran, so it's refused rather than promoted.
///
/// # Errors
///
/// Returns an error when the digests differ.
pub(crate) fn check_source_current(
	src: &str,
	name: &str,
	channel_digest: &str,
	release_digest: &str,
	target: Option<&str>,
) -> Result<()> {
	if channel_digest != release_digest {
		let named = target.map_or_else(
			|| format!("`bootcher channel promote {name} <channel>`"),
			|t| format!("`bootcher channel promote {name} {t}`"),
		);
		bail!(
			":{src} lags {name}: {name} was rebuilt ({}) since :{src} got it ({}), so :{src}'s \
			 devices haven't run what {name} is now. Promote {name} to {src} first \
			 (`bootcher channel promote {name} {src}`), or name the release explicitly ({named}).",
			oci::short(release_digest),
			oci::short(channel_digest),
		);
	}
	Ok(())
}

/// Resolve `--from src` to the release `:src` serves (by its `bootcher.release`
/// annotation, as `channel status` reads it) and that release as published, refusing
/// an undeclared or empty source and one that lags its release
/// ([`check_source_current`]).
fn resolve_from(
	manifest: &Manifest,
	src: &str,
	target: Option<&str>,
) -> Result<(String, Published)> {
	manifest.check_channel(src)?;
	let src_ref = manifest.tag_ref(src).context("a channel ref")?;
	let chan = oci::fetch(&src_ref)?
		.with_context(|| format!(":{src} serves nothing yet — promote a release to it first"))?;
	let name = chan
		.manifest
		.release()
		.with_context(|| {
			format!(":{src} carries no `bootcher.release` annotation — name the release explicitly")
		})?
		.to_owned();
	let release = release::read(manifest, &name)?.with_context(|| {
		format!(
			":{src} serves {name}, but :{name} isn't published any more — name a release explicitly"
		)
	})?;
	check_source_current(src, &name, &chan.digest, &release.index.digest, target)?;
	Ok((name, release))
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
	// Copy the digest that was checked, not the tag: a tag moved between the checks and
	// the copy must not slip an unchecked image onto the channel.
	let src = format!("{}@{}", manifest.repo().context("a registry")?, release.index.digest);
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
	if let (Some(src), Some(target)) = (&args.from, &args.channel) {
		check_not_source(src, target)?;
	}
	// `--from`: the release and its index are resolved (and checked) together.
	let mut resolved = None;
	let name = match (&args.from, &args.release) {
		(Some(src), _) => {
			let (name, published) = resolve_from(manifest, src, args.channel.as_deref())?;
			eprintln!(":{src} serves {name}");
			resolved = Some(published);
			name
		}
		(None, Some(r)) => r.clone(),
		(None, None) if interactive::is_tty() => {
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
		(None, None) => bail!(
			"`channel promote` needs the release and the channel (`bootcher channel promote v1.4 \
			 stable`, or `bootcher channel promote --from next stable`)"
		),
	};
	let channel = match &args.channel {
		Some(c) => c.clone(),
		None if interactive::is_tty() => {
			prompted = true;
			let serving: Vec<(String, Option<String>)> = manifest
				.channels()
				.iter()
				.filter(|c| args.from.as_ref() != Some(*c))
				.map(|c| {
					let serves = manifest
						.tag_ref(c)
						.and_then(|r| oci::fetch(&r).ok().flatten())
						.and_then(|f| f.manifest.release().map(str::to_owned));
					(c.clone(), serves)
				})
				.collect();
			if serving.is_empty() {
				bail!("no channel to promote {name} to besides the one it comes from");
			}
			interactive::select(
				&format!("Promote {name} to which channel?"),
				&channel_choices(&serving),
			)?
		}
		None => match &args.from {
			Some(src) => bail!(
				"`channel promote` needs the channel (`bootcher channel promote --from {src} <channel>`)"
			),
			None => bail!(
				"`channel promote` needs the channel (`bootcher channel promote {name} <channel>`)"
			),
		},
	};
	manifest.check_channel(&channel)?;
	if let Some(src) = &args.from {
		check_not_source(src, &channel)?;
	}
	let release = match resolved {
		Some(r) => r,
		None => release::read(manifest, &name)?.with_context(|| {
			format!("{name} isn't published — publish it first (`bootcher release publish {name}`)")
		})?,
	};
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
	let what = args.from.as_ref().map_or_else(String::new, |s| format!(", what :{s} serves"));
	let mut lines = vec![format!(
		"channel promote will point :{channel} at {name}{what} (it serves {from} now)"
	)];
	if args.skip_remotes {
		lines.push("  devices following it pick it up on their next auto-update".to_owned());
	} else {
		lines.extend(targets.devices.iter().map(|(_, d)| format!("  then update {}", d.name())));
	}
	if prompted {
		let head = match &args.from {
			Some(src) => Equivalent::new(&["channel", "promote", "--from", src, &channel]),
			None => Equivalent::new(&["channel", "promote", &name, &channel]),
		};
		head.flag_if(allow_stale, "--allow-stale-base")
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
	fn from_refuses_its_own_channel() {
		assert!(check_not_source("next", "stable").is_ok());
		let err = check_not_source("next", "next").unwrap_err().to_string();
		assert!(err.contains("names the channel being moved"), "{err}");
	}

	#[test]
	fn from_refuses_a_source_that_lags_its_release() {
		let (old, new) = ("sha256:aaaaaaaaaaaaaaaaaaaa", "sha256:bbbbbbbbbbbbbbbbbbbb");
		assert!(check_source_current("next", "v1.4", old, old, Some("stable")).is_ok());
		let err =
			check_source_current("next", "v1.4", old, new, Some("stable")).unwrap_err().to_string();
		assert!(err.contains(":next lags v1.4"), "{err}");
		assert!(err.contains("bootcher channel promote v1.4 next"), "{err}");
		assert!(err.contains("bootcher channel promote v1.4 stable"), "{err}");
		// The target isn't known yet (picked on a TTY afterwards).
		let err = check_source_current("next", "v1.4", old, new, None).unwrap_err().to_string();
		assert!(err.contains("bootcher channel promote v1.4 <channel>"), "{err}");
	}

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
		let m: oci::Manifest = serde_json::from_str(
			r#"{"manifests":[{"digest":"sha256:a","annotations":{"bootcher.release":"stable"}}]}"#,
		)
		.unwrap();
		let f = oci::Fetched { digest: "sha256:x".into(), manifest: m };
		assert!(check_not_a_release("stable", Some(&f)).is_err());
		assert!(check_not_a_release("next", Some(&f)).is_ok());
		assert!(check_not_a_release("stable", None).is_ok());
	}
}
