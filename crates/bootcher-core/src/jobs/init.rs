//! `bootcher init [name]`: scaffold a new project directory from the embedded
//! template — the standalone-CLI entry point.
//!
//! A project is a single image: a `Containerfile` starting `FROM fedora-bootc`
//! with the hardening baseline layered inline, a `sysroot/` overlay, and a
//! `bootcher.toml` naming the image. The template is embedded at compile time so
//! a `cargo install` of the CLI carries everything `init` needs — no network, no
//! repo checkout. Once stamped out the project is fully user-owned and editable.
//!
//! Destination: a `path` is created and scaffolded — missing parent dirs are
//! made, and `.`/`..` are resolved — with its final component as the image name.
//! Omit it to scaffold the **current directory** in place, which must be empty
//! (hard fail otherwise). `-f`/`--force` lifts both the "already exists" and the
//! "must be empty" guards, landing the scaffold in place and overwriting any
//! files it collides with.
//!
//! Interaction: `-y`/`--yes` is non-interactive (host platform, LAN deploys,
//! all-`local` builders); a TTY without `-y` runs a short questionnaire
//! (platform, registry, signing, deploy targets, build/image builders per arch);
//! a non-TTY without `-y` is a hard fail (can't prompt, and defaults weren't
//! opted into).

use crate::builder;
use crate::context::{
	Arch, ArchBuilder, BuilderConfig, BuilderSpec, ConcurrencyConfig, DeployConfig, DiskType,
	DiskTypes, General, Hooks, MANIFEST, Manifest, RegistryConfig, RemoteConfig, Rootfs, SCHEMA,
	SCHEMA_DIRECTIVE, manifest_schema_json,
};
use crate::hooks::{METADATA_SCHEMA, metadata_schema_json};
use anyhow::{Context, Result, bail};
use include_dir::{Dir, include_dir};
use std::ffi::OsStr;
use std::fs;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use strum::VariantArray;

/// The project template, embedded verbatim from the crate's `scaffold/` dir.
/// Carries only template (`*.template`) credentials — never a rendered secret
/// (see the scaffold `.gitignore`).
static SCAFFOLD: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/scaffold");

/// Commented per-arch builder note appended to the scaffolded manifest, after the
/// live `[builder]` table. The flat `build`/`image` apply to every target arch in
/// `[general.disk_types]`; a `[builder.<arch>]` subtable overrides either role for
/// one arch. The questionnaire writes these when the answers differ per arch; this
/// comment documents the syntax for any manual additions the user might want later.
const BUILDER_OVERRIDE_EXAMPLE: &str = "\
# Per-arch builder override: the [builder] build/image above are the defaults for
# every target arch. Add a [builder.<arch>] table to override one or both
# roles for a specific arch — e.g. on an x86_64 host building both arches, keep
# x86_64 native while routing the aarch64 image step to a VM:
#
# [builder.aarch64]
# image = \"vm\"
#
# Only the keys you set here override the flat default; the other role inherits it.
";

/// Commented `[hooks]` block appended to the scaffolded manifest. The table is
/// omitted from serialization when empty (see [`Hooks::is_empty`]), so this is
/// where `init` documents the extension point. Kept as a comment rather than a
/// live table so a fresh project has *no* hooks until the user opts in.
const HOOKS_EXAMPLE: &str = "\
# Lifecycle hooks (optional): shell commands bootcher runs around its phases —
# the container build, the disk-image step, and the deploy upgrade. This is the
# extension point for board- or project-specific work; bootcher itself stays
# generic. Each command runs with `sh -c` from this directory, with the progress
# UI suspended and the terminal handed over (so it can print, prompt, or `sudo`);
# a non-zero exit aborts the run. Each phase has an optional pre/post pair, and
# fires wherever that phase runs (build also during provision/deploy, image also
# during provision, upgrade also during deploy).
#
# Each hook gets a BOOTCHER_METADATA env var (JSON) describing the phase and the
# artifacts it concerns — e.g. disk.post lists each built disk's resolved path, so
# a recipe can post-process it without knowing bootcher's output layout. See the
# bootcher.toml reference and recipes/ for the schema and ready-made hooks.
#
# [hooks.build]     # around the container build
# pre  = \"echo REPLACEME\"
# post = \"echo REPLACEME\"
#
# [hooks.disk]      # around image-builder (the disk image)
# pre  = \"echo REPLACEME\"
# post = \"echo REPLACEME\"
#
# [hooks.upgrade]   # around the deploy push + per-device `bootc upgrade`
# pre  = \"echo REPLACEME\"
# post = \"echo REPLACEME\"
";

