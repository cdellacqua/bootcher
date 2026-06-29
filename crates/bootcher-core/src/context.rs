use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use crate::ssh::Ssh;

/// The per-project manifest filename, read from the cwd. Its presence marks a
/// directory as a bootcher project; [`Manifest::load`] parses it.
pub const MANIFEST: &str = "bootcher.toml";

/// Directory `bootcher init` writes the generated JSON Schemas into (the manifest
/// schema [`SCHEMA`] and the hook-metadata schema), keeping them out of the
/// project root. Relative to the project root.
pub const SCHEMA_DIR: &str = "schemas";

/// JSON Schema path `bootcher init` writes under [`SCHEMA_DIR`], for editor
/// validation/autocomplete. Referenced from the manifest's first line via the
/// Taplo `#:schema` directive (see [`SCHEMA_DIRECTIVE`]); the relative path
/// resolves against the manifest's own directory, so the schema needs no hosting.
pub const SCHEMA: &str = "schemas/bootcher.schema.json";

/// First line `bootcher init` prepends to the scaffolded [`MANIFEST`]: the Taplo
/// `#:schema` directive binding the file to [`SCHEMA`]. Honoured by Taplo / the
/// "Even Better TOML" editor extension; ignored by other TOML tooling.
pub const SCHEMA_DIRECTIVE: &str = "#:schema ./schemas/bootcher.schema.json";

/// Render the [`Manifest`] type as a pretty-printed JSON Schema (draft 2020-12).
/// Generated from the same serde-derived structs the parser uses, so the schema
/// can't drift from what [`Manifest::load`] accepts. `bootcher init` writes the
/// result to [`SCHEMA`] beside the scaffolded manifest.
///
/// # Panics
///
/// Never in practice: a `schemars` `Schema` is plain JSON values, whose
/// `Serialize` is infallible — the `expect` guards a case that can't arise.
#[must_use]
pub fn manifest_schema_json() -> String {
	let schema = schemars::schema_for!(Manifest);
	// Pretty so the emitted file is human-diffable; infallible for a `Schema`.
	serde_json::to_string_pretty(&schema).expect("a JSON Schema always serializes")
}

/// Device-side path bootc reads the registry pull secret from. Not part of the
/// image: injected into the device's persistent `/etc` at provision (a blueprint
/// `customizations.files` entry), where the ostree `/etc` 3-way merge preserves
/// it across upgrades since no image ships a `/usr/etc` default for it.
pub const DEVICE_AUTH_JSON: &str = "/etc/ostree/auth.json";

/// Device-side authorized-keys path for the admin user, matching the scaffold's
/// `AuthorizedKeysFile` sshd drop-in. Injected at provision like
/// [`DEVICE_AUTH_JSON`].
pub const DEVICE_ADMIN_AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys.d/admin";

/// Parent directory of [`DEVICE_ADMIN_AUTHORIZED_KEYS`]. The scaffold's
/// `Containerfile` creates this directory (matching the `AuthorizedKeysFile` sshd
/// drop-in), so the blueprint's `customizations.files` entry for the key finds the parent
/// present without needing a separate `customizations.directories` entry.
pub const DEVICE_ADMIN_AUTHORIZED_KEYS_DIR: &str = "/etc/ssh/authorized_keys.d";

/// Device-side signature-verification policy the containers/image stack consults
/// on every image pull. fedora-bootc ships a permissive `insecureAcceptAnything`
/// default; in signing mode provision overwrites it with one that requires a
/// valid cosign signature for the project's registry namespace, and bootc records
/// a signature-enforcing origin (`bootc switch --enforce-container-sigpolicy` /
/// the baked install config) so `bootc upgrade` honours it. Injected into
/// persistent `/etc` like [`DEVICE_AUTH_JSON`] (3-way merged across upgrades,
/// editable over ssh by `rotate sign-key`).
pub const DEVICE_POLICY_JSON: &str = "/etc/containers/policy.json";

/// Device-side `containers-registries.d` directory; in signing mode provision
/// drops a per-registry config here enabling `use-sigstore-attachments` so the
/// cosign signature is fetched alongside the image on pull.
pub const DEVICE_REGISTRIES_D: &str = "/etc/containers/registries.d";

/// Device-side directory holding the cosign **public** key(s) the policy verifies
/// signatures against (the `keyPath`/`keyPaths` in [`DEVICE_POLICY_JSON`]).
pub const DEVICE_COSIGN_PUBKEY_DIR: &str = "/etc/pki/containers";

/// Target CPU architecture. Read from the `[targets]` keys (defaulted
/// to the host), and used to derive the podman `--platform` flag, the
/// `image-builder` `--arch`, and the per-arch tag/output suffix.
///
/// Variants use the **uname-style** spellings (`aarch64` / `x86_64`) — the
/// lowercase rename feeds both `strum` (`Display`) and `serde` (the manifest
/// `[targets]` keys). Both podman (`--platform linux/<arch>`, which it normalises) and
/// `image-builder` (`--arch`) accept these verbatim, so a single spelling flows
/// everywhere — manifest, container tags, output dirs, and both tools' flags —
/// with no per-tool translation.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	strum::Display,
	strum::EnumString,
	strum::VariantArray,
	serde::Deserialize,
	serde::Serialize,
	schemars::JsonSchema,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
// `aarch64` / `x86_64` are the canonical lowercase arch spellings; the Rust
// upper-camel-case convention doesn't fit them.
#[allow(non_camel_case_types)]
pub enum Arch {
	Aarch64,
	X86_64,
}

impl Arch {
	/// Canonical OCI architecture name (`amd64`/`arm64`) — what appears in an
	/// image config and a manifest list's `platform.architecture`. It's the value
	/// every podman invocation passes via `--arch` (paired with a fixed `--os
	/// linux`): on `podman build` it replaces a `--platform linux/<arch>` triple,
	/// and on `podman manifest add` it stamps the member's platform.
	#[must_use]
	pub fn oci_arch(self) -> &'static str {
		match self {
			Arch::Aarch64 => "arm64",
			Arch::X86_64 => "amd64",
		}
	}

	/// QEMU system-emulator binary for this arch (`qemu-system-<arch>`), used by
	/// the VM builder to boot a target-arch guest.
	#[must_use]
	pub fn qemu_system_bin(self) -> &'static str {
		match self {
			Arch::Aarch64 => "qemu-system-aarch64",
			Arch::X86_64 => "qemu-system-x86_64",
		}
	}

	/// The arch bootcher is *running* on, if it's one we build for. `None` on an
	/// unrecognised host arch (in which case every target is treated as
	/// cross-arch). Drives builder selection: a target matching the host builds
	/// natively in-process, everything else goes to a remote/VM builder.
	#[must_use]
	pub fn host() -> Option<Arch> {
		Arch::from_uname(std::env::consts::ARCH)
	}

	/// Map a `uname -m` machine string to an [`Arch`], or `None` for one bootcher
	/// doesn't build. Used both for the host arch ([`Arch::host`]) and to identify
	/// a LAN deploy target's arch over ssh so it's shipped the matching image. The
	/// uname spelling is the variant spelling, so this is effectively a validity
	/// filter.
	#[must_use]
	pub fn from_uname(machine: &str) -> Option<Arch> {
		match machine {
			"x86_64" => Some(Arch::X86_64),
			"aarch64" => Some(Arch::Aarch64),
			_ => None,
		}
	}
}

/// `image-builder` output format, passed verbatim as the positional image type.
/// Read from `[targets]` (each arch maps to one or more; defaulted to
/// `qcow2`, a ready-to-boot VM disk), it's a near-pure proxy: bootcher doesn't
/// interpret the artifact, so every disk type `image-builder` accepts for a bootc
/// input is offered here — pick whatever suits the target (`qcow2` for VMs, `raw`
/// to write to a block device, `bootc-installer` for an installer ISO, a cloud
/// `ami`/`vhd`/`gce`, …). Each variant's spelling is its `image-builder` type
/// name, shared by `strum` (`Display`) and `serde` (the manifest key).
///
/// Because the build passes `--output-name disk`, every type lands as a single
/// flat `disk.<ext>` in the target's [`DiskTarget::output_dir`] (e.g. `disk.qcow2`,
/// `disk.raw`, `disk.iso`).
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	strum::Display,
	strum::EnumString,
	strum::VariantArray,
	serde::Deserialize,
	serde::Serialize,
	schemars::JsonSchema,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum DiskType {
	/// Raw disk image (`disk.raw`) — write it straight to a block device.
	Raw,
	/// QEMU copy-on-write v2 image (`disk.qcow2`) — the default; boots as-is
	/// under qemu/libvirt and most clouds.
	Qcow2,
	/// `VMware` disk image (`disk.vmdk`).
	Vmdk,
	/// Amazon Machine Image disk (`disk.raw`, uploaded as an AMI).
	Ami,
	/// Microsoft Azure / Hyper-V virtual hard disk (`disk.vhd`).
	Vhd,
	/// Google Compute Engine image tarball (`disk.tar.gz`).
	Gce,
	/// Anaconda-based installer ISO (`disk.iso`). Requires the container to ship
	/// the installer payload (anaconda + build tools); see the `image-builder`
	/// bootc ISO docs. (Replaces the predecessor's `anaconda-iso`, which
	/// `image-builder` no longer accepts for bootc inputs.)
	BootcInstaller,
}

/// Root filesystem `image-builder` formats the image with, passed verbatim as its
/// `--bootc-default-fs`. Read from the manifest's `[general] rootfs` (defaulted to
/// `ext4`). Like [`DiskType`] each variant's spelling is its `image-builder` flag
/// value, shared by `strum` and `serde`.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	strum::Display,
	strum::VariantArray,
	serde::Deserialize,
	serde::Serialize,
	schemars::JsonSchema,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Rootfs {
	Ext4,
	Xfs,
	Btrfs,
}

/// The per-project manifest, parsed from [`MANIFEST`] in the project root (the
/// cwd). A bootcher project is a single image: the manifest names it (used for
/// the container tag and the output dir) and everything else is convention
/// relative to the project root — the `Containerfile` and the `sysroot/` overlay
/// live alongside it. Keys live under tables (`[general]`, `[builder]`,
/// `[deploy]`, `[concurrency]`, `[hooks]`) to leave room for future sections.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[schemars(title = "bootcher.toml", description = "The bootcher project manifest.")]
pub struct Manifest {
	/// Inherit from another manifest: the named file (a path relative to *this*
	/// file's directory) is loaded first and this file's keys are layered over it —
	/// tables merge key-by-key, while scalars and arrays replace wholesale. Use it to
	/// keep a pipeline-specific override (e.g. a `bootcher.ci.toml` that only sets
	/// `[builder] build = "vm"`) beside the base `bootcher.toml` without restating it.
	/// `extend` chains are followed and cycles rejected. Resolved at load time, so it
	/// never appears in a serialized manifest. See [`Manifest::load`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub extend: Option<String>,
	pub general: General,
	/// The build matrix (`[targets]`): each target arch mapped to the
	/// `image-builder` disk type(s) to render it as. See [`Targets`]. The present
	/// fields are the arches the project builds; defaults to the host arch as a
	/// single `qcow2` when the table is absent.
	#[serde(default = "default_targets")]
	pub targets: Targets,
	/// Where the build and image steps run; see [`BuilderConfig`]. Defaults to
	/// all-`local` when the table is absent.
	#[serde(default)]
	pub builder: BuilderConfig,
	/// Deployment targets; see [`DeployConfig`]. Defaults to no targets when the
	/// table is absent.
	#[serde(default)]
	pub deploy: DeployConfig,
	/// Per-activity caps on parallel worker counts; see [`ConcurrencyConfig`].
	/// Defaults to unbounded (host core count) for every activity when the table
	/// is absent, and is omitted from a freshly serialized manifest so `bootcher
	/// init` can document it with commented examples instead of an empty table.
	#[serde(default, skip_serializing_if = "ConcurrencyConfig::is_empty")]
	pub concurrency: ConcurrencyConfig,
	/// Lifecycle hook commands wrapping the build/image/upgrade phases; see [`Hooks`].
	/// Defaults to no hooks when the table is absent, and is omitted from a
	/// freshly serialized manifest so `bootcher init` can document it with
	/// commented examples instead of an empty table.
	#[serde(default, skip_serializing_if = "Hooks::is_empty")]
	pub hooks: Hooks,
}

