//! `bootcher device …`: everything that reaches the configured devices.
//!
//! Every device step first reads each device's `spec.image` over SSH, classifies it
//! (channel, direct, foreign, unreachable) and acts per class:
//!
//! | Command | Default targets | `--remote` |
//! |---|---|---|
//! | `device deploy` | direct devices | restricts within them; a non-direct one is an error |
//! | `device refresh` | direct devices whose base is stale | restricts within direct devices |
//! | `device switch` | none (TTY checklist; non-TTY requires `--remote`) | the devices to move |
//! | `device takeover` | none (TTY checklist; non-TTY requires `--remote`) | the hosts to convert |
//! | `device rotate ssh-key` | every reachable remote | the subset |
//! | `device rotate pull-token` | channel devices | the subset |
//!
//! Skipped devices are logged with their class. Unreachable devices never fail
//! silently and never block the others: they're listed, the reachable ones are
//! attempted, and the run exits non-zero afterwards. Only `device switch` ever
//! moves a device between channels or to/from direct.

use crate::context::{Arch, DiskType, Manifest, RemoteConfig};
use crate::device::{self, Class, Device};
use crate::fleet::Outcome;
use crate::hooks::{HookMetadata, Phase};
use crate::interactive::{self, Choice, Equivalent, Gate, Prompted, gate};
use crate::jobs::secrets::{InstallMode, Provisioning, TrustFiles};
use crate::jobs::{build, disk, rotate, takeover, upgrade};
use crate::preflight::Checks;
use crate::progress::Scope;
use crate::ssh::Ssh;
use crate::{oci, preflight};
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// The mode a device command acts in, from `--channel ch` / `--direct`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
	/// Follow (or move to) the declared channel.
	Channel(String),
	/// Updated over SSH from this machine.
	Direct,
}

impl std::fmt::Display for Mode {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Mode::Channel(ch) => write!(f, ":{ch}"),
			Mode::Direct => f.write_str("direct"),
		}
	}
}

impl Mode {
	/// The flags that select this mode.
	fn flags(&self) -> Vec<String> {
		match self {
			Mode::Channel(ch) => vec!["--channel".to_owned(), ch.clone()],
			Mode::Direct => vec!["--direct".to_owned()],
		}
	}
}

/// The options offered when a mode isn't given on a TTY: every declared channel
/// (annotated with what it serves) plus "direct". `annotations` maps a channel to
/// its note (`v1.3`, `not published yet`). `preselect` is the non-TTY default, if
/// any (none here: off a TTY a mode is required).
pub(crate) fn mode_choices(
	channels: &[String],
	annotations: &[(String, String)],
) -> Vec<Choice<Mode>> {
	let mut out: Vec<Choice<Mode>> = channels
		.iter()
		.map(|ch| {
			let note = annotations
				.iter()
				.find(|(c, _)| c == ch)
				.map_or_else(String::new, |(_, n)| format!(": {n}"));
			Choice::new(format!("channel {ch}{note}"), Mode::Channel(ch.clone()), false)
		})
		.collect();
	out.push(Choice::new(
		"direct (built here, updated with `bootcher device deploy`)",
		Mode::Direct,
		false,
	));
	out
}

/// A short note on what each declared channel serves, read from the registry
/// (best effort: an unreadable channel just says so).
fn channel_notes(manifest: &Manifest) -> Vec<(String, String)> {
	manifest
		.channels()
		.iter()
		.map(|ch| {
			let note = match manifest.tag_ref(ch).map(|r| oci::fetch(&r)) {
				Some(Ok(Some(f))) => {
					f.manifest.release().map_or_else(|| "published".to_owned(), str::to_owned)
				}
				Some(Ok(None)) => "not published yet".to_owned(),
				_ => "unreadable".to_owned(),
			};
			(ch.clone(), note)
		})
		.collect()
}

/// Resolve the install mode of `device provision`/`takeover`: without a registry
/// `--direct` is implied (and `--channel` an error); with one, the flag, a TTY
/// select, or an error naming the flags. Returns the mode and whether it was
/// prompted for.
fn resolve_mode(manifest: &Manifest, flag: Option<Mode>, what: &str) -> Result<(Mode, bool)> {
	let Some(_) = manifest.registry() else {
		if let Some(Mode::Channel(ch)) = flag {
			bail!(
				"`--channel {ch}` needs a registry, but bootcher.toml has no `[registry]` — without \
				 one every device is direct (`--direct` is implied)"
			);
		}
		return Ok((Mode::Direct, false));
	};
	match flag {
		Some(Mode::Channel(ch)) => {
			manifest.check_channel(&ch)?;
			Ok((Mode::Channel(ch), false))
		}
		Some(Mode::Direct) => Ok((Mode::Direct, false)),
		None if interactive::is_tty() => {
			let choices = mode_choices(manifest.channels(), &channel_notes(manifest));
			Ok((
				interactive::select(&format!("How should {what} receive updates?"), &choices)?,
				true,
			))
		}
		None => bail!(
			"{what}: pass `--channel <{}>` to follow a channel, or `--direct` to update it with \
			 `bootcher device deploy`",
			manifest.channels().join("|")
		),
	}
}

/// Check the remotes list is non-empty for a command that needs devices.
fn require_remotes(manifest: &Manifest, cmd: &str) -> Result<()> {
	if manifest.remotes().is_empty() {
		bail!("`{cmd}` reaches the devices in `remotes`, but bootcher.toml lists none");
	}
	Ok(())
}

/// The remotes a command probes: the `--remote` subset, or all of them.
fn candidates<'m>(manifest: &'m Manifest, names: &[String]) -> Result<Vec<&'m RemoteConfig>> {
	Ok(device::select_remotes(manifest, names)?
		.unwrap_or_else(|| manifest.remotes().iter().collect()))
}

/// Probe `remotes`, logging each unreachable one, and split them into the reachable
/// devices and failure outcomes for the unreachable.
fn probe(manifest: &Manifest, remotes: &[&RemoteConfig]) -> (Vec<Device>, Vec<Outcome>) {
	let owned: Vec<RemoteConfig> = remotes.iter().map(|r| (*r).clone()).collect();
	let mut reachable = Vec::new();
	let mut unreachable = Vec::new();
	for d in device::probe_all(manifest, &owned) {
		if let Class::Unreachable(why) = &d.class {
			eprintln!("{}: unreachable ({why})", d.name());
			unreachable.push(Outcome::failed(d.name(), format!("unreachable: {why}")));
		} else {
			reachable.push(d);
		}
	}
	(reachable, unreachable)
}