/// Commented `[concurrency]` block appended to the scaffolded manifest. bootcher
/// runs each of its fan-outs (the multi-arch build/image steps, the per-device
/// upgrade/rotate rollouts) with as many workers as the host has cores; this is
/// where a constrained machine can cap them. Kept as comments (like the hooks)
/// since the default — unbounded — suits most projects.
const CONCURRENCY_EXAMPLE: &str = "\
# Parallelism caps (optional): bootcher builds every target arch and rolls
# out to every device in parallel, by default using up to one worker per host
# core. On a constrained machine — tight on RAM/CPU, or where two cross-arch
# builder VMs at once would thrash — cap an activity here. Each value is a max
# worker count (>= 1); an unset key means unbounded.
#
# Each key defaults to unbounded; the values below are example caps, not defaults.
# [concurrency]
# build    = 2  # arches building containers at once       (default: unbounded)
# disk     = 1  # disk images at once (one builder VM boot) (default: unbounded)
# upgrade  = 4  # devices upgrading at once                 (default: unbounded)
# rotate   = 4  # devices rotating their token at once      (default: unbounded)
# takeover = 2  # live hosts being converted to bootc       (default: unbounded)
";

/// Signing hint appended to the scaffolded manifest when a plain-URL registry is
/// configured — documents how to opt into signing. Omitted in LAN mode (nothing to
/// expand) and when signing is already live in the rendered manifest.
const SIGNING_EXAMPLE: &str = "\
# Image signing (optional): sign pushed images with a cosign key and make devices
# reject unsigned/tampered ones on `bootc upgrade`. To enable, run:
#   bootcher sign enroll
# which writes cosign.key/.pub, patches `[deploy] registry` to the signing form, and
# bakes sysroot/usr/lib/bootc/install/30-bootcher-signing.toml so a freshly
# provisioned device enforces signatures from first boot. The passphrase comes from
# BOOTCHER_SIGN_PASSPHRASE (or a prompt); the public key is injected into each device
# at `provision`. Rotate a leaked/expiring key with `bootcher rotate sign-key`.
";

/// Resolve the destination first (so an invalid/occupied target fails before any
/// prompting), collect the manifest settings per interaction mode, then scaffold.
///
/// # Errors
///
/// Returns an error if the target directory can't be created, a prompt fails, or
/// any file can't be written.
pub fn run(name: Option<String>, yes: bool, force: bool) -> Result<()> {
	let target = Target::resolve(name, force)?;
	let settings = collect(yes)?;
	target.write(&settings)
}

/// The manifest settings gathered per interaction mode, beyond the project name
/// (which the destination path supplies). Mirrors the `bootcher.toml` shape.
struct Settings {
	/// The build matrix (`[general.disk_types]`): each target arch mapped to its
	/// image-builder output format(s). `-y` seeds the host arch with a single qcow2;
	/// the questionnaire picks the arches, then a format list per arch.
	disk_types: DiskTypes,
	/// Root filesystem to format (`[general] rootfs`).
	rootfs: Rootfs,
	registry: Option<String>,
	/// Whether to enable opt-in image signing. Only offered in registry mode; when
	/// set, `write` calls `signing::enable_signing` to generate the keypair, upgrade
	/// `registry` to the signing inline-table form, and bake the
	/// `enforce-container-sigpolicy` install config so first boot enforces it.
	signing: bool,
	/// SSH deploy targets collected at init time (`[deploy] remotes`). Each is a
	/// bare connection string (`[user@]host` or `ssh://[user@]host[:port]`) with
	/// no per-target SSH options — those can be added by hand later.
	remotes: Vec<String>,
	/// Builder config for the `[builder]` table; always concrete so the scaffolded
	/// manifest records every key explicitly.
	builder: BuilderConfig,
}

/// Where the scaffold lands, plus the derived project name.
struct Target {
	dest: PathBuf,
	name: String,
	/// True when scaffolding the (empty) cwd in place rather than a fresh subdir.
	in_place: bool,
}

