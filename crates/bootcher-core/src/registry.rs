//! A minimal, read-only OCI Distribution v2 registry serving a locally built
//! image (or a whole multi-arch manifest list) over plain HTTP on the
//! **loopback** interface.
//!
//! It exists purely to give the LAN deploy backend the registry/blob transport
//! `bootc`/`podman` already speak, so a push only ships the layers the device
//! is missing.
//! Bound to `127.0.0.1` on an OS-assigned port and reached only through an SSH
//! remote forward ([`crate::jobs::upgrade`]), it is never exposed on the LAN —
//! encryption and authentication ride the SSH channel, and the device requires no
//! registry auth or TLS configuration.
//!
//! A pool of handler threads serves requests, so a parallel LAN rollout can have
//! every device pull at once (each through its own remote forward) from one
//! shared registry rather than standing one up per device. [`serve_manifest_list`]
//! exports a multi-arch list so a mixed-arch fleet is covered by that single
//! instance — each client resolves its own arch member out of the served index.
//!
//! Only the three GET/HEAD endpoints a pull exercises are implemented:
//! `GET /v2/`, `…/manifests/<tag-or-digest>`, `…/blobs/<digest>`. The bytes come
//! from an OCI image layout exported once with `podman push … oci:`, whose
//! on-disk shape (`index.json` + `blobs/<algo>/<hex>`) maps almost 1:1 onto the
//! API.

use crate::exec::run;
use crate::progress::Scope;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tempfile::TempDir;
use tiny_http::{Header, Method, Request, Response, Server};

/// A running LAN registry. Holds the pool of handler threads serving requests
/// and the temp OCI layout it serves from; dropping it unblocks the server, joins
/// the threads, and removes the layout.
pub(crate) struct LanRegistry {
	port: u16,
	server: Arc<Server>,
	threads: Vec<JoinHandle<()>>,
	/// Kept solely for its `Drop` (removes the exported layout on the way out).
	_layout: TempDir,
}

impl LanRegistry {
	/// Loopback port the registry is listening on — forward to it with `ssh -R`.
	#[must_use]
	pub(crate) fn port(&self) -> u16 {
		self.port
	}
}

impl Drop for LanRegistry {
	fn drop(&mut self) {
		// Unblock the `incoming_requests` loops so the handler threads can finish,
		// then join them all before the `TempDir` (still being read by request
		// handlers) is removed. `unblock()` releases exactly one blocked `recv`
		// (it queues a single sentinel), so call it once per handler or the rest
		// would hang in `join` forever.
		for _ in 0..self.threads.len() {
			self.server.unblock();
		}
		for thread in self.threads.drain(..) {
			let _ = thread.join();
		}
	}
}

/// Export the local image `src_ref` to a temp OCI layout under the OCI reference
/// `oci_ref` and start a loopback-only registry serving it. Returns once it's
/// listening; keep the [`LanRegistry`] alive for as long as a client needs to
/// pull, then drop it.
///
/// `src_ref` is any ref podman resolves in the invoking user's rootless storage
/// (a per-arch member tag, where `build::run` left it); `oci_ref` is the tag the
/// puller asks for (e.g. `latest`, or `latest-<arch>` when shipping one member to
/// a build host).
///
/// # Errors
///
/// Returns an error if the `podman push` to the OCI layout fails or the server can't start.
pub(crate) fn serve(src_ref: &str, oci_ref: &str, job: &Scope) -> Result<LanRegistry> {
	let layout = tempfile::tempdir().context("creating temp dir for the OCI layout")?;
	// `podman push … oci:<dir>:<tag>` writes an OCI image layout (`oci-layout`,
	// `index.json`, `blobs/`). Rootless — the image is in the invoking user's
	// storage, where `build::run` left it. The `<tag>` becomes the
	// `org.opencontainers.image.ref.name` annotation the device matches on.
	let dst = format!("oci:{}:{oci_ref}", layout.path().display());
	run!(job, "podman", "push", src_ref, dst)?;
	start_server(layout)
}

/// Like [`serve`] but exports a multi-arch manifest `list_ref` — every member,
/// via `podman manifest push --all` — into the layout, so one registry serves a
/// whole mixed-arch fleet: each client pulls `<oci_ref>` and resolves its own
/// arch member out of the served index (the server returns the index by
/// digest, then the member manifest, config, and blobs the puller asks for). The LAN
/// deploy backend uses this to stand up a single shared registry for a parallel
/// rollout instead of one per device.
///
/// # Errors
///
/// Returns an error if the `podman manifest push` fails or the server can't start.
pub(crate) fn serve_manifest_list(
	list_ref: &str,
	oci_ref: &str,
	job: &Scope,
) -> Result<LanRegistry> {
	let layout = tempfile::tempdir().context("creating temp dir for the OCI layout")?;
	let dst = format!("oci:{}:{oci_ref}", layout.path().display());
	// `--all` writes every member manifest + its blobs into the layout (not just
	// the host-arch one), so a device of any built arch can pull from it.
	run!(job, "podman", "manifest", "push", "--all", list_ref, dst)?;
	start_server(layout)
}