/// Raise a run that attempted nothing but has unreachable devices to report.
pub(crate) fn finish_unreachable(unreachable: &[Outcome]) -> Result<()> {
	if unreachable.is_empty() {
		return Ok(());
	}
	let names: Vec<&str> = unreachable.iter().map(|o| o.host.as_str()).collect();
	bail!("{} device(s) unreachable: {}", names.len(), names.join(", "))
}

/// Ask the TTY which devices to act on (or confirm a `--remote` selection), per the
/// `-y` rules: `-y` proceeds; a TTY with no `--remote` gets `choices` as a checklist
/// (which doubles as the confirmation); a TTY with `--remote` gets a yes/no listing
/// `lines`; a non-TTY without `-y` fails naming `what`. Returns the picked values,
/// and whether a prompt ran.
fn pick<T: Clone>(
	yes: bool,
	explicit: bool,
	what: &str,
	question: &str,
	choices: Vec<Choice<T>>,
	lines: &[String],
) -> Result<(Vec<T>, bool)> {
	let checked = || {
		choices
			.iter()
			.filter(|c| c.checked && c.disabled.is_none())
			.map(|c| c.value.clone())
			.collect()
	};
	match gate(yes, interactive::is_tty()) {
		Gate::Proceed => Ok((checked(), false)),
		Gate::Bail => {
			bail!("{what}, and stdin is not a TTY to confirm — re-run with -y/--yes to proceed")
		}
		Gate::Prompt if explicit => {
			interactive::confirm(false, what, lines, "Proceed?", true)?;
			Ok((checked(), true))
		}
		Gate::Prompt => Ok((interactive::checklist(question, choices)?, true)),
	}
}

// ------------------------------------------------------------------ device status

/// One `device status` row (also its `--format json` element; template fields
/// match case-insensitively).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusRow {
	pub(crate) device: String,
	/// `channel`, `direct`, `foreign` or `unreachable`.
	pub(crate) class: &'static str,
	pub(crate) channel: Option<String>,
	/// The origin the next upgrade follows (`spec.image`).
	pub(crate) origin: Option<String>,
	pub(crate) transport: Option<String>,
	pub(crate) signature: Option<String>,
	pub(crate) arch: Option<String>,
	pub(crate) booted: Option<String>,
	pub(crate) staged: Option<String>,
	/// The booted image's `version` label.
	pub(crate) version: Option<String>,
	/// The release the booted digest belongs to (channel devices).
	pub(crate) release: Option<String>,
	/// Whether the device lags its channel (channel devices).
	pub(crate) lags: Option<bool>,
	/// A human summary of the state.
	pub(crate) state: String,
	pub(crate) error: Option<String>,
}

/// What the registry says about one channel: its index digest, release, and the
/// member digest per arch.
#[derive(Clone, Debug, Default)]
struct ChannelView {
	release: Option<String>,
	members: Vec<(Arch, String)>,
}

/// Build the status row for `d`, given the current channel views and a lookup from
/// member digest to the release it belongs to.
fn status_row(
	d: &Device,
	channels: &[(String, Option<ChannelView>)],
	release_of: &dyn Fn(&str) -> Option<String>,
) -> StatusRow {
	let status = d.status();
	let spec = status.and_then(|s| s.spec.as_ref());
	let booted = status.and_then(|s| s.booted.as_ref());
	let staged = status.and_then(|s| s.staged.as_ref());
	let mut row = StatusRow {
		device: d.name().to_owned(),
		class: match d.class {
			Class::Channel(_) => "channel",
			Class::Direct => "direct",
			Class::Foreign(_) => "foreign",
			Class::Unreachable(_) => "unreachable",
		},
		channel: None,
		origin: spec.map(|s| s.image.clone()),
		transport: spec.map(|s| s.transport.clone()),
		signature: spec.and_then(|s| s.signature.clone()),
		arch: status.and_then(|s| s.arch).map(|a| a.to_string()),
		booted: booted.map(|b| b.digest.clone()),
		staged: staged.map(|s| s.digest.clone()),
		version: booted.and_then(|b| b.version.clone()),
		release: None,
		lags: None,
		state: String::new(),
		error: None,
	};
	row.state = match &d.class {
		Class::Unreachable(why) => {
			row.error = Some(why.clone());
			"unreachable".to_owned()
		}
		Class::Foreign(what) => format!("foreign: follows {what}"),
		Class::Direct => match staged {
			Some(_) => "direct, update staged (reboot pending)".to_owned(),
			None => "direct".to_owned(),
		},
		Class::Channel(ch) => {
			row.channel = Some(ch.clone());
			let booted_digest = booted.map(|b| b.digest.as_str()).unwrap_or_default();
			row.release = release_of(booted_digest);
			match channels.iter().find(|(c, _)| c == ch).and_then(|(_, v)| v.as_ref()) {
				None => format!("on :{ch} (channel not published or unreadable)"),
				Some(view) => {
					let current = view.members.iter().any(|(_, dg)| dg == booted_digest);
					let staged_current =
						staged.is_some_and(|s| view.members.iter().any(|(_, dg)| *dg == s.digest));
					row.lags = Some(!current);
					let serves = view.release.as_deref().unwrap_or("an unnamed release");
					if current {
						format!("current on :{ch} ({serves})")
					} else if staged_current {
						format!("lags :{ch} — {serves} staged, reboot pending")
					} else if row.release.is_none() {
						// A fresh cross-arch disk boots a lock-layer derivative that no
						// published member matches; it catches up on its first update.
						format!("on :{ch}, provisioned, not yet updated (channel serves {serves})")
					} else {
						format!("lags :{ch} (channel serves {serves})")
					}
				}
			}
		}
	};
	row
}

