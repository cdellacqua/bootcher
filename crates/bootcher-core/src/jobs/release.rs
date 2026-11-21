//! `bootcher release …`: named releases in the registry.
//!
//! A release `:X` is any valid OCI tag the user picks. Each entry of its multi-arch
//! index carries the `bootcher.release=X` annotation and each member the `bootcher.source=<sha>`
//! label of the clean commit it was built from. The one rule: `:X` may only be
//! written by a clean build of the commit its members record, which keeps
//! publishing idempotent, makes refreshes legal, and stops `:X` from ever changing
//! source. Lists are pushed `--format oci`, because Docker v2 lists carry no
//! annotations.

use crate::context::{Arch, ImageRef, Manifest, RESERVED_TAG_PREFIXES, SOURCE_LABEL, valid_tag};
use crate::hooks::{HookMetadata, Phase, Stage};
use crate::interactive::{self, Choice, Equivalent, Prompted};
use crate::jobs::build::{self, BuildOpts};
use crate::oci::{self, BaseCache, BaseStatus};
use crate::preflight::{self, Checks};
use crate::progress::Scope;
use crate::{git, signals};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;

/// Whether `tag` could name a release: not a declared channel and not reserved.
pub(crate) fn is_release_candidate(manifest: &Manifest, tag: &str) -> bool {
	!manifest.channels().iter().any(|c| c == tag)
		&& !RESERVED_TAG_PREFIXES.iter().any(|p| tag.starts_with(p))
}

/// Check a release name: OCI tag grammar, not a declared channel, not reserved.
///
/// # Errors
///
/// Returns an error saying what's wrong with it.
pub(crate) fn check_name(manifest: &Manifest, name: &str) -> Result<()> {
	if !valid_tag(name) {
		bail!(
			"`{name}` isn't a valid OCI tag (letters, digits, `_`, `.`, `-`; at most 128 \
			 characters; not starting with `.` or `-`)"
		);
	}
	if manifest.channels().iter().any(|c| c == name) {
		bail!("`{name}` is a declared channel; a release needs its own name (e.g. v1.4)");
	}
	if let Some(p) = RESERVED_TAG_PREFIXES.iter().find(|p| name.starts_with(*p)) {
		bail!("`{name}` uses the reserved `{p}` prefix");
	}
	Ok(())
}

/// Every published release: each tag (filtered by the reserved names) whose index
/// carries a release annotation, with its fetched index.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn releases(manifest: &Manifest) -> Result<Vec<(String, oci::Fetched)>> {
	let Some(repo) = manifest.repo() else { return Ok(Vec::new()) };
	let mut out = Vec::new();
	for tag in oci::list_tags(&repo)? {
		if !is_release_candidate(manifest, &tag) {
			continue;
		}
		if let Some(f) = oci::fetch(&format!("{repo}:{tag}"))?
			&& f.manifest.release().is_some()
		{
			out.push((tag, f));
		}
	}
	Ok(out)
}

/// Every release with its member digests — for mapping a device's booted digest
/// (a per-arch member digest, S2) to the release it belongs to.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn member_digests(manifest: &Manifest) -> Result<Vec<(String, Vec<String>)>> {
	Ok(releases(manifest)?
		.into_iter()
		.map(|(tag, f)| (tag, f.manifest.members().into_iter().map(|(_, d)| d).collect()))
		.collect())
}

/// One member of a published index, as read from the registry: its arch and digest,
/// the commit it records, and its manifest (base annotations).
#[derive(Clone, Debug)]
pub(crate) struct Member {
	pub(crate) arch: Arch,
	pub(crate) digest: String,
	pub(crate) source: Option<String>,
	pub(crate) manifest: oci::Manifest,
}

/// A published index (a release, or a channel's copy of one) with its members read.
#[derive(Clone, Debug)]
pub(crate) struct Published {
	pub(crate) index: oci::Fetched,
	pub(crate) members: Vec<Member>,
}

impl Published {
	/// The single commit every member records, `None` when none does.
	///
	/// # Errors
	///
	/// Returns an error when members record different commits (or only some do).
	pub(crate) fn source(&self) -> Result<Option<String>> {
		let mut sources: Vec<Option<&str>> =
			self.members.iter().map(|m| m.source.as_deref()).collect();
		sources.dedup();
		match sources.as_slice() {
			[] | [None] => Ok(None),
			[Some(s)] => Ok(Some((*s).to_owned())),
			_ => bail!("the members of {} record different source commits", self.index.digest),
		}
	}