/// Opt-in cosign/sigstore image signing config, embedded in [`RegistryConfig::WithSigning`].
/// When present, `deploy`/`upgrade` sign the pushed multi-arch image with the local private
/// key, `provision` injects the matching public key + a signature-requiring `policy.json`
/// into the device's `/etc`, and the device's bootc origin is recorded with the verifying
/// `ostree-image-signed:` scheme — so a tampered or unsigned push is rejected on `bootc upgrade`.
///
/// ```toml
/// [deploy]
/// registry = { url = "registry.example.com/org", key = "cosign.key" }
/// ```
///
/// The public key is always the `<key>.pub` sibling (`cosign.key` → `cosign.pub`),
/// matching the naming `bootcher sign enroll` produces. The signing **passphrase** is
/// never stored here: it's read from `BOOTCHER_SIGN_PASSPHRASE` (or prompted on a TTY),
/// mirroring the registry pull token (see [`crate::jobs::secrets`]).
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SigningConfig {
	/// Path (relative to the project root, or absolute) to the cosign/sigstore
	/// **private** key used to sign the pushed image. Secret — keep it out of the
	/// repo (the scaffold `.gitignore` covers `*.key`).
	pub key: String,
}

impl SigningConfig {
	/// The public-key path: the sibling formed by swapping the private key's
	/// extension for `.pub` — matching the cosign (`cosign.key` → `cosign.pub`) and
	/// skopeo (`<p>.private` → `<p>.pub`) conventions; an unrecognised extension just
	/// gets `.pub` appended.
	#[must_use]
	pub fn public_key_path(&self) -> String {
		for ext in [".key", ".private"] {
			if let Some(stem) = self.key.strip_suffix(ext) {
				return format!("{stem}.pub");
			}
		}
		format!("{}.pub", self.key)
	}
}

/// Schema-only mirror of a [`Targets`] entry's accepted shapes: a bare
/// `image-builder` type *or* an array of them. The runtime parse goes through
/// [`de_target_disks`], whose `deserialize_with` schemars can't introspect, so
/// this names both shapes for the generated JSON Schema (each arch field points at
/// it via `#[schemars(with)]`). Never constructed — it exists purely for its
/// derived [`schemars::JsonSchema`].
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
// The doc comment above is internal meta-commentary; give the schema a clean,
// user-facing description instead of leaking it into editor tooltips.
#[schemars(description = "A single image-builder disk type, or an array of them.")]
enum DiskTypeList {
	One(DiskType),
	Many(Vec<DiskType>),
}

/// The `[general]` section of [`Manifest`].
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct General {
	/// Image name — the `<name>` in `localhost/<name>:latest-<arch>` and the
	/// container tag. Seeded from the project directory by `bootcher init`.
	pub name: String,
	/// Root filesystem `image-builder` formats the image with (its
	/// `--bootc-default-fs`). Defaults to `ext4`. See [`Rootfs`].
	#[serde(default = "default_rootfs")]
	pub rootfs: Rootfs,
}

/// `[targets]` — the project's build matrix. Each target arch is mapped
/// to the `image-builder` disk type(s) to render it as; the present (`Some`)
/// fields *are* the arches the project builds. The two axes are independent: arch
/// is the **container/registry** axis the `build`, `deploy` and `upgrade` fan-outs
/// use (one container image and one multi-arch manifest-list member per arch),
/// while each arch's type list is the **disk-artifact** axis only the `disk` step
/// fans out over. An arch listing several types reuses its one container build to
/// render each. A field is a bare type (`x86_64 = "qcow2"`) or an array
/// (`x86_64 = ["qcow2", "bootc-installer"]`).
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Targets {
	/// Disk types to build for an `x86_64` target; absent ⇒ not an `x86_64` project.
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		deserialize_with = "de_target_disks"
	)]
	#[schemars(with = "Option<DiskTypeList>")]
	pub x86_64: Option<Vec<DiskType>>,
	/// Disk types to build for an `aarch64` target; absent ⇒ not an `aarch64` project.
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		deserialize_with = "de_target_disks"
	)]
	#[schemars(with = "Option<DiskTypeList>")]
	pub aarch64: Option<Vec<DiskType>>,
}

impl Targets {
	/// The target arches (the present fields), `x86_64` before `aarch64`. Empty only
	/// for an explicitly-empty `[targets]` table, which [`Manifest::load`]
	/// rejects.
	#[must_use]
	pub fn arches(&self) -> Vec<Arch> {
		let mut v = Vec::new();
		if self.x86_64.is_some() {
			v.push(Arch::X86_64);
		}
		if self.aarch64.is_some() {
			v.push(Arch::Aarch64);
		}
		v
	}

	/// The disk types listed for `arch`, or `&[]` when `arch` isn't a target.
	#[must_use]
	pub fn types(&self, arch: Arch) -> &[DiskType] {
		match arch {
			Arch::X86_64 => self.x86_64.as_deref().unwrap_or(&[]),
			Arch::Aarch64 => self.aarch64.as_deref().unwrap_or(&[]),
		}
	}

	/// Make `arch` a target with disk type list `types` (replacing any prior list).
	/// The constructor `bootcher init` builds the matrix with.
	pub fn set(&mut self, arch: Arch, types: Vec<DiskType>) {
		match arch {
			Arch::X86_64 => self.x86_64 = Some(types),
			Arch::Aarch64 => self.aarch64 = Some(types),
		}
	}

	/// True when no arch is a target (an empty or absent table).
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.x86_64.is_none() && self.aarch64.is_none()
	}
}

/// A `[builder]` spec: either a bare-string shorthand (`"local"`, `"vm"`, or an
/// ssh destination) for the no-opts common case, or an explicit table tagged by
/// `type` ([`BuilderTable`]) when it carries extra ssh args and/or `podman` flags.
///
/// ```toml
/// # shorthand — local, vm, or a remote with default ssh, no extra opts:
/// build = "user@build-host"
///
/// # explicit table — each `type` exposes only its valid fields:
/// build = { type = "local",  podman_opts = ["--network=host"] }
/// build = { type = "vm",     podman_opts = ["--network=host"] }
/// build = { type = "remote", host = "user@build-host", ssh_opts = ["-i", "/key"] }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum BuilderSpec {
	/// Bare-string shorthand: `"local"`, `"vm"`, or an ssh destination, with no
	/// extra opts. Sugar for the corresponding [`BuilderTable`] variant.
	Str(String),
	/// Explicit table form, tagged by `type` — carries the extra opts the
	/// shorthand can't.
	Table(BuilderTable),
}

/// The explicit `[builder]` table form, tagged by `type` so each builder kind
/// exposes *only* the fields valid for it — `host`/`ssh_opts` exist solely on
/// `remote`, never silently ignored on a `local`/`vm` builder. `podman_opts` is
/// the one field common to all three (see [`BuilderSpec::podman_opts`]).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BuilderTable {
	/// In-process builder on this host (`type = "local"`).
	Local {
		/// See [`BuilderSpec::podman_opts`].
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		podman_opts: Vec<String>,
	},
	/// Throwaway local VM matching the target arch (`type = "vm"`).
	Vm {
		/// See [`BuilderSpec::podman_opts`].
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		podman_opts: Vec<String>,
	},
	/// Native-arch remote driven over ssh (`type = "remote"`).
	Remote {
		/// ssh destination: `"[user@]host"` or `"ssh://[user@]host[:port]"`.
		host: String,
		/// Extra `ssh` args prepended to every connection to the build host —
		/// e.g. `["-i", "/path/to/key"]`. For a remote whose ssh needs more than
		/// the user's `~/.ssh/config` provides, without a config file.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		ssh_opts: Vec<String>,
		/// See [`BuilderSpec::podman_opts`].
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		podman_opts: Vec<String>,
	},
}

impl BuilderSpec {
	/// The bare spec string the builder selector resolves: `"local"`, `"vm"`, or
	/// the ssh destination.
	#[must_use]
	pub fn spec(&self) -> &str {
		match self {
			Self::Str(s) => s,
			Self::Table(BuilderTable::Local { .. }) => "local",
			Self::Table(BuilderTable::Vm { .. }) => "vm",
			Self::Table(BuilderTable::Remote { host, .. }) => host,
		}
	}

	/// Extra ssh args for this spec — only a `remote` table carries them; empty
	/// for everything else (a bare string, `local`, or `vm`).
	#[must_use]
	pub fn ssh_opts(&self) -> &[String] {
		match self {
			Self::Table(BuilderTable::Remote { ssh_opts, .. }) => ssh_opts,
			_ => &[],
		}
	}

	/// Extra flags spliced into this builder's `podman` invocation — the container
	/// build's `podman build` for the `build` role, the privileged image-builder
	/// `podman run` for the `image` role. Passed through verbatim (an escape hatch,
	/// e.g. `["--network=host"]` where podman's default per-container networking
	/// can't be set up); a flag clashing with one bootcher already sets (`--arch`,
	/// `--privileged`) is the caller's problem. Scoped to this builder + role + arch
	/// by where the spec sits in the `[builder]` table. Empty for a bare string.
	#[must_use]
	pub fn podman_opts(&self) -> &[String] {
		match self {
			Self::Str(_) => &[],
			Self::Table(
				BuilderTable::Local { podman_opts }
				| BuilderTable::Vm { podman_opts }
				| BuilderTable::Remote { podman_opts, .. },
			) => podman_opts,
		}
	}
}

