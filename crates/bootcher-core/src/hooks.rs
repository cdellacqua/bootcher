//! Lifecycle hooks: user-supplied shell commands run before and after bootcher's
//! phases (the container build, the image step, the deploy upgrade, the in-place
//! takeover, and the credential rotation),
//! configured in the manifest's `[hooks.<phase>]` tables (see
//! [`crate::context::Hooks`]).
//!
//! Hooks are bootcher's extension point: anything project-specific that bootcher
//! itself shouldn't know about (editing the built disk image to embed files
//! outside the Containerfile's reach, building auxiliary container images before
//! the main build, …) lives in a script the manifest points at, instead of in
//! this binary.
//!
//! A hook is a command string run with `sh -c` from the project root (bootcher's
//! working directory, where `bootcher.toml` and the build context live), with the
//! progress UI suspended and stdio inherited — so the hook owns the terminal for
//! its duration and may print freely, prompt, or `sudo`. A non-zero exit aborts
//! the surrounding step.
//!
//! A `post` hook normally runs only once its phase succeeded. The device-fleet
//! phases (`upgrade`, `takeover`, `rotate`) are the exception: their `post` runs
//! once the per-device rollout has run *whatever its outcome* — a device that
//! failed doesn't undo the ones that succeeded — with each device's outcome in
//! [`HookMetadata::results`], and the run fails afterwards if any device did (see
//! [`run_post`]). A failure before any device is attempted (e.g. the registry
//! push) still aborts without `post`.
//!
//! ## The `BOOTCHER_METADATA` contract
//!
//! Every hook is handed a single `BOOTCHER_METADATA` environment variable holding
//! a JSON object ([`HookMetadata`]) describing the phase, stage, and the artifacts
//! it concerns — so a *portable* recipe (e.g. a `disk.post` that embeds board
//! firmware) finds what bootcher just built without re-deriving the output layout
//! from `bootcher.toml`. One schema spans every phase; fields a phase can't fill
//! are omitted. The `disk` phase additionally lists the per-target output dirs and
//! (at `post`, where they exist) the resolved `disk.<ext>` paths, so a recipe walks
//! the build matrix straight from the JSON:
//!
//! ```sh
//! echo "$BOOTCHER_METADATA" \
//!   | jq -r '.targets[] | select(.arch=="aarch64" and .disk_type=="raw") | .file' \
//!   | while read -r img; do embed_pi_firmware "$img"; done
//! ```

use crate::context::{Arch, DiskType};
use crate::fleet::{Outcome, Report};
use crate::progress::Scope;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::PathBuf;

/// Which lifecycle phase a hook brackets. Serializes lowercase (`build` / `disk`
/// / `upgrade` / `takeover` / `rotate`) into [`HookMetadata::phase`].
#[derive(Clone, Copy, Debug, Serialize, strum::Display, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum Phase {
	Build,
	Disk,
	Upgrade,
	Takeover,
	Rotate,
}

/// Whether a hook runs before (`pre`) or after (`post`) its phase. Serializes
/// lowercase into [`HookMetadata::stage`].
#[derive(Clone, Copy, Debug, Serialize, strum::Display, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum Stage {
	Pre,
	Post,
}

/// Which device credential a `rotate` run replaces — one per `bootcher rotate`
/// subcommand, serialized as its name into [`HookMetadata::credential`] so a
/// shared `[hooks.rotate]` script can branch on it.
#[derive(Clone, Copy, Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Credential {
	/// The registry pull token (`rotate pull-token`).
	PullToken,
	/// The admin SSH authorized key (`rotate ssh-key`).
	SshKey,
	/// The image-signing public key + policy (`rotate sign-key`).
	SignKey,
}

/// One disk artifact in the `disk` phase's [`HookMetadata::targets`] matrix: the
/// arch + `image-builder` type, the output dir (`output/<arch>/<disk_type>/`,
/// relative to the project root), and — at `disk.post`, once it exists — the
/// resolved `disk.<ext>` path. `file` is omitted at `disk.pre` (nothing is built
/// yet) and whenever the dir doesn't hold exactly one `disk.*` artifact (the
/// recipe falls back to `dir`).
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DiskTargetMeta {
	pub arch: Arch,
	pub disk_type: DiskType,
	pub dir: PathBuf,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub file: Option<PathBuf>,
}