/// Start the loopback registry serving the already-exported `layout`. A small
/// pool of handler threads (sized to the host's parallelism) serves requests, so
/// concurrent clients — a parallel LAN rollout, each device pulling through its
/// own remote forward — don't serialize behind a single request loop.
fn start_server(layout: TempDir) -> Result<LanRegistry> {
	let server =
		Arc::new(Server::http("127.0.0.1:0").map_err(|e| anyhow!("starting LAN registry: {e}"))?);
	let port =
		server.server_addr().to_ip().context("LAN registry bound to a non-IP address")?.port();

	let handlers = thread::available_parallelism().map_or(1, std::num::NonZero::get);
	let layout_path = layout.path().to_path_buf();
	let threads = (0..handlers)
		.map(|_| {
			let srv = Arc::clone(&server);
			let layout_path = layout_path.clone();
			thread::spawn(move || {
				for request in srv.incoming_requests() {
					// A handler error fails just this request; the puller surfaces it.
					// Keep serving the rest.
					let _ = handle(request, &layout_path);
				}
			})
		})
		.collect();

	Ok(LanRegistry { port, server, threads, _layout: layout })
}

/// Route one request. GET and HEAD share a response shape — `tiny_http` omits
/// the body for HEAD on its own, so a GET-built response covers both.
fn handle(request: Request, layout: &Path) -> Result<()> {
	if !matches!(request.method(), Method::Get | Method::Head) {
		return respond_status(request, 405);
	}

	let path = request.url().split('?').next().unwrap_or("").to_owned();

	if path == "/v2/" || path == "/v2" {
		let resp = Response::from_string("{}")
			.with_header(header("Content-Type", "application/json")?)
			.with_header(header("Docker-Distribution-API-Version", "registry/2.0")?);
		return request.respond(resp).map_err(Into::into);
	}

	// `…/<name>/manifests/<ref>` and `…/<name>/blobs/<digest>`: the repository
	// `<name>` is irrelevant (one served image), so split on the trailing verb.
	let rest = path.strip_prefix("/v2/");
	if let Some((_name, reference)) = rest.and_then(|s| s.rsplit_once("/manifests/")) {
		return serve_manifest(request, layout, reference);
	}
	if let Some((_name, digest)) = rest.and_then(|s| s.rsplit_once("/blobs/")) {
		return serve_blob(request, layout, digest);
	}

	respond_status(request, 404)
}

fn serve_manifest(request: Request, layout: &Path, reference: &str) -> Result<()> {
	let digest = if reference.starts_with("sha256:") {
		reference.to_owned()
	} else {
		match resolve_tag(layout, reference)? {
			Some(d) => d,
			None => return respond_status(request, 404),
		}
	};
	let Some(path) = blob_path(layout, &digest) else {
		return respond_status(request, 404);
	};
	if !path.is_file() {
		return respond_status(request, 404);
	}
	// A manifest advertises its own `mediaType`; echo it so podman parses the
	// document as the right kind (image manifest vs index).
	let media = media_type(&path)
		.unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned());
	let resp = Response::from_file(File::open(&path)?)
		.with_header(header("Content-Type", &media)?)
		.with_header(header("Docker-Content-Digest", &digest)?);
	request.respond(resp).map_err(Into::into)
}

fn serve_blob(request: Request, layout: &Path, digest: &str) -> Result<()> {
	let Some(path) = blob_path(layout, digest) else {
		return respond_status(request, 404);
	};
	if !path.is_file() {
		return respond_status(request, 404);
	}
	let resp = Response::from_file(File::open(&path)?)
		.with_header(header("Content-Type", "application/octet-stream")?)
		.with_header(header("Docker-Content-Digest", digest)?);
	request.respond(resp).map_err(Into::into)
}

#[derive(Deserialize)]
struct Index {
	manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Descriptor {
	digest: String,
	#[serde(default)]
	annotations: BTreeMap<String, String>,
}

/// Resolve a tag to its manifest digest via `index.json`: match the
/// `org.opencontainers.image.ref.name` annotation, falling back to the sole
/// entry (our layout always holds exactly one image).
fn resolve_tag(layout: &Path, tag: &str) -> Result<Option<String>> {
	let raw = fs::read_to_string(layout.join("index.json")).context("reading index.json")?;
	let index: Index = serde_json::from_str(&raw).context("parsing index.json")?;
	let by_name = index.manifests.iter().find(|m| {
		m.annotations.get("org.opencontainers.image.ref.name").map(String::as_str) == Some(tag)
	});
	let chosen = by_name.or_else(|| (index.manifests.len() == 1).then(|| &index.manifests[0]));
	Ok(chosen.map(|m| m.digest.clone()))
}

/// The top-level `mediaType` of a manifest/index document, if present.
fn media_type(path: &Path) -> Option<String> {
	#[derive(Deserialize)]
	struct MediaOnly {
		#[serde(rename = "mediaType")]
		media_type: Option<String>,
	}
	let raw = fs::read_to_string(path).ok()?;
	serde_json::from_str::<MediaOnly>(&raw).ok()?.media_type
}

/// Map a `<algo>:<hex>` digest to its blob file under the layout, rejecting
/// anything that isn't a bare `algo`/`hex` pair (guards against path traversal).
fn blob_path(layout: &Path, digest: &str) -> Option<PathBuf> {
	let (algo, hex) = digest.split_once(':')?;
	let ok = |s: &str, f: fn(u8) -> bool| !s.is_empty() && s.bytes().all(f);
	if !ok(algo, |b| b.is_ascii_lowercase() || b.is_ascii_digit())
		|| !ok(hex, |b| b.is_ascii_hexdigit())
	{
		return None;
	}
	Some(layout.join("blobs").join(algo).join(hex))
}

fn header(name: &str, value: &str) -> Result<Header> {
	Header::from_bytes(name.as_bytes(), value.as_bytes())
		.map_err(|()| anyhow!("invalid HTTP header {name}: {value}"))
}

fn respond_status(request: Request, code: u16) -> Result<()> {
	request.respond(Response::empty(code)).map_err(Into::into)
}