/// `bootcher device status`: what each remote runs. Read-only; unreachable devices
/// are a row, not a failure. `format` is `json`, a template applied to each row,
/// or `None` for a table.
///
/// # Errors
///
/// Returns an error if there are no remotes, a `--remote` is unknown, or the
/// format is invalid.
pub fn status(manifest: &Manifest, remotes: &[String], format: Option<&str>) -> Result<()> {
	require_remotes(manifest, "device status")?;
	let mut checks = Checks::default();
	checks.bin("ssh", "reach the devices over SSH");
	checks.finish()?;
	let picked = candidates(manifest, remotes)?;
	let owned: Vec<RemoteConfig> = picked.iter().map(|r| (*r).clone()).collect();
	let devices = device::probe_all(manifest, &owned);

	// Registry reads only when a channel device needs them, and only with skopeo.
	let any_channel = devices.iter().any(|d| matches!(d.class, Class::Channel(_)));
	let can_read = any_channel && preflight::binary_available("skopeo");
	if any_channel && !can_read {
		eprintln!("warning: skopeo isn't installed, so channel devices' releases aren't shown");
	}
	let channels: Vec<(String, Option<ChannelView>)> = if can_read {
		manifest
			.channels()
			.iter()
			.map(|ch| {
				let view =
					manifest.tag_ref(ch).and_then(|r| oci::fetch(&r).ok().flatten()).map(|f| {
						ChannelView {
							release: f.manifest.release().map(str::to_owned),
							members: f.manifest.members(),
						}
					});
				(ch.clone(), view)
			})
			.collect()
	} else {
		Vec::new()
	};
	// Digests the channels serve map straight to their release; any other digest is
	// looked up across the published releases, lazily (one registry pass).
	let releases = std::cell::OnceCell::<Vec<(String, Vec<String>)>>::new();
	let release_of = |digest: &str| -> Option<String> {
		if digest.is_empty() || !can_read {
			return None;
		}
		for (_, view) in &channels {
			if let Some(v) = view
				&& v.members.iter().any(|(_, dg)| dg == digest)
			{
				return v.release.clone();
			}
		}
		let all = releases
			.get_or_init(|| crate::jobs::release::member_digests(manifest).unwrap_or_default());
		all.iter().find(|(_, ds)| ds.iter().any(|d| d == digest)).map(|(r, _)| r.clone())
	};
	let rows: Vec<StatusRow> =
		devices.iter().map(|d| status_row(d, &channels, &release_of)).collect();
	println!(
		"{}",
		render_rows(&rows, format, &["device", "class", "arch", "release", "state", "version"])?
	);
	Ok(())
}

/// Render rows as `json` (an array), a per-row template, or a table of `columns`.
pub(crate) fn render_rows<T: Serialize>(
	rows: &[T],
	format: Option<&str>,
	columns: &[&str],
) -> Result<String> {
	let values: Vec<serde_json::Value> = rows
		.iter()
		.map(serde_json::to_value)
		.collect::<Result<_, _>>()
		.context("serializing rows")?;
	match format {
		Some("json") => Ok(serde_json::to_string_pretty(&values)?),
		Some(tmpl) => Ok(values
			.iter()
			.map(|v| crate::jobs::info::render_template(tmpl, v))
			.collect::<Result<Vec<_>>>()?
			.join("\n")),
		None => Ok(table(&values, columns)),
	}
}

/// A left-aligned text table of `columns` (upper-cased headers) over `rows`.
pub(crate) fn table(rows: &[serde_json::Value], columns: &[&str]) -> String {
	let cell = |v: &serde_json::Value, c: &str| match &v[c] {
		serde_json::Value::Null => "-".to_owned(),
		serde_json::Value::String(s) => s.clone(),
		other => other.to_string(),
	};
	let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
	for r in rows {
		for (i, c) in columns.iter().enumerate() {
			widths[i] = widths[i].max(cell(r, c).chars().count());
		}
	}
	let mut out = String::new();
	let line = |cells: Vec<String>, out: &mut String| {
		let last = cells.len() - 1;
		for (i, c) in cells.into_iter().enumerate() {
			out.push_str(&c);
			if i < last {
				out.push_str(&" ".repeat(widths[i] - c.chars().count() + 2));
			}
		}
		out.push('\n');
	};
	line(columns.iter().map(|c| c.to_uppercase()).collect(), &mut out);
	for r in rows {
		line(columns.iter().map(|c| cell(r, c)).collect(), &mut out);
	}
	out.trim_end().to_owned()
}

// ------------------------------------------------------------------ provision

/// `bootcher device provision`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProvisionArgs {
	/// `--channel ch` / `--direct`; `None` to resolve (implied, prompted, or an error).
	pub mode: Option<Mode>,
	pub target: Option<Arch>,
	pub disks: Vec<DiskType>,
	pub ssh_key: Option<String>,
	pub skip_build: bool,
	pub skip_pull_check: bool,
	pub anonymous: bool,
}

/// `bootcher device provision`: build disk images for new devices.
///
/// - `--channel ch` never builds: it pulls the selected arches' members of `:ch`
///   (every time, by digest, into `localhost/<name>:pull-<ch>-<arch>`, never
///   `latest-<arch>`), bakes the pull credential and the trust files, and records
///   the channel as the origin.
/// - `--direct` builds here (or reuses the local images with `--skip-build`) and
///   records `localhost/<name>:latest`; no pull credential, no trust files.
///
/// # Errors
///
/// Returns an error if a flag combination or selection is invalid, a prerequisite
/// is missing, secret collection fails, or the build/pull/disk step fails.
pub fn provision(manifest: &Manifest, args: &ProvisionArgs) -> Result<()> {
	manifest.check_selection(args.target, &args.disks)?;
	let (mode, prompted) = resolve_mode(manifest, args.mode.clone(), "the new device")?;
	if prompted {
		Equivalent::new(&["device", "provision"])
			.arg_flags(&mode)
			.opts("--target", args.target.map(|t| t.to_string()))
			.opts("--disk", args.disks.iter().map(ToString::to_string))
			.opts("--ssh-key", args.ssh_key.iter())
			.flag_if(args.skip_build, "--skip-build")
			.flag_if(args.anonymous, "--anonymous")
			.flag_if(args.skip_pull_check, "--skip-pull-check")
			.print();
	}
	if mode == Mode::Direct && (args.anonymous || args.skip_pull_check) {
		bail!("`--anonymous` and `--skip-pull-check` only apply to a `--channel` provision");
	}
	if matches!(mode, Mode::Channel(_)) && args.skip_build {
		bail!(
			"`--skip-build` only applies to a direct provision: a channel provision never builds"
		);
	}
	// Pre-flight everything both phases need before collecting secrets or building.
	match &mode {
		Mode::Direct if !args.skip_build => build::preflight(manifest, args.target)?,
		Mode::Direct => {}
		Mode::Channel(_) => {
			let mut checks = Checks::default();
			checks.bin("podman", preflight::PODMAN_HINT);
			checks.bin("skopeo", oci::SKOPEO_HINT);
			checks.finish()?;
		}
	}
	disk::preflight(manifest, args.target)?;
	let install = install_mode(&mode, args.anonymous, args.skip_pull_check);
	let config = Provisioning::collect(manifest, args.ssh_key.as_deref(), &install)?.blueprint()?;

	let mut b = crate::progress::Scope::root("provision", Some(2));
	let (images, source_ref) = match &mode {
		Mode::Direct => {
			if args.skip_build {
				build::require_local(manifest, args.target)?;
			} else {
				build::run(manifest, args.target, &mut b.child("build"))?;
			}
			(manifest.images_for(args.target), manifest.local_list_ref())
		}
		Mode::Channel(ch) => {
			let images = pull_channel(manifest, ch, args.target, &mut b.child("pull"))?;
			(images, manifest.tag_ref(ch).context("a channel ref")?)
		}
	};
	disk::run(manifest, &images, &args.disks, Some(&config), &source_ref, &mut b.child("disk"))
}