	/// Each member's base status.
	pub(crate) fn bases(&self, cache: &mut BaseCache) -> Vec<(Arch, BaseStatus)> {
		self.members.iter().map(|m| (m.arch, cache.status(&m.manifest, m.arch))).collect()
	}
}

/// Read `tag` (a release or channel) and its members, or `None` when it doesn't
/// exist.
///
/// # Errors
///
/// Returns an error if the registry can't be read.
pub(crate) fn read(manifest: &Manifest, tag: &str) -> Result<Option<Published>> {
	let repo = manifest.repo().context("reading a release needs `[registry]`")?;
	let Some(index) = oci::fetch(&format!("{repo}:{tag}"))? else { return Ok(None) };
	let mut members = Vec::new();
	for (arch, digest) in index.manifest.members() {
		let r = oci::by_digest(&repo, &digest);
		let member = oci::fetch_existing(&r)?.manifest;
		members.push(Member { arch, source: oci::source_of(&r)?, digest, manifest: member });
	}
	Ok(Some(Published { index, members }))
}

/// The [`SOURCE_LABEL`] a local image records (`None` when absent or empty).
///
/// # Errors
///
/// Returns an error if the image doesn't exist locally.
pub(crate) fn local_source(tag: &str) -> Result<Option<String>> {
	let out = duct::cmd!("podman", "image", "inspect", "--format", "{{json .Labels}}", tag)
		.stderr_capture()
		.read()
		.with_context(|| format!("{tag} isn't in local storage"))?;
	let labels: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
	Ok(labels
		.get(SOURCE_LABEL)
		.and_then(|v| v.as_str())
		.filter(|s| !s.is_empty())
		.map(str::to_owned))
}

/// `--skip-build` for a release: every arch's local `latest-<arch>` must exist and
/// record `commit`, else the hint names what to do.
///
/// # Errors
///
/// Returns an error naming each arch that's missing or records another commit.
pub(crate) fn require_local_sources(
	manifest: &Manifest,
	arches: &[Arch],
	commit: &str,
) -> Result<()> {
	let mut problems = Vec::new();
	for &arch in arches {
		let tag = manifest.image(arch).tag();
		if !build::image_present(&tag) {
			problems.push(format!("{tag} is missing"));
			continue;
		}
		match local_source(&tag)? {
			Some(s) if s == commit => {}
			Some(s) => problems.push(format!("{tag} was built from {}", oci::short(&s))),
			None => problems
				.push(format!("{tag} records no source (built from a dirty tree, or outside git)")),
		}
	}
	if !problems.is_empty() {
		bail!(
			"`--skip-build` needs every arch built from {}: {} — rebuild, or check out {}",
			oci::short(commit),
			problems.join("; "),
			oci::short(commit)
		);
	}
	Ok(())
}

/// Fail loudly when a member just built isn't on the current base (a mirror lagging,
/// a pinned base): pushing it wouldn't patch anything.
///
/// # Errors
///
/// Returns an error naming each stale member.
pub(crate) fn require_current_base(images: &[ImageRef], cache: &mut BaseCache) -> Result<()> {
	let mut stale = Vec::new();
	for image in images {
		let local = oci::fetch_existing(&format!("containers-storage:{}", image.tag()))?;
		let status = cache.status(&local.manifest, image.arch);
		if status.is_stale() {
			stale.push(format!("{}: {}", image.arch, status.describe()));
		}
	}
	if !stale.is_empty() {
		bail!("the rebuild still isn't on the current base — {}", stale.join("; "));
	}
	Ok(())
}

/// Assemble `list` from the local `images` plus `remote` members added by digest
/// (no pull, S5), and annotate each entry as release `name`.
///
/// # Errors
///
/// Returns an error if a `podman manifest` step fails.
pub(crate) fn assemble(
	list: &str,
	images: &[ImageRef],
	remote: &[(Arch, String)],
	name: &str,
	job: &mut Scope,
) -> Result<()> {
	build::manifest_list(images, list, job)?;
	for (arch, r) in remote {
		crate::exec::run!(
			job,
			"podman",
			"manifest",
			"add",
			"--os",
			"linux",
			"--arch",
			arch.oci_arch(),
			list,
			format!("docker://{r}")
		)?;
	}
	// Name the release on every entry, not on the index: `manifest annotate --index`
	// needs podman 5, and CI runners still ship 4.9.
	let raw = duct::cmd!("podman", "manifest", "inspect", list)
		.stderr_capture()
		.read()
		.with_context(|| format!("inspecting {list}"))?;
	let index: oci::Manifest = serde_json::from_str(&raw).context("parsing the local list")?;
	for entry in &index.manifests {
		crate::exec::run!(
			job,
			"podman",
			"manifest",
			"annotate",
			"--annotation",
			format!("{}={name}", crate::context::RELEASE_ANNOTATION),
			list,
			&entry.digest
		)?;
	}
	Ok(())
}