/// The `[builder]` section: where each build step runs. Both values are builder
/// *specs* — `local` (in-process), `vm` (a throwaway local VM), or
/// `[user@]host` / `ssh://[user@]host[:port]` (a native-arch remote) — resolved by
/// the builder selector. Split into two roles because the image step
/// (`image-builder`) is the heavier, emulation-prone one cross-arch, so a project
/// can build the container locally yet offload just the image step to a VM or
/// remote.
///
/// Both keys are *optional in the file* — a missing key (or a missing
/// `[builder]` table) is filled with the safe `local` default during
/// deserialization, exactly like `[targets]`, so the in-memory value is
/// always a concrete spec. `bootcher init` writes both keys regardless, purely
/// so the scaffolded manifest documents what's configurable.
///
/// The flat `build`/`image` apply to every target arch; a `[builder.<arch>]`
/// subtable overrides either role for one arch (see [`ArchBuilder`]). This is the
/// per-arch dimension a multi-arch project needs: on an `x86_64` host building
/// both arches, the native `x86_64` image step can stay `local` while the `aarch64`
/// one is routed to a `vm`, e.g.
///
/// ```toml
/// [builder]
/// build = "local"
/// image = "local"
///
/// [builder.aarch64]
/// image = "vm"
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BuilderConfig {
	/// Default builder for the container build (and `deploy`/`upgrade`'s rebuild).
	#[serde(default = "default_builder")]
	pub build: BuilderSpec,
	/// Default builder for the `image-builder` (disk image) step.
	#[serde(default = "default_builder")]
	pub image: BuilderSpec,
	/// Per-arch override for `x86_64` targets; falls back to `build`/`image`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub x86_64: Option<ArchBuilder>,
	/// Per-arch override for `aarch64` targets; falls back to `build`/`image`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub aarch64: Option<ArchBuilder>,
}

impl Default for BuilderConfig {
	/// Matches the per-key serde default, so an absent `[builder]` table and an
	/// absent key resolve identically (both `local`, no per-arch overrides).
	fn default() -> Self {
		Self { build: default_builder(), image: default_builder(), x86_64: None, aarch64: None }
	}
}

impl BuilderConfig {
	/// The `[builder.<arch>]` override table for `arch`, if present.
	fn arch_override(&self, arch: Arch) -> Option<&ArchBuilder> {
		match arch {
			Arch::X86_64 => self.x86_64.as_ref(),
			Arch::Aarch64 => self.aarch64.as_ref(),
		}
	}

	/// Container-build builder spec for `arch`: its `[builder.<arch>] build`
	/// override if set, else the flat `build` default.
	#[must_use]
	pub fn build_for(&self, arch: Arch) -> &BuilderSpec {
		self.arch_override(arch).and_then(|o| o.build.as_ref()).unwrap_or(&self.build)
	}

	/// Image-step (`image-builder`) builder spec for `arch`: its `[builder.<arch>] image`
	/// override if set, else the flat `image` default.
	#[must_use]
	pub fn image_for(&self, arch: Arch) -> &BuilderSpec {
		self.arch_override(arch).and_then(|o| o.image.as_ref()).unwrap_or(&self.image)
	}
}

/// A `[builder.<arch>]` override table: either role may be set (to a `local` /
/// `vm` / `[user@]host` spec) to override the flat [`BuilderConfig`] default for
/// that one arch; an unset role inherits the default.
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ArchBuilder {
	/// Overrides the container-build builder for this arch.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub build: Option<BuilderSpec>,
	/// Overrides the image-step builder for this arch.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub image: Option<BuilderSpec>,
}

/// One entry in the `[deploy] remotes` array. Two interchangeable shapes, so the
/// common case stays terse and the rare per-target tweak doesn't force every entry
/// into the verbose form:
///
/// ```toml
/// remotes = [
///   "admin@device-a",                                  # ConnStr — just the host
///   { remote = "admin@device-b", ssh_opts = ["-i", "/path/to/key"] },  # WithOpts
/// ]
/// ```
///
/// The host is whatever ssh accepts as a destination — `[user@]host`, or
/// `ssh://[user@]host[:port]` for a non-default port. Resolved to an [`Ssh`] via the
/// `From` impl below.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RemoteConfig {
	/// Bare connection string: the host with no extra ssh args.
	ConnStr(#[serde(default)] String),
	/// A host plus per-target extra ssh args.
	WithOpts {
		#[serde(default)]
		remote: String,
		/// Extra `ssh` args prepended to every connection to a `remotes` device
		/// (`upgrade`/`deploy`/`rotate`) — e.g. `["-o", "StrictHostKeyChecking=accept-new"]`
		/// or `["-i", "/path/to/key"]`. For a target whose ssh needs more than the
		/// user's `~/.ssh/config` provides, without a config file.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		ssh_opts: Vec<String>,
		/// The stock cloud login (`debian`/`ubuntu`/`cloud-user`/`root`) `bootcher
		/// takeover` uses for its *initial* connection to this host, before the
		/// image's `admin` user replaces it. Per-host override of the fleet-wide
		/// `--login`; unused outside takeover. The steady-state identity stays
		/// `admin@` (the `remote` above), so after takeover this is an ordinary
		/// remote. See [`RemoteConfig::takeover_ssh`].
		#[serde(default, skip_serializing_if = "Option::is_none")]
		takeover_login: Option<String>,
	},
}

/// The `[deploy] registry` field: either a plain namespace string (no signing)
/// or a rich object pairing the namespace with cosign signing config. Mirrors the
/// `remotes` dual-shape — a bare string for the common case, an inline table to
/// add signing without a separate top-level section.
///
/// ```toml
/// # plain (no signing):
/// registry = "registry.gitlab.com/org/project"
///
/// # with signing:
/// registry = { url = "registry.gitlab.com/org/project", key = "cosign.key" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RegistryConfig {
	/// Bare namespace: registry URL with no image signing.
	Url(String),
	/// Namespace plus cosign signing configuration.
	WithSigning { url: String, key: String },
}

impl RegistryConfig {
	/// The registry namespace URL (the push/pull address), regardless of variant.
	#[must_use]
	pub fn namespace(&self) -> &str {
		match self {
			Self::Url(url) | Self::WithSigning { url, .. } => url,
		}
	}

	/// The signing config embedded in this registry entry, if any.
	#[must_use]
	pub fn signing(&self) -> Option<SigningConfig> {
		match self {
			Self::WithSigning { key, .. } => Some(SigningConfig { key: key.clone() }),
			Self::Url(_) => None,
		}
	}
}

impl From<&RemoteConfig> for Ssh {
	fn from(val: &RemoteConfig) -> Self {
		let (host, opts, _) = val.parts();
		Ssh::new(host, opts.to_vec())
	}
}

impl RemoteConfig {
	/// The destructured parts of a remote: its ssh destination, extra ssh args, and
	/// (object form only) the takeover stock login. A `ConnStr` carries no opts and
	/// no `takeover_login`. The shared accessor the [`Ssh`] bridges below read from.
	fn parts(&self) -> (&str, &[String], Option<&str>) {
		match self {
			RemoteConfig::ConnStr(s) => (s, &[], None),
			RemoteConfig::WithOpts { remote, ssh_opts, takeover_login } => {
				(remote, ssh_opts, takeover_login.as_deref())
			}
		}
	}

	/// The steady-state **admin** [`Ssh`] for this remote (`admin@host` + `ssh_opts`)
	/// with `identity` appended as an extra `-i <key>`. `bootcher takeover` uses this
	/// after the reboot, when the stock cloud user is gone and only the just-injected
	/// admin key (`--ssh-key`'s private half) authenticates. Equivalent to
	/// `Ssh::from(self)` plus the identity flag.
	#[must_use]
	pub fn admin_ssh(&self, identity: &str) -> Ssh {
		let (host, opts, _) = self.parts();
		let mut opts: Vec<String> = opts.to_vec();
		opts.push("-i".to_owned());
		opts.push(identity.to_owned());
		Ssh::new(host, opts)
	}

	/// The **initial-connection** [`Ssh`] `bootcher takeover` uses *before* the
	/// reboot — the same host as the steady-state remote but with the user rewritten
	/// to the resolved stock cloud login. The login is the remote's own
	/// `takeover_login`, else `default_login` (the fleet-wide `--login`), else a hard
	/// error naming the host (there is no safe cross-distro default). The destination
	/// is rewritten (not given a `-l`) so an explicit `user@` in the configured host
	/// is overridden rather than ignored; both `[user@]host` and
	/// `ssh://[user@]host[:port]` forms are handled.
	///
	/// # Errors
	///
	/// Returns an error if neither a per-host `takeover_login` nor a `default_login`
	/// is available.
	pub fn takeover_ssh(&self, default_login: Option<&str>) -> Result<Ssh> {
		let (host, opts, login) = self.parts();
		let login = login.or(default_login).ok_or_else(|| {
			anyhow::anyhow!(
				"no stock login for takeover target {host} — set `takeover_login` on this remote \
				 in bootcher.toml or pass `--login <user>`"
			)
		})?;
		Ok(Ssh::new(rewrite_ssh_user(host, login), opts.to_vec()))
	}
}

/// Rewrite the user in an ssh destination to `login`, handling both the plain
/// `[user@]host` and the `ssh://[user@]host[:port]` URL forms (preserving the
/// scheme and any port). An existing `user@` is replaced; a bare host gains one.
/// Used by [`RemoteConfig::takeover_ssh`] to retarget the steady-state `admin@host`
/// destination at the stock cloud login for takeover's initial connection.
fn rewrite_ssh_user(dest: &str, login: &str) -> String {
	if let Some(rest) = dest.strip_prefix("ssh://") {
		let hostpart = rest.split_once('@').map_or(rest, |(_, h)| h);
		format!("ssh://{login}@{hostpart}")
	} else {
		let hostpart = dest.split_once('@').map_or(dest, |(_, h)| h);
		format!("{login}@{hostpart}")
	}
}

/// Resolve a slice of deploy targets into ready-to-drive [`Ssh`] connections.
/// The `to_ssh` companion to the [`From<&RemoteConfig>`] bridge, so call sites read
/// `ctx.deploy_remotes().to_ssh()` (cf. `[T]::to_vec`) instead of spelling out the
/// iterate-map-collect — keeping the `Ssh` operations type out of [`Manifest`]'s API.
pub trait ToSsh {
	fn to_ssh(&self) -> Vec<Ssh>;
}

impl ToSsh for [RemoteConfig] {
	fn to_ssh(&self) -> Vec<Ssh> {
		self.iter().map(Ssh::from).collect()
	}
}

impl Default for RemoteConfig {
	fn default() -> Self {
		Self::ConnStr(String::default())
	}
}

/// The `[deploy]` section: optional registry config plus the SSH targets a
/// `deploy` / `upgrade` ships the image to. `registry` is either a bare namespace
/// string or a [`RegistryConfig::WithSigning`] inline table; absent means LAN
/// (SSH-tunnel) mode. `remotes` is the device list; in LAN mode at least one is
/// required, in registry mode it may be empty (devices self-update on their timer;
/// listed remotes are additionally upgraded immediately). The default is an empty
/// table; `bootcher init` writes both keys for discoverability.
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DeployConfig {
	/// Registry namespace + optional signing config. Absent selects the LAN backend.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub registry: Option<RegistryConfig>,
	#[serde(default)]
	pub remotes: Vec<RemoteConfig>,
}

