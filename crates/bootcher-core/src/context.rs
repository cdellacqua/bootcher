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

/// The image-config label every member is stamped with at build time, naming the
/// commit it was built from (`git rev-parse HEAD`). Only a clean work tree stamps a
/// commit; a dirty tree, or a project outside git, stamps it empty, so a dirty build
/// can never pass a source check (an inherited value from the base is overwritten
/// either way). The `release` and `channel` commands read it to decide which commit a
/// release records, and the refreshes rebuild that commit.
pub const SOURCE_LABEL: &str = "bootcher.source";

/// The manifest-list (index) annotation naming the release a list was published as
/// (`bootcher release publish X`). Written when the list is assembled, so the build
/// never needs to know the release; a channel, being a byte-for-byte copy of a
/// release's index, carries it too.
pub const RELEASE_ANNOTATION: &str = "bootcher.release";

/// The OCI annotation buildah stamps on every image built `FROM` a registry image,
/// naming that base (`quay.io/fedora/fedora-bootc:44`).
pub const BASE_NAME_ANNOTATION: &str = "org.opencontainers.image.base.name";

/// The OCI annotation buildah stamps with the base's **per-arch** manifest digest
/// at build time. Compared against the base's current digest to tell whether a
/// newer base (a patch) has been published since.
pub const BASE_DIGEST_ANNOTATION: &str = "org.opencontainers.image.base.digest";

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
	Hash,
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

/// The per-project manifest, parsed from [`MANIFEST`] in the project root. A
/// bootcher project is a single image: the manifest names it (used for the
/// container tag and the output dir) and everything else is convention relative to
/// the project root — the `Containerfile` and the `sysroot/` overlay live alongside
/// it. Keys live under tables (`[general]`, `[targets]`, `[builder]`, `[registry]`,
/// `[concurrency]`, `[hooks]`) plus the top-level `remotes` device list.
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
	/// The devices bootcher reaches over SSH (`device deploy`, `device status`, the
	/// channel device steps, `device rotate`, `device takeover`). Each entry is a bare
	/// `[user@]host` or a table with per-device `ssh_opts`; see [`RemoteConfig`]. There
	/// is no per-device mode or channel: what a device follows is read live from the
	/// device itself. Defaults to no devices.
	#[serde(default)]
	pub remotes: Vec<RemoteConfig>,
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
	/// The registry releases and channels are published to; see [`RegistryConfig`].
	/// Absent for a project that only updates its devices directly over SSH.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub registry: Option<RegistryConfig>,
	/// Per-activity caps on parallel worker counts; see [`ConcurrencyConfig`].
	/// Defaults to unbounded (host core count) for every activity when the table
	/// is absent, and is omitted from a freshly serialized manifest so `bootcher
	/// init` can document it with commented examples instead of an empty table.
	#[serde(default, skip_serializing_if = "ConcurrencyConfig::is_empty")]
	pub concurrency: ConcurrencyConfig,
	/// Lifecycle hook commands wrapping bootcher's phases; see [`Hooks`].
	/// Defaults to no hooks when the table is absent, and is omitted from a
	/// freshly serialized manifest so `bootcher init` can document it with
	/// commented examples instead of an empty table.
	#[serde(default, skip_serializing_if = "Hooks::is_empty")]
	pub hooks: Hooks,
	/// The project directory this manifest describes: the build context, the root
	/// every relative path resolves against, the hooks' working directory, and the
	/// tree git provenance is read from. Set by [`Manifest::load_in`]; the current
	/// directory unless a refresh loads a commit's own config from a temporary
	/// worktree. Never part of the file.
	#[serde(skip)]
	#[schemars(skip)]
	pub root: PathBuf,
	/// The manifest's path relative to [`Manifest::root`] (the `--manifest` value), so
	/// a refresh loads the same relative path from a release commit's worktree.
	#[serde(skip)]
	#[schemars(skip)]
	pub manifest_path: PathBuf,
}

/// Opt-in cosign/sigstore image signing config, derived from [`RegistryConfig::key`].
/// When present, every publish signs the pushed release with the local private key,
/// a channel device gets the matching public key + a signature-requiring
/// `policy.json` (provision, takeover, and every channel device step install them),
/// and the image's install drop-in records a verifying `ostree-image-signed` origin —
/// so a tampered or unsigned push is rejected on `bootc upgrade`.
///
/// The public key is always the `<key>.pub` sibling (`cosign.key` → `cosign.pub`),
/// matching the naming `bootcher signing enroll` produces. The signing **passphrase**
/// is never stored: it's read from `BOOTCHER_SIGN_PASSPHRASE` (or prompted on a TTY),
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
		public_key_path(&self.key)
	}
}

