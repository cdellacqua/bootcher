//! `bootcher info`: print the project's *resolved* facts as a structured document,
//! and render it either whole (default, or `--format json`) or through a
//! podman-style `{{ .Field }}` Go-template subset that extracts one value — so a
//! script or CI pipeline can read a single fact (`{{ .Image }}`, `{{ .Name }}`)
//! without re-parsing `bootcher.toml`.
//!
//! The document is built from the manifest *after* its `extend` chain is merged and
//! the inline-table `[deploy] registry` form is resolved, so the fields it exposes —
//! notably [`Info::image`], the suffix-free `<registry>/<name>` ref — are the same
//! values the pipeline itself computes, not a shell re-derivation that can drift.

use crate::context::Manifest;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The resolved, machine-readable view of a project a `bootcher info` run prints.
/// Every field is a fact the build/deploy/provision pipelines act on, surfaced so
/// external tooling can consume it verbatim. Serialized `camelCase` so the JSON keys
/// read naturally (`localImage`); the `{{ .Field }}` template resolver matches keys
/// case-insensitively, so `{{ .Image }}`, `{{ .image }}` and `{{ .localImage }}` all
/// work.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
	/// Image/project name (`[general] name`) — the `<name>` in every image ref and
	/// the GHCR/GitLab package name.
	pub name: String,
	/// `"registry"` when `[deploy] registry` is set (devices self-update from a
	/// registry), else `"lan"` (the image is tunnelled over ssh; no registry refs).
	pub mode: &'static str,
	/// Registry namespace (`<registry>`), or `null` in LAN mode.
	pub registry: Option<String>,
	/// Suffix-free fully-qualified image ref (`<registry>/<name>`), or `null` in LAN
	/// mode — what a pipeline appends `:latest` / `:ci-<arch>` / a `CalVer` tag to.
	pub image: Option<String>,
	/// Local manifest-list ref (`localhost/<name>:latest`) the build assembles.
	pub local_image: String,
	/// Declared release channels, `latest` first.
	pub channels: Vec<String>,
	/// Whether image signing is configured (the `[deploy] registry` inline-table form).
	pub signing: bool,
	/// Target arches, `x86_64` before `aarch64`.
	pub arches: Vec<String>,
	/// Each target arch mapped to its `image-builder` disk type(s).
	pub targets: BTreeMap<String, Vec<String>>,
}

impl Info {
	/// Derive the info view from a resolved manifest (post-`extend`).
	#[must_use]
	pub fn from_manifest(m: &Manifest) -> Self {
		let arches = m.targets.arches();
		let targets = arches
			.iter()
			.map(|&a| (a.to_string(), m.targets.types(a).iter().map(ToString::to_string).collect()))
			.collect();
		Info {
			name: m.general.name.clone(),
			mode: if m.registry().is_some() { "registry" } else { "lan" },
			registry: m.registry().map(ToOwned::to_owned),
			image: m.registry().map(|ns| format!("{ns}/{}", m.general.name)),
			local_image: m.local_list_ref(),
			channels: m.declared_channels().iter().map(|s| (*s).to_owned()).collect(),
			signing: m.signing().is_some(),
			arches: arches.iter().map(ToString::to_string).collect(),
			targets,
		}
	}
}

/// Render an [`Info`] for `bootcher info`:
/// - `None` or `Some("json")` → the whole document as pretty JSON.
/// - `Some("{{ .Field }}")` → a Go-template-style subset, extracting one value.
///
/// # Errors
///
/// Propagates a template error (unclosed action, unknown field, unsupported syntax).
pub fn render(info: &Info, format: Option<&str>) -> Result<String> {
	let value = serde_json::to_value(info).context("serializing project info")?;
	match format {
		None | Some("json") => {
			Ok(serde_json::to_string_pretty(&value).context("formatting project info as JSON")?)
		}
		Some(tmpl) => render_template(tmpl, &value),
	}
}

/// A deliberately tiny subset of Go's `text/template` — enough to pull one field out
/// of the info document the way `podman info --format` does, without a template
/// engine. Literal text passes through; each `{{ ... }}` action is one of:
/// - `.` — the whole document, as compact JSON;
/// - `.Field.Sub` — a dotted path, resolved case-insensitively against the document;
/// - `json <arg>` — the above, but always JSON-encoded (strings keep their quotes).
///
/// A scalar renders bare (a string without quotes, a number/bool as its literal, a
/// `null` as the empty string); an array/object renders as compact JSON. An unknown
/// field or unsupported action is an error rather than a silent blank, so a
/// misspelled `--format` fails loudly in a pipeline instead of yielding "".
fn render_template(tmpl: &str, root: &Value) -> Result<String> {
	let mut out = String::new();
	let mut rest = tmpl;
	while let Some(open) = rest.find("{{") {
		out.push_str(&rest[..open]);
		let after = &rest[open + 2..];
		let close = after
			.find("}}")
			.with_context(|| format!("unclosed `{{{{` in --format template {tmpl:?}"))?;
		let action = after[..close].trim();
		out.push_str(&eval_action(action, root, tmpl)?);
		rest = &after[close + 2..];
	}
	out.push_str(rest);
	Ok(out)
}

