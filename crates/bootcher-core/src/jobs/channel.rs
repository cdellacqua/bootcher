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

// ------------------------------------------------------------------ status

/// One arch of a channel's release, for `channel status` / `release list`.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchRow {
	pub(crate) arch: String,
	pub(crate) digest: String,
	pub(crate) base: oci::BaseStatus,
}

/// One `channel status` row (also its JSON; template fields match
/// case-insensitively: `{{ .Release }}`, `{{ .Commit }}`, `{{ .Stale }}`).
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChannelRow {
	pub(crate) channel: String,
	/// Whether `:ch` exists.
	pub(crate) published: bool,
	/// The release it serves (its `bootcher.release`).
	pub(crate) release: Option<String>,
	/// `:ch`'s digest.
	pub(crate) digest: Option<String>,
	/// `:X`'s digest.
	pub(crate) release_digest: Option<String>,
	/// The release's source commit.
	pub(crate) commit: Option<String>,
	/// Whether any member's base moved (or can't be read).
	pub(crate) stale: bool,
	/// Whether `:ch` isn't `:X` any more (a refresh or rebuild since the promote).
	pub(crate) lags: bool,
	/// `current`, `stale`, `lags`, … for the table.
	pub(crate) state: String,
	pub(crate) arches: Vec<ArchRow>,
}

/// Read one channel's status.
pub(crate) fn channel_row(
	manifest: &Manifest,
	channel: &str,
	cache: &mut BaseCache,
) -> Result<ChannelRow> {
	let mut row = ChannelRow {
		channel: channel.to_owned(),
		published: false,
		release: None,
		digest: None,
		release_digest: None,
		commit: None,
		stale: false,
		lags: false,
		state: "not published".to_owned(),
		arches: Vec::new(),
	};
	let Some(chan) = release::read(manifest, channel)? else { return Ok(row) };
	row.published = true;
	row.digest = Some(chan.index.digest.clone());
	row.release = chan.index.manifest.release().map(str::to_owned);
	row.commit = chan.source().ok().flatten();
	row.arches = chan
		.bases(cache)
		.into_iter()
		.zip(&chan.members)
		.map(|((arch, base), m)| ArchRow { arch: arch.to_string(), digest: m.digest.clone(), base })
		.collect();
	row.stale = row.arches.iter().any(|a| a.base.is_stale());
	if let Some(x) = &row.release {
		let r = manifest.tag_ref(x).context("a release ref")?;
		row.release_digest = oci::fetch(&r)?.map(|f| f.digest);
		row.lags = row.release_digest.as_deref() != Some(chan.index.digest.as_str());
	}
	let mut notes = Vec::new();
	if row.stale {
		notes.push("base stale");
	}
	if row.lags {
		notes.push(if row.release_digest.is_some() {
			"lags its release"
		} else {
			"its release is gone"
		});
	}
	row.state = if notes.is_empty() { "current".to_owned() } else { notes.join(", ") };
	Ok(row)
}

/// The channels a `channel status|refresh` run selects: the named ones (each must
/// be declared), else every declared one.
fn select_channels(manifest: &Manifest, named: &[String]) -> Result<Vec<String>> {
	if named.is_empty() {
		return Ok(manifest.channels().to_vec());
	}
	for ch in named {
		manifest.check_channel(ch)?;
	}
	Ok(named.to_vec())
}