/// The `[concurrency]` section: optional caps on how many workers each parallel
/// fan-out may run at once. bootcher parallelises four activities — the
/// multi-arch container `build` and disk `image` steps (each worker a
/// `podman`/`image-builder` run, possibly its own emulated VM), and the
/// per-device `upgrade` and `rotate` rollouts (each worker an ssh round-trip).
/// By default each scales to the host's core count; on a constrained machine
/// (limited RAM/CPU, or where two cross-arch VMs booting at once would thrash)
/// throttle the heavy ones here.
///
/// Every key is optional; an unset one — or an absent `[concurrency]` table —
/// means unbounded. A value is the *maximum* worker count for that activity: it
/// only ever lowers the pool (the effective size is still bounded by the number
/// of work items and the host core count), and must be ≥ 1 (a `0` is rejected at
/// parse time).
///
/// ```toml
/// [concurrency]
/// build = 2    # at most 2 arches building containers at once
/// image = 1    # one disk image at a time (e.g. so only one builder VM boots)
/// upgrade = 4  # at most 4 devices upgrading in parallel
/// rotate = 4
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ConcurrencyConfig {
	/// Max parallel workers for the multi-arch container `build` fan-out
	/// (`build` / `provision` / `deploy`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub build: Option<NonZeroUsize>,
	/// Max parallel workers for the multi-arch `disk` (`image-builder`) fan-out
	/// (`disk` / `provision`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub disk: Option<NonZeroUsize>,
	/// Max parallel workers for the per-device `upgrade` rollout (`upgrade` /
	/// `deploy`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub upgrade: Option<NonZeroUsize>,
	/// Max parallel workers for the per-device `rotate` rollout.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rotate: Option<NonZeroUsize>,
	/// Max parallel workers for the per-host `takeover` rollout (`takeover`). Its own
	/// knob rather than sharing `upgrade`'s: a takeover moves files in place on a live
	/// host (and may pull a multi-GB image per host) rather than wiping a disk, a
	/// different per-host time/resource profile.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub takeover: Option<NonZeroUsize>,
}

impl ConcurrencyConfig {
	/// True when no activity is capped — drives `skip_serializing_if` so an
	/// unconfigured `[concurrency]` section never appears in a serialized manifest.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.build.is_none()
			&& self.disk.is_none()
			&& self.upgrade.is_none()
			&& self.rotate.is_none()
			&& self.takeover.is_none()
	}
}

/// The `[hooks]` section: shell commands bootcher runs around its phases. This is
/// the tool's extension point — project-specific work (e.g. editing the built disk
/// image to embed files outside the Containerfile's reach, or building a sidecar
/// container image before the main build) lives in a script the manifest points
/// at, not in bootcher itself. Each command is run with `sh -c` from the project
/// root, terminal handed over, exit code honoured.
///
/// Each phase is a `[hooks.<phase>]` table with optional `pre`/`post` keys (see
/// [`HookPair`]), and the hooks fire wherever that phase runs:
///
/// - `[hooks.build]` wraps the container build, so it applies to `build`,
///   `provision` and `deploy` alike.
/// - `[hooks.disk]` wraps the `image-builder` step, so it applies to `disk`
///   and `provision`.
/// - `[hooks.upgrade]` wraps the push + per-device `bootc upgrade`, so it applies
///   to `upgrade` and `deploy`.
///
/// Every table and key is optional; an absent one is no hook. Each is
/// `skip_serializing_if`-empty so a hookless project serializes no `[hooks]`
/// tables.
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Hooks {
	/// Hooks around the container build (`build` / `provision` / `deploy`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub build: HookPair,
	/// Hooks around the `image-builder` disk-image step (`disk` /
	/// `provision`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub disk: HookPair,
	/// Hooks around the deploy push + per-device `bootc upgrade` (`upgrade` /
	/// `deploy`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub upgrade: HookPair,
}

/// A `[hooks.<phase>]` table: the optional `pre` command run before a phase and
/// `post` command run after it. Either may be set on its own; an unset key is no
/// hook. Both are `skip_serializing_if`-empty so an unused phase serializes no
/// table.
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HookPair {
	/// Command run before the phase.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pre: Option<String>,
	/// Command run after the phase completes.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub post: Option<String>,
}

impl HookPair {
	/// True when neither `pre` nor `post` is set — drives `skip_serializing_if` so
	/// an unused phase's table never appears in a serialized manifest.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.pre.is_none() && self.post.is_none()
	}
}

impl Hooks {
	/// True when no hook is configured in any phase — drives `skip_serializing_if`
	/// so an unused `[hooks]` section never appears in a serialized manifest.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.build.is_empty() && self.disk.is_empty() && self.upgrade.is_empty()
	}
}

/// Canonicalize `path` for `extend`-cycle detection, falling back to the path
/// as-written when it can't be resolved — so two spellings of the same file
/// (`bootcher.toml` vs `./bootcher.toml`) still compare equal.
fn canonical(path: &Path) -> PathBuf {
	path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve a manifest's `extend` chain into a single merged table. `child` is the
/// already-parsed table (its `extend` key still present), `child_path` is where it
/// was read (parents resolve relative to its directory), and `chain` holds the
/// canonical paths already on the inheritance path so a cycle is caught instead of
/// looping forever.
fn resolve_extends(
	mut child: toml::Table,
	child_path: &Path,
	chain: &mut Vec<PathBuf>,
) -> Result<toml::Table> {
	let parent_rel = match child.remove("extend") {
		Some(toml::Value::String(s)) => s,
		Some(_) => {
			bail!("`extend` in {} must be a string path to another manifest", child_path.display())
		}
		None => return Ok(child),
	};
	let base = child_path.parent().unwrap_or_else(|| Path::new(""));
	let parent_path = base.join(&parent_rel);
	let parent_display = parent_path.display();
	let canon = canonical(&parent_path);
	if chain.contains(&canon) {
		bail!(
			"`extend` cycle: {} eventually extends itself (via {parent_display})",
			child_path.display()
		);
	}
	let raw = match fs::read_to_string(&parent_path) {
		Ok(raw) => raw,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			bail!(
				"{} extends \"{parent_rel}\", but {parent_display} doesn't exist",
				child_path.display()
			)
		}
		Err(e) => return Err(e).with_context(|| format!("reading {parent_display}")),
	};
	let parent: toml::Table =
		toml::from_str(&raw).with_context(|| format!("parsing {parent_display}"))?;
	chain.push(canon);
	// Resolve the parent's own `extend` first (grandparents apply underneath), then
	// layer this child over the fully-resolved parent.
	let parent = resolve_extends(parent, &parent_path, chain)?;
	Ok(deep_merge(parent, child))
}

/// Recursively merge `over` onto `base`: a key that is a table in both is merged
/// field-by-field (so a child overrides one key of a section without restating the
/// rest), while every other value — scalars and arrays alike — is replaced wholesale
/// by `over`'s.
fn deep_merge(mut base: toml::Table, over: toml::Table) -> toml::Table {
	for (key, val) in over {
		match (base.remove(&key), val) {
			(Some(toml::Value::Table(b)), toml::Value::Table(o)) => {
				base.insert(key, toml::Value::Table(deep_merge(b, o)));
			}
			(_, val) => {
				base.insert(key, val);
			}
		}
	}
	base
}