/// Check the local list `list` holds exactly one member per `[targets]` arch (an
/// empty list would push without error).
///
/// # Errors
///
/// Returns an error describing the mismatch.
pub(crate) fn check_members(manifest: &Manifest, list: &str) -> Result<()> {
	let raw = duct::cmd!("podman", "manifest", "inspect", list)
		.stderr_capture()
		.read()
		.with_context(|| format!("inspecting {list}"))?;
	let index: oci::Manifest = serde_json::from_str(&raw).context("parsing the local list")?;
	let mut have: Vec<Arch> = index.members().into_iter().map(|(a, _)| a).collect();
	have.sort_by_key(ToString::to_string);
	let mut want = manifest.targets.arches();
	want.sort_by_key(ToString::to_string);
	if have != want || index.manifests.len() != want.len() {
		bail!(
			"{list} holds {} member(s) ({}) but `[targets]` builds {} — refusing to publish an \
			 incomplete release",
			index.manifests.len(),
			have.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
			want.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
		);
	}
	Ok(())
}

/// Push the local list `list` as release `name` (`--all --format oci`, signed when
/// signing is configured), bracketed by `[hooks.publish]`. Returns the pushed
/// index digest.
///
/// # Errors
///
/// Returns an error if a hook or the push fails.
pub(crate) fn push(
	manifest: &Manifest,
	list: &str,
	name: &str,
	source: Option<&str>,
	job: &mut Scope,
) -> Result<String> {
	let dest = manifest.tag_ref(name).context("publishing needs `[registry]`")?;
	let mut meta = HookMetadata::new(manifest, Phase::Publish, dest.clone());
	meta.release = Some(name.to_owned());
	meta.revision = source.map(str::to_owned);
	let hooks = manifest.hooks();
	crate::hooks::run(&meta, hooks.publish.pre.as_deref(), job)?;
	let sign = match manifest.signing() {
		Some(cfg) => {
			let ns = &manifest.require_registry("signing")?.url;
			crate::jobs::signing::ensure_push_attachments(ns, job)?;
			let key = manifest.root().join(&cfg.key);
			Some(crate::jobs::signing::sign_args_with(
				&key.to_string_lossy(),
				&crate::jobs::signing::collect_passphrase(false)?,
			)?)
		}
		None => None,
	};
	job.step(format!("push {name}"));
	let digest_dir = tempfile::tempdir().context("creating a temp dir for the pushed digest")?;
	let digest_file = digest_dir.path().join("digest");
	let mut argv: Vec<OsString> = ["podman", "manifest", "push", "--all", "--format", "oci"]
		.iter()
		.map(OsString::from)
		.collect();
	argv.push("--digestfile".into());
	argv.push(digest_file.clone().into_os_string());
	if let Some(sign) = &sign {
		argv.extend(sign.flags().iter().cloned());
	}
	argv.push(list.into());
	argv.push(format!("docker://{dest}").into());
	crate::exec::run_argv_labeled(job, &argv, format!("podman manifest push {dest}"))
		.with_context(|| {
			let host = dest.split('/').next().unwrap_or(&dest);
			format!("pushing {dest} failed — is this machine logged in? (`podman login {host}`)")
		})?;
	drop(sign);
	let digest = std::fs::read_to_string(&digest_file)
		.with_context(|| format!("reading the digest `podman manifest push` wrote for {dest}"))?
		.trim()
		.to_owned();
	job.println(format!("published {dest} at {digest}"));
	meta.stage = Stage::Post;
	crate::hooks::run(&meta, hooks.publish.post.as_deref(), job)?;
	Ok(digest)
}