/// `bootcher channel status [ch…]`: per channel, the release it serves, its source
/// commit, per-arch base current/stale, and whether it lags its release. A table by
/// default, `--format json` for any selection, or a template (`{{ .Stale }}`) when
/// exactly one channel is selected — which is what a scheduled CI refresh gates on.
/// Read-only.
///
/// # Errors
///
/// Returns an error if a channel isn't declared, the registry can't be read, or the
/// format is invalid.
pub fn status(manifest: &Manifest, channels: &[String], format: Option<&str>) -> Result<()> {
	manifest.require_registry("`channel status`")?;
	let mut checks = Checks::default();
	checks.bin("skopeo", oci::SKOPEO_HINT);
	checks.finish()?;
	let selected = select_channels(manifest, channels)?;
	let mut cache = BaseCache::default();
	let rows: Vec<ChannelRow> =
		selected.iter().map(|ch| channel_row(manifest, ch, &mut cache)).collect::<Result<_>>()?;
	let out = match format {
		None | Some("json") => crate::jobs::device::render_rows(
			&rows,
			format,
			&["channel", "release", "commit", "state"],
		)?,
		Some(tmpl) => match rows.as_slice() {
			[one] => crate::jobs::info::render_template(tmpl, &serde_json::to_value(one)?)?,
			_ => bail!(
				"a `--format` template needs exactly one channel (`bootcher channel status <channel> --format …`); use `--format json` for several"
			),
		},
	};
	println!("{out}");
	Ok(())
}

// ------------------------------------------------------------------ refresh

/// `bootcher channel refresh`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct RefreshArgs {
	pub channels: Vec<String>,
	pub exclude: Vec<String>,
	pub force: bool,
	pub skip_build: bool,
	pub remotes: Vec<String>,
	pub skip_remotes: bool,
	pub yes: bool,
}

/// The `channel refresh` checklist: every declared channel with its status,
/// pre-checking the stale or lagging ones.
pub(crate) fn refresh_choices(rows: &[ChannelRow]) -> Vec<Choice<String>> {
	rows.iter()
		.map(|r| {
			let what = r.release.as_deref().map_or_else(
				|| "nothing".to_owned(),
				|x| format!("{x} @ {}", r.commit.as_deref().map_or("no source", oci::short)),
			);
			let label = format!("{}: {what}, {}", r.channel, r.state);
			if r.published {
				Choice::new(label, r.channel.clone(), r.stale || r.lags)
			} else {
				Choice::disabled(label, r.channel.clone(), "not published")
			}
		})
		.collect()
}

/// One release a refresh run works on: its name, the channels (selected) serving
/// it and the channel digests read at the start.
#[derive(Debug)]
pub(crate) struct Group {
	pub(crate) release: String,
	pub(crate) channels: Vec<(String, String)>,
}

/// Group selected channel rows by the release they serve (skipping unpublished
/// channels and ones whose index names no release).
pub(crate) fn group_by_release(rows: &[ChannelRow]) -> (Vec<Group>, Vec<String>) {
	let mut groups: Vec<Group> = Vec::new();
	let mut skipped = Vec::new();
	for r in rows {
		match (&r.release, &r.digest) {
			(Some(x), Some(d)) => match groups.iter_mut().find(|g| g.release == *x) {
				Some(g) => g.channels.push((r.channel.clone(), d.clone())),
				None => groups.push(Group {
					release: x.clone(),
					channels: vec![(r.channel.clone(), d.clone())],
				}),
			},
			(None, Some(_)) => skipped
				.push(format!(":{} serves an index with no release name — skipped", r.channel)),
			_ => skipped.push(format!(":{} isn't published — skipped", r.channel)),
		}
	}
	(groups, skipped)
}