impl Target {
	/// A `path` targets a fresh directory (with any missing parents); no path
	/// targets the cwd, which must be empty. Either way the project name is the
	/// path's final component (after resolving `.`/`..`). `force` lets the scaffold
	/// land in an existing/non-empty directory, overwriting colliding files.
	fn resolve(path: Option<String>, force: bool) -> Result<Self> {
		// A path → create it (and any missing parents); the leaf is the image name.
		if let Some(path) = path {
			let dest = PathBuf::from(&path);
			if dest.exists() && !force {
				bail!(
					"'{path}' already exists — pick a path that doesn't, or pass `--force` to \
					 scaffold into it anyway (overwriting colliding files)"
				);
			}
			let name = project_name(&dest)?;
			return Ok(Self { dest, name, in_place: false });
		}

		// No path → the cwd, in place, which must be empty (unless forced).
		let cwd = std::env::current_dir().context("getting the current directory")?;
		if !force && fs::read_dir(&cwd).context("reading the current directory")?.next().is_some() {
			bail!(
				"`bootcher init` with no path scaffolds the current directory, but it \
				 isn't empty.\nRun it in an empty directory, pass a path \
				 (`bootcher init <path>`), or pass `--force` to scaffold here anyway \
				 (overwriting colliding files)."
			);
		}
		let name = project_name(&cwd)?;
		Ok(Self { dest: cwd, name, in_place: true })
	}

	/// Create the dir + any missing parents (unless in place), extract the
	/// scaffold, and write the manifest.
	fn write(&self, settings: &Settings) -> Result<()> {
		if !self.in_place {
			fs::create_dir_all(&self.dest)
				.with_context(|| format!("creating {}", self.dest.display()))?;
		}
		extract(&SCAFFOLD, &self.dest)?;

		// Render the manifest by serialising the real `Manifest` struct (rather than
		// hand-building TOML) so the file stays in lockstep with the schema and lists
		// every builder key explicitly — the scaffold doubles as documentation. The
		// embedded files stay project-agnostic. `annotate_values` then prepends each
		// enumerated key with a `# a, b, c` hint of its accepted values.
		// Build the registry config: always a plain URL here (or absent in LAN mode).
		// When signing is enabled, `signing::enable_signing` below upgrades this entry
		// to the inline-table signing form — the single code path that wires signing.
		let registry_config = settings.registry.clone().map(RegistryConfig::Url);
		let manifest = Manifest {
			// A scaffold is a standalone base, never an `extend` override.
			extend: None,
			general: General {
				name: self.name.clone(),
				rootfs: settings.rootfs,
				disk_types: settings.disk_types.clone(),
			},
			builder: settings.builder.clone(),
			deploy: DeployConfig {
				registry: registry_config,
				remotes: settings
					.remotes
					.iter()
					.map(|r| RemoteConfig::ConnStr(r.clone()))
					.collect(),
			},
			// Unbounded by default (each fan-out scales to the host's cores);
			// `ConcurrencyConfig::is_empty` keeps the `[concurrency]` table out of the
			// serialized manifest, and `CONCURRENCY_EXAMPLE` documents it as comments.
			concurrency: ConcurrencyConfig::default(),
			// No hooks by default; `Hooks::is_empty` keeps the `[hooks]` table out of
			// the serialized manifest, and `HOOKS_EXAMPLE` documents it as comments
			// instead (an empty table would read as "configured but blank").
			hooks: Hooks::default(),
		};
		let rendered =
			annotate_values(&toml::to_string(&manifest).context("serialising the manifest")?);
		let manifest_path = self.dest.join(MANIFEST);
		// Show the signing example only when a plain-URL registry is configured —
		// it shows how to expand it. Irrelevant in LAN mode (no registry to expand)
		// and redundant when signing is already live in the rendered manifest.
		let signing_doc =
			if settings.registry.is_some() && !settings.signing { SIGNING_EXAMPLE } else { "" };
		// Emit the JSON Schemas (generated from the live types, so they can't drift)
		// into `schemas/`: the manifest schema, bound to the manifest by a first-line
		// `#:schema` directive so editors (Taplo / Even Better TOML) validate and
		// autocomplete bootcher.toml; and the BOOTCHER_METADATA schema, reference docs
		// for hook/recipe authors. `SCHEMA`/`METADATA_SCHEMA` share the `schemas/` dir.
		let schema_path = self.dest.join(SCHEMA);
		if let Some(parent) = schema_path.parent() {
			fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
		}
		fs::write(&schema_path, manifest_schema_json())
			.with_context(|| format!("writing {}", schema_path.display()))?;
		let metadata_schema_path = self.dest.join(METADATA_SCHEMA);
		fs::write(&metadata_schema_path, metadata_schema_json())
			.with_context(|| format!("writing {}", metadata_schema_path.display()))?;
		fs::write(
			&manifest_path,
			format!(
				"{SCHEMA_DIRECTIVE}\n{rendered}{BUILDER_OVERRIDE_EXAMPLE}{CONCURRENCY_EXAMPLE}{HOOKS_EXAMPLE}{signing_doc}"
			),
		)
		.with_context(|| format!("writing {}", manifest_path.display()))?;

		// With signing enabled, fully wire it now (single path, shared with
		// `bootcher sign enroll`): generate the keypair, upgrade `[deploy] registry`
		// to the signing form, and bake the enforce-container-sigpolicy install config
		// so first boot enforces signatures. Prompts for a passphrase (this is an
		// interactive run — non-interactive `-y` leaves signing off). Doing it here
		// means a signed scaffold is never left referencing a key that doesn't exist.
		if settings.signing {
			crate::jobs::signing::enable_signing(&self.dest, "cosign", false)
				.context("enrolling the signing key")?;
		}

		let where_ = if self.in_place {
			"the current directory".to_owned()
		} else {
			format!("{}/", self.dest.display())
		};
		let cd =
			if self.in_place { String::new() } else { format!("  cd {}\n", self.dest.display()) };
		// Signing is already enrolled above; note the key so the user keeps it secret.
		let sign = if settings.signing {
			"  # signing enrolled: keep cosign.key secret (the scaffold .gitignore covers *.key)\n"
		} else {
			""
		};
		eprintln!(
			"init: scaffolded {where_} as '{}'\n\nNext:\n{cd}{sign}  \
			 # edit Containerfile / bootcher.toml ([general.disk_types] sets target arches + formats), then:\n  \
			 bootcher provision   # build the disk image (prompts for your admin SSH key)\n  \
			 # the artifacts land under output/<arch>/ (the image grows its root FS on first boot)",
			self.name,
		);
		Ok(())
	}
}