/// Rebuild the `stale` arches of `release` (published as `name`, recorded at
/// `commit`) on the current base and push it over `:name`, keeping the untouched
/// members by digest. With `skip_build` the local `latest-<arch>` images are taken
/// instead (each must record `commit`); otherwise the commit is built in a temporary
/// worktree under its own config (the same relative `--manifest` path), tagged
/// `localhost/<name>:refresh-<sha12>-<arch>`, so `latest-<arch>` is never touched.
/// Returns the new index digest.
///
/// # Errors
///
/// Returns an error if the worktree, build, base check or push fails.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rebuild(
	manifest: &Manifest,
	name: &str,
	release: &Published,
	commit: &str,
	stale: &[Arch],
	force: bool,
	skip_build: bool,
	cache: &mut BaseCache,
	job: &mut Scope,
) -> Result<String> {
	let repo = manifest.repo().context("refreshing needs `[registry]`")?;
	let prefix = format!("refresh-{}", &commit[..12.min(commit.len())]);
	let images: Vec<ImageRef> = if skip_build {
		require_local_sources(manifest, stale, commit)?;
		stale.iter().map(|&a| manifest.image(a)).collect()
	} else {
		build_commit(manifest, commit, stale, &prefix, force, job)?
	};
	require_current_base(&images, cache)?;
	let untouched: Vec<(Arch, String)> = release
		.members
		.iter()
		.filter(|m| !stale.contains(&m.arch))
		.map(|m| (m.arch, oci::by_digest(&repo, &m.digest)))
		.collect();
	let list = manifest.local_list_ref_tagged(&prefix);
	assemble(&list, &images, &untouched, name, &mut job.child("manifest list"))?;
	check_members(manifest, &list)?;
	push(manifest, &list, name, Some(commit), job)
}

/// Build `arches` of `commit` in a temporary worktree under that commit's own
/// config, tagged `<prefix>-<arch>` with the root project's name. Returns the
/// built images.
///
/// # Errors
///
/// Returns an error if the worktree or build fails, or the build didn't record
/// `commit`.
pub(crate) fn build_commit(
	manifest: &Manifest,
	commit: &str,
	arches: &[Arch],
	prefix: &str,
	force: bool,
	job: &mut Scope,
) -> Result<Vec<ImageRef>> {
	let wt = git::Worktree::add(manifest.root(), commit)?;
	let mut wt_manifest =
		Manifest::load_in(&wt.project(), &manifest.manifest_path).with_context(|| {
			format!(
				"loading {} from commit {} — a refresh builds each release under its own config",
				manifest.manifest_path.display(),
				oci::short(commit)
			)
		})?;
	// Tag with the root project's name, whatever the commit called it.
	wt_manifest.general.name.clone_from(&manifest.general.name);
	for &arch in arches {
		if wt_manifest.targets.types(arch).is_empty() {
			bail!(
				"commit {} doesn't build {arch} (`[targets]` there lists no {arch})",
				oci::short(commit)
			);
		}
	}
	build::preflight_arches(&wt_manifest, arches)?;
	let opts = BuildOpts {
		arches: arches.to_vec(),
		tag_prefix: prefix.to_owned(),
		no_cache: force,
		list: None,
	};
	let provenance = build::run_opts(
		&wt_manifest,
		&opts,
		&mut job.child(format!("build {}", oci::short(commit))),
	)?;
	if provenance.source.as_deref() != Some(commit) {
		bail!("the worktree build of {} didn't record it as its source", oci::short(commit));
	}
	signals::check()?;
	Ok(arches.iter().map(|&a| wt_manifest.image_tagged(a, prefix)).collect())
}

// ------------------------------------------------------------------ publish

/// `bootcher release publish`'s options.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PublishArgs {
	pub name: Option<String>,
	pub skip_build: bool,
	pub rebuild: bool,
	pub git_tag: Option<GitTag>,
	pub promote: Option<String>,
	pub remotes: Vec<String>,
	pub skip_remotes: bool,
	pub yes: bool,
}

/// `--git-tag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitTag {
	Push,
	Local,
	No,
}

impl GitTag {
	/// The `--git-tag` value.
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			GitTag::Push => "push",
			GitTag::Local => "local",
			GitTag::No => "no",
		}
	}
}

/// What the registry holds for `:X`, relative to HEAD.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublishState {
	/// `:X` doesn't exist: build and push it.
	Absent,
	/// `:X` records HEAD: nothing to build.
	AtHead(String),
	/// `:X` records another commit (or none): refuse.
	Other(Option<String>),
}

/// Decide what publishing over `:X` means given what it records.
pub(crate) fn publish_state(
	existing: Option<(&str, Option<&str>)>,
	head: Option<&str>,
) -> PublishState {
	match existing {
		None => PublishState::Absent,
		Some((digest, Some(src))) if Some(src) == head => PublishState::AtHead(digest.to_owned()),
		Some((_, src)) => PublishState::Other(src.map(str::to_owned)),
	}
}