impl Manifest {
	/// Read and parse a manifest at `path` (default `./bootcher.toml`, see
	/// [`MANIFEST`]), failing with a pointer to `bootcher init` when the cwd isn't a
	/// bootcher project. The build context and every relative path the manifest names
	/// (Containerfile, `sysroot/`, keys) stay anchored to the cwd, not to `path`, so a
	/// `--manifest bootcher.ci.toml` is a per-pipeline override layered over the same
	/// project tree.
	///
	/// # Errors
	///
	/// Returns an error if the manifest is missing, unreadable, or fails to parse.
	pub fn load(path: impl AsRef<Path>) -> Result<Self> {
		let path = path.as_ref();
		let display = path.display();
		let raw = match fs::read_to_string(path) {
			Ok(raw) => raw,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
				"no {display} in the current directory — not a bootcher project.\n\
				 Run `bootcher init <name>` to scaffold one, or cd into an existing project."
			),
			Err(e) => return Err(e).with_context(|| format!("reading {display}")),
		};
		let table: toml::Table =
			toml::from_str(&raw).with_context(|| format!("parsing {display}"))?;
		// A manifest may layer onto another via a top-level `extend = "<path>"`: the
		// parent is loaded first and this file's keys merged over it (tables merge
		// recursively, scalars/arrays replace), so a pipeline variant restates only its
		// overrides. Resolve the whole chain into one table before deserializing. The
		// common no-`extend` case stays on the original path, keeping toml's
		// line-located type errors that a post-merge deserialize would lose.
		let manifest: Manifest = if table.contains_key("extend") {
			let merged = resolve_extends(table, path, &mut vec![canonical(path)])?;
			toml::Value::Table(merged).try_into().with_context(|| format!("parsing {display}"))?
		} else {
			toml::from_str(&raw).with_context(|| format!("parsing {display}"))?
		};
		// A present-but-empty `[targets]` table leaves nothing to build — the per-entry
		// deserializer can't catch it (it never sees an arch), so reject it here where
		// the whole table is in hand.
		if manifest.targets.is_empty() {
			bail!("`[targets]` must list at least one architecture to build");
		}
		Ok(manifest)
	}

	/// The container image this project builds for `arch` (its disk artifacts, if
	/// any, come from [`Self::disk_types_for`]). Arch is the container/registry axis,
	/// so this carries no disk type — see [`DiskTarget`].
	#[must_use]
	pub fn image(&self, arch: Arch) -> ImageRef {
		ImageRef {
			name: self.general.name.clone(),
			arch,
			rootfs: self.general.rootfs,
			build_ctx: PathBuf::from("."),
			registry: self.deploy.registry.as_ref().map(|r| r.namespace().to_owned()),
			// Filled on the build path (see `jobs::build`); empty for the many
			// non-build callers that only read a ref, so this never invokes git.
			labels: Vec::new(),
		}
	}

	/// The `image-builder` disk types to build for `arch` (`&[]` if `arch` isn't a
	/// target), from `[targets]`. The `disk` step renders one artifact per entry from
	/// this arch's single container build.
	#[must_use]
	pub fn disk_types_for(&self, arch: Arch) -> &[DiskType] {
		self.targets.types(arch)
	}

	/// The registry namespace URL, if configured. `None` selects the LAN backend.
	pub fn registry(&self) -> Option<&str> {
		self.deploy.registry.as_ref().map(RegistryConfig::namespace)
	}

	/// Suffix-free, fully-qualified registry reference for registry-mode deploys
	/// (`<namespace>/<name>:latest`, e.g.
	/// `registry.gitlab.com/org/project/kiosk:latest`), or `None` when no registry
	/// is configured — in which case `deploy`/`upgrade` use the LAN (ssh-tunnelled
	/// pull from a temporary builder-local registry) backend. It's the multi-arch
	/// manifest list bootcher pushes and that each device pulls its own arch from.
	/// Project-level (it spans every arch): both name and namespace come from the
	/// manifest, so it doesn't belong to any one [`ImageRef`].
	#[must_use]
	pub fn registry_list_ref(&self) -> Option<String> {
		self.registry().map(|ns| format!("{ns}/{}:latest", self.general.name))
	}

	/// Suffix-free **local** manifest-list ref (`localhost/<name>:latest`) — the
	/// build's final artifact (the assembled manifest list) and the
	/// ref the push/serve paths name to ship the list the build already assembled,
	/// without rebuilding it. The arch-independent local counterpart to
	/// [`Self::registry_list_ref`]: the name is the project's, not any one
	/// [`ImageRef`]'s, so it derives straight from the manifest.
	#[must_use]
	pub fn local_list_ref(&self) -> String {
		format!("localhost/{}:latest", self.general.name)
	}

	/// Fully-qualified registry reference for an immutable `CalVer` tag
	/// (`<namespace>/<name>:<version>`), or `None` when no registry is configured.
	/// Pushed alongside [`Self::registry_list_ref`] in registry mode: the mutable
	/// `:latest` is the channel devices track, while this version tag is a durable,
	/// human-readable handle for the same manifest digest — a stable anchor for
	/// rollback/audit that a registry GC pass won't reap (an untagged digest can be).
	/// `version` comes from [`calver_now`].
	#[must_use]
	pub fn registry_version_ref(&self, version: &str) -> Option<String> {
		self.registry().map(|ns| format!("{ns}/{}:{version}", self.general.name))
	}

	/// One [`ImageRef`] per target arch in `[targets]`. The container/registry
	/// fan-out the `build`, `deploy` and `upgrade` subcommands iterate (disk types
	/// don't factor in here — see [`Self::disk_types_for`]); a single-arch project
	/// yields a one-element vec.
	#[must_use]
	pub fn images(&self) -> Vec<ImageRef> {
		self.targets.arches().into_iter().map(|arch| self.image(arch)).collect()
	}

	/// Validate a `--target`/`--disk` run selection against `[targets]` *without*
	/// touching the manifest — the manifest stays the whole, authoritative project
	/// config; the selection is a separate input threaded alongside it (see
	/// [`Self::images_for`] / [`Self::disks_for`]). Called once up front so a bad
	/// selection fails before any build work, with the same message the filters would
	/// otherwise hit lazily.
	///
	/// `target` `None` (no `--target`) is the whole matrix; a `target` must be one of
	/// the project's arches; `disks` (only meaningful with a `target`) must each be one
	/// of that arch's types.
	///
	/// # Errors
	///
	/// Returns an error if `disks` is given without a `target`, if `target` isn't one
	/// of the project's arches, or if a requested disk type isn't built for it — each
	/// naming the valid choices.
	pub fn check_selection(&self, target: Option<Arch>, disks: &[DiskType]) -> Result<()> {
		if !disks.is_empty() && target.is_none() {
			bail!(
				"`--disk` needs `--target`: name the architecture whose disk(s) to build, \
				 e.g. `--target {} --disk {}`",
				self.targets
					.arches()
					.first()
					.map_or_else(|| "aarch64".to_string(), Arch::to_string),
				disks[0],
			);
		}
		let Some(arch) = target else { return Ok(()) };
		let available = self.targets.types(arch);
		if available.is_empty() {
			bail!(
				"`--target {arch}` isn't a target of this project; `[targets]` builds: {}",
				join_display(self.targets.arches()),
			);
		}
		for d in disks {
			if !available.contains(d) {
				bail!(
					"`--disk {d}` isn't built for {arch}; `[targets] {arch}` lists: {}",
					join_display(available.iter().copied()),
				);
			}
		}
		Ok(())
	}

	/// The [`ImageRef`]s a run acts on: every arch in `[targets]` for an unfiltered
	/// run (`target` `None`), or just `target`'s when one is named. The `--target`
	/// projection of [`Self::images`] — same vec, optionally restricted to one arch —
	/// so the build/disk fan-outs iterate the run's arches without the manifest itself
	/// being narrowed. Validate `target` with [`Self::check_selection`] first (a
	/// `target` not in `[targets]` yields an [`ImageRef`] for an arch the project
	/// doesn't build).
	#[must_use]
	pub fn images_for(&self, target: Option<Arch>) -> Vec<ImageRef> {
		match target {
			Some(arch) => vec![self.image(arch)],
			None => self.images(),
		}
	}

	/// The disk types to build for `arch` this run: its full `[targets]` list, or —
	/// when `filter` (`provision --disk`) is non-empty — only the listed types, in
	/// manifest order (a type repeated on the command line is built once). The
	/// `--disk` projection of [`Self::disk_types_for`]. `filter` is only ever
	/// non-empty alongside a `--target` (enforced by [`Self::check_selection`]), so it
	/// only narrows that one arch.
	#[must_use]
	pub fn disks_for(&self, arch: Arch, filter: &[DiskType]) -> Vec<DiskType> {
		let available = self.disk_types_for(arch);
		if filter.is_empty() {
			available.to_vec()
		} else {
			available.iter().copied().filter(|d| filter.contains(d)).collect()
		}
	}

	/// Builder spec for `arch`'s container build / `deploy` / `upgrade` (its
	/// `[builder.<arch>] build` override, else the flat `[builder] build`; `local`
	/// unless configured).
	#[must_use]
	pub fn build_builder(&self, arch: Arch) -> &BuilderSpec {
		self.builder.build_for(arch)
	}

	/// Builder spec for `arch`'s `image-builder` step (its
	/// `[builder.<arch>] image` override, else the flat `[builder] image`; `local`
	/// unless configured).
	#[must_use]
	pub fn image_builder(&self, arch: Arch) -> &BuilderSpec {
		self.builder.image_for(arch)
	}

	/// The configured deployment targets (the `[deploy] remotes` array; empty
	/// unless configured).
	#[must_use]
	pub fn deploy_remotes(&self) -> &[RemoteConfig] {
		&self.deploy.remotes
	}

	/// The configured parallel-worker caps (the `[concurrency]` table; every
	/// activity unbounded unless configured).
	#[must_use]
	pub fn concurrency(&self) -> &ConcurrencyConfig {
		&self.concurrency
	}

	/// The configured lifecycle hooks (the `[hooks]` table; all-empty unless
	/// configured).
	#[must_use]
	pub fn hooks(&self) -> &Hooks {
		&self.hooks
	}

	/// The signing config when image signing is in effect — i.e. `[deploy] registry`
	/// is a [`RegistryConfig::WithSigning`] entry. `None` means no signing. The
	/// single switch the push/provision/upgrade paths gate on.
	#[must_use]
	pub fn signing(&self) -> Option<SigningConfig> {
		self.deploy.registry.as_ref()?.signing()
	}
}

/// The `[targets]` default when the manifest omits the table: a single entry for
/// the host arch (or `x86_64` on an arch bootcher doesn't build for — a deliberate,
/// overridable fallback) mapped to a lone `qcow2`.
fn default_targets() -> Targets {
	let mut dt = Targets::default();
	// A ready-to-boot qcow2 for the host arch — the most broadly useful default
	// artifact (runs as-is under qemu/libvirt and most clouds). Edit the table for a
	// cross-arch target, more arches, or more types per arch.
	match Arch::host().unwrap_or(Arch::X86_64) {
		Arch::X86_64 => dt.x86_64 = Some(vec![DiskType::Qcow2]),
		Arch::Aarch64 => dt.aarch64 = Some(vec![DiskType::Qcow2]),
	}
	dt
}

/// Deserialize one `[targets]` entry from either a bare type or an array of them,
/// yielding `Some(Vec<DiskType>)`. An empty array is rejected (nothing to build for
/// that arch), and duplicates are dropped keeping first-listed order so a type is
/// never built twice.
fn de_target_disks<'de, D>(d: D) -> Result<Option<Vec<DiskType>>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	use serde::de::Error;
	#[derive(Deserialize)]
	#[serde(untagged)]
	enum OneOrMany {
		One(DiskType),
		Many(Vec<DiskType>),
	}
	let mut types = match OneOrMany::deserialize(d)? {
		OneOrMany::One(t) => vec![t],
		OneOrMany::Many(v) => v,
	};
	if types.is_empty() {
		return Err(D::Error::custom("a `[targets]` entry must list at least one type"));
	}
	let mut seen = Vec::new();
	types.retain(|t| {
		let new = !seen.contains(t);
		if new {
			seen.push(*t);
		}
		new
	});
	Ok(Some(types))
}

/// The `rootfs` default when the manifest omits it: `ext4`, the most broadly
/// compatible rootfs `image-builder` supports.
fn default_rootfs() -> Rootfs {
	Rootfs::Ext4
}

/// Comma-join a sequence of `Display` values — for the "valid choices were …"
/// tail of a [`Manifest::check_selection`] error.
fn join_display<T: std::fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
	items.into_iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

/// The `[builder]` default for either role when the manifest omits it: in-process
/// `local`. See `choose` in [`crate::builder`] for the spec grammar.
fn default_builder() -> BuilderSpec {
	BuilderSpec::Str("local".to_owned())
}

/// The single image a bootcher project builds, for a given target [`Arch`].
/// Constructed from the project [`Manifest`] plus the CLI `<arch>` via
/// [`Manifest::image`], so downstream code treats its fields as plain data.
#[derive(Clone, Debug)]
pub struct ImageRef {
	pub name: String,
	pub arch: Arch,
	/// Root filesystem to format (`--bootc-default-fs`), from the manifest's
	/// `[general] rootfs`. See [`Rootfs`].
	pub rootfs: Rootfs,
	/// Build context directory (the project root). The `Containerfile` is
	/// resolved as `build_ctx.join("Containerfile")`.
	pub build_ctx: PathBuf,
	/// Registry namespace from the manifest, if any — the registry/LAN deploy
	/// switch (see [`Manifest::registry_list_ref`]).
	pub registry: Option<String>,
	/// OCI provenance labels to stamp on the built container, as ready-to-splice
	/// `--label <key>=<value>` argv tokens (from [`GitProvenance::label_args`]).
	/// Empty unless the build path enriched it — [`Manifest::image`] leaves it empty
	/// so reading an image ref never shells out to git.
	pub labels: Vec<String>,
}

impl fmt::Display for ImageRef {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}:{}", self.name, self.arch)
	}
}

impl ImageRef {
	/// `localhost/<name>:latest-<arch>` — the per-arch **member** tag. Each arch's
	/// container is built and stored under it so the arches coexist in podman's
	/// flat tag namespace; it's also a member of the suffix-free
	/// [`Manifest::local_list_ref`] manifest list. The `latest-` prefix keeps the
	/// door open for future channels (`stable-aarch64`, etc.).
	#[must_use]
	pub fn tag(&self) -> String {
		format!("localhost/{}:latest-{}", self.name, self.arch)
	}
}

/// One disk artifact to build: an arch's container [`ImageRef`] plus the
/// `image-builder` type to render it as. The unit the `disk` step builds
/// (`Manifest::disk_types_for` yields an arch's types); an arch with N disk types
/// produces N targets that share its one container build.
#[derive(Clone, Debug)]
pub struct DiskTarget {
	pub image: ImageRef,
	pub disk_type: DiskType,
}