/// Collect the manifest settings per interaction mode (see the module docs).
fn collect(yes: bool) -> Result<Settings> {
	if yes {
		// Defaults: host arch (so never cross-arch) building a single qcow2, ext4
		// rootfs, LAN deploys, all-`local` builders.
		let mut disk_types = DiskTypes::default();
		disk_types.set(default_platform(), vec![DiskType::Qcow2]);
		Ok(Settings {
			disk_types,
			rootfs: Rootfs::Ext4,
			registry: None,
			// No registry under `-y`, so signing (registry-only) is off.
			signing: false,
			remotes: Vec::new(),
			builder: BuilderConfig::default(),
		})
	} else if std::io::stdin().is_terminal() {
		questionnaire()
	} else {
		bail!(
			"`bootcher init` needs a TTY for its interactive setup.\n\
			 Run it in a terminal, or pass `-y` to scaffold with defaults."
		);
	}
}

/// Interactive setup: prompt for the bits the manifest needs beyond the name.
#[allow(clippy::too_many_lines)]
fn questionnaire() -> Result<Settings> {
	let host = default_platform();
	let options = vec![Arch::Aarch64, Arch::X86_64];
	let start = options.iter().position(|a| *a == host).unwrap_or(0);
	// Multi-select: pick one arch (the common case) or several for a multi-arch
	// image, with the host arch pre-checked. At least one is required.
	let platform = inquire::MultiSelect::new("Target platform(s):", options)
		.with_default(&[start])
		.with_help_message("space to toggle, enter to confirm; pick several for a multi-arch image")
		.prompt()?;
	if platform.is_empty() {
		bail!("pick at least one target platform");
	}

	// Image format(s) per arch — matching the per-arch `[general.disk_types]` table.
	// Each arch gets its own list, so one arch can build (say) a qcow2 to test in a VM
	// while another builds a raw for a device. Proxied straight to image-builder; offer
	// every value the enums know, with qcow2 pre-checked.
	let qcow2_at = DiskType::VARIANTS.iter().position(|t| *t == DiskType::Qcow2).unwrap_or(0);
	let mut disk_types = DiskTypes::default();
	for &arch in &platform {
		let chosen = inquire::MultiSelect::new(
			&format!("Image format(s) for {arch}:"),
			DiskType::VARIANTS.to_vec(),
		)
		.with_default(&[qcow2_at])
		.with_help_message(
			"space to toggle, enter to confirm; qcow2 boots as-is in a VM, raw writes to a block device, bootc-installer makes an installer ISO",
		)
		.prompt()?;
		if chosen.is_empty() {
			bail!("pick at least one image format for {arch}");
		}
		disk_types.set(arch, chosen);
	}

	let rootfses = Rootfs::VARIANTS.to_vec();
	let start = rootfses.iter().position(|r| *r == Rootfs::Ext4).unwrap_or(0);
	let rootfs =
		inquire::Select::new("Root filesystem (image-builder --bootc-default-fs):", rootfses)
			.with_starting_cursor(start)
			.prompt()?;

	let registry = inquire::Text::new("Registry namespace (blank = LAN deploys over SSH):")
		.with_help_message(
			"e.g. registry.gitlab.com/org/project — switches deploy to registry mode",
		)
		.prompt()?;
	let registry = Some(registry.trim().to_owned()).filter(|s| !s.is_empty());

	// Image signing is registry-mode only (a LAN deploy's integrity rides the ssh
	// channel), so only offer it when a registry is set. Saying yes enrolls the
	// signing key right away (generates the keypair — prompting for a passphrase —
	// expands the registry to the signing form, and bakes the
	// enforce-container-sigpolicy install config), so no separate step is needed.
	let signing = registry.is_some()
		&& inquire::Confirm::new("Enforce image signatures on devices?")
			.with_default(false)
			.with_help_message(
				"signs pushes with a cosign key and makes devices reject unsigned/tampered \
				 images; enrolls a cosign key now (prompts for a passphrase)",
			)
			.prompt()?;

	// Collect deploy targets: plain connection strings only (no per-remote SSH
	// options — those can be added by hand in the manifest later). An empty entry
	// ends the loop; the list may be left empty and filled in afterward.
	let mut remotes: Vec<String> = Vec::new();
	loop {
		let prompt = if remotes.is_empty() {
			"Deploy target (blank to skip / finish):"
		} else {
			"Another deploy target (blank to finish):"
		};
		let raw = inquire::Text::new(prompt)
			.with_initial_value("admin@")
			.with_help_message(
				"type the IP or hostname after admin@; edit the prefix to change the user or use a full ssh:// URL",
			)
			.prompt()?;
		let raw = raw.trim().to_owned();
		if raw.is_empty() || raw == "admin@" {
			break;
		}
		remotes.push(raw);
	}

	// Ask build/image per selected arch so the user can route each one independently
	// (e.g. keep x86_64 `local` while sending the aarch64 image step to a `vm`).
	// For a single arch the question is identical to before; for multiple arches each
	// gets its own pair of prompts, labelled by arch. The first arch's answers become
	// the flat `[builder]` defaults; later arches that differ get a `[builder.<arch>]`
	// override table.
	let builder_config = {
		let mut specs: Vec<(Arch, String, String)> = Vec::new();
		for &arch in &platform {
			let cross_arch = Arch::host() != Some(arch);
			let build_q = if platform.len() == 1 {
				"Where should the container build run?".to_owned()
			} else {
				format!("[{arch}] Where should the container build run?")
			};
			let image_q = if platform.len() == 1 {
				"Where should the disk image (image-builder) step run?".to_owned()
			} else {
				format!("[{arch}] Where should the disk image (image-builder) step run?")
			};
			let build = builder::prompt_spec(&build_q, builder::BuilderRole::Build, cross_arch)?;
			let image = builder::prompt_spec(&image_q, builder::BuilderRole::Image, cross_arch)?;
			specs.push((arch, build, image));
		}
		// The first arch's values are the flat defaults; any arch that differs gets a
		// per-arch override (only the differing role is written to avoid redundancy).
		let flat_build = specs[0].1.clone();
		let flat_image = specs[0].2.clone();
		let arch_override = |target: Arch| -> Option<ArchBuilder> {
			let (_, b, i) = specs.iter().find(|(a, ..)| *a == target)?;
			let ob = (b != &flat_build).then(|| BuilderSpec::Str(b.clone()));
			let oi = (i != &flat_image).then(|| BuilderSpec::Str(i.clone()));
			(ob.is_some() || oi.is_some()).then_some(ArchBuilder { build: ob, image: oi })
		};
		let x86_64 = arch_override(Arch::X86_64);
		let aarch64 = arch_override(Arch::Aarch64);
		BuilderConfig {
			build: BuilderSpec::Str(flat_build),
			image: BuilderSpec::Str(flat_image),
			x86_64,
			aarch64,
		}
	};

	Ok(Settings { disk_types, rootfs, registry, signing, remotes, builder: builder_config })
}