/// The `BOOTCHER_METADATA` payload handed to a hook: a single JSON object spanning
/// every phase, with phase-specific fields omitted where they don't apply. The
/// caller builds it once per phase and flips [`Self::stage`] between the `pre` and
/// `post` calls (the `disk` phase also fills [`DiskTargetMeta::file`] for `post`).
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
#[schemars(
	title = "BOOTCHER_METADATA",
	description = "The JSON object bootcher sets as the BOOTCHER_METADATA env var on every lifecycle hook."
)]
pub(crate) struct HookMetadata {
	/// The lifecycle phase being bracketed.
	pub phase: Phase,
	/// Whether this is the `pre` or `post` hook of the phase.
	pub stage: Stage,
	/// The project image name (`[general] name`).
	pub image_name: String,
	/// The target arches being built/deployed (`[targets]` keys).
	pub arches: Vec<Arch>,
	/// The suffix-free image ref this phase concerns: the local manifest-list ref
	/// at `build`, the bootc-origin source ref at `disk`, the pushed/served list ref
	/// at `upgrade` and `takeover`, the devices' bootc-origin ref at `rotate`.
	/// Deterministic from the manifest, so it's valid even at `pre`.
	pub image_ref: String,
	/// The git commit the image was built from (`org.opencontainers.image.revision`),
	/// `-dirty`-suffixed for an uncommitted tree. Mirrors the OCI label stamped on the
	/// container, so a hook and the device's `bootc status` agree. Omitted when the
	/// project isn't a git work tree (see [`crate::context::GitProvenance`]), and at
	/// `rotate`, which builds nothing.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub revision: Option<String>,
	/// A human description of the build (`org.opencontainers.image.version` —
	/// `git describe`: nearest tag, else short SHA). Mirrors the OCI label; omitted
	/// outside a git work tree and at `rotate`. See [`crate::context::GitProvenance`].
	#[serde(skip_serializing_if = "Option::is_none")]
	pub version: Option<String>,
	/// The base output dir (relative to the project root) — `disk` phase only.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output_dir: Option<PathBuf>,
	/// The per-target build matrix — `disk` phase only.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub targets: Option<Vec<DiskTargetMeta>>,
	/// The deploy targets (`user@host`) — `upgrade`, `takeover` and `rotate` phases
	/// only. Omitted at `upgrade` in pure-registry mode (no remotes listed); always
	/// set at `takeover` (as the steady-state `admin@host` each host ends up as) and
	/// `rotate`.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub remotes: Option<Vec<String>>,
	/// The credential being rotated — `rotate` phase only.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub credential: Option<Credential>,
	/// Each device's outcome, in `[deploy] remotes` order — `post` stage of the
	/// `upgrade`, `takeover` and `rotate` phases only, which run `post` even after a
	/// partial failure. Empty when the rollout attempted no device (e.g. a
	/// registry-mode `upgrade` with no remotes, which only pushes).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub results: Option<Vec<Outcome>>,
}

impl HookMetadata {
	/// `phase.stage`, e.g. `disk.post` — the label printed and used in errors.
	fn label(&self) -> String {
		format!("{}.{}", self.phase, self.stage)
	}
}

/// JSON Schema filename `bootcher init` writes for the [`HookMetadata`] /
/// `BOOTCHER_METADATA` contract, beside the manifest schema under
/// [`crate::context::SCHEMA_DIR`]. Reference-only (recipe authors), so — unlike the
/// manifest schema — nothing binds to it via a `#:schema` directive.
pub(crate) const METADATA_SCHEMA: &str = "schemas/metadata.schema.json";

/// Render the [`HookMetadata`] type as a pretty-printed JSON Schema (draft
/// 2020-12), documenting the `BOOTCHER_METADATA` env var hooks receive. Generated
/// from the same serde-derived struct hooks are handed, so it can't drift. Mirrors
/// [`crate::context::manifest_schema_json`]; `bootcher init` writes it to
/// [`METADATA_SCHEMA`].
///
/// # Panics
///
/// Never in practice: a `schemars` `Schema` is plain JSON whose `Serialize` is
/// infallible.
#[must_use]
pub(crate) fn metadata_schema_json() -> String {
	let schema = schemars::schema_for!(HookMetadata);
	serde_json::to_string_pretty(&schema).expect("a JSON Schema always serializes")
}