/// The [`InstallMode`] for a resolved [`Mode`].
fn install_mode(mode: &Mode, anonymous: bool, skip_pull_check: bool) -> InstallMode {
	match mode {
		Mode::Channel(ch) => {
			InstallMode::Channel { channel: ch.clone(), anonymous, skip_pull_check }
		}
		Mode::Direct => InstallMode::Direct,
	}
}

/// Pull the members of `:ch` for the run's arches into
/// `localhost/<name>:pull-<ch>-<arch>`, by digest (so what's pulled is exactly what
/// the channel serves), every time. Returns those image refs.
fn pull_channel(
	manifest: &Manifest,
	ch: &str,
	target: Option<Arch>,
	job: &mut Scope,
) -> Result<Vec<crate::context::ImageRef>> {
	let channel_ref = manifest.tag_ref(ch).context("a channel provision needs `[registry]`")?;
	let repo = manifest.repo().context("a registry repo")?;
	let index = oci::fetch(&channel_ref)?.with_context(|| {
		format!(
			"{channel_ref} doesn't exist yet — publish a release and `bootcher channel promote` it first"
		)
	})?;
	let arches: Vec<Arch> = manifest.images_for(target).iter().map(|i| i.arch).collect();
	job.set_total(arches.len() as u64);
	let mut images = Vec::new();
	for arch in arches {
		job.step(format!("pull {arch}"));
		let digest = if index.manifest.is_index() {
			index
				.manifest
				.member(arch)
				.with_context(|| format!("{channel_ref} has no {arch} member"))?
				.digest
				.clone()
		} else {
			index.digest.clone()
		};
		let image = manifest.image_tagged(arch, &format!("pull-{ch}"));
		let src = oci::by_digest(&repo, &digest);
		crate::exec::run!(job, "podman", "pull", "--os", "linux", "--arch", arch.oci_arch(), &src)?;
		crate::exec::run!(job, "podman", "tag", &src, image.tag())?;
		images.push(image);
	}
	Ok(images)
}

// ------------------------------------------------------------------ takeover

/// `bootcher device takeover`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct TakeoverArgs {
	pub mode: Option<Mode>,
	pub remotes: Vec<String>,
	pub login: Option<String>,
	pub ssh_key: Option<String>,
	pub skip_build: bool,
	pub skip_pull_check: bool,
	pub anonymous: bool,
	pub yes: bool,
}

/// The takeover checklist: every remote, none pre-checked (D5: nothing is taken
/// over by default), each annotated with its configured stock login.
pub(crate) fn takeover_choices(remotes: &[RemoteConfig]) -> Vec<Choice<String>> {
	remotes
		.iter()
		.map(|r| {
			let login = match r {
				RemoteConfig::WithOpts { takeover_login: Some(l), .. } => {
					format!(" (stock login {l})")
				}
				_ => String::new(),
			};
			Choice::new(format!("{}{login}", r.dest()), r.dest().to_owned(), false)
		})
		.collect()
}

/// `bootcher device takeover`: convert live hosts to bootc in place.
///
/// # Errors
///
/// Returns an error if no host is selected, a flag combination is invalid, a host
/// fails pre-flight, the user declines, or any host's takeover fails.
#[allow(clippy::too_many_lines)]
pub fn takeover(manifest: &Manifest, args: &TakeoverArgs) -> Result<()> {
	require_remotes(manifest, "device takeover")?;
	let mut prompted = false;
	// Targets: `--remote`, else a TTY checklist with nothing pre-checked; a non-TTY
	// run must name them (D5).
	let names: Vec<String> = if args.remotes.is_empty() {
		if !interactive::is_tty() {
			bail!(
				"`device takeover` needs `--remote <host>` (repeatable) off a TTY: it never converts every remote by default"
			);
		}
		prompted = true;
		interactive::checklist(
			"Which hosts should be taken over?",
			takeover_choices(manifest.remotes()),
		)?
	} else {
		args.remotes.clone()
	};
	if names.is_empty() {
		bail!("no hosts selected — nothing to take over");
	}
	let targets: Vec<RemoteConfig> = device::select_remotes(manifest, &names)?
		.unwrap_or_default()
		.into_iter()
		.cloned()
		.collect();
	let (mode, mode_prompted) = resolve_mode(manifest, args.mode.clone(), "the converted hosts")?;
	prompted |= mode_prompted;
	if mode == Mode::Direct && (args.anonymous || args.skip_pull_check) {
		bail!("`--anonymous` and `--skip-pull-check` only apply to a `--channel` takeover");
	}
	if matches!(mode, Mode::Channel(_)) && args.skip_build {
		bail!("`--skip-build` only applies to a direct takeover: a channel takeover never builds");
	}
	// A host with no `takeover_login` needs `--login`: prompt for it on a TTY.
	let mut login = args.login.clone();
	let needs_login = targets
		.iter()
		.any(|t| !matches!(t, RemoteConfig::WithOpts { takeover_login: Some(_), .. }));
	if login.is_none() && needs_login && interactive::is_tty() {
		let l = inquire::Text::new(
			"Stock login for the initial connection (e.g. debian, ubuntu, cloud-user, root):",
		)
		.prompt()
		.prompted()?;
		login = Some(l.trim().to_owned()).filter(|l| !l.is_empty());
		prompted = true;
	}

	// The admin key first: the pre-flight probes each host as admin@ with it, so an
	// already-converted host is recognised.
	let install = install_mode(&mode, args.anonymous, args.skip_pull_check);
	if !args.skip_build && mode == Mode::Direct {
		build::preflight(manifest, None)?;
	}
	if let Mode::Channel(ch) = &mode {
		let mut checks = Checks::default();
		checks.bin("skopeo", oci::SKOPEO_HINT);
		checks.finish()?;
		let r = manifest.tag_ref(ch).context("a channel ref")?;
		oci::fetch(&r)?.with_context(|| {
			format!("{r} doesn't exist yet — publish and promote a release first")
		})?;
	}
	takeover::preflight_tools(&install)?;
	let (_, key_path) =
		crate::jobs::secrets::collect_authorized_keys(args.ssh_key.as_deref(), "--ssh-key")?;
	let key = key_path.to_str().context("admin SSH key path is not valid UTF-8")?.to_owned();
	takeover::host_preflight(&targets, login.as_deref(), &key)?;
	let (provisioning, _) = Provisioning::collect_with_key(manifest, Some(&key), &install)?;

	if prompted {
		Equivalent::new(&["device", "takeover"])
			.opts("--remote", &names)
			.arg_flags(&mode)
			.opts("--login", login.iter())
			.opts("--ssh-key", [&key])
			.flag_if(args.skip_build, "--skip-build")
			.flag_if(args.anonymous, "--anonymous")
			.flag_if(args.skip_pull_check, "--skip-pull-check")
			.arg("-y")
			.print();
	}
	let hosts: Vec<String> = targets.iter().map(|t| t.dest().to_owned()).collect();
	takeover::confirm(&hosts, &mode.to_string(), args.yes)?;

	if mode == Mode::Direct {
		let mut b = Scope::root("takeover", Some(2));
		if args.skip_build {
			build::require_local(manifest, None)?;
			build::manifest_list(
				&manifest.images(),
				&manifest.local_list_ref(),
				&mut b.child("manifest list"),
			)?;
		} else {
			build::run(manifest, None, &mut b.child("build"))?;
		}
		return takeover::run(
			manifest,
			&targets,
			login.as_deref(),
			&provisioning,
			&key,
			&install,
			args.yes,
			&mut b.child("takeover"),
		);
	}
	takeover::run(
		manifest,
		&targets,
		login.as_deref(),
		&provisioning,
		&key,
		&install,
		args.yes,
		&mut Scope::standalone(),
	)
}