/// The `platform` default: the host arch, or `x86_64` on an arch bootcher doesn't
/// build for (a deliberate, overridable fallback).
fn default_platform() -> Arch {
	Arch::host().unwrap_or(Arch::X86_64)
}

/// The project (image) name for a target path: its final component after
/// lexically resolving `.`/`..` against the cwd. Bails for a path with no real
/// final segment (e.g. the filesystem root).
fn project_name(dest: &Path) -> Result<String> {
	let cwd = std::env::current_dir().context("getting the current directory")?;
	let abs = if dest.is_absolute() { dest.to_path_buf() } else { cwd.join(dest) };

	// Lexical normalization (the leaf doesn't exist yet, so we can't canonicalize):
	// keep a stack of real segments, popping on `..` and skipping `.`.
	let mut stack: Vec<&OsStr> = Vec::new();
	for comp in abs.components() {
		match comp {
			Component::Normal(s) => stack.push(s),
			Component::ParentDir => {
				stack.pop();
			}
			Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
		}
	}
	stack
		.last()
		.and_then(|s| s.to_str())
		.map(str::to_owned)
		.with_context(|| format!("can't derive a project name from '{}'", dest.display()))
}

/// Prepend each enumerated manifest key in `rendered` with a `# a, b, c` comment
/// listing its accepted values, so the scaffolded `bootcher.toml` documents the
/// options inline (the user's first edits are picking among known values, not
/// guessing them). Keys whose value is free text — `name`, the registry URL, the
/// `remotes` connection strings — are left bare; only constrained keys get a hint.
///
/// The enum hints are pulled from the live `VariantArray`s (so they can't drift
/// from what the parser accepts); the builder roles are prose, since a spec can be
/// an arbitrary ssh destination beyond the `local`/`vm` literals. Keys are matched
/// under their table (tracked as we stream the lines) so the same key name in two
/// tables can't cross-annotate.
fn annotate_values(rendered: &str) -> String {
	fn variants<T: std::fmt::Display>(vs: &[T]) -> String {
		vs.iter().map(T::to_string).collect::<Vec<_>>().join(", ")
	}
	// A builder spec is one of these two literals or a free-text ssh destination,
	// so this hint is prose rather than an enum listing.
	const BUILDER_SPEC: &str = "local, vm, [user@]<host>";
	let disk_types = variants(DiskType::VARIANTS);
	let rootfses = variants(Rootfs::VARIANTS);

	let mut out = String::with_capacity(rendered.len() + 256);
	let mut table = "";
	for line in rendered.lines() {
		let trimmed = line.trim();
		if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
			table = name;
		}
		let key = trimmed.split('=').next().unwrap_or_default().trim();
		let hint = match table {
			"general" => match key {
				"rootfs" => Some(rootfses.as_str()),
				_ => None,
			},
			// Each key in this table is a target arch; its value is the disk type(s) to
			// build for it. Hint the accepted types on the arch keys only — not the
			// table header or blank lines, which also stream through here.
			"general.disk_types" => {
				matches!(key, "x86_64" | "aarch64").then_some(disk_types.as_str())
			}
			// Only the flat [builder] table — the [builder.<arch>] override tables sit
			// right below it, so re-stating the values there would just be noise.
			"builder" => matches!(key, "build" | "image").then_some(BUILDER_SPEC),
			_ => None,
		};
		if let Some(hint) = hint {
			out.push_str("# ");
			out.push_str(hint);
			out.push('\n');
		}
		out.push_str(line);
		out.push('\n');
	}
	out
}

/// Write an embedded directory tree under `dest`. Each file's `path()` is
/// relative to the scaffold root, so joining it onto `dest` (and creating parent
/// dirs) reproduces the layout regardless of nesting.
fn extract(dir: &Dir, dest: &Path) -> Result<()> {
	for file in dir.files() {
		let target = dest.join(file.path());
		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
		}
		fs::write(&target, file.contents())
			.with_context(|| format!("writing {}", target.display()))?;
	}
	for sub in dir.dirs() {
		extract(sub, dest)?;
	}
	Ok(())
}