/// `bootcher channel refresh [ch…]`: base patches for channels. Per release the
/// selected channels serve: rebuild its stale arches (all with `--force`) from its
/// recorded commit under that commit's own config, re-push `:X` signed, then copy
/// `:X` to every selected channel serving it whose digest differs — re-checking each
/// channel first and skipping one a concurrent promote moved. Then the device step
/// for every selected channel (`--skip-remotes` skips it).
///
/// # Errors
///
/// Returns an error if a selection is invalid, the user declines, a rebuild/push
/// fails, or any device fails or is unreachable.
#[allow(clippy::too_many_lines)]
pub fn refresh(manifest: &Manifest, args: &RefreshArgs) -> Result<()> {
	manifest.require_registry("`channel refresh`")?;
	let mut checks = Checks::default();
	checks.bin("skopeo", oci::SKOPEO_HINT);
	checks.bin("podman", crate::preflight::PODMAN_HINT);
	if !args.skip_build {
		checks.bin("git", "check out each release's commit");
	}
	checks.finish()?;
	for ch in &args.exclude {
		manifest.check_channel(ch)?;
	}
	let mut cache = BaseCache::default();
	let mut prompted = false;
	let mut selected = select_channels(manifest, &args.channels)?;
	selected.retain(|c| !args.exclude.contains(c));
	let mut rows: Vec<ChannelRow> =
		selected.iter().map(|ch| channel_row(manifest, ch, &mut cache)).collect::<Result<_>>()?;
	if args.channels.is_empty() && interactive::is_tty() && !args.yes {
		prompted = true;
		let picked = interactive::checklist("Refresh which channels?", refresh_choices(&rows))?;
		rows.retain(|r| picked.contains(&r.channel));
	}
	let (groups, skipped) = group_by_release(&rows);
	for s in &skipped {
		eprintln!("{s}");
	}
	if args.skip_build && groups.len() > 1 {
		bail!(
			"`--skip-build` takes the local images for one release, but the selected channels 			 serve {} — name the channels of one release (`bootcher channel refresh <channel>`)",
			groups.iter().map(|g| g.release.as_str()).collect::<Vec<_>>().join(", ")
		);
	}

	// Plan each release: what to rebuild, what to copy.
	let mut plans = Vec::new();
	let mut lines = vec!["channel refresh will:".to_owned()];
	for group in groups {
		let release = release::read(manifest, &group.release)?;
		let Some(rel) = &release else {
			eprintln!(
				"warning: {} (served by {}) no longer exists — skipped",
				group.release,
				chan_list(&group)
			);
			continue;
		};
		let commit = rel.source().ok().flatten();
		let stale: Vec<_> = rel
			.bases(&mut cache)
			.into_iter()
			.filter(|(_, s)| args.force || s.is_stale())
			.map(|(a, _)| a)
			.collect();
		match (&commit, stale.is_empty()) {
			(None, false) => eprintln!(
				"warning: {} records no source commit, so it can't be rebuilt — skipped (publish a new release)",
				group.release
			),
			(Some(c), false) => lines.push(format!(
				"  rebuild {} ({}) at {} on the current base, re-push it",
				group.release,
				stale.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
				oci::short(c)
			)),
			(_, true) => {}
		}
		lines.push(format!("  point {} at {}", chan_list(&group), group.release));
		plans.push(Plan { group, release, commit, stale });
	}
	let targets = if args.skip_remotes {
		ChannelDevices::default()
	} else {
		let chans: Vec<String> =
			plans.iter().flat_map(|p| p.group.channels.iter().map(|(c, _)| c.clone())).collect();
		device::channel_devices(manifest, &chans, &args.remotes)?
	};
	lines.extend(
		targets.devices.iter().map(|(ch, d)| format!("  then update {} (on :{ch})", d.name())),
	);
	if plans.is_empty() {
		eprintln!("nothing to refresh");
		return Ok(());
	}
	if prompted {
		Equivalent::new(&["channel", "refresh"])
			.args(plans.iter().flat_map(|p| p.group.channels.iter().map(|(c, _)| c.clone())))
			.flag_if(args.force, "--force")
			.flag_if(args.skip_build, "--skip-build")
			.opts("--remote", &args.remotes)
			.flag_if(args.skip_remotes, "--skip-remotes")
			.arg("-y")
			.print();
	}
	interactive::confirm(args.yes, "`channel refresh` moves channels", &lines, "Proceed?", true)?;

	let mut job = Scope::standalone();
	let mut moved_channels: Vec<(String, String)> = Vec::new();
	for plan in plans {
		let name = plan.group.release.clone();
		let Some(mut rel) = plan.release else { continue };
		if let Some(commit) = &plan.commit
			&& !plan.stale.is_empty()
		{
			release::rebuild(
				manifest,
				&name,
				&rel,
				commit,
				&plan.stale,
				args.force,
				args.skip_build,
				&mut cache,
				&mut job,
			)?;
			rel = release::read(manifest, &name)?
				.with_context(|| format!("{name} vanished after its push"))?;
		}
		for (ch, seen) in &plan.group.channels {
			let ch_ref = manifest.tag_ref(ch).context("a channel ref")?;
			let now = oci::fetch(&ch_ref)?;
			if now.as_ref().map(|f| f.digest.as_str()) != Some(seen.as_str()) {
				eprintln!(
					":{ch} moved since this run read it (a concurrent promote?) — left alone"
				);
				continue;
			}
			move_channel(manifest, &name, &rel, ch, now.as_ref(), &mut job)?;
			moved_channels.push((ch.clone(), name.clone()));
		}
	}
	if args.skip_remotes {
		return Ok(());
	}
	let mut first_err = None;
	let ChannelDevices { devices, mut unreachable } = targets;
	for (ch, name) in &moved_channels {
		let on: Vec<_> = devices.iter().filter(|(c, _)| c == ch).cloned().collect();
		let t = ChannelDevices { devices: on, unreachable: std::mem::take(&mut unreachable) };
		if let Err(e) = device_step(manifest, ch, Some(name), t, &mut job) {
			eprintln!("{e:#}");
			first_err.get_or_insert(e);
		}
	}
	match first_err {
		Some(e) => Err(e),
		None => Ok(()),
	}
}