/// The `.pub` sibling of a private-key path (see [`SigningConfig::public_key_path`]).
#[must_use]
pub fn public_key_path(key: &str) -> String {
	for ext in [".key", ".private"] {
		if let Some(stem) = key.strip_suffix(ext) {
			return format!("{stem}.pub");
		}
	}
	format!("{key}.pub")
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

/// One entry in the top-level `remotes` array. Two interchangeable shapes, so the
/// common case stays terse and the rare per-device tweak doesn't force every entry
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
/// `From` impl below. A remote is named on the command line (`--remote`) by its full
/// destination or by its bare host (see [`RemoteConfig::matches`]).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RemoteConfig {
	/// Bare connection string: the host with no extra ssh args.
	ConnStr(#[serde(default)] String),
	/// A host plus per-device extra ssh args.
	WithOpts {
		#[serde(default)]
		remote: String,
		/// Extra `ssh` args prepended to every connection to this device — e.g.
		/// `["-o", "StrictHostKeyChecking=accept-new"]` or `["-i", "/path/to/key"]`.
		/// For a device whose ssh needs more than the user's `~/.ssh/config`
		/// provides, without a config file.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		ssh_opts: Vec<String>,
		/// The stock cloud login (`debian`/`ubuntu`/`cloud-user`/`root`) `bootcher
		/// device takeover` uses for its *initial* connection to this host, before the
		/// image's `admin` user replaces it. Per-host override of the fleet-wide
		/// `--login`; unused outside takeover. The steady-state identity stays
		/// `admin@` (the `remote` above), so after takeover this is an ordinary
		/// remote. See [`RemoteConfig::takeover_ssh`].
		#[serde(default, skip_serializing_if = "Option::is_none")]
		takeover_login: Option<String>,
	},
}

/// The `[registry]` table: where releases and channels are published, and the
/// optional signing key. A project without it only updates its devices directly
/// over SSH (`bootcher device deploy`).
///
/// ```toml
/// [registry]
/// url = "registry.example.com/org/project"
/// channels = ["stable", "next"]
/// key = "cosign.key"   # optional: sign every release
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RegistryConfig {
	/// Registry namespace the image is pushed under (`<url>/<name>:<tag>`), e.g.
	/// `ghcr.io/acme` or `registry.example.com/org/project`.
	pub url: String,
	/// Release channels: mutable tags (`<url>/<name>:<channel>`) devices follow. A
	/// channel is a byte-for-byte copy of some release, moved by `bootcher channel
	/// promote`. Required and non-empty; each must be a valid OCI tag. `latest` is
	/// not special: declare it to use it.
	pub channels: Vec<String>,
	/// Path to the cosign/sigstore **private** signing key (its public half is the
	/// `.pub` sibling). Set by `bootcher signing enroll`/`rotate`; when present,
	/// every release is signed and channel devices verify signatures.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub key: Option<String>,
}

impl RegistryConfig {
	/// The signing config, when a key is configured.
	#[must_use]
	pub fn signing(&self) -> Option<SigningConfig> {
		self.key.clone().map(|key| SigningConfig { key })
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

	/// The configured ssh destination (`[user@]host` or `ssh://…`), the remote's
	/// display name.
	#[must_use]
	pub fn dest(&self) -> &str {
		self.parts().0
	}

	/// Whether `name` (a `--remote` argument) names this remote: its full
	/// destination, or its bare host (no user, scheme or port).
	#[must_use]
	pub fn matches(&self, name: &str) -> bool {
		let dest = self.dest();
		dest == name || bare_host(dest) == name
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

/// The bare host of an ssh destination: `admin@h` → `h`, `ssh://admin@h:2222` → `h`.
#[must_use]
pub fn bare_host(dest: &str) -> &str {
	let url = dest.strip_prefix("ssh://");
	let rest = url.unwrap_or(dest);
	let rest = rest.split_once('@').map_or(rest, |(_, h)| h);
	if url.is_some() { rest.split(':').next().unwrap_or(rest) } else { rest }
}

/// Resolve a slice of remotes into ready-to-drive [`Ssh`] connections.
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
	/// (every building command).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub build: Option<NonZeroUsize>,
	/// Max parallel workers for the multi-arch `disk` (`image-builder`) fan-out
	/// (`device provision`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub disk: Option<NonZeroUsize>,
	/// Max parallel workers for every per-device rollout (`device deploy|refresh|switch`,
	/// and the channel device steps of `channel promote|refresh` and `signing`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub upgrade: Option<NonZeroUsize>,
	/// Max parallel workers for the per-device `device rotate` rollout.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rotate: Option<NonZeroUsize>,
	/// Max parallel workers for the per-host `takeover` rollout (`device takeover`). Its own
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
/// - `[hooks.build]` wraps the container build, so it fires wherever one runs:
///   `build`, `device provision|deploy|switch|refresh|takeover`, `release
///   publish|refresh`, `channel refresh` (a refresh builds in a temporary worktree of
///   the release's commit and runs *that commit's* hook).
/// - `[hooks.disk]` wraps the `image-builder` step (`device provision`).
/// - `[hooks.publish]` wraps pushing a release `:X` (`release publish|refresh`,
///   `channel refresh`, and the re-signing in `signing enroll|rotate`).
/// - `[hooks.promote]` wraps moving a channel (`channel promote|refresh`, `release
///   publish --promote`).
/// - `[hooks.upgrade]` wraps the device rollout only: `device
///   deploy|refresh|switch` and every channel device step.
/// - `[hooks.takeover]` wraps the per-host in-place conversion (`device takeover`).
/// - `[hooks.rotate]` wraps the per-device credential rollout (`device rotate`, and
///   the `signing` device steps with `credential = "sign-key"`).
///
/// Every table and key is optional; an absent one is no hook. Each is
/// `skip_serializing_if`-empty so a hookless project serializes no `[hooks]`
/// tables.
#[derive(Clone, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Hooks {
	/// Hooks around every container build.
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub build: HookPair,
	/// Hooks around the `image-builder` disk-image step (`device provision`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub disk: HookPair,
	/// Hooks around pushing a release to the registry.
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub publish: HookPair,
	/// Hooks around moving a channel to a release.
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub promote: HookPair,
	/// Hooks around a per-device rollout (`bootc switch` + `upgrade` + reboot).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub upgrade: HookPair,
	/// Hooks around the per-host in-place conversion (`device takeover`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub takeover: HookPair,
	/// Hooks around the per-device credential rollout (`device rotate`, `signing`).
	#[serde(default, skip_serializing_if = "HookPair::is_empty")]
	pub rotate: HookPair,
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
		self.build.is_empty()
			&& self.disk.is_empty()
			&& self.publish.is_empty()
			&& self.promote.is_empty()
			&& self.upgrade.is_empty()
			&& self.takeover.is_empty()
			&& self.rotate.is_empty()
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
	/// Read and parse the manifest at `path` (default `./bootcher.toml`, see
	/// [`MANIFEST`]) for the project in the current directory. See [`Self::load_in`].
	///
	/// # Errors
	///
	/// Returns an error if the manifest is missing, unreadable, fails to parse, or
	/// fails validation.
	pub fn load(path: impl AsRef<Path>) -> Result<Self> {
		Self::load_in(Path::new("."), path)
	}

	/// Read and parse the manifest at `root/path` for the project rooted at `root`,
	/// failing with a pointer to `bootcher init` when it's missing. The build context
	/// and every relative path the manifest names (Containerfile, `sysroot/`) stay
	/// anchored to `root`, not to the manifest's directory, so a `--manifest
	/// bootcher.ci.toml` is a per-pipeline override layered over the same project
	/// tree. A refresh loads a release commit's own config this way, from a temporary
	/// worktree of that commit, with the same relative `path`.
	///
	/// # Errors
	///
	/// Returns an error if the manifest is missing, unreadable, fails to parse, or
	/// fails validation (an empty `[targets]`, a `[registry]` without valid
	/// channels).
	pub fn load_in(root: &Path, path: impl AsRef<Path>) -> Result<Self> {
		let rel = path.as_ref();
		let path = if root == Path::new(".") { rel.to_path_buf() } else { root.join(rel) };
		let display = path.display();
		let raw = match fs::read_to_string(&path) {
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
		let mut manifest: Manifest = if table.contains_key("extend") {
			let merged = resolve_extends(table, &path, &mut vec![canonical(&path)])?;
			toml::Value::Table(merged).try_into().with_context(|| format!("parsing {display}"))?
		} else {
			toml::from_str(&raw).with_context(|| format!("parsing {display}"))?
		};
		manifest.root = root.to_path_buf();
		manifest.manifest_path = rel.to_path_buf();
		manifest.validate().with_context(|| format!("invalid {display}"))?;
		Ok(manifest)
	}

	/// The checks serde can't express: a present-but-empty `[targets]` table leaves
	/// nothing to build, and `[registry] channels` must be non-empty, unique, valid
	/// OCI tags.
	///
	/// # Errors
	///
	/// Returns an error naming the first problem found.
	pub fn validate(&self) -> Result<()> {
		if self.targets.is_empty() {
			bail!("`[targets]` must list at least one architecture to build");
		}
		if let Some(reg) = &self.registry {
			if reg.url.trim().is_empty() {
				bail!("`[registry] url` is empty");
			}
			if reg.channels.is_empty() {
				bail!(
					"`[registry] channels` must name at least one channel devices can follow, \
					 e.g. `channels = [\"stable\"]`"
				);
			}
			for (i, ch) in reg.channels.iter().enumerate() {
				check_channel_name(ch)?;
				if reg.channels[..i].contains(ch) {
					bail!("`[registry] channels` lists `{ch}` twice");
				}
			}
		}
		Ok(())
	}

	/// The project directory (see [`Manifest::root`]); `.` when unset.
	#[must_use]
	pub fn root(&self) -> &Path {
		if self.root.as_os_str().is_empty() { Path::new(".") } else { &self.root }
	}

	/// The container image this project builds for `arch`, under the default
	/// `latest-<arch>` member tag (its disk artifacts, if any, come from
	/// [`Self::disk_types_for`]). Arch is the container/registry axis, so this
	/// carries no disk type — see [`DiskTarget`].
	#[must_use]
	pub fn image(&self, arch: Arch) -> ImageRef {
		self.image_tagged(arch, LATEST)
	}

	/// [`Self::image`] under another member tag prefix: `localhost/<name>:<prefix>-<arch>`
	/// (`pull-<channel>` for a provision pulled from a channel, `refresh-<sha12>` for a
	/// refresh build), so those never overwrite what `build` left under `latest-<arch>`.
	#[must_use]
	pub fn image_tagged(&self, arch: Arch, prefix: &str) -> ImageRef {
		ImageRef {
			name: self.general.name.clone(),
			arch,
			rootfs: self.general.rootfs,
			build_ctx: self.root().to_path_buf(),
			tag_prefix: prefix.to_owned(),
			// Filled on the build path (see `jobs::build`); empty for the many
			// non-build callers that only read a ref, so this never invokes git.
			provenance_args: Vec::new(),
			no_cache: false,
		}
	}

	/// The `image-builder` disk types to build for `arch` (`&[]` if `arch` isn't a
	/// target), from `[targets]`. The `disk` step renders one artifact per entry from
	/// this arch's single container build.
	#[must_use]
	pub fn disk_types_for(&self, arch: Arch) -> &[DiskType] {
		self.targets.types(arch)
	}

	/// The `[registry]` table, if configured.
	#[must_use]
	pub fn registry(&self) -> Option<&RegistryConfig> {
		self.registry.as_ref()
	}

	/// The `[registry]` table, or an error explaining that `what` needs one.
	///
	/// # Errors
	///
	/// Returns an error when no `[registry]` is configured.
	pub fn require_registry(&self, what: &str) -> Result<&RegistryConfig> {
		self.registry.as_ref().with_context(|| {
			format!(
				"{what} needs a registry, but bootcher.toml has no `[registry]` table — add \
				 `[registry]` with a `url` and `channels` (devices without one are updated \
				 with `bootcher device deploy`)"
			)
		})
	}

	/// The suffix-free registry repository (`<url>/<name>`), or `None` without a
	/// registry. Every release and channel is a tag on it.
	#[must_use]
	pub fn repo(&self) -> Option<String> {
		self.registry.as_ref().map(|r| format!("{}/{}", r.url, self.general.name))
	}

	/// The fully-qualified ref of `tag` (a release or channel) in the registry
	/// repository (`<url>/<name>:<tag>`), or `None` without a registry.
	#[must_use]
	pub fn tag_ref(&self, tag: &str) -> Option<String> {
		self.repo().map(|repo| format!("{repo}:{tag}"))
	}

	/// The declared channels (`[registry] channels`); empty without a registry.
	#[must_use]
	pub fn channels(&self) -> &[String] {
		self.registry.as_ref().map_or(&[], |r| r.channels.as_slice())
	}

	/// Check that `channel` is one of the declared channels.
	///
	/// # Errors
	///
	/// Returns an error if there is no registry, or `channel` isn't declared (naming
	/// the declared ones).
	pub fn check_channel(&self, channel: &str) -> Result<()> {
		let reg = self.require_registry(&format!("channel `{channel}`"))?;
		if !reg.channels.iter().any(|c| c == channel) {
			bail!(
				"`{channel}` isn't a declared channel; `[registry] channels` lists: {}. Add it \
				 there first.",
				reg.channels.join(", ")
			);
		}
		Ok(())
	}

	/// Suffix-free **local** manifest-list ref (`localhost/<name>:latest`) — the
	/// build's final artifact (the assembled manifest list) and what `device deploy`
	/// serves to direct devices, which store it under the same name.
	#[must_use]
	pub fn local_list_ref(&self) -> String {
		self.local_list_ref_tagged(LATEST)
	}

	/// The local manifest-list ref for another tag (`localhost/<name>:<tag>`), e.g. a
	/// refresh's `refresh-<sha12>` list.
	#[must_use]
	pub fn local_list_ref_tagged(&self, tag: &str) -> String {
		format!("localhost/{}:{tag}", self.general.name)
	}

	/// One [`ImageRef`] per target arch in `[targets]`. The container/registry
	/// fan-out the build and publish steps iterate (disk types don't factor in here —
	/// see [`Self::disk_types_for`]); a single-arch project yields a one-element vec.
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
	/// run (`target` `None`), or just `target`'s when one is named. Validate `target`
	/// with [`Self::check_selection`] first.
	#[must_use]
	pub fn images_for(&self, target: Option<Arch>) -> Vec<ImageRef> {
		match target {
			Some(arch) => vec![self.image(arch)],
			None => self.images(),
		}
	}

	/// The disk types to build for `arch` this run: its full `[targets]` list, or —
	/// when `filter` (`provision --disk`) is non-empty — only the listed types, in
	/// manifest order (a type repeated on the command line is built once).
	#[must_use]
	pub fn disks_for(&self, arch: Arch, filter: &[DiskType]) -> Vec<DiskType> {
		let available = self.disk_types_for(arch);
		if filter.is_empty() {
			available.to_vec()
		} else {
			available.iter().copied().filter(|d| filter.contains(d)).collect()
		}
	}

	/// Builder spec for `arch`'s container build (its `[builder.<arch>] build`
	/// override, else the flat `[builder] build`; `local` unless configured).
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

	/// The configured devices (the top-level `remotes` array; empty unless
	/// configured).
	#[must_use]
	pub fn remotes(&self) -> &[RemoteConfig] {
		&self.remotes
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

	/// The signing config when image signing is in effect — i.e. `[registry] key`
	/// is set. `None` means no signing.
	#[must_use]
	pub fn signing(&self) -> Option<SigningConfig> {
		self.registry.as_ref()?.signing()
	}
}

/// The default member tag prefix and local list tag (`latest-<arch>`, `latest`):
/// what `build` produces and direct devices store. Unrelated to any registry tag.
pub const LATEST: &str = "latest";

/// Whether `tag` is a valid OCI tag: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
#[must_use]
pub fn valid_tag(tag: &str) -> bool {
	let mut chars = tag.chars();
	let Some(first) = chars.next() else { return false };
	(first.is_ascii_alphanumeric() || first == '_')
		&& tag.len() <= 128
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Tag prefixes bootcher reserves in the registry repository: `sha256-…` are the
/// sigstore signature attachments, `ci-…` the CI hand-off scratch tags. Neither a
/// release nor a channel may use them.
pub const RESERVED_TAG_PREFIXES: &[&str] = &["sha256-", "ci-"];

/// Check a `[registry] channels` entry: a valid OCI tag outside the reserved
/// prefixes.
fn check_channel_name(ch: &str) -> Result<()> {
	if !valid_tag(ch) {
		bail!(
			"`[registry] channels` entry `{ch}` isn't a valid OCI tag (letters, digits, `_`, \
			 `.`, `-`; at most 128 characters; not starting with `.` or `-`)"
		);
	}
	if let Some(p) = RESERVED_TAG_PREFIXES.iter().find(|p| ch.starts_with(*p)) {
		bail!("`[registry] channels` entry `{ch}` uses the reserved `{p}` prefix");
	}
	Ok(())
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
	/// Member tag prefix: the image lives at `localhost/<name>:<tag_prefix>-<arch>`.
	/// `latest` for a normal build; `pull-<channel>` and `refresh-<sha12>` keep a
	/// channel pull or a refresh build from overwriting it.
	pub tag_prefix: String,
	/// OCI provenance to stamp on the built container, as ready-to-splice
	/// `--label`/`--annotation <key>=<value>` argv tokens (from
	/// [`GitProvenance::provenance_args`]). Empty unless the build path enriched it —
	/// [`Manifest::image`] leaves it empty so reading an image ref never shells out to git.
	pub provenance_args: Vec<String>,
	/// Build with `--no-cache` (a forced refresh), so no cached layer from an earlier
	/// build of the same source survives.
	pub no_cache: bool,
}

impl fmt::Display for ImageRef {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}:{}", self.name, self.arch)
	}
}

impl ImageRef {
	/// `localhost/<name>:<prefix>-<arch>` — the per-arch **member** tag
	/// (`latest-<arch>` unless overridden, see [`ImageRef::tag_prefix`]). Each arch's
	/// container is built and stored under it so the arches coexist in podman's flat
	/// tag namespace; it's also a member of the suffix-free
	/// [`Manifest::local_list_ref`] manifest list.
	#[must_use]
	pub fn tag(&self) -> String {
		format!("localhost/{}:{}-{}", self.name, self.tag_prefix, self.arch)
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

/// Git provenance for the project tree at `dir` (the build context / project
/// root): the commit a build came from and a human-readable description of it.
/// Stamped onto the built container as the standard OCI `org.opencontainers.image.*`
/// labels and manifest annotations (so a device's `bootc status` can name what it's
/// running) and surfaced in `BOOTCHER_METADATA` for hooks.
///
/// Strictly best-effort: every field is `None` when it can't be resolved, and
/// detection never fails a build. A project that doesn't live in git (`dir` isn't
/// a git work tree) just builds without provenance, silently. Anything else that
/// loses provenance — git refusing the repo (e.g. "dubious ownership" when the
/// checkout belongs to another uid), a corrupt or commit-less repo, or no `git`
/// binary while a `.git` is present — is reported as one `warning:` line on the
/// job, quoting git's own error, so it can't pass for "not in git". This is the
/// one place bootcher consults git; it never gates a build.
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
	/// The commit a **clean** tree is at — what [`SOURCE_LABEL`] records. `None` for
	/// a dirty tree or outside git, so a dirty build never claims a source.
	pub source: Option<String>,
}

impl GitProvenance {
	/// Detect the provenance of the work tree at `dir` (the build context). Runs a
	/// few `git` queries; any failure leaves the corresponding field `None`, and a
	/// failure other than "not a git repository" prints a single warning through
	/// `job` (see the type docs). Called once on the build path, not from
	/// [`Manifest::image`], so merely reading an image ref never shells out to git.
	#[must_use]
	pub fn detect(dir: &Path, job: &crate::progress::Scope) -> Self {
		let (provenance, warning) = Self::resolve(dir);
		if let Some(warning) = warning {
			job.println(warning);
		}
		provenance
	}

	/// The detection itself, returning the warning line (if any) instead of
	/// printing it, so tests can assert on it without a progress scope. Only the
	/// first failure is reported: the later queries usually fail for the same reason.
	fn resolve(dir: &Path) -> (Self, Option<String>) {
		let mut failure: Option<String> = None;
		let mut note = |e: GitError| {
			if let GitError::Failed(reason) = e {
				failure.get_or_insert(reason);
			}
		};
		let revision = match git_run(dir, &["rev-parse", "HEAD"]) {
			Ok(sha) if !sha.is_empty() => match git_run(dir, &["status", "--porcelain"]) {
				Ok(status) if status.is_empty() => Some(sha),
				Ok(_) => Some(format!("{sha}-dirty")),
				// rev-parse worked but status didn't: keep the commit, without `-dirty`
				// since we can't tell, and warn rather than read the failure as "clean".
				Err(e) => {
					note(e);
					Some(sha)
				}
			},
			Ok(_) => None,
			Err(e) => {
				note(e);
				None
			}
		};
		let version = match git_run(dir, &["describe", "--tags", "--always", "--dirty"]) {
			Ok(v) => (!v.is_empty()).then_some(v),
			Err(e) => {
				note(e);
				None
			}
		};
		let source = revision.as_deref().filter(|r| !r.ends_with("-dirty")).map(str::to_owned);
		let warning = failure.map(|reason| {
			let consequence = if revision.is_none() && version.is_none() {
				"images will carry no revision/version labels"
			} else {
				"image revision/version labels may be incomplete or lack a -dirty marker"
			};
			format!("warning: git provenance unavailable ({reason}); {consequence}")
		});
		(Self { revision, version, source }, warning)
	}

	/// Whether the tree was clean at a commit (so a build records its source).
	#[must_use]
	pub fn is_clean(&self) -> bool {
		self.source.is_some()
	}

	/// The provenance as ready-to-splice `podman build` argv tokens: for each field
	/// that resolved, a `--label <key>=<value>` (image config) and a matching
	/// `--annotation <key>=<value>` (image manifest); empty when none did. The
	/// annotation matters because buildah copies the base image's manifest annotations
	/// into the new image, so without it a Fedora base's own `version`/`revision`
	/// would show through to anything reading annotations (registry UIs, scanners)
	/// while the labels say otherwise. An unresolved field passes nothing, leaving the
	/// base's value inherited. Reused for both the argv (in-process build) and
	/// shell-string (remote build) invocations, so the two backends stamp identically.
	///
	/// [`SOURCE_LABEL`] is always stamped (a config label only): the clean commit, or
	/// empty for a dirty tree / no git, so a value inherited from the base image can't
	/// pass for this build's source.
	#[must_use]
	pub fn provenance_args(&self) -> Vec<String> {
		let mut out = Vec::new();
		for (key, val) in [
			("org.opencontainers.image.revision", &self.revision),
			("org.opencontainers.image.version", &self.version),
		] {
			if let Some(val) = val {
				let kv = format!("{key}={val}");
				out.extend(["--label".to_owned(), kv.clone(), "--annotation".to_owned(), kv]);
			}
		}
		out.extend([
			"--label".to_owned(),
			format!("{SOURCE_LABEL}={}", self.source.as_deref().unwrap_or("")),
		]);
		out
	}
}

/// Why a provenance `git` query produced nothing.
#[derive(Debug, PartialEq, Eq)]
enum GitError {
	/// `dir` isn't in a git work tree (or there's neither `git` nor a `.git`): a
	/// legitimate setup, so provenance is skipped silently.
	NotARepo,
	/// Anything else (git refused or failed, or isn't installed next to a `.git`),
	/// carrying a one-line reason for the warning.
	Failed(String),
}

/// Run `git -C <dir> <args>`, returning trimmed stdout (possibly empty, so a clean
/// `git status --porcelain` is `Ok("")`). Runs under `LC_ALL=C` so git's stderr is
/// the untranslated text the failure classification matches on.
fn git_run(dir: &Path, args: &[&str]) -> Result<String, GitError> {
	let mut full: Vec<OsString> = vec!["-C".into(), dir.into()];
	full.extend(args.iter().map(OsString::from));
	let out = match duct::cmd("git", full)
		.env("LC_ALL", "C")
		.stdout_capture()
		.stderr_capture()
		.unchecked()
		.run()
	{
		Ok(out) => out,
		// git couldn't be spawned at all. Only worth a warning when the project
		// evidently is in git; with no `.git` anywhere up the tree there was nothing
		// to read anyway.
		Err(e) => {
			let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
			return if abs.ancestors().any(|a| a.join(".git").exists()) {
				Err(GitError::Failed(format!("couldn't run git: {e}")))
			} else {
				Err(GitError::NotARepo)
			};
		}
	};
	if out.status.success() {
		Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
	} else {
		Err(classify_git_failure(&String::from_utf8_lossy(&out.stderr)))
	}
}

/// Classify a non-zero `git` exit from its stderr: git's "not a git repository"
/// (with or without the filesystem-boundary variant) is [`GitError::NotARepo`];
/// anything else is a failure reported by git's first non-empty stderr line.
fn classify_git_failure(stderr: &str) -> GitError {
	if stderr.contains("not a git repository") {
		return GitError::NotARepo;
	}
	let first = stderr.lines().map(str::trim).find(|l| !l.is_empty());
	GitError::Failed(first.unwrap_or("git exited with an error and no message").to_owned())
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
		// refs carry no arch suffix.
		let m = parse(
			"[general]\nname = \"kiosk\"\n[targets]\naarch64 = \"qcow2\"\n\
			 [registry]\nurl = \"reg.example.com/org\"\nchannels = [\"stable\"]\n",
		);
		let img = m.image(Arch::Aarch64);
		assert_eq!(img.tag(), "localhost/kiosk:latest-aarch64");
		// A tag override keeps a channel pull / refresh build off `latest-<arch>`.
		assert_eq!(
			m.image_tagged(Arch::Aarch64, "pull-stable").tag(),
			"localhost/kiosk:pull-stable-aarch64"
		);
		assert_eq!(m.local_list_ref(), "localhost/kiosk:latest");
		assert_eq!(m.local_list_ref_tagged("refresh-abc"), "localhost/kiosk:refresh-abc");
		assert_eq!(m.repo().as_deref(), Some("reg.example.com/org/kiosk"));
		assert_eq!(m.tag_ref("stable").as_deref(), Some("reg.example.com/org/kiosk:stable"));
		assert_eq!(m.tag_ref("v1.4").as_deref(), Some("reg.example.com/org/kiosk:v1.4"));
	}

	#[test]
	fn no_registry_means_no_refs_and_no_channels() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.repo().is_none() && m.tag_ref("stable").is_none());
		assert!(m.channels().is_empty());
		assert!(m.check_channel("stable").unwrap_err().to_string().contains("needs a registry"));
	}

	#[test]
	fn check_channel_validates_against_declared_set() {
		let m = parse(
			"[general]\nname = \"kiosk\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [registry]\nurl = \"reg.example.com/org\"\nchannels = [\"stable\", \"next\"]\n",
		);
		assert!(m.check_channel("stable").is_ok());
		// `latest` is not special any more: undeclared, it's rejected like any other.
		let err = m.check_channel("latest").unwrap_err().to_string();
		assert!(err.contains("isn't a declared channel") && err.contains("stable, next"), "{err}");
	}

	#[test]
	fn registry_channels_are_required_non_empty_and_valid() {
		let base =
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n[registry]\nurl = \"r/o\"\n";
		// Missing `channels` is a parse error (no default).
		assert!(toml::from_str::<Manifest>(base).is_err());
		let validate = |channels: &str| parse(&format!("{base}channels = {channels}\n")).validate();
		assert!(validate("[]").unwrap_err().to_string().contains("at least one channel"));
		assert!(validate("[\"-bad\"]").unwrap_err().to_string().contains("valid OCI tag"));
		assert!(validate("[\"ci-x\"]").unwrap_err().to_string().contains("reserved"));
		assert!(validate("[\"a\", \"a\"]").unwrap_err().to_string().contains("twice"));
		assert!(validate("[\"stable\", \"v1.x_y-z\"]").is_ok());
	}

	#[test]
	fn tag_grammar() {
		for ok in ["v1.4", "latest", "_x", "A.b-c_9", &"a".repeat(128)] {
			assert!(valid_tag(ok), "{ok}");
		}
		for bad in ["", ".x", "-x", "a/b", "a:b", "a b", &"a".repeat(129)] {
			assert!(!valid_tag(bad), "{bad}");
		}
	}

	#[test]
	fn load_in_threads_the_project_dir() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(
			dir.path().join("ci.toml"),
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n",
		)
		.unwrap();
		let m = Manifest::load_in(dir.path(), "ci.toml").unwrap();
		assert_eq!(m.root(), dir.path());
		assert_eq!(m.image(Arch::X86_64).build_ctx, dir.path());
		// The default (cwd) project dir.
		assert_eq!(parse("[general]\nname = \"x\"\n").root(), Path::new("."));
	}

	#[test]
	fn remote_matches_full_dest_or_bare_host() {
		let plain = RemoteConfig::ConnStr("admin@kiosk-1".into());
		assert!(plain.matches("admin@kiosk-1") && plain.matches("kiosk-1"));
		assert!(!plain.matches("kiosk"));
		let url = RemoteConfig::ConnStr("ssh://admin@10.0.0.2:2222".into());
		assert!(url.matches("10.0.0.2") && url.matches("ssh://admin@10.0.0.2:2222"));
		assert_eq!(bare_host("h"), "h");
	}

	/// Run `git -C dir <args>` in a test, asserting it succeeds — the setup half of
	/// the provenance tests (the production [`git_run`] is the read half).
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
		let head = git_run(dir, &["rev-parse", "HEAD"]).expect("HEAD sha");
		let (clean, warning) = GitProvenance::resolve(dir);
		assert_eq!(clean.revision.as_deref(), Some(head.as_str()));
		assert_eq!(clean.version.as_deref(), Some("v1.0.0"));
		// A clean tree records its commit as the source.
		assert_eq!(clean.source.as_deref(), Some(head.as_str()));
		assert_eq!(warning, None, "a healthy repo warns about nothing");

		// Dirty the work tree: both fields gain the `-dirty` suffix.
		fs::write(dir.join("Containerfile"), "FROM scratch\nRUN true\n").unwrap();
		let (dirty, warning) = GitProvenance::resolve(dir);
		assert_eq!(dirty.revision.as_deref(), Some(format!("{head}-dirty").as_str()));
		assert_eq!(dirty.version.as_deref(), Some("v1.0.0-dirty"));
		// …and a dirty one records none.
		assert_eq!(dirty.source, None);
		assert_eq!(warning, None);
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
		let head = git_run(dir, &["rev-parse", "HEAD"]).expect("HEAD sha");
		let (p, warning) = GitProvenance::resolve(dir);
		assert_eq!(warning, None);
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
		// A bare temp dir (no `git init`) resolves nothing — provenance is best-effort,
		// and a project outside git is a legitimate setup, so nothing is warned either.
		let dir = tempfile::TempDir::new().unwrap();
		let (p, warning) = GitProvenance::resolve(dir.path());
		assert_eq!(p.revision, None);
		assert_eq!(p.version, None);
		// Only the (empty) source label, overriding anything the base carried.
		assert_eq!(p.provenance_args(), ["--label", "bootcher.source="]);
		assert_eq!(warning, None, "not being in git is silent");
	}

	#[test]
	fn provenance_warns_in_a_repo_without_commits() {
		// `git init` with no commit: git is clearly in use but HEAD resolves to nothing,
		// so the labels are lost — that must be reported, quoting git's own error.
		let repo = tempfile::TempDir::new().unwrap();
		git(repo.path(), &["init", "-q"]);
		let (p, warning) = GitProvenance::resolve(repo.path());
		assert_eq!(p.revision, None);
		assert_eq!(p.version, None);
		let warning = warning.expect("an empty repo warns");
		assert!(warning.starts_with("warning: git provenance unavailable (fatal: "), "{warning}");
		assert!(warning.ends_with("; images will carry no revision/version labels"), "{warning}");
	}

	#[test]
	fn git_failure_classification() {
		// "Not a git repository" (incl. the filesystem-boundary wording) is silent.
		assert_eq!(
			classify_git_failure(
				"fatal: not a git repository (or any of the parent directories): .git\n"
			),
			GitError::NotARepo
		);
		assert_eq!(
			classify_git_failure(
				"fatal: not a git repository (or any parent up to mount point /)\n\
				 Stopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set).\n"
			),
			GitError::NotARepo
		);
		// Dubious ownership (root building another uid's checkout) is a failure,
		// reported by its first line only — not the multi-line safe.directory hint.
		assert_eq!(
			classify_git_failure(
				"fatal: detected dubious ownership in repository at '/__w/x'\n\
				 To add an exception for this directory, call:\n\
				 \n\
				 \tgit config --global --add safe.directory /__w/x\n"
			),
			GitError::Failed("fatal: detected dubious ownership in repository at '/__w/x'".into())
		);
		// No stderr at all still yields a reason rather than an empty pair of parens.
		assert!(matches!(classify_git_failure(""), GitError::Failed(r) if !r.is_empty()));
	}

	#[test]
	fn provenance_args_emits_only_present_fields() {
		// Both fields → a `--label k=v` + `--annotation k=v` pair each, revision first.
		let both = GitProvenance {
			revision: Some("abc123".into()),
			version: Some("v1.0.0".into()),
			source: Some("abc123".into()),
		};
		assert_eq!(
			both.provenance_args(),
			[
				"--label",
				"org.opencontainers.image.revision=abc123",
				"--annotation",
				"org.opencontainers.image.revision=abc123",
				"--label",
				"org.opencontainers.image.version=v1.0.0",
				"--annotation",
				"org.opencontainers.image.version=v1.0.0",
				"--label",
				"bootcher.source=abc123",
			]
		);
		// A single resolved field stamps only that key…
		let ver_only =
			GitProvenance { revision: None, version: Some("v1.0.0".into()), source: None };
		assert_eq!(
			ver_only.provenance_args(),
			[
				"--label",
				"org.opencontainers.image.version=v1.0.0",
				"--annotation",
				"org.opencontainers.image.version=v1.0.0",
				"--label",
				"bootcher.source=",
			]
		);
		// …and nothing resolved (not a git tree) stamps only the empty source label, so
		// the base image's own annotations are inherited untouched.
		assert_eq!(GitProvenance::default().provenance_args(), ["--label", "bootcher.source="]);
	}

	#[test]
	fn absent_remotes_means_no_remotes() {
		let m = parse("[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n");
		assert!(m.remotes().is_empty());
	}

	#[test]
	fn remotes_parse_in_order() {
		let m = parse(
			"remotes = [\"a@h1\", \"b@h2:2222\"]\n\
			 [general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n",
		);
		assert_eq!(
			m.remotes(),
			[RemoteConfig::ConnStr("a@h1".into()), RemoteConfig::ConnStr("b@h2:2222".into())]
		);
	}

	#[test]
	fn remotes_accept_the_object_form_with_ssh_opts() {
		// A bare string and an object with per-remote `ssh_opts` may be mixed in one
		// array (the untagged `RemoteConfig`).
		let m = parse(
			"remotes = [\
			 \"plain@h1\", \
			 { remote = \"ssh://o@h2:2222\", ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"] }]\n\
			 [general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n",
		);
		assert_eq!(
			m.remotes(),
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
		let ssh = &m.remotes().to_ssh()[1];
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
			 [hooks.disk]\npost = \"b.sh\"\n\
			 [hooks.takeover]\npre = \"c.sh\"\n\
			 [hooks.rotate]\npost = \"d.sh\"\n",
		);
		assert!(!m.hooks().is_empty());
		assert_eq!(m.hooks().build.pre.as_deref(), Some("a.sh"));
		assert!(m.hooks().build.post.is_none());
		assert_eq!(m.hooks().disk.post.as_deref(), Some("b.sh"));
		assert!(m.hooks().disk.pre.is_none());
		assert!(m.hooks().upgrade.is_empty());
		assert_eq!(m.hooks().takeover.pre.as_deref(), Some("c.sh"));
		assert!(m.hooks().takeover.post.is_none());
		assert_eq!(m.hooks().rotate.post.as_deref(), Some("d.sh"));
		assert!(m.hooks().rotate.pre.is_none());
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
	fn registry_without_key_has_no_signing() {
		let m = parse(
			"[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [registry]\nurl = \"reg.example.com/org\"\nchannels = [\"stable\"]\n",
		);
		assert_eq!(m.registry().unwrap().url, "reg.example.com/org");
		assert!(m.signing().is_none());
		// No `key` serializes no `key`.
		let s = toml::to_string(&m).unwrap();
		assert!(s.contains("[registry]") && !s.contains("key ="), "{s}");
	}

	#[test]
	fn registry_key_enables_signing_and_defaults_pub_key_to_the_sibling() {
		let m = parse(
			"[general]\nname = \"kiosk\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [registry]\nurl = \"reg.example.com/org\"\nchannels = [\"stable\"]\nkey = \"cosign.key\"\n",
		);
		let s = m.signing().expect("signing present");
		assert_eq!(s.key, "cosign.key");
		assert_eq!(s.public_key_path(), "cosign.pub");
	}

	#[test]
	fn a_serialized_manifest_round_trips_with_remotes_first() {
		// `remotes` is a top-level array, so it must serialize before the tables (TOML
		// would otherwise read it as a key of the last table).
		let m = parse(
			"remotes = [\"admin@a\"]\n[general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n\
			 [registry]\nurl = \"r/o\"\nchannels = [\"stable\"]\n",
		);
		let back: Manifest = toml::from_str(&toml::to_string(&m).unwrap()).unwrap();
		assert_eq!(back.remotes(), m.remotes());
		assert_eq!(back.registry(), m.registry());
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
			"remotes = [{ remote = \"admin@h\", takeover_login = \"debian\" }]\n\
			 [general]\nname = \"x\"\n[targets]\nx86_64 = \"qcow2\"\n",
		);
		assert_eq!(
			m.remotes(),
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