impl DiskTarget {
	/// Path to this target's `image-builder` output directory (holding the flat
	/// `disk.<ext>`), `output/<arch>/<disk_type>/`. Nested by arch *and* type so
	/// neither a cross-arch build nor an arch's multiple disk types clobber each
	/// other — some types even share an extension (`raw` and `ami` both yield
	/// `disk.raw`), so the type segment, not the filename, keeps them apart.
	#[must_use]
	pub fn output_dir(&self) -> PathBuf {
		PathBuf::from("output").join(self.image.arch.to_string()).join(self.disk_type.to_string())
	}
}

/// The current UTC instant as a `CalVer` tag. With a git `short_sha` it reads
/// `YYYYMMDD.HHMM.g<short-sha>` (e.g. `20260113.1233.g0a1b2c3d4e5f`); without one
/// (no git) it falls back to `YYYYMMDD.HH.MM` (e.g. `20260113.12.33`).
///
/// The commit suffix is what makes the tag a *stable identity* rather than a mere
/// clock reading: it pins the exact source the image was built from, so the tag
/// can't silently mean two different trees built in the same minute. Without git
/// the timestamp is all we have, so minute resolution is the collision floor: two
/// separate deploys within the same minute reuse the tag, and the later one
/// silently re-points it at its own (freshly built, distinct) digest.
/// UTC throughout so tags from different machines sort and compare unambiguously.
#[must_use]
pub fn calver_now(short_sha: Option<&str>) -> String {
	let now = jiff::Timestamp::now();
	match short_sha {
		Some(sha) => format!("{}.g{sha}", now.strftime("%Y%m%d.%H%M")),
		None => now.strftime("%Y%m%d.%H.%M").to_string(),
	}
}

/// Git provenance for the project tree at `dir` (the build context / project
/// root): the commit a build came from and a human-readable description of it.
/// Stamped onto the built container as the standard OCI `org.opencontainers.image.*`
/// labels (so a device's `bootc status` can name what it's running) and surfaced
/// in `BOOTCHER_METADATA` for hooks.
///
/// Strictly best-effort: every field is `None` when it can't be resolved — `dir`
/// isn't a git work tree, `git` isn't installed, or the repo has no commit yet —
/// so a project that doesn't live in git just builds without provenance rather
/// than failing. This is the one place bootcher consults git; it never gates a build.
#[derive(Clone, Debug, Default)]
pub struct GitProvenance {
	/// Full commit SHA (`git rev-parse HEAD`), suffixed `-dirty` when the work tree
	/// has uncommitted changes — so a label can't silently claim a clean commit the
	/// image wasn't actually built from. `org.opencontainers.image.revision`.
	pub revision: Option<String>,
	/// Human description (`git describe --tags --always --dirty`): the nearest tag
	/// (with the commit distance/SHA when ahead of it), or a short SHA when the repo
	/// is untagged, `-dirty`-suffixed likewise. `org.opencontainers.image.version`.
	pub version: Option<String>,
}

impl GitProvenance {
	/// Detect the provenance of the work tree at `dir` (the build context). Runs a
	/// few quiet `git` queries; any failure leaves the corresponding field `None`
	/// (see the type docs). Called once on the build path, not from
	/// [`Manifest::image`], so merely reading an image ref never shells out to git.
	#[must_use]
	pub fn detect(dir: &Path) -> Self {
		let revision = git_capture(dir, &["rev-parse", "HEAD"])
			.map(|sha| if git_dirty(dir) { format!("{sha}-dirty") } else { sha });
		let version = git_capture(dir, &["describe", "--tags", "--always", "--dirty"]);
		Self { revision, version }
	}

	/// The abbreviated commit for use as the `CalVer` tag suffix (see [`calver_now`]):
	/// the first 12 hex chars of [`Self::revision`], with its `-dirty` marker preserved
	/// (so a tag built from an uncommitted tree can't masquerade as the clean commit).
	/// `None` when there's no revision — i.e. no git — which is exactly when
	/// [`calver_now`] drops back to its timestamp-only form.
	#[must_use]
	pub fn short_sha(&self) -> Option<String> {
		self.revision.as_deref().map(|rev| {
			let (sha, dirty) = match rev.strip_suffix("-dirty") {
				Some(clean) => (clean, "-dirty"),
				None => (rev, ""),
			};
			let short: String = sha.chars().take(12).collect();
			format!("{short}{dirty}")
		})
	}

	/// The provenance as ready-to-splice `--label <key>=<value>` argv tokens — one
	/// pair per field that resolved, empty when none did. Reused for both the argv
	/// (in-process build) and shell-string (remote build, via
	/// [`crate::podman::opts_shell`]) `podman build` invocations, so the two backends
	/// stamp identical labels.
	#[must_use]
	pub fn label_args(&self) -> Vec<String> {
		let mut out = Vec::new();
		for (key, val) in [
			("org.opencontainers.image.revision", &self.revision),
			("org.opencontainers.image.version", &self.version),
		] {
			if let Some(val) = val {
				out.push("--label".to_owned());
				out.push(format!("{key}={val}"));
			}
		}
		out
	}
}

/// Run `git -C <dir> <args>`, returning trimmed stdout, or `None` on any failure
/// (not a git work tree, `git` missing, empty repo, non-zero exit). git's stderr is
/// discarded — provenance detection is silent and best-effort. An empty result is
/// also `None`, so a clean `git status --porcelain` reads as "no output".
fn git_capture(dir: &Path, args: &[&str]) -> Option<String> {
	let mut full: Vec<OsString> = vec!["-C".into(), dir.into()];
	full.extend(args.iter().map(OsString::from));
	duct::cmd("git", full)
		.stderr_null()
		.stdout_capture()
		.unchecked()
		.run()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| {
			let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
			(!s.is_empty()).then_some(s)
		})
}