/// Evaluate one trimmed `{{ ... }}` action against the document root.
fn eval_action(action: &str, root: &Value, tmpl: &str) -> Result<String> {
	// `json <arg>` forces JSON encoding (quoted strings); a bare arg renders scalars raw.
	let (arg, json) = action.strip_prefix("json").map_or((action, false), |a| (a.trim(), true));
	if arg.is_empty() || !arg.starts_with('.') {
		bail!(
			"unsupported --format action {{{{ {action} }}}} in {tmpl:?}: expected `.Field`, \
			 `.Field.Sub`, `.`, or `json .Field`"
		);
	}
	let value = resolve_path(arg, root, tmpl)?;
	Ok(if json { value.to_string() } else { render_value(value) })
}

/// Resolve a `.Field.Sub` dotted path against `root` (a leading/bare `.` is the root
/// itself). Each segment matches an object key exactly, else case-insensitively.
fn resolve_path<'a>(path: &str, root: &'a Value, tmpl: &str) -> Result<&'a Value> {
	let mut cur = root;
	for seg in path.split('.').filter(|s| !s.is_empty()) {
		let obj = cur.as_object().with_context(|| {
			format!("--format {tmpl:?}: `.{seg}` has no field to select (value is not an object)")
		})?;
		cur = obj
			.get(seg)
			.or_else(|| obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(seg)).map(|(_, v)| v))
			.with_context(|| {
				format!(
					"--format {tmpl:?}: no such field `{seg}`; available: {}",
					obj.keys().cloned().collect::<Vec<_>>().join(", ")
				)
			})?;
	}
	Ok(cur)
}

/// A resolved value rendered for bare (non-`json`) output: strings unquoted, scalars
/// as their literal, `null` blank, and composites as compact JSON.
fn render_value(v: &Value) -> String {
	match v {
		Value::String(s) => s.clone(),
		Value::Null => String::new(),
		// bool / number render as their literal; array / object as compact JSON.
		other => other.to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample() -> Value {
		serde_json::json!({
			"name": "kiosk",
			"mode": "registry",
			"registry": "ghcr.io/acme",
			"image": "ghcr.io/acme/kiosk",
			"localImage": "localhost/kiosk:latest",
			"channels": ["latest", "stable"],
			"signing": false,
			"arches": ["x86_64", "aarch64"],
			"targets": { "x86_64": ["qcow2"], "aarch64": ["raw"] },
		})
	}

	#[test]
	fn extracts_a_scalar_field_unquoted() {
		assert_eq!(render_template("{{ .image }}", &sample()).unwrap(), "ghcr.io/acme/kiosk");
	}

	#[test]
	fn field_lookup_is_case_insensitive_like_podman() {
		assert_eq!(render_template("{{ .Image }}", &sample()).unwrap(), "ghcr.io/acme/kiosk");
		assert_eq!(
			render_template("{{ .localImage }}", &sample()).unwrap(),
			"localhost/kiosk:latest"
		);
		assert_eq!(
			render_template("{{ .LocalImage }}", &sample()).unwrap(),
			"localhost/kiosk:latest"
		);
	}

	#[test]
	fn nested_path_and_surrounding_literals() {
		assert_eq!(
			render_template("member={{ .name }}:ci-x86_64", &sample()).unwrap(),
			"member=kiosk:ci-x86_64"
		);
	}

	#[test]
	fn composite_renders_as_compact_json_and_json_quotes_scalars() {
		assert_eq!(render_template("{{ .arches }}", &sample()).unwrap(), r#"["x86_64","aarch64"]"#);
		assert_eq!(render_template("{{ json .name }}", &sample()).unwrap(), r#""kiosk""#);
	}

	#[test]
	fn null_field_renders_blank() {
		let v = serde_json::json!({ "image": Value::Null });
		assert_eq!(render_template("{{ .image }}", &v).unwrap(), "");
	}

	#[test]
	fn unknown_field_is_a_loud_error() {
		let err = render_template("{{ .nope }}", &sample()).unwrap_err().to_string();
		assert!(err.contains("no such field `nope`"), "unexpected error: {err}");
	}

	#[test]
	fn unclosed_action_errors() {
		assert!(render_template("{{ .name ", &sample()).is_err());
	}

	#[test]
	fn unsupported_action_errors() {
		assert!(render_template("{{ name }}", &sample()).is_err());
	}
}