// ------------------------------------------------------------------ deploy

/// The `device deploy` checklist: every probed device, the direct ones pre-checked,
/// the others disabled with their class.
pub(crate) fn deploy_choices(devices: &[Device]) -> Vec<Choice<usize>> {
	devices
		.iter()
		.enumerate()
		.map(|(i, d)| match d.class {
			Class::Direct => Choice::new(format!("{}: {}", d.name(), d.describe()), i, true),
			_ => Choice::disabled(d.name(), i, d.describe()),
		})
		.collect()
}

/// Split probed devices into `device deploy`'s targets (the direct ones) and the
/// skipped ones (with why). With an explicit `--remote` selection a non-direct
/// device is an error pointing at `device switch --direct` (D3).
pub(crate) fn deploy_targets(
	devices: &[Device],
	explicit: bool,
) -> Result<(Vec<usize>, Vec<String>)> {
	let mut targets = Vec::new();
	let mut skipped = Vec::new();
	for (i, d) in devices.iter().enumerate() {
		match &d.class {
			Class::Direct => targets.push(i),
			other if explicit => bail!(
				"{} isn't a direct device ({}) — `device deploy` never moves a device; run \
				 `bootcher device switch --direct --remote {}` to make it one",
				d.name(),
				other.describe(),
				d.name()
			),
			_ => skipped.push(format!("{}: {}, skipped", d.name(), d.describe())),
		}
	}
	Ok((targets, skipped))
}

/// `bootcher device deploy`: build here (or `--skip-build`) and push to the direct
/// devices. Dirty trees are fine: this is the inner loop. Never moves a device.
///
/// # Errors
///
/// Returns an error if there are no remotes, a `--remote` is unknown or not direct,
/// the user declines, the build fails, or any device fails or is unreachable.
pub fn deploy(manifest: &Manifest, skip_build: bool, remotes: &[String], yes: bool) -> Result<()> {
	require_remotes(manifest, "device deploy")?;
	if !skip_build {
		build::preflight(manifest, None)?;
	}
	upgrade::preflight_direct()?;
	let explicit = !remotes.is_empty();
	let picked = candidates(manifest, remotes)?;
	let (devices, unreachable) = probe(manifest, &picked);
	let (targets, skipped) = deploy_targets(&devices, explicit)?;
	for s in &skipped {
		eprintln!("{s}");
	}
	if targets.is_empty() {
		eprintln!(
			"no direct devices to deploy to — channel devices get releases through `bootcher \
			 release publish` + `bootcher channel promote`"
		);
		return finish_unreachable(&unreachable);
	}
	let lines: Vec<String> =
		targets.iter().map(|&i| format!("  {} → this checkout", devices[i].name())).collect();
	let (chosen, prompted) = pick(
		yes,
		explicit,
		"`device deploy` updates devices",
		"Deploy this checkout to which direct devices?",
		deploy_choices(&devices),
		&std::iter::once("device deploy will update:".to_owned()).chain(lines).collect::<Vec<_>>(),
	)?;
	let chosen: Vec<&Device> =
		devices.iter().enumerate().filter(|(i, _)| chosen.contains(i)).map(|(_, d)| d).collect();
	if chosen.is_empty() {
		eprintln!("no devices selected — nothing to deploy");
		return finish_unreachable(&unreachable);
	}
	if prompted {
		Equivalent::new(&["device", "deploy"])
			.flag_if(skip_build, "--skip-build")
			.opts("--remote", chosen.iter().map(|d| d.name()))
			.arg("-y")
			.print();
	}
	let ssh: Vec<Ssh> = chosen.iter().map(|d| d.ssh.clone()).collect();
	let mut b = Scope::root("deploy", Some(2));
	if skip_build {
		build::require_local(manifest, None)?;
		build::manifest_list(
			&manifest.images(),
			&manifest.local_list_ref(),
			&mut b.child("manifest list"),
		)?;
	} else {
		build::run(manifest, None, &mut b.child("build"))?;
	}
	direct_rollout(
		manifest,
		&manifest.images(),
		&manifest.local_list_ref(),
		None,
		&ssh,
		unreachable,
		&mut b.child("upgrade"),
	)
}

/// The direct device step across `targets` with the list `list_ref`, bracketed by
/// `[hooks.upgrade]`; `unreachable` are reported as failures alongside. `commit` is
/// the commit the images were built from when it isn't the checkout's (a refresh);
/// `None` reports the checkout's revision.
pub(crate) fn direct_rollout(
	manifest: &Manifest,
	images: &[crate::context::ImageRef],
	list_ref: &str,
	commit: Option<&str>,
	targets: &[Ssh],
	unreachable: Vec<Outcome>,
	job: &mut Scope,
) -> Result<()> {
	let mut meta = HookMetadata::new(manifest, Phase::Upgrade, list_ref.to_owned());
	meta.remotes = Some(targets.iter().map(|s| s.host().to_owned()).collect());
	if let Some(c) = commit {
		meta.revision = Some(c.to_owned());
	} else {
		let provenance = crate::context::GitProvenance::detect(manifest.root(), job);
		meta.revision = provenance.revision;
		meta.version = provenance.version;
	}
	let hooks = manifest.hooks();
	crate::hooks::run(&meta, hooks.upgrade.pre.as_deref(), job)?;
	let report = upgrade::run_direct(
		images,
		list_ref,
		&manifest.local_list_ref(),
		targets,
		manifest.concurrency().upgrade,
		job,
	)?
	.with(unreachable);
	crate::hooks::run_post(&mut meta, hooks.upgrade.post.as_deref(), &report, job)
}