/// The git-tag choices offered after a push on a TTY, "don't create" pre-selected
/// (the non-TTY default).
pub(crate) fn git_tag_choices(remote: Option<&str>) -> Vec<Choice<GitTag>> {
	let mut v = vec![
		Choice::new("don't create a git tag", GitTag::No, true),
		Choice::new("create it locally", GitTag::Local, false),
	];
	match remote {
		Some(r) => {
			v.push(Choice::new(format!("create it and push it to {r}"), GitTag::Push, false));
		}
		None => {
			v.push(Choice::disabled("create it and push it", GitTag::Push, "no remote to push to"));
		}
	}
	v
}

/// The promote choices offered after a push on a TTY: "don't promote" pre-selected,
/// then each channel.
pub(crate) fn promote_choices(channels: &[String]) -> Vec<Choice<Option<String>>> {
	std::iter::once(Choice::new("don't promote", None, true))
		.chain(
			channels.iter().map(|c| Choice::new(format!("promote to {c}"), Some(c.clone()), false)),
		)
		.collect()
}

/// `bootcher release publish X`: build every arch from a clean HEAD (or take the
/// local images with `--skip-build`) and push the signed index as `:X`; a no-op when
/// `:X` already records HEAD. Then optionally a git tag and a promote.
///
/// # Errors
///
/// Returns an error if the tree is dirty, the name is invalid, `:X` records another
/// commit, a prerequisite is missing, or a build/push/promote step fails.
#[allow(clippy::too_many_lines)]
pub fn publish(manifest: &Manifest, args: &PublishArgs) -> Result<()> {
	manifest.require_registry("`release publish`")?;
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	checks.bin("skopeo", oci::SKOPEO_HINT);
	checks.finish()?;
	if let Some(ch) = &args.promote {
		manifest.check_channel(ch)?;
	}
	let root = manifest.root();
	let head = git::head(root)?;
	if head.is_some() && !git::is_clean(root)? {
		bail!(
			"the work tree has uncommitted changes — a release records the commit it was built \
			 from, so commit or stash them first (`bootcher device deploy` ships a dirty tree to \
			 direct devices)"
		);
	}
	let mut prompted = false;
	let (name, existing) = loop {
		let name = match &args.name {
			Some(n) => n.clone(),
			None if interactive::is_tty() => {
				prompted = true;
				prompt_name(manifest, head.as_deref())?
			}
			None => bail!(
				"`release publish` needs the release name (e.g. `bootcher release publish v1.4`)"
			),
		};
		check_name(manifest, &name)?;
		if let Some(h) = &head
			&& let Some(at) = git::tag_commit(root, &name)?
			&& at != *h
		{
			bail!(
				"git tag `{name}` points at {}, not HEAD ({}) — check out {name}, or pick another name",
				oci::short(&at),
				oci::short(h)
			);
		}
		let existing = read(manifest, &name)?;
		let state = publish_state(
			existing.as_ref().map(|e| {
				(e.index.digest.as_str(), e.members.first().and_then(|m| m.source.as_deref()))
			}),
			head.as_deref(),
		);
		if let (Some(e), PublishState::AtHead(_)) = (&existing, &state) {
			e.source()?;
		}
		match &state {
			PublishState::Other(src) if prompted => {
				eprintln!(
					"{name} is already published from {} — pick another name",
					src.as_deref().map_or("an unrecorded commit", oci::short)
				);
			}
			_ => break (name, (existing, state)),
		}
	};
	let (_, state) = existing;
	let tag_ref = manifest.tag_ref(&name).context("a release ref")?;
	let mut promote = args.promote.clone();
	match &state {
		PublishState::Other(src) => bail!(
			"{tag_ref} records {} — a release only ever holds one commit; pick another name",
			src.as_deref().map_or_else(
				|| "no source commit".to_owned(),
				|s| format!("commit {}", oci::short(s))
			)
		),
		PublishState::AtHead(digest) if !args.rebuild => {
			eprintln!("{tag_ref} is already published from HEAD at {digest} — nothing to build");
			if prompted && promote.is_none() {
				let choice = interactive::select(
					&format!("Promote {name} instead?"),
					&promote_choices(manifest.channels()),
				)?;
				promote = choice;
			}
		}
		PublishState::Absent if head.is_none() => {
			eprintln!(
				"warning: not a git repository — {name} records no source, so it can never be refreshed or republished"
			);
		}
		_ => {}
	}
	// A `--git-tag` that can't be created fails here, before anything is pushed.
	if let Some(h) = &head
		&& matches!(args.git_tag, Some(GitTag::Push | GitTag::Local))
		&& git::tag_commit(root, &name)?.as_deref() != Some(h.as_str())
		&& !git::valid_tag_name(root, &name)?
	{
		bail!("`{name}` isn't a valid git tag name — pass `--git-tag no`, or pick another name");
	}
	let needs_push = matches!(state, PublishState::Absent) || args.rebuild;
	if needs_push {
		let mut b = Scope::root("publish", Some(2));
		let arches = manifest.targets.arches();
		if args.skip_build {
			match &head {
				Some(h) => require_local_sources(manifest, &arches, h)?,
				None => build::require_local(manifest, None)?,
			}
		} else {
			build::preflight(manifest, None)?;
			let provenance = build::run_opts(
				manifest,
				&BuildOpts {
					arches: arches.clone(),
					tag_prefix: crate::context::LATEST.to_owned(),
					no_cache: false,
					list: None,
				},
				&mut b.child("build"),
			)?;
			if provenance.source != head {
				bail!("the tree changed during the build — commit, then publish again");
			}
		}
		let list = manifest.local_list_ref();
		assemble(&list, &manifest.images(), &[], &name, &mut b.child("manifest list"))?;
		check_members(manifest, &list)?;
		push(manifest, &list, &name, head.as_deref(), &mut b.child("publish"))?;
	}

	// The git tag: image first, tag second.
	let tag_choice = match &head {
		Some(h) => git_tag(manifest, &name, h, args.git_tag, &mut prompted)?,
		None => None,
	};

	// Then "promote to…", when asked or (on a TTY with nothing given) offered.
	if promote.is_none() && args.name.is_none() && prompted && needs_push {
		promote = interactive::select("Promote it?", &promote_choices(manifest.channels()))?;
	}
	if prompted {
		Equivalent::new(&["release", "publish", &name])
			.flag_if(args.skip_build, "--skip-build")
			.flag_if(args.rebuild, "--rebuild")
			.opts("--git-tag", tag_choice.map(GitTag::as_str))
			.opts("--promote", promote.iter())
			.opts("--remote", promote.iter().flat_map(|_| &args.remotes))
			.flag_if(promote.is_some() && args.skip_remotes, "--skip-remotes")
			.flag_if(promote.is_some(), "-y")
			.print();
	}
	if let Some(ch) = promote {
		crate::jobs::channel::promote(
			manifest,
			&crate::jobs::channel::PromoteArgs {
				release: Some(name),
				from: None,
				channel: Some(ch),
				allow_stale_base: false,
				remotes: args.remotes.clone(),
				skip_remotes: args.skip_remotes,
				yes: args.yes,
			},
		)?;
	}
	Ok(())
}