/// One release's part of a `channel refresh` run: what to rebuild, what to copy.
struct Plan {
	group: Group,
	release: Option<Published>,
	commit: Option<String>,
	stale: Vec<crate::context::Arch>,
}

/// `:a, :b` for a group's channels.
fn chan_list(group: &Group) -> String {
	group.channels.iter().map(|(c, _)| format!(":{c}")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn row(channel: &str, release: Option<&str>, stale: bool, lags: bool) -> ChannelRow {
		ChannelRow {
			channel: channel.into(),
			published: release.is_some(),
			release: release.map(str::to_owned),
			digest: release.map(|r| format!("sha256:{r}")),
			release_digest: None,
			commit: Some("0123456789abcdef".into()),
			stale,
			lags,
			state: "x".into(),
			arches: Vec::new(),
		}
	}

	#[test]
	fn refresh_checklist_prechecks_stale_or_lagging_channels() {
		let rows = vec![
			row("stable", Some("v1"), false, false),
			row("next", Some("v2"), true, false),
			row("beta", Some("v2"), false, true),
			row("edge", None, false, false),
		];
		let c = refresh_choices(&rows);
		assert_eq!(c.iter().map(|c| c.checked).collect::<Vec<_>>(), [false, true, true, false]);
		assert!(c[3].disabled.is_some());
		assert_eq!(c[0].label, "stable: v1 @ 0123456789ab, x");
	}

	#[test]
	fn channels_group_by_the_release_they_serve() {
		let rows = vec![
			row("stable", Some("v1"), false, false),
			row("next", Some("v2"), false, false),
			row("beta", Some("v2"), false, false),
			row("edge", None, false, false),
		];
		let (groups, skipped) = group_by_release(&rows);
		assert_eq!(groups.len(), 2);
		assert_eq!(groups[1].release, "v2");
		assert_eq!(
			groups[1].channels.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
			["next", "beta"]
		);
		assert_eq!(skipped, [":edge isn't published — skipped"]);
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
		let mut m = oci::Manifest::default();
		m.annotations.insert(crate::context::RELEASE_ANNOTATION.into(), "stable".into());
		let f = oci::Fetched { digest: "sha256:x".into(), manifest: m };
		assert!(check_not_a_release("stable", Some(&f)).is_err());
		assert!(check_not_a_release("next", Some(&f)).is_ok());
		assert!(check_not_a_release("stable", None).is_ok());
	}
}