/// A per-device action run before a device step (e.g. installing a pull credential).
pub(crate) type DeviceHook<'a> = dyn Fn(&Ssh, &mut Scope) -> Result<()> + Sync + 'a;

/// The channel device step across `targets` (signing on ⇒ trust files first),
/// bracketed by `[hooks.upgrade]`; `before` runs on each device first (e.g. a pull
/// credential install) and `unreachable` are reported alongside. Shared by `device
/// switch --channel`, `channel promote|refresh` and `signing`.
pub(crate) fn channel_rollout(
	manifest: &Manifest,
	channel: &str,
	targets: &[Ssh],
	unreachable: Vec<Outcome>,
	release: Option<String>,
	before: &DeviceHook<'_>,
	job: &mut Scope,
) -> Result<()> {
	let channel_ref = manifest.tag_ref(channel).context("a channel rollout needs `[registry]`")?;
	let trust = TrustFiles::for_manifest(manifest)?.map(|t| t.install_script(false));
	let mut meta = HookMetadata::new(manifest, Phase::Upgrade, channel_ref.clone());
	meta.channel = Some(channel.to_owned());
	meta.release = release;
	meta.remotes = Some(targets.iter().map(|s| s.host().to_owned()).collect());
	let hooks = manifest.hooks();
	crate::hooks::run(&meta, hooks.upgrade.pre.as_deref(), job)?;
	let report = crate::fleet::for_each_remote(
		targets,
		"upgrade",
		manifest.concurrency().upgrade,
		job,
		|remote, scope| {
			before(remote, scope)?;
			upgrade::apply_channel(&channel_ref, trust.as_deref(), remote, scope)
		},
	)?
	.with(unreachable);
	crate::hooks::run_post(&mut meta, hooks.upgrade.post.as_deref(), &report, job)
}

// ------------------------------------------------------------------ switch

/// `device switch`'s checklist: every reachable device not already on `target`,
/// annotated with its current class, none pre-checked.
pub(crate) fn switch_choices(devices: &[Device], target: &Mode) -> Vec<Choice<usize>> {
	devices
		.iter()
		.enumerate()
		.filter(|(_, d)| !already_on(&d.class, target))
		.map(|(i, d)| Choice::new(format!("{}: {}", d.name(), d.describe()), i, false))
		.collect()
}

/// Whether a device of class `class` is already on `target`.
fn already_on(class: &Class, target: &Mode) -> bool {
	match (class, target) {
		(Class::Channel(ch), Mode::Channel(t)) => ch == t,
		(Class::Direct, Mode::Direct) => true,
		_ => false,
	}
}

/// `bootcher device switch`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SwitchArgs {
	pub mode: Option<Mode>,
	pub remotes: Vec<String>,
	pub skip_build: bool,
	pub anonymous: bool,
	pub skip_pull_check: bool,
	pub yes: bool,
}

/// `bootcher device switch`: the one verb that moves a device — onto a channel
/// (pull credential if missing, trust files with signing on, then the channel
/// device step) or to direct (build here, then the direct device step).
///
/// # Errors
///
/// Returns an error if the mode or targets can't be resolved, the channel doesn't
/// exist, the user declines, or any device fails or is unreachable.
#[allow(clippy::too_many_lines)]
pub fn switch(manifest: &Manifest, args: &SwitchArgs) -> Result<()> {
	require_remotes(manifest, "device switch")?;
	let mut prompted = false;
	let mode = match args.mode.clone() {
		Some(Mode::Channel(ch)) => {
			manifest.check_channel(&ch)?;
			Mode::Channel(ch)
		}
		Some(Mode::Direct) => Mode::Direct,
		None if interactive::is_tty() => {
			prompted = true;
			if manifest.registry().is_some() {
				interactive::select(
					"Switch the devices to:",
					&mode_choices(manifest.channels(), &channel_notes(manifest)),
				)?
			} else {
				Mode::Direct
			}
		}
		None => bail!("`device switch` needs `--channel <ch>` or `--direct`"),
	};
	if mode == Mode::Direct && (args.anonymous || args.skip_pull_check) {
		bail!("`--anonymous` and `--skip-pull-check` only apply to `--channel`");
	}
	if matches!(mode, Mode::Channel(_)) && args.skip_build {
		bail!("`--skip-build` only applies to `--direct`: a channel switch builds nothing");
	}
	let mut checks = Checks::default();
	checks.bin("ssh", "reach the devices over SSH");
	if matches!(mode, Mode::Channel(_)) {
		checks.bin("skopeo", oci::SKOPEO_HINT);
	}
	checks.finish()?;
	if mode == Mode::Direct {
		if !args.skip_build {
			build::preflight(manifest, None)?;
		}
		upgrade::preflight_direct()?;
	}
	if let Mode::Channel(ch) = &mode {
		let r = manifest.tag_ref(ch).context("a channel ref")?;
		oci::fetch(&r)?
			.with_context(|| format!("{r} doesn't exist yet — promote a release to it first"))?;
	}

	// Targets: `--remote`, or a TTY checklist of the devices not already there.
	let explicit = !args.remotes.is_empty();
	if !explicit && !interactive::is_tty() {
		bail!("`device switch` needs `--remote <device>` (repeatable) off a TTY");
	}
	let picked = candidates(manifest, &args.remotes)?;
	let (devices, unreachable) = probe(manifest, &picked);
	let mut chosen: Vec<usize> = if explicit {
		(0..devices.len()).collect()
	} else {
		prompted = true;
		interactive::checklist(
			&format!("Which devices should move to {mode}?"),
			switch_choices(&devices, &mode),
		)?
	};
	chosen.retain(|&i| {
		let d = &devices[i];
		let on = already_on(&d.class, &mode);
		if on {
			eprintln!("{}: already {}, nothing to do", d.name(), d.describe());
		}
		!on
	});
	if chosen.is_empty() {
		eprintln!("no device to move");
		return finish_unreachable(&unreachable);
	}
	let signing = manifest.signing().is_some();
	let mut lines = vec!["device switch will move:".to_owned()];
	for &i in &chosen {
		lines.push(format!("  {}: {} → {mode}", devices[i].name(), devices[i].class.describe()));
	}
	if signing && mode == Mode::Direct {
		lines.push(
			"  note: a direct device stops verifying image signatures (its trust is the SSH channel)"
				.to_owned(),
		);
	}
	if prompted {
		Equivalent::new(&["device", "switch"])
			.arg_flags(&mode)
			.opts("--remote", chosen.iter().map(|&i| devices[i].name()))
			.flag_if(args.skip_build, "--skip-build")
			.flag_if(args.anonymous, "--anonymous")
			.flag_if(args.skip_pull_check, "--skip-pull-check")
			.arg("-y")
			.print();
	}
	interactive::confirm(args.yes, "`device switch` moves devices", &lines, "Proceed?", true)?;
	let targets: Vec<&Device> = chosen.iter().map(|&i| &devices[i]).collect();

	match &mode {
		Mode::Direct => {
			let ssh: Vec<Ssh> = targets.iter().map(|d| d.ssh.clone()).collect();
			let mut b = Scope::root("switch", Some(2));
			if args.skip_build {
				build::require_local(manifest, None)?;
				build::manifest_list(
					&manifest.images(),
					&manifest.local_list_ref(),
					&mut b.child("manifest list"),
				)?;
			} else {
				build::run(manifest, None, &mut b.child("build"))?;
			}
			direct_rollout(
				manifest,
				&manifest.images(),
				&manifest.local_list_ref(),
				None,
				&ssh,
				unreachable,
				&mut b.child("upgrade"),
			)
		}
		Mode::Channel(ch) => {
			let channel_ref = manifest.tag_ref(ch).context("a channel ref")?;
			let reg = manifest.require_registry("`device switch --channel`")?;
			// One pull credential for the whole run, for the devices that have none.
			let needs_auth: Vec<String> = targets
				.iter()
				.filter(|d| !d.status().is_some_and(|s| s.has_auth))
				.map(|d| d.name().to_owned())
				.collect();
			let auth = if needs_auth.is_empty() || args.anonymous {
				None
			} else {
				eprintln!("no pull credential on: {}", needs_auth.join(", "));
				let (user, token) = crate::jobs::secrets::collect_pull_credential(&reg.url)?
					.with_context(|| {
						"no credential given — pass `--anonymous` if the registry is public"
							.to_owned()
					})?;
				if !args.skip_pull_check {
					crate::jobs::secrets::verify_pull_login(&reg.url, &user, &token)?;
				}
				Some(crate::jobs::secrets::render_auth_json(&reg.url, &user, &token))
			};
			let ssh: Vec<Ssh> = targets.iter().map(|d| d.ssh.clone()).collect();
			let before = |remote: &Ssh, scope: &mut Scope| -> Result<()> {
				match &auth {
					Some(auth) if needs_auth.iter().any(|n| n == remote.host()) => {
						rotate::verify_and_commit_token(
							remote,
							scope,
							&channel_ref,
							crate::context::DEVICE_AUTH_JSON,
							auth,
						)
					}
					_ => Ok(()),
				}
			};
			channel_rollout(
				manifest,
				ch,
				&ssh,
				unreachable,
				None,
				&before,
				&mut Scope::standalone(),
			)
		}
	}
}