/// The TTY release-name prompt: pre-filled with the git tag at HEAD (if exactly
/// one), validated as you type.
fn prompt_name(manifest: &Manifest, head: Option<&str>) -> Result<String> {
	let tags = if head.is_some() { git::tags_at_head(manifest.root())? } else { Vec::new() };
	let initial = match tags.as_slice() {
		[one] => one.clone(),
		_ => String::new(),
	};
	let m = manifest.clone();
	let name = inquire::Text::new("Release name:")
		.with_initial_value(&initial)
		.with_help_message("any valid OCI tag that isn't a channel, e.g. v1.4")
		.with_validator(move |s: &str| {
			Ok(match check_name(&m, s.trim()) {
				Ok(()) => inquire::validator::Validation::Valid,
				Err(e) => inquire::validator::Validation::Invalid(e.to_string().into()),
			})
		})
		.prompt()
		.prompted()?;
	Ok(name.trim().to_owned())
}

/// Create (and maybe push) the git tag `name` at HEAD per `flag`, or ask on a TTY.
/// Returns what was chosen, or `None` when the tag already points at HEAD.
fn git_tag(
	manifest: &Manifest,
	name: &str,
	head: &str,
	flag: Option<GitTag>,
	prompted: &mut bool,
) -> Result<Option<GitTag>> {
	let root = manifest.root();
	if git::tag_commit(root, name)?.as_deref() == Some(head) {
		return Ok(None);
	}
	let remote = git::push_remote(root)?;
	let choice = match flag {
		Some(f) => f,
		None if interactive::is_tty() => {
			*prompted = true;
			interactive::select(
				&format!("Create git tag {name} at HEAD?"),
				&git_tag_choices(remote.as_deref()),
			)?
		}
		None => GitTag::No,
	};
	if choice == GitTag::No {
		return Ok(Some(choice));
	}
	if !git::valid_tag_name(root, name)? {
		bail!("`{name}` isn't a valid git tag name — publish succeeded, but pass `--git-tag no`");
	}
	git::create_tag(root, name, &format!("bootcher release {name}"))?;
	eprintln!("created git tag {name}");
	if choice == GitTag::Push {
		match remote {
			Some(r) => {
				git::push_tag(root, &r, name)?;
				eprintln!("pushed git tag {name} to {r}");
			}
			None => eprintln!("warning: no remote to push git tag {name} to — it stays local"),
		}
	}
	Ok(Some(choice))
}