/// Whether the work tree at `dir` has uncommitted changes (`git status --porcelain`
/// is non-empty). Only consulted once `rev-parse HEAD` has confirmed `dir` is a git
/// repo, so a `false` here always means "clean", never "not git".
fn git_dirty(dir: &Path) -> bool {
	git_capture(dir, &["status", "--porcelain"]).is_some()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(toml: &str) -> Manifest {
		toml::from_str(toml).unwrap()
	}

	#[test]
	fn disk_type_renders_its_image_builder_type_name() {
		// Display must equal the literal `image-builder` image-type value, since it's
		// passed verbatim as the positional argument. `qcow2` (no word break) and
		// `bootc-installer` (hyphenated) are the kebab-case edge cases.
		assert_eq!(DiskType::Raw.to_string(), "raw");
		assert_eq!(DiskType::Qcow2.to_string(), "qcow2");
		assert_eq!(DiskType::BootcInstaller.to_string(), "bootc-installer");
	}

	#[test]
	fn rootfs_renders_its_flag_value() {
		assert_eq!(Rootfs::Ext4.to_string(), "ext4");
		assert_eq!(Rootfs::Btrfs.to_string(), "btrfs");
	}

	#[test]
	fn absent_disk_types_and_rootfs_default_to_host_qcow2_ext4() {
		// Omitting `[targets]` builds a qcow2 for the host arch, and `rootfs`
		// defaults to ext4 — the general-purpose defaults.
		let m = parse("[general]\nname = \"x\"\n");
		let host = Arch::host().unwrap_or(Arch::X86_64);
		assert_eq!(m.disk_types_for(host), [DiskType::Qcow2]);
		assert_eq!(m.images().iter().map(|i| i.arch).collect::<Vec<_>>(), [host]);
		assert_eq!(m.general.rootfs, Rootfs::Ext4);
	}

	#[test]
	fn absent_concurrency_table_is_all_unbounded() {
		// No `[concurrency]` → every activity unset (None = unbounded), and the empty
		// table is skipped on re-serialization so it never leaks into the manifest.
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		let c = m.concurrency();
		assert!(c.build.is_none() && c.disk.is_none() && c.upgrade.is_none() && c.rotate.is_none());
		assert!(c.is_empty());
		assert!(!toml::to_string(&m).unwrap().contains("[concurrency]"));
	}

	#[test]
	fn concurrency_caps_parse_per_activity() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [concurrency]\nbuild = 2\ndisk = 1\n",
		);
		let c = m.concurrency();
		assert_eq!(c.build.map(NonZeroUsize::get), Some(2));
		assert_eq!(c.disk.map(NonZeroUsize::get), Some(1));
		// Unset keys stay unbounded even when the table is present.
		assert!(c.upgrade.is_none() && c.rotate.is_none());
	}

	#[test]
	fn zero_concurrency_cap_is_rejected() {
		// A `0` worker cap is nonsensical; `NonZeroUsize` rejects it at parse time
		// rather than silently meaning "serial" or "unbounded".
		let err = toml::from_str::<Manifest>(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n[concurrency]\nbuild = 0\n",
		)
		.unwrap_err();
		assert!(err.to_string().contains("nonzero"), "unexpected error: {err}");
	}

	#[test]
	fn disk_types_and_rootfs_parse_from_the_manifest() {
		let m = parse(
			"[general]\nname = \"x\"\nrootfs = \"btrfs\"\n\
			 [targets]\nx86_64 = \"qcow2\"\n",
		);
		assert_eq!(m.disk_types_for(Arch::X86_64), [DiskType::Qcow2]);
		assert_eq!(m.general.rootfs, Rootfs::Btrfs);
		// `rootfs` flows onto the derived ImageRef the builders read.
		let img = m.image(Arch::X86_64);
		assert_eq!(img.rootfs, Rootfs::Btrfs);
	}

	#[test]
	fn images_for_and_disks_for_project_the_run_selection() {
		let m = parse(
			"[general]\nname = \"x\"\n\
			 [targets]\nx86_64 = [\"qcow2\", \"raw\"]\naarch64 = \"qcow2\"\n",
		);
		// No `--target`: the whole matrix, untouched.
		assert_eq!(
			m.images_for(None).iter().map(|i| i.arch).collect::<Vec<_>>(),
			[Arch::X86_64, Arch::Aarch64]
		);
		// `--target x86_64`: only that arch's image — but the manifest is unchanged, so
		// its other arch still resolves through the unfiltered accessors.
		assert_eq!(
			m.images_for(Some(Arch::X86_64)).iter().map(|i| i.arch).collect::<Vec<_>>(),
			[Arch::X86_64]
		);
		assert_eq!(m.disk_types_for(Arch::Aarch64), [DiskType::Qcow2]);
		// No `--disk`: the arch's full type list. With a filter: only those, in manifest
		// order (not the order requested).
		assert_eq!(m.disks_for(Arch::X86_64, &[]), [DiskType::Qcow2, DiskType::Raw]);
		assert_eq!(
			m.disks_for(Arch::X86_64, &[DiskType::Raw, DiskType::Qcow2]),
			[DiskType::Qcow2, DiskType::Raw]
		);
		assert_eq!(m.disks_for(Arch::X86_64, &[DiskType::Raw]), [DiskType::Raw]);
	}

	#[test]
	fn check_selection_rejects_bad_requests() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		// Valid selections pass.
		assert!(m.check_selection(Some(Arch::X86_64), &[DiskType::Qcow2]).is_ok());
		assert!(m.check_selection(None, &[]).is_ok());
		// `--disk` without `--target`.
		assert!(m.check_selection(None, &[DiskType::Qcow2]).is_err());
		// An arch the project doesn't build.
		assert!(m.check_selection(Some(Arch::Aarch64), &[]).is_err());
		// A disk type not listed for the requested arch.
		assert!(m.check_selection(Some(Arch::X86_64), &[DiskType::Raw]).is_err());
	}

	#[test]
	fn absent_builder_table_is_all_local() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert_eq!(m.build_builder(Arch::X86_64).spec(), "local");
		assert_eq!(m.image_builder(Arch::X86_64).spec(), "local");
	}

	#[test]
	fn split_roles_are_independent() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [builder]\nbuild = \"local\"\nimage = \"vm\"\n",
		);
		assert_eq!(m.build_builder(Arch::X86_64).spec(), "local");
		assert_eq!(m.image_builder(Arch::X86_64).spec(), "vm");
	}

	#[test]
	fn partial_builder_table_fills_the_other_with_local() {
		// Keys are optional in the file: a missing one defaults to `local` at the
		// accessor rather than failing to parse.
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n[builder]\nbuild = \"vm\"\n",
		);
		assert_eq!(m.build_builder(Arch::X86_64).spec(), "vm");
		assert_eq!(m.image_builder(Arch::X86_64).spec(), "local");
	}

	#[test]
	fn disk_types_entry_parses_scalar_or_array_and_dedups() {
		// A bare type is shorthand for a single-element list…
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert_eq!(m.disk_types_for(Arch::X86_64), [DiskType::Qcow2]);
		// …and an array yields each listed type, in order, with duplicates dropped.
		let m =
			parse("[general]\nname = \"x\"\n[targets]\nx86_64 = [\"raw\", \"qcow2\", \"raw\"]\n");
		assert_eq!(m.disk_types_for(Arch::X86_64), [DiskType::Raw, DiskType::Qcow2]);
		// The present arch fields drive `images()`, x86_64 before aarch64 regardless of
		// the order they're written.
		let m =
			parse("[general]\nname = \"x\"\n[targets]\naarch64 = \"raw\"\nx86_64 = \"qcow2\"\n");
		assert_eq!(
			m.images().iter().map(|i| i.arch).collect::<Vec<_>>(),
			[Arch::X86_64, Arch::Aarch64]
		);
	}

	#[test]
	fn empty_disk_types_entry_is_rejected() {
		// An empty type list for an arch — nothing to build for it — is a manifest
		// error, not a silent no-op. (An arch-less table is rejected by `Manifest::load`.)
		assert!(
			toml::from_str::<Manifest>("[general]\nname = \"x\"\n[targets]\nx86_64 = []\n")
				.is_err()
		);
	}

	#[test]
	fn per_arch_builder_override_falls_back_to_the_flat_default() {
		// `[builder.aarch64]` overrides only the aarch64 image role; everything else —
		// aarch64 build, and both x86_64 roles — inherits the flat default.
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\naarch64 = \"qcow2\"\n\
			 [builder]\nbuild = \"local\"\nimage = \"local\"\n\
			 [builder.aarch64]\nimage = \"vm\"\n",
		);
		assert_eq!(m.build_builder(Arch::X86_64).spec(), "local");
		assert_eq!(m.image_builder(Arch::X86_64).spec(), "local");
		assert_eq!(m.build_builder(Arch::Aarch64).spec(), "local");
		assert_eq!(m.image_builder(Arch::Aarch64).spec(), "vm");
	}

	#[test]
	fn builder_spec_with_ssh_opts_parses_and_threads_opts() {
		// Explicit `type = "remote"` table: host and ssh_opts reach the resolved spec.
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [builder]\nbuild = { type = \"remote\", host = \"user@build-host\", ssh_opts = [\"-i\", \"/key\"] }\n",
		);
		let spec = m.build_builder(Arch::X86_64);
		assert_eq!(spec.spec(), "user@build-host");
		assert_eq!(spec.ssh_opts(), &["-i", "/key"]);
	}

	#[test]
	fn builder_spec_bare_string_has_empty_ssh_opts() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [builder]\nbuild = \"user@build-host\"\n",
		);
		let spec = m.build_builder(Arch::X86_64);
		assert_eq!(spec.spec(), "user@build-host");
		assert!(spec.ssh_opts().is_empty());
		assert!(spec.podman_opts().is_empty());
	}

	#[test]
	fn builder_spec_local_table_carries_podman_opts() {
		// Explicit `type = "local"` resolves to the in-process builder yet still
		// carries its podman_opts — the cross-arch-local case.
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [builder]\nbuild = { type = \"local\", podman_opts = [\"--network=host\"] }\n",
		);
		let spec = m.build_builder(Arch::X86_64);
		assert_eq!(spec.spec(), "local");
		assert!(crate::builder::is_local(spec));
		assert!(spec.ssh_opts().is_empty());
		assert_eq!(spec.podman_opts(), &["--network=host"]);
	}

	#[test]
	fn builder_spec_remote_table_threads_podman_opts() {
		// `type = "remote"` host + podman_opts coexist and both reach the resolved spec.
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [builder]\nimage = { type = \"remote\", host = \"user@h\", podman_opts = [\"--network=host\"] }\n",
		);
		let spec = m.image_builder(Arch::X86_64);
		assert_eq!(spec.spec(), "user@h");
		assert_eq!(spec.podman_opts(), &["--network=host"]);
	}

	#[test]
	fn suffix_free_refs_span_every_arch() {
		// The member tag is per-arch (coexistence), but the list tag and registry
		// ref the device sees carry no arch suffix.
		let m = parse(
			"[general]\nname = \"kiosk\"\n[targets]\naarch64 = \"qcow2\"\n\
			 [deploy]\nregistry = \"reg.example.com/org\"\n",
		);
		let img = m.image(Arch::Aarch64);
		assert_eq!(img.tag(), "localhost/kiosk:latest-aarch64");
		// The list ref and registry refs are project-level (arch-independent) — they
		// live on the manifest, not the per-arch image.
		assert_eq!(m.local_list_ref(), "localhost/kiosk:latest");
		assert_eq!(m.registry_list_ref().as_deref(), Some("reg.example.com/org/kiosk:latest"));
		assert_eq!(
			m.registry_version_ref("20260113.12.33").as_deref(),
			Some("reg.example.com/org/kiosk:20260113.12.33")
		);
	}

	#[test]
	fn registry_version_ref_is_none_without_registry() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.registry_list_ref().is_none());
		assert!(m.registry_version_ref("20260113.12.33").is_none());
	}

	#[test]
	fn calver_now_without_git_is_timestamp_only() {
		// `YYYYMMDD.HH.MM`: 8 digits, dot, 2 digits, dot, 2 digits — all numeric.
		let v = calver_now(None);
		let (date, time) = v.split_once('.').expect("a dot after the date");
		let (hh, mm) = time.split_once('.').expect("a dot between hour and minute");
		assert_eq!(date.len(), 8, "date is YYYYMMDD: {v}");
		assert_eq!(hh.len(), 2, "hour is HH: {v}");
		assert_eq!(mm.len(), 2, "minute is MM: {v}");
		assert!(v.chars().all(|c| c.is_ascii_digit() || c == '.'), "digits and dots only: {v}");
	}

	#[test]
	fn calver_now_with_git_appends_commit_suffix() {
		// `YYYYMMDD.HHMM.g<short-sha>`: minute folded into one field, commit suffixed.
		let v = calver_now(Some("0a1b2c3d4e5f"));
		let (date, rest) = v.split_once('.').expect("a dot after the date");
		let (hhmm, sha) = rest.split_once('.').expect("a dot before the commit suffix");
		assert_eq!(date.len(), 8, "date is YYYYMMDD: {v}");
		assert_eq!(hhmm.len(), 4, "time is HHMM: {v}");
		assert!(hhmm.chars().all(|c| c.is_ascii_digit()), "time is numeric: {v}");
		assert_eq!(sha, "g0a1b2c3d4e5f", "commit suffix is g-prefixed: {v}");
	}

	#[test]
	fn short_sha_truncates_and_keeps_dirty_marker() {
		let clean = GitProvenance { revision: Some("0a1b2c3d4e5f6a7b8c9d".to_owned()), version: None };
		assert_eq!(clean.short_sha().as_deref(), Some("0a1b2c3d4e5f"));

		let dirty =
			GitProvenance { revision: Some("0a1b2c3d4e5f6a7b8c9d-dirty".to_owned()), version: None };
		assert_eq!(dirty.short_sha().as_deref(), Some("0a1b2c3d4e5f-dirty"));

		let none = GitProvenance::default();
		assert_eq!(none.short_sha(), None);
	}

	/// Run `git -C dir <args>` in a test, asserting it succeeds — the setup half of
	/// the provenance tests (the production [`git_capture`] is the read half).
	fn git(dir: &Path, args: &[&str]) {
		let mut full: Vec<OsString> = vec!["-C".into(), dir.into()];
		full.extend(args.iter().map(OsString::from));
		let out = duct::cmd("git", full).stdout_null().stderr_capture().unchecked().run().unwrap();
		assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
	}

	#[test]
	fn provenance_detects_revision_and_tag_then_dirty() {
		let repo = tempfile::TempDir::new().unwrap();
		let dir = repo.path();
		// A self-contained repo: local identity and no signing, so the commit doesn't
		// depend on (or trip over) the host's global git config.
		git(dir, &["init", "-q"]);
		git(dir, &["config", "user.email", "t@example.com"]);
		git(dir, &["config", "user.name", "Test"]);
		git(dir, &["config", "commit.gpgsign", "false"]);
		fs::write(dir.join("Containerfile"), "FROM scratch\n").unwrap();
		git(dir, &["add", "-A"]);
		git(dir, &["commit", "-qm", "init"]);
		git(dir, &["tag", "v1.0.0"]);

		// Clean tree: revision is the full HEAD sha (no `-dirty`), version is the tag.
		let head = git_capture(dir, &["rev-parse", "HEAD"]).expect("HEAD sha");
		let clean = GitProvenance::detect(dir);
		assert_eq!(clean.revision.as_deref(), Some(head.as_str()));
		assert_eq!(clean.version.as_deref(), Some("v1.0.0"));

		// Dirty the work tree: both fields gain the `-dirty` suffix.
		fs::write(dir.join("Containerfile"), "FROM scratch\nRUN true\n").unwrap();
		let dirty = GitProvenance::detect(dir);
		assert_eq!(dirty.revision.as_deref(), Some(format!("{head}-dirty").as_str()));
		assert_eq!(dirty.version.as_deref(), Some("v1.0.0-dirty"));
	}

	#[test]
	fn provenance_in_untagged_repo_uses_short_sha() {
		let repo = tempfile::TempDir::new().unwrap();
		let dir = repo.path();
		git(dir, &["init", "-q"]);
		git(dir, &["config", "user.email", "t@example.com"]);
		git(dir, &["config", "user.name", "Test"]);
		git(dir, &["config", "commit.gpgsign", "false"]);
		fs::write(dir.join("Containerfile"), "FROM scratch\n").unwrap();
		git(dir, &["add", "-A"]);
		git(dir, &["commit", "-qm", "init"]);
		// No tag: revision is still the full HEAD sha, but `git describe --always`
		// falls back to the abbreviated sha — which is a prefix of the full revision.
		let head = git_capture(dir, &["rev-parse", "HEAD"]).expect("HEAD sha");
		let p = GitProvenance::detect(dir);
		assert_eq!(p.revision.as_deref(), Some(head.as_str()));
		let version = p.version.expect("describe falls back to a short sha");
		assert!(!version.is_empty(), "version is the abbreviated sha, not empty");
		assert!(
			head.starts_with(&version),
			"untagged describe ({version}) should be a prefix of HEAD ({head})"
		);
	}

	#[test]
	fn provenance_is_empty_outside_a_git_tree() {
		// A bare temp dir (no `git init`) resolves nothing — provenance is best-effort.
		let dir = tempfile::TempDir::new().unwrap();
		let p = GitProvenance::detect(dir.path());
		assert_eq!(p.revision, None);
		assert_eq!(p.version, None);
		assert!(p.label_args().is_empty());
	}

	#[test]
	fn provenance_label_args_emits_only_present_fields() {
		// Both fields → two `--label k=v` pairs, revision before version.
		let both =
			GitProvenance { revision: Some("abc123".into()), version: Some("v1.0.0".into()) };
		assert_eq!(
			both.label_args(),
			[
				"--label",
				"org.opencontainers.image.revision=abc123",
				"--label",
				"org.opencontainers.image.version=v1.0.0",
			]
		);
		// A single resolved field yields a single pair…
		let rev_only = GitProvenance { revision: Some("abc123".into()), version: None };
		assert_eq!(rev_only.label_args(), ["--label", "org.opencontainers.image.revision=abc123"]);
		// …and nothing resolved (not a git tree) stamps no labels at all.
		assert!(GitProvenance::default().label_args().is_empty());
	}

	#[test]
	fn absent_deploy_table_has_no_remotes() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.deploy_remotes().is_empty());
	}

	#[test]
	fn deploy_remotes_parse_in_order() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [deploy]\nremotes = [\"a@h1\", \"b@h2:2222\"]\n",
		);
		assert_eq!(
			m.deploy_remotes(),
			[RemoteConfig::ConnStr("a@h1".into()), RemoteConfig::ConnStr("b@h2:2222".into())]
		);
	}

	#[test]
	fn deploy_remotes_accept_the_object_form_with_ssh_opts() {
		// A bare string and an object with per-remote `ssh_opts` may be mixed in one
		// array (the untagged `RemoteConfig`).
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [deploy]\nremotes = [\
			 \"plain@h1\", \
			 { remote = \"ssh://o@h2:2222\", ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }]\n",
		);
		assert_eq!(
			m.deploy_remotes(),
			[
				RemoteConfig::ConnStr("plain@h1".into()),
				RemoteConfig::WithOpts {
					remote: "ssh://o@h2:2222".into(),
					ssh_opts: vec!["-o".into(), "UserKnownHostsFile=/dev/null".into()],
					takeover_login: None,
				},
			]
		);
		// The opts reach the resolved `Ssh`: they land in the argv before the host.
		let ssh = &m.deploy_remotes().to_ssh()[1];
		let argv: Vec<String> =
			ssh.argv(&[], "sh").iter().map(|a| a.to_string_lossy().into_owned()).collect();
		assert!(argv.windows(2).any(|w| w[0] == "-o" && w[1] == "UserKnownHostsFile=/dev/null"));
		let opt = argv.iter().position(|a| a == "UserKnownHostsFile=/dev/null").unwrap();
		let host = argv.iter().position(|a| a == "ssh://o@h2:2222").unwrap();
		assert!(opt < host, "ssh_opts must precede the host");
	}

	#[test]
	fn absent_hooks_table_is_empty() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.hooks().is_empty());
		assert!(m.hooks().build.is_empty() && m.hooks().upgrade.is_empty());
	}

	#[test]
	fn hooks_parse_each_key_independently() {
		// Only the keys present are set; the rest stay `None`, and an untouched phase
		// stays empty (a partial section is fine, like `[builder]`).
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [hooks.build]\npre = \"a.sh\"\n\
			 [hooks.disk]\npost = \"b.sh\"\n",
		);
		assert!(!m.hooks().is_empty());
		assert_eq!(m.hooks().build.pre.as_deref(), Some("a.sh"));
		assert!(m.hooks().build.post.is_none());
		assert_eq!(m.hooks().disk.post.as_deref(), Some("b.sh"));
		assert!(m.hooks().disk.pre.is_none());
		assert!(m.hooks().upgrade.is_empty());
	}

	#[test]
	fn empty_hooks_are_omitted_from_a_serialized_manifest() {
		// A hookless project round-trips with no `[hooks]` table at all, so
		// `bootcher init` can document the section as comments instead.
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		let rendered = toml::to_string(&m).unwrap();
		assert!(!rendered.contains("[hooks"), "empty hooks must not serialize: {rendered}");
	}

	#[test]
	fn plain_registry_url_has_no_signing() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [deploy]\nregistry = \"reg.example.com/org\"\n",
		);
		assert_eq!(m.registry(), Some("reg.example.com/org"));
		assert!(m.signing().is_none());
		// A plain URL serializes as a bare string, not an inline table.
		let s = toml::to_string(&m).unwrap();
		assert!(s.contains("registry = \"reg.example.com/org\""), "{s}");
	}

	#[test]
	fn signing_registry_parses_and_defaults_pub_key_to_the_sibling() {
		let m = parse(
			"[general]\nname = \"kiosk\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [deploy]\nregistry = { url = \"reg.example.com/org\", key = \"cosign.key\" }\n",
		);
		assert_eq!(m.registry(), Some("reg.example.com/org"));
		let s = m.signing().expect("signing present");
		assert_eq!(s.key, "cosign.key");
		// Unset `pub_key` ⇒ the `<key>.pub` sibling.
		assert_eq!(s.public_key_path(), "cosign.pub");
	}

	#[test]
	fn manifest_schema_advertises_draft_and_the_disk_types_shorthand() {
		// The emitted schema is draft 2020-12 and, crucially, accepts each
		// `[targets]` entry as either a bare type or an array — the
		// shorthand the custom deserializer allows but schemars can't read off
		// `deserialize_with`.
		let schema: serde_json::Value =
			serde_json::from_str(&manifest_schema_json()).expect("schema is valid JSON");
		assert_eq!(
			schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
			"schema: {schema}"
		);
		let any_of = &schema["$defs"]["DiskTypeList"]["anyOf"];
		assert!(any_of.is_array() && any_of.as_array().unwrap().len() == 2, "schema: {schema}");
		// DiskType is a closed string enum (rendered as `oneOf` of `const`s because each
		// variant carries a doc description), so editors can complete the type values.
		assert!(schema["$defs"]["DiskType"]["oneOf"].is_array(), "schema: {schema}");
	}

	#[test]
	fn rewrite_ssh_user_handles_both_destination_forms() {
		// Bare host gains a user; an existing `user@` is replaced.
		assert_eq!(rewrite_ssh_user("host", "admin"), "admin@host");
		assert_eq!(rewrite_ssh_user("debian@host", "admin"), "admin@host");
		// `ssh://` URLs keep their scheme and port; the user is rewritten in place.
		assert_eq!(rewrite_ssh_user("ssh://host:2222", "admin"), "ssh://admin@host:2222");
		assert_eq!(rewrite_ssh_user("ssh://root@host:2222", "admin"), "ssh://admin@host:2222");
	}

	#[test]
	fn takeover_ssh_resolves_login_per_host_then_flag_then_errors() {
		// Per-host `takeover_login` wins over the `--login` default and retargets the
		// destination's user (the steady-state remote is `admin@h`).
		let per_host = RemoteConfig::WithOpts {
			remote: "admin@h".into(),
			ssh_opts: vec!["-i".into(), "/k".into()],
			takeover_login: Some("cloud-user".into()),
		};
		let ssh = per_host.takeover_ssh(Some("ubuntu")).unwrap();
		assert_eq!(ssh.host(), "cloud-user@h");
		// The ssh_opts ride along.
		let argv: Vec<String> =
			ssh.argv(&[], "sh").iter().map(|a| a.to_string_lossy().into_owned()).collect();
		assert!(argv.windows(2).any(|w| w[0] == "-i" && w[1] == "/k"));

		// A bare ConnStr has no per-host login, so it falls back to the `--login` flag.
		let bare = RemoteConfig::ConnStr("admin@h".into());
		assert_eq!(bare.takeover_ssh(Some("debian")).unwrap().host(), "debian@h");
		// …and with neither, it's a hard error (no safe cross-distro default).
		assert!(bare.takeover_ssh(None).is_err());
	}

	#[test]
	fn admin_ssh_appends_the_injected_identity() {
		// The post-reboot identity is `admin@host` (the steady-state remote) plus the
		// just-injected `--ssh-key` private half as an extra `-i`.
		let rc = RemoteConfig::ConnStr("admin@h".into());
		let ssh = rc.admin_ssh("/path/to/key");
		assert_eq!(ssh.host(), "admin@h");
		let argv: Vec<String> =
			ssh.argv(&[], "sh").iter().map(|a| a.to_string_lossy().into_owned()).collect();
		assert!(argv.windows(2).any(|w| w[0] == "-i" && w[1] == "/path/to/key"));
	}

	#[test]
	fn takeover_login_parses_in_the_object_form() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [deploy]\nremotes = [{ remote = \"admin@h\", takeover_login = \"debian\" }]\n",
		);
		assert_eq!(
			m.deploy_remotes(),
			[RemoteConfig::WithOpts {
				remote: "admin@h".into(),
				ssh_opts: vec![],
				takeover_login: Some("debian".into()),
			}]
		);
	}

	#[test]
	fn absent_registry_has_no_signing() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.registry().is_none());
		assert!(m.signing().is_none());
	}

	fn table(toml: &str) -> toml::Table {
		toml::from_str(toml).unwrap()
	}

	#[test]
	fn deep_merge_recurses_tables_and_replaces_leaves() {
		// Sub-tables merge field-by-field: the override touches one key of `[general]`
		// and adds a `[builder]` section, while leaving the base's other `[general]`
		// keys intact. Scalars present in both take the override's value.
		let base = table("[general]\nname = \"base\"\nrootfs = \"ext4\"\n");
		let over = table("[general]\nname = \"ci\"\n[builder]\nbuild = \"vm\"\n");
		let merged = deep_merge(base, over);
		let m: Manifest = toml::Value::Table(merged).try_into().unwrap();
		assert_eq!(m.general.name, "ci"); // override wins
		assert_eq!(m.general.rootfs, Rootfs::Ext4); // base key preserved
		assert_eq!(m.build_builder(Arch::host().unwrap_or(Arch::X86_64)).spec(), "vm"); // added
	}

	#[test]
	fn deep_merge_replaces_arrays_wholesale() {
		// An array isn't concatenated — the override's list replaces the base's entirely.
		let base = table("x86_64 = [\"raw\", \"qcow2\"]\n");
		let over = table("x86_64 = [\"qcow2\"]\n");
		let dt: Targets = toml::Value::Table(deep_merge(base, over)).try_into().unwrap();
		assert_eq!(dt.types(Arch::X86_64), [DiskType::Qcow2]);
	}
}