// ------------------------------------------------------------------ rotate

/// `bootcher device rotate ssh-key`: roll the admin SSH key on every reachable
/// remote (or the `--remote` subset), lockout-safe.
///
/// # Errors
///
/// Returns an error if key collection fails, the user declines, or any device
/// fails or is unreachable.
pub fn rotate_ssh_key(
	manifest: &Manifest,
	ssh_key: Option<&str>,
	remotes: &[String],
	yes: bool,
) -> Result<()> {
	require_remotes(manifest, "device rotate ssh-key")?;
	rotate::preflight(false)?;
	let picked = candidates(manifest, remotes)?;
	let (devices, unreachable) = probe(manifest, &picked);
	if devices.is_empty() {
		return finish_unreachable(&unreachable);
	}
	let (authorized_keys, key_path) =
		crate::jobs::secrets::collect_authorized_keys(ssh_key, "--ssh-key")?;
	let mut lines = vec!["device rotate ssh-key will replace the admin key on:".to_owned()];
	lines.extend(devices.iter().map(|d| format!("  {}", d.name())));
	interactive::confirm(yes, "`device rotate ssh-key` changes devices", &lines, "Proceed?", true)?;
	let ssh: Vec<Ssh> = devices.iter().map(|d| d.ssh.clone()).collect();
	rotate::ssh_key(
		manifest,
		&ssh,
		&authorized_keys,
		&key_path,
		unreachable,
		&mut Scope::standalone(),
	)
}

/// `bootcher device rotate pull-token`: replace the pull credential on the channel
/// devices (or the `--remote` subset of them), each verified against its own
/// channel ref before it's committed.
///
/// # Errors
///
/// Returns an error if there's no registry, a named device isn't a channel device,
/// the user declines, or any device fails or is unreachable.
pub fn rotate_pull_token(
	manifest: &Manifest,
	skip_pull_check: bool,
	remotes: &[String],
	yes: bool,
) -> Result<()> {
	manifest.require_registry("`device rotate pull-token`")?;
	require_remotes(manifest, "device rotate pull-token")?;
	rotate::preflight(!skip_pull_check)?;
	let explicit = !remotes.is_empty();
	let picked = candidates(manifest, remotes)?;
	let (devices, unreachable) = probe(manifest, &picked);
	let mut targets: Vec<(Ssh, String)> = Vec::new();
	for d in &devices {
		match &d.class {
			Class::Channel(ch) => {
				targets.push((d.ssh.clone(), manifest.tag_ref(ch).context("a channel ref")?));
			}
			other if explicit => {
				bail!(
					"{} isn't a channel device ({}) — it carries no pull credential",
					d.name(),
					other.describe()
				)
			}
			_ => eprintln!("{}: {}, skipped", d.name(), d.describe()),
		}
	}
	if targets.is_empty() {
		eprintln!("no channel devices — nothing carries a pull credential to rotate");
		return finish_unreachable(&unreachable);
	}
	let mut lines =
		vec!["device rotate pull-token will replace the pull credential on:".to_owned()];
	lines.extend(targets.iter().map(|(s, r)| format!("  {} ({r})", s.host())));
	interactive::confirm(
		yes,
		"`device rotate pull-token` changes devices",
		&lines,
		"Proceed?",
		true,
	)?;
	rotate::registry_token(
		manifest,
		&targets,
		skip_pull_check,
		unreachable,
		&mut Scope::standalone(),
	)
}