// ------------------------------------------------------------------ refresh

/// `bootcher release refresh X`: rebuild `:X` on the current base from its recorded
/// commit — its stale arches, or all with `force` (`--no-cache`) — and re-push it.
/// Moves no channel.
///
/// # Errors
///
/// Returns an error if `:X` is missing or records no source, or the rebuild/push
/// fails.
pub fn refresh(manifest: &Manifest, name: &str, force: bool, skip_build: bool) -> Result<()> {
	manifest.require_registry("`release refresh`")?;
	let mut checks = Checks::default();
	checks.bin("podman", preflight::PODMAN_HINT);
	checks.bin("skopeo", oci::SKOPEO_HINT);
	checks.bin("git", "check out the release's commit");
	checks.finish()?;
	let release = read(manifest, name)?.with_context(|| format!("{name} isn't published"))?;
	let commit = release.source()?.with_context(|| {
		format!(
			"{name} records no source commit, so it can't be rebuilt — publish a new release instead"
		)
	})?;
	let mut cache = BaseCache::default();
	let bases = release.bases(&mut cache);
	let stale: Vec<Arch> =
		bases.iter().filter(|(_, s)| force || s.is_stale()).map(|(a, _)| *a).collect();
	for (arch, s) in &bases {
		eprintln!("{name} {arch}: {}", s.describe());
	}
	if stale.is_empty() {
		eprintln!("{name}: every base is current — nothing to refresh (`--force` rebuilds anyway)");
		return Ok(());
	}
	let mut job = Scope::root("refresh", None);
	let digest = rebuild(
		manifest, name, &release, &commit, &stale, force, skip_build, &mut cache, &mut job,
	)?;
	eprintln!(
		"{name} refreshed at {digest}; promote it to move a channel (`bootcher channel promote {name} <channel>`)"
	);
	Ok(())
}

// ------------------------------------------------------------------ list

/// One `release list` row (also its JSON; template fields match
/// case-insensitively).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListRow {
	pub(crate) release: String,
	pub(crate) digest: String,
	pub(crate) commit: Option<String>,
	pub(crate) stale: bool,
	/// `base current` / `x86_64: base stale (…)` per arch, for the table.
	pub(crate) base: String,
	/// The channels serving this release (by digest).
	pub(crate) channels: Vec<String>,
	pub(crate) arches: Vec<crate::jobs::channel::ArchRow>,
}