/// Run the optional hook `command` (a no-op when `None`), handing it
/// `meta` as the `BOOTCHER_METADATA` env var. Suspends the progress bars and
/// inherits stdio so the hook has full control of the terminal, exactly like the
/// sudo'd children; a non-zero exit becomes an error that aborts the step that
/// scheduled the hook.
///
/// # Errors
///
/// Returns an error if the metadata can't be serialized, or the hook can't be
/// spawned or exits with a non-zero status.
pub(crate) fn run(meta: &HookMetadata, command: Option<&str>, job: &Scope) -> Result<()> {
	let Some(command) = command else { return Ok(()) };
	let label = meta.label();
	job.println(format!("{label}: {command}"));
	let metadata = serde_json::to_string(meta).context("serializing hook metadata")?;
	let out = job.suspend(|| {
		duct::cmd!("sh", "-c", command)
			.env("BOOTCHER_METADATA", metadata)
			.unchecked()
			.run()
			.with_context(|| format!("spawning {label} hook `{command}`"))
	})?;
	if !out.status.success() {
		bail!("{label} hook failed ({}): {command}", out.status);
	}
	Ok(())
}

/// Close a device-fleet phase: flip `meta` to `post`, attach `report`'s
/// per-device outcomes as [`HookMetadata::results`], run the `post` hook
/// `command`, then raise the rollout's verdict ([`Report::finish`]). So `post`
/// runs on a partial failure too, and the run still fails if any device did.
///
/// # Errors
///
/// Returns an error if any device failed, the hook failed, or both (the two
/// combined into one message).
pub(crate) fn run_post(
	meta: &mut HookMetadata,
	command: Option<&str>,
	report: &Report,
	job: &Scope,
) -> Result<()> {
	meta.stage = Stage::Post;
	meta.results = Some(report.outcomes().to_vec());
	let hook = run(meta, command, job);
	match (report.finish(job), hook) {
		(Ok(()), hook) => hook,
		(Err(fleet), Ok(())) => Err(fleet),
		(Err(fleet), Err(hook)) => Err(anyhow!("{fleet:#}; then {hook:#}")),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::{Value, json};

	fn json_of(meta: &HookMetadata) -> Value {
		serde_json::from_str(&serde_json::to_string(meta).unwrap()).unwrap()
	}

	#[test]
	fn disk_post_serializes_the_target_matrix_with_resolved_files() {
		let meta = HookMetadata {
			phase: Phase::Disk,
			stage: Stage::Post,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64, Arch::Aarch64],
			image_ref: "registry.example.com/org/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: Some(PathBuf::from("output")),
			targets: Some(vec![
				DiskTargetMeta {
					arch: Arch::Aarch64,
					disk_type: DiskType::Raw,
					dir: PathBuf::from("output/aarch64/raw"),
					file: Some(PathBuf::from("output/aarch64/raw/disk.raw")),
				},
				DiskTargetMeta {
					arch: Arch::X86_64,
					disk_type: DiskType::BootcInstaller,
					dir: PathBuf::from("output/x86_64/bootc-installer"),
					file: None,
				},
			]),
			remotes: None,
			credential: None,
			results: None,
		};
		assert_eq!(
			json_of(&meta),
			json!({
				"phase": "disk",
				"stage": "post",
				"image_name": "kiosk",
				"arches": ["x86_64", "aarch64"],
				"image_ref": "registry.example.com/org/kiosk:latest",
				"output_dir": "output",
				"targets": [
					{ "arch": "aarch64", "disk_type": "raw", "dir": "output/aarch64/raw", "file": "output/aarch64/raw/disk.raw" },
					// unresolved file is omitted, not null
					{ "arch": "x86_64", "disk_type": "bootc-installer", "dir": "output/x86_64/bootc-installer" }
				]
			})
		);
	}

	#[test]
	fn build_omits_disk_and_upgrade_only_fields() {
		let meta = HookMetadata {
			phase: Phase::Build,
			stage: Stage::Pre,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "localhost/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: None,
			targets: None,
			remotes: None,
			credential: None,
			results: None,
		};
		let v = json_of(&meta);
		assert_eq!(v["phase"], "build");
		assert_eq!(v["stage"], "pre");
		assert_eq!(v["image_ref"], "localhost/kiosk:latest");
		let obj = v.as_object().unwrap();
		assert!(!obj.contains_key("output_dir"));
		assert!(!obj.contains_key("targets"));
		assert!(!obj.contains_key("remotes"));
		// Provenance is absent outside a git work tree — omitted, not null.
		assert!(!obj.contains_key("revision"));
		assert!(!obj.contains_key("version"));
	}

	#[test]
	fn provenance_fields_serialize_when_present() {
		let meta = HookMetadata {
			phase: Phase::Build,
			stage: Stage::Post,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "localhost/kiosk:latest".into(),
			revision: Some("abc123-dirty".into()),
			version: Some("v1.2.3-4-gabc123-dirty".into()),
			output_dir: None,
			targets: None,
			remotes: None,
			credential: None,
			results: None,
		};
		let v = json_of(&meta);
		assert_eq!(v["revision"], "abc123-dirty");
		assert_eq!(v["version"], "v1.2.3-4-gabc123-dirty");
	}

	#[test]
	fn upgrade_carries_remotes_when_set() {
		let meta = HookMetadata {
			phase: Phase::Upgrade,
			stage: Stage::Post,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "registry.example.com/org/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: None,
			targets: None,
			remotes: Some(vec!["root@10.0.0.2".into()]),
			credential: None,
			results: None,
		};
		let v = json_of(&meta);
		assert_eq!(v["remotes"], json!(["root@10.0.0.2"]));
		assert!(!v.as_object().unwrap().contains_key("targets"));
	}

	#[test]
	fn takeover_serializes_its_phase_and_remotes() {
		let meta = HookMetadata {
			phase: Phase::Takeover,
			stage: Stage::Pre,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "localhost/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: None,
			targets: None,
			remotes: Some(vec!["admin@vps.example.com".into()]),
			credential: None,
			results: None,
		};
		let v = json_of(&meta);
		assert_eq!(v["phase"], "takeover");
		assert_eq!(v["remotes"], json!(["admin@vps.example.com"]));
		assert!(!v.as_object().unwrap().contains_key("credential"));
	}

	#[test]
	fn rotate_serializes_the_credential_kebab_cased() {
		let meta = HookMetadata {
			phase: Phase::Rotate,
			stage: Stage::Post,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "registry.example.com/org/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: None,
			targets: None,
			remotes: Some(vec!["admin@10.0.0.2".into()]),
			credential: Some(Credential::PullToken),
			results: None,
		};
		let v = json_of(&meta);
		assert_eq!(v["phase"], "rotate");
		assert_eq!(v["credential"], "pull-token");
		assert_eq!(v["remotes"], json!(["admin@10.0.0.2"]));
		assert!(!v.as_object().unwrap().contains_key("results"));
	}

	#[test]
	fn fleet_post_lists_each_outcome_with_errors_only_on_failures() {
		let meta = HookMetadata {
			phase: Phase::Upgrade,
			stage: Stage::Post,
			image_name: "kiosk".into(),
			arches: vec![Arch::X86_64],
			image_ref: "registry.example.com/org/kiosk:latest".into(),
			revision: None,
			version: None,
			output_dir: None,
			targets: None,
			remotes: Some(vec!["admin@a".into(), "admin@b".into()]),
			credential: None,
			results: Some(vec![
				Outcome { host: "admin@a".into(), ok: true, error: None },
				Outcome { host: "admin@b".into(), ok: false, error: Some("ssh: timed out".into()) },
			]),
		};
		assert_eq!(
			json_of(&meta)["results"],
			json!([
				{ "host": "admin@a", "ok": true },
				{ "host": "admin@b", "ok": false, "error": "ssh: timed out" }
			])
		);
	}
}