impl Equivalent {
	/// Append a mode's flags (`--channel ch` / `--direct`).
	#[must_use]
	fn arg_flags(self, mode: &Mode) -> Self {
		mode.flags().into_iter().fold(self, Equivalent::arg)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::device::HostStatus;

	fn dev(name: &str, class: Class) -> Device {
		let remote = RemoteConfig::ConnStr(name.to_owned());
		Device {
			ssh: Ssh::from(&remote),
			remote,
			status: Ok(HostStatus { arch: Some(Arch::Aarch64), ..HostStatus::default() }),
			class,
		}
	}

	fn fleet() -> Vec<Device> {
		vec![
			dev("admin@rpi-1", Class::Direct),
			dev("admin@kiosk-2", Class::Channel("next".into())),
			dev("admin@odd", Class::Foreign("q.io/x:y".into())),
			dev("admin@rpi-2", Class::Direct),
		]
	}

	#[test]
	fn deploy_targets_only_direct_devices_and_skips_the_rest() {
		let (targets, skipped) = deploy_targets(&fleet(), false).unwrap();
		assert_eq!(targets, [0, 3]);
		assert_eq!(
			skipped,
			[
				"admin@kiosk-2: on :next, aarch64, skipped",
				"admin@odd: foreign (q.io/x:y), aarch64, skipped"
			]
		);
	}

	#[test]
	fn deploy_with_remote_naming_a_channel_device_points_at_switch() {
		let err = deploy_targets(&fleet()[1..2], true).unwrap_err().to_string();
		assert!(err.contains("device switch --direct --remote admin@kiosk-2"), "{err}");
	}

	#[test]
	fn deploy_checklist_prechecks_direct_and_disables_others() {
		let c = deploy_choices(&fleet());
		assert!(c[0].checked && c[0].disabled.is_none());
		assert_eq!(c[0].label, "admin@rpi-1: direct, aarch64");
		assert!(!c[1].checked && c[1].disabled.as_deref() == Some("on :next, aarch64"));
		assert!(c[2].disabled.is_some());
		assert!(c[3].checked);
	}

	#[test]
	fn switch_checklist_hides_devices_already_there_and_prechecks_none() {
		let c = switch_choices(&fleet(), &Mode::Channel("next".into()));
		assert_eq!(c.iter().map(|c| c.value).collect::<Vec<_>>(), [0, 2, 3]);
		assert!(c.iter().all(|c| !c.checked));
		let c = switch_choices(&fleet(), &Mode::Direct);
		assert_eq!(c.iter().map(|c| c.value).collect::<Vec<_>>(), [1, 2]);
	}

	#[test]
	fn mode_choices_annotate_channels_and_offer_direct() {
		let c =
			mode_choices(&["stable".into(), "next".into()], &[("stable".into(), "v1.3".into())]);
		assert_eq!(c[0].label, "channel stable: v1.3");
		assert_eq!(c[0].value, Mode::Channel("stable".into()));
		assert_eq!(c[1].label, "channel next");
		assert_eq!(c[2].value, Mode::Direct);
		// Nothing pre-selected: off a TTY a mode is required, so there's no default.
		assert!(c.iter().all(|c| !c.checked));
	}

	#[test]
	fn takeover_checklist_prechecks_nothing() {
		let remotes = vec![
			RemoteConfig::ConnStr("admin@vps1".into()),
			RemoteConfig::WithOpts {
				remote: "admin@vps2".into(),
				ssh_opts: vec![],
				takeover_login: Some("debian".into()),
			},
		];
		let c = takeover_choices(&remotes);
		assert!(c.iter().all(|c| !c.checked));
		assert_eq!(c[1].label, "admin@vps2 (stock login debian)");
	}

	fn manifest(registry: bool) -> Manifest {
		let reg =
			if registry { "[registry]\nurl = \"r.io/o\"\nchannels = [\"stable\"]\n" } else { "" };
		toml::from_str(&format!("[general]\nname = \"kiosk\"\n{reg}")).unwrap()
	}

	#[test]
	fn without_a_registry_direct_is_implied_and_channel_rejected() {
		assert_eq!(resolve_mode(&manifest(false), None, "x").unwrap(), (Mode::Direct, false));
		let err =
			resolve_mode(&manifest(false), Some(Mode::Channel("stable".into())), "x").unwrap_err();
		assert!(err.to_string().contains("needs a registry"), "{err}");
	}

	#[test]
	fn with_a_registry_the_mode_is_validated() {
		let m = manifest(true);
		assert_eq!(
			resolve_mode(&m, Some(Mode::Channel("stable".into())), "x").unwrap().0,
			Mode::Channel("stable".into())
		);
		assert!(resolve_mode(&m, Some(Mode::Channel("beta".into())), "x").is_err());
		assert_eq!(resolve_mode(&m, Some(Mode::Direct), "x").unwrap().0, Mode::Direct);
	}

	fn channel_dev(booted: &str, staged: Option<&str>) -> Device {
		let remote = RemoteConfig::ConnStr("admin@k".into());
		let d = |digest: &str| crate::device::Deployed {
			image: None,
			digest: digest.into(),
			version: None,
		};
		Device {
			ssh: Ssh::from(&remote),
			remote,
			status: Ok(HostStatus {
				booted: Some(d(booted)),
				staged: staged.map(d),
				..HostStatus::default()
			}),
			class: Class::Channel("stable".into()),
		}
	}

	#[test]
	fn status_maps_member_digests_to_releases_and_lag() {
		let view = ChannelView {
			release: Some("v2".into()),
			members: vec![(Arch::X86_64, "sha256:new".into())],
		};
		let channels = vec![("stable".to_owned(), Some(view))];
		let lookup = |d: &str| match d {
			"sha256:new" => Some("v2".to_owned()),
			"sha256:old" => Some("v1".to_owned()),
			_ => None,
		};
		let row = status_row(&channel_dev("sha256:new", None), &channels, &lookup);
		assert_eq!((row.release.as_deref(), row.lags), (Some("v2"), Some(false)));
		let row = status_row(&channel_dev("sha256:old", None), &channels, &lookup);
		assert_eq!((row.release.as_deref(), row.lags), (Some("v1"), Some(true)));
		assert!(row.state.starts_with("lags :stable"), "{}", row.state);
		let row = status_row(&channel_dev("sha256:old", Some("sha256:new")), &channels, &lookup);
		assert!(row.state.contains("reboot pending"), "{}", row.state);
		// A cross-arch fresh disk matches no member: not foreign, just not updated yet.
		let row = status_row(&channel_dev("sha256:lock-layer", None), &channels, &lookup);
		assert_eq!(row.class, "channel");
		assert!(row.state.contains("provisioned, not yet updated"), "{}", row.state);
	}

	#[test]
	fn table_aligns_columns() {
		let rows = vec![
			serde_json::json!({"a": "x", "b": null}),
			serde_json::json!({"a": "longer", "b": 2}),
		];
		assert_eq!(table(&rows, &["a", "b"]), "A       B\nx       -\nlonger  2");
	}
}