/// `bootcher release list`: the tags whose index carries `bootcher.release`, with
/// their source commit, per-arch base status and the channels serving each.
/// Read-only.
///
/// # Errors
///
/// Returns an error if the registry can't be read or the format is invalid.
pub fn list(manifest: &Manifest, format: Option<&str>) -> Result<()> {
	manifest.require_registry("`release list`")?;
	let mut checks = Checks::default();
	checks.bin("skopeo", oci::SKOPEO_HINT);
	checks.finish()?;
	let repo = manifest.repo().context("a registry")?;
	let channel_digests: Vec<(String, String)> = manifest
		.channels()
		.iter()
		.filter_map(|c| {
			oci::fetch(&format!("{repo}:{c}")).ok().flatten().map(|f| (c.clone(), f.digest))
		})
		.collect();
	let mut cache = BaseCache::default();
	let mut rows = Vec::new();
	for (tag, _) in releases(manifest)? {
		let Some(p) = read(manifest, &tag)? else { continue };
		let arches: Vec<crate::jobs::channel::ArchRow> = p
			.bases(&mut cache)
			.into_iter()
			.zip(&p.members)
			.map(|((arch, base), m)| crate::jobs::channel::ArchRow {
				arch: arch.to_string(),
				digest: m.digest.clone(),
				base,
			})
			.collect();
		let stale = arches.iter().any(|a| a.base.is_stale());
		let base = if stale {
			arches
				.iter()
				.filter(|a| a.base.is_stale())
				.map(|a| format!("{}: {}", a.arch, a.base.describe()))
				.collect::<Vec<_>>()
				.join("; ")
		} else {
			"base current".to_owned()
		};
		rows.push(ListRow {
			channels: channel_digests
				.iter()
				.filter(|(_, d)| *d == p.index.digest)
				.map(|(c, _)| c.clone())
				.collect(),
			commit: p.source().ok().flatten(),
			digest: p.index.digest.clone(),
			release: tag,
			stale,
			base,
			arches,
		});
	}
	println!(
		"{}",
		crate::jobs::device::render_rows(
			&rows,
			format,
			&["release", "commit", "channels", "base"]
		)?
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn manifest() -> Manifest {
		toml::from_str(
			"[general]\nname = \"k\"\n[registry]\nurl = \"r.io/o\"\nchannels = [\"stable\"]\n",
		)
		.unwrap()
	}

	#[test]
	fn release_names_follow_the_tag_grammar_and_avoid_channels_and_reserved_prefixes() {
		let m = manifest();
		assert!(check_name(&m, "v1.4").is_ok());
		assert!(check_name(&m, "2026.11").is_ok());
		assert!(check_name(&m, "stable").unwrap_err().to_string().contains("declared channel"));
		assert!(check_name(&m, "sha256-abc.sig").unwrap_err().to_string().contains("reserved"));
		assert!(check_name(&m, "ci-abc-x86_64").unwrap_err().to_string().contains("reserved"));
		assert!(check_name(&m, "-x").unwrap_err().to_string().contains("valid OCI tag"));
		assert!(check_name(&m, "a/b").is_err());
		assert!(!is_release_candidate(&m, "stable") && is_release_candidate(&m, "v1"));
	}

	#[test]
	fn publish_state_follows_the_one_rule() {
		assert_eq!(publish_state(None, Some("h")), PublishState::Absent);
		assert_eq!(publish_state(None, None), PublishState::Absent);
		assert_eq!(
			publish_state(Some(("sha256:d", Some("h"))), Some("h")),
			PublishState::AtHead("sha256:d".into())
		);
		assert_eq!(
			publish_state(Some(("sha256:d", Some("o"))), Some("h")),
			PublishState::Other(Some("o".into()))
		);
		// A release without a recorded source can never be overwritten, git or not.
		assert_eq!(publish_state(Some(("sha256:d", None)), Some("h")), PublishState::Other(None));
		assert_eq!(publish_state(Some(("sha256:d", None)), None), PublishState::Other(None));
	}

	#[test]
	fn after_push_choices_preselect_the_non_tty_default() {
		let c = git_tag_choices(Some("origin"));
		assert_eq!(interactive::start_cursor(&c), Some(0));
		assert_eq!(c[0].value, GitTag::No);
		assert_eq!(c[2].label, "create it and push it to origin");
		let c = git_tag_choices(None);
		assert!(c[2].disabled.is_some());
		let p = promote_choices(&["stable".into(), "next".into()]);
		assert_eq!(p[0].value, None);
		assert!(p[0].checked);
		assert_eq!(p[2].value.as_deref(), Some("next"));
	}

	fn member(arch: Arch, source: Option<&str>) -> Member {
		Member {
			arch,
			digest: format!("sha256:{arch}"),
			source: source.map(str::to_owned),
			manifest: oci::Manifest::default(),
		}
	}

	fn published(members: Vec<Member>) -> Published {
		Published {
			index: oci::Fetched { digest: "sha256:i".into(), manifest: oci::Manifest::default() },
			members,
		}
	}

	#[test]
	fn a_release_source_is_the_commit_every_member_records() {
		let p = published(vec![member(Arch::X86_64, Some("c")), member(Arch::Aarch64, Some("c"))]);
		assert_eq!(p.source().unwrap().as_deref(), Some("c"));
		let p = published(vec![member(Arch::X86_64, None), member(Arch::Aarch64, None)]);
		assert_eq!(p.source().unwrap(), None);
		let p = published(vec![member(Arch::X86_64, Some("c")), member(Arch::Aarch64, Some("d"))]);
		assert!(p.source().is_err());
		let p = published(vec![member(Arch::X86_64, Some("c")), member(Arch::Aarch64, None)]);
		assert!(p.source().is_err());
	}
}
