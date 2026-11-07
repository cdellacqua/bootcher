//! A minimal client for the OCI distribution (registry v2) HTTP API: just enough to
//! read a manifest's exact bytes by tag and write those same bytes under another tag.
//!
//! That is what `bootcher promote` needs to move a channel onto an existing build
//! without changing its digest. Re-serializing the manifest (as a `podman manifest
//! push` from a list re-created out of the registry does) is free to reorder or
//! rewrite fields, and any byte that differs makes a new digest — one the build's
//! sigstore signature, keyed by digest, doesn't cover. Writing back the bytes the
//! registry served makes the digest identical by construction, and needs no layer
//! traffic: the blobs are already in the same repository.
//!
//! Authentication reuses the credentials `podman login` stored (the
//! `containers-auth.json` files, falling back to Docker's `config.json`), and does the
//! standard bearer-token exchange when the registry asks for it. Credential helpers
//! are not supported: an entry delegated to one is reported as an error naming it.

use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::Digest as _;
use std::path::PathBuf;

/// Every manifest type a bootcher build can be published as: the multi-arch list
/// (OCI index or Docker manifest list), or a single image manifest.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json,\
	application/vnd.docker.distribution.manifest.list.v2+json,\
	application/vnd.oci.image.manifest.v1+json,\
	application/vnd.docker.distribution.manifest.v2+json";

/// A repository on a registry, split into the parts the HTTP API needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Repo {
	/// The registry host as written in the image ref (`ghcr.io`, `localhost:5000`):
	/// the key credentials are stored under.
	pub(crate) host: String,
	/// The repository path within the registry (`owner/name`).
	pub(crate) path: String,
}

impl Repo {
	/// Split a fully-qualified repository (`<host>/<path>`, no tag or digest).
	///
	/// # Errors
	///
	/// Returns an error if `repo` has no host component, i.e. isn't fully qualified.
	pub(crate) fn parse(repo: &str) -> Result<Self> {
		let Some((host, path)) = repo.split_once('/') else {
			bail!("{repo} is not a fully-qualified repository (<registry host>/<path>)");
		};
		// The same rule podman uses: a first component is a host only when it has a
		// dot or a port, or is `localhost`.
		let is_host = host.contains('.') || host.contains(':') || host == "localhost";
		if !is_host || path.is_empty() {
			bail!("{repo} is not a fully-qualified repository (<registry host>/<path>)");
		}
		Ok(Self { host: host.to_owned(), path: path.to_owned() })
	}

	/// The host the HTTP API answers on: Docker Hub's API lives on its own host.
	fn api_host(&self) -> &str {
		if self.host == "docker.io" { "registry-1.docker.io" } else { &self.host }
	}

	/// The `<host>/<path>` form, for messages.
	pub(crate) fn display(&self) -> String {
		format!("{}/{}", self.host, self.path)
	}
}

/// A manifest exactly as the registry served it.
#[derive(Debug, Clone)]
pub(crate) struct Manifest {
	/// Its media type, from the response's `Content-Type`.
	pub(crate) media_type: String,
	/// The raw bytes, never re-serialized.
	pub(crate) bytes: Vec<u8>,
	/// `sha256:<hex>` of [`Self::bytes`]: the manifest's digest.
	pub(crate) digest: String,
}

/// `sha256:<hex>` of `bytes`, the content digest of a manifest.
pub(crate) fn digest_of(bytes: &[u8]) -> String {
	use std::fmt::Write as _;
	let mut out = String::from("sha256:");
	for b in sha2::Sha256::digest(bytes) {
		let _ = write!(out, "{b:02x}");
	}
	out
}

/// A connection to one repository, holding the `Authorization` header once the
/// registry has asked for one.
pub(crate) struct Client {
	agent: ureq::Agent,
	/// `https://<api host>` (or `http://` for a plain-HTTP registry under
	/// `tls_verify = false`).
	base: String,
	repo: Repo,
	auth: Option<String>,
}

impl Client {
	/// Connect to `repo` (`<host>/<path>`). With `tls_verify` false the registry's
	/// certificate isn't checked, and a registry that doesn't speak TLS at all is
	/// reached over plain HTTP — the same fallback podman's `--tls-verify=false` makes.
	///
	/// # Errors
	///
	/// Returns an error if `repo` isn't fully qualified.
	pub(crate) fn connect(repo: &str, tls_verify: bool) -> Result<Self> {
		let repo = Repo::parse(repo)?;
		let tls = ureq::tls::TlsConfig::builder().disable_verification(!tls_verify).build();
		let agent: ureq::Agent = ureq::Agent::config_builder()
			.http_status_as_error(false)
			.tls_config(tls)
			.timeout_global(Some(std::time::Duration::from_mins(2)))
			.build()
			.into();
		let mut base = format!("https://{}", repo.api_host());
		if !tls_verify && agent.get(&format!("{base}/v2/")).call().is_err() {
			base = format!("http://{}", repo.api_host());
		}
		Ok(Self { agent, base, repo, auth: None })
	}

	/// Fetch the manifest `reference` (a tag or digest) points at, or `None` when the
	/// registry reports it unknown.
	///
	/// # Errors
	///
	/// Returns an error on a transport failure, a refused authentication, or any
	/// other unexpected status.
	pub(crate) fn get_manifest(&mut self, reference: &str) -> Result<Option<Manifest>> {
		let url = format!("{}/v2/{}/manifests/{reference}", self.base, self.repo.path);
		let mut resp = self.send(|agent, auth| {
			let mut req = agent.get(&url).header("Accept", MANIFEST_TYPES);
			if let Some(a) = auth {
				req = req.header("Authorization", a);
			}
			req.call()
		})?;
		let status = resp.status().as_u16();
		if status == 404 {
			return Ok(None);
		}
		if status != 200 {
			bail!("{}", self.status_error("reading", reference, &mut resp));
		}
		let media_type = header(&resp, "content-type")
			.map(|t| t.split(';').next().unwrap_or(t).trim().to_owned())
			.context("the registry served the manifest without a Content-Type")?;
		let bytes = resp.body_mut().read_to_vec().context("reading the manifest body")?;
		let digest = digest_of(&bytes);
		// A registry names the digest it computed; ours must agree, or the bytes
		// aren't what it stores.
		if let Some(served) = header(&resp, "docker-content-digest")
			&& served != digest
		{
			bail!(
				"{}:{reference}: the registry says the manifest is {served} but its bytes hash \
				 to {digest}",
				self.repo.display()
			);
		}
		Ok(Some(Manifest { media_type, bytes, digest }))
	}

	/// Store `manifest`'s exact bytes under `tag`, returning the digest the registry
	/// reports for it (its own computation when it sends one, else ours).
	///
	/// # Errors
	///
	/// Returns an error on a transport failure, a refused authentication (e.g. this
	/// machine's credentials can only pull), or any other unexpected status.
	pub(crate) fn put_manifest(&mut self, tag: &str, manifest: &Manifest) -> Result<String> {
		let url = format!("{}/v2/{}/manifests/{tag}", self.base, self.repo.path);
		let mut resp = self.send(|agent, auth| {
			let mut req = agent.put(&url).header("Content-Type", &manifest.media_type);
			if let Some(a) = auth {
				req = req.header("Authorization", a);
			}
			req.send(&manifest.bytes[..])
		})?;
		let status = resp.status().as_u16();
		if !(200..300).contains(&status) {
			bail!("{}", self.status_error("writing", tag, &mut resp));
		}
		Ok(header(&resp, "docker-content-digest")
			.map_or_else(|| manifest.digest.clone(), str::to_owned))
	}

	/// Send a request built by `build`, and on a `401` answer the registry's
	/// challenge (see [`Self::authenticate`]) and send it once more.
	fn send(
		&mut self,
		build: impl Fn(
			&ureq::Agent,
			Option<&str>,
		) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
	) -> Result<ureq::http::Response<ureq::Body>> {
		let resp = build(&self.agent, self.auth.as_deref())
			.with_context(|| format!("reaching the registry at {}", self.base))?;
		if resp.status().as_u16() != 401 || self.auth.is_some() {
			return Ok(resp);
		}
		let challenge = header(&resp, "www-authenticate").unwrap_or_default().to_owned();
		self.auth = Some(self.authenticate(&challenge)?);
		build(&self.agent, self.auth.as_deref())
			.with_context(|| format!("reaching the registry at {}", self.base))
	}

	/// The `Authorization` header answering `challenge` (a `WWW-Authenticate` value):
	/// the stored credentials directly for `Basic`, or a bearer token fetched with them
	/// for `Bearer`. The token's scope asks for push as well as pull up front, so one
	/// token serves both the read and the write of a promotion.
	fn authenticate(&self, challenge: &str) -> Result<String> {
		let creds = stored_credentials(&self.repo)?;
		let basic = creds.as_ref().map(|(user, pass)| {
			let b64 = base64::engine::general_purpose::STANDARD;
			format!("Basic {}", b64.encode(format!("{user}:{pass}")))
		});
		let login_hint = format!("is this machine logged in? (`podman login {}`)", self.repo.host);
		let (scheme, params) = parse_challenge(challenge);
		if scheme.eq_ignore_ascii_case("basic") {
			return basic.with_context(|| {
				format!("{} needs credentials — {login_hint}", self.repo.display())
			});
		}
		if !scheme.eq_ignore_ascii_case("bearer") {
			bail!("{} asked for an unsupported authentication: {challenge:?}", self.repo.host);
		}
		let realm = params
			.iter()
			.find(|(k, _)| k == "realm")
			.map(|(_, v)| v.clone())
			.with_context(|| format!("{}'s bearer challenge names no realm", self.repo.host))?;
		let mut req = self
			.agent
			.get(&realm)
			.query("scope", format!("repository:{}:pull,push", self.repo.path));
		if let Some((_, service)) = params.iter().find(|(k, _)| k == "service") {
			req = req.query("service", service);
		}
		if let Some(b) = &basic {
			req = req.header("Authorization", b);
		}
		let mut resp = req.call().with_context(|| format!("requesting a token from {realm}"))?;
		if resp.status().as_u16() != 200 {
			bail!(
				"{realm} refused a token for {} ({}) — {login_hint}",
				self.repo.display(),
				resp.status()
			);
		}
		let raw =
			resp.body_mut().read_to_vec().with_context(|| format!("reading {realm}'s token"))?;
		let body: serde_json::Value =
			serde_json::from_slice(&raw).with_context(|| format!("parsing {realm}'s token"))?;
		let token = body
			.get("token")
			.or_else(|| body.get("access_token"))
			.and_then(serde_json::Value::as_str)
			.with_context(|| format!("{realm} answered without a token"))?;
		Ok(format!("Bearer {token}"))
	}

	/// An error for an unexpected `resp` while `doing` (reading/writing) `reference`,
	/// quoting the registry's own error body.
	fn status_error(
		&self,
		doing: &str,
		reference: &str,
		resp: &mut ureq::http::Response<ureq::Body>,
	) -> String {
		let status = resp.status();
		let body = resp.body_mut().read_to_string().unwrap_or_default();
		let hint = if matches!(status.as_u16(), 401 | 403) {
			format!(
				" — is this machine logged in with push access? (`podman login {}`)",
				self.repo.host
			)
		} else {
			String::new()
		};
		format!(
			"{doing} {}:{reference} failed: {status} {}{hint}",
			self.repo.display(),
			body.trim()
		)
	}
}

/// A response header as a string, when present and valid.
fn header<'a>(resp: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
	resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Split a `WWW-Authenticate` value into its scheme and `key="value"` parameters.
/// Values are quoted and may themselves contain commas (`scope="repo:x:pull,push"`).
pub(crate) fn parse_challenge(challenge: &str) -> (String, Vec<(String, String)>) {
	let challenge = challenge.trim();
	let (scheme, rest) = challenge.split_once(' ').unwrap_or((challenge, ""));
	let mut params = Vec::new();
	let mut chars = rest.chars().peekable();
	loop {
		while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
			chars.next();
		}
		let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
		if key.is_empty() {
			break;
		}
		let value: String = if chars.peek() == Some(&'"') {
			chars.next();
			let mut v = String::new();
			while let Some(c) = chars.next() {
				match c {
					'\\' => v.extend(chars.next()),
					'"' => break,
					c => v.push(c),
				}
			}
			v
		} else {
			chars.by_ref().take_while(|c| *c != ',').collect()
		};
		params.push((key.trim().to_ascii_lowercase(), value));
	}
	(scheme.to_owned(), params)
}

/// The credentials `podman login` stored for `repo`, or `None` when there are none
/// (an anonymous pull may still work).
///
/// # Errors
///
/// Returns an error if an auth file can't be parsed, or the entry for `repo` is
/// delegated to a credential helper this client can't call.
fn stored_credentials(repo: &Repo) -> Result<Option<(String, String)>> {
	for path in auth_files() {
		let Ok(text) = std::fs::read_to_string(&path) else { continue };
		let doc: serde_json::Value = serde_json::from_str(&text)
			.with_context(|| format!("parsing the registry auth file {}", path.display()))?;
		match lookup_auth(&doc, repo) {
			Lookup::Found(user, pass) => return Ok(Some((user, pass))),
			Lookup::Helper(helper) => bail!(
				"{} delegates the credentials for {} to the `{helper}` credential helper, which \
				 `bootcher promote` can't use — `podman login` with the credentials stored in the \
				 auth file instead",
				path.display(),
				repo.host
			),
			Lookup::Missing => {}
		}
	}
	Ok(None)
}

/// The auth files podman consults, in its lookup order: `$REGISTRY_AUTH_FILE`, the
/// per-session `auth.json` (`$XDG_RUNTIME_DIR/containers/`, or
/// `/run/containers/<uid>/` without one — where root's `podman login` writes), the
/// persistent `$XDG_CONFIG_HOME/containers/auth.json`, then Docker's `config.json`.
fn auth_files() -> Vec<PathBuf> {
	let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
	let home = env("HOME");
	let mut files = Vec::new();
	files.extend(env("REGISTRY_AUTH_FILE"));
	files.push(
		env("XDG_RUNTIME_DIR")
			.map_or_else(
				|| PathBuf::from(format!("/run/containers/{}", rustix::process::getuid().as_raw())),
				|d| d.join("containers"),
			)
			.join("auth.json"),
	);
	if let Some(cfg) = env("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config"))) {
		files.push(cfg.join("containers/auth.json"));
	}
	if let Some(h) = &home {
		files.push(h.join(".docker/config.json"));
	}
	files
}

/// What an auth file says about a repository.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
	/// A username and password (or token).
	Found(String, String),
	/// No inline credentials: they're delegated to this credential helper.
	Helper(String),
	/// Nothing for this repository.
	Missing,
}

/// Look `repo` up in one parsed auth file. Like podman, the most specific key wins:
/// `host/path`, then each shorter namespace prefix, then the bare host. Keys written
/// Docker-style with a scheme (`https://host/v1/`) are matched by their host.
pub(crate) fn lookup_auth(doc: &serde_json::Value, repo: &Repo) -> Lookup {
	let normalize = |key: &str| -> String {
		let k = key.strip_prefix("https://").or_else(|| key.strip_prefix("http://")).unwrap_or(key);
		let k = k.trim_end_matches('/');
		let k = k.strip_suffix("/v1").or_else(|| k.strip_suffix("/v2")).unwrap_or(k);
		if k == "index.docker.io" { "docker.io".to_owned() } else { k.to_owned() }
	};
	let full = format!("{}/{}", repo.host, repo.path);
	let mut candidates = vec![full.clone()];
	let mut ns = full.as_str();
	while let Some(i) = ns.rfind('/') {
		ns = &ns[..i];
		candidates.push(ns.to_owned());
	}
	let auths = doc.get("auths").and_then(serde_json::Value::as_object);
	for want in &candidates {
		let entry = auths.and_then(|a| a.iter().find(|(k, _)| normalize(k) == *want));
		let Some((_, entry)) = entry else { continue };
		if let Some(encoded) = entry.get("auth").and_then(serde_json::Value::as_str)
			&& let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(encoded)
			&& let Ok(raw) = String::from_utf8(raw)
			&& let Some((user, pass)) = raw.split_once(':')
		{
			return Lookup::Found(user.to_owned(), pass.to_owned());
		}
	}
	let helpers = doc.get("credHelpers").and_then(serde_json::Value::as_object);
	if let Some(helper) = helpers
		.and_then(|h| h.get(&repo.host))
		.or_else(|| doc.get("credsStore"))
		.and_then(serde_json::Value::as_str)
	{
		return Lookup::Helper(helper.to_owned());
	}
	Lookup::Missing
}

#[cfg(test)]
mod tests {
	use super::*;

	fn repo(s: &str) -> Repo {
		Repo::parse(s).unwrap()
	}

	#[test]
	fn repo_parse_needs_a_host() {
		assert_eq!(
			repo("ghcr.io/owner/name"),
			Repo { host: "ghcr.io".into(), path: "owner/name".into() }
		);
		assert_eq!(repo("localhost:5000/name").host, "localhost:5000");
		assert_eq!(repo("localhost/a/b").path, "a/b");
		assert!(Repo::parse("owner/name").is_err(), "first component isn't a host");
		assert!(Repo::parse("name").is_err());
		assert!(Repo::parse("ghcr.io/").is_err());
		assert_eq!(repo("docker.io/me/x").api_host(), "registry-1.docker.io");
	}

	#[test]
	fn challenge_params_keep_quoted_commas() {
		let (scheme, params) = parse_challenge(
			r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/n:pull,push""#,
		);
		assert_eq!(scheme, "Bearer");
		assert_eq!(
			params,
			vec![
				("realm".into(), "https://ghcr.io/token".into()),
				("service".into(), "ghcr.io".into()),
				("scope".into(), "repository:o/n:pull,push".into()),
			]
		);
		let (scheme, params) = parse_challenge(r#"Basic realm="Registry Realm""#);
		assert_eq!(scheme, "Basic");
		assert_eq!(params, vec![("realm".into(), "Registry Realm".into())]);
		assert_eq!(parse_challenge("").1, vec![]);
	}

	fn auth(user_pass: &str) -> serde_json::Value {
		serde_json::json!({ "auth": base64::engine::general_purpose::STANDARD.encode(user_pass) })
	}

	#[test]
	fn lookup_prefers_the_most_specific_key() {
		let doc = serde_json::json!({ "auths": {
			"ghcr.io": auth("host:h"),
			"ghcr.io/owner": auth("ns:n"),
		}});
		assert_eq!(
			lookup_auth(&doc, &repo("ghcr.io/owner/name")),
			Lookup::Found("ns".into(), "n".into())
		);
		assert_eq!(
			lookup_auth(&doc, &repo("ghcr.io/other/name")),
			Lookup::Found("host".into(), "h".into())
		);
		assert_eq!(lookup_auth(&doc, &repo("quay.io/owner/name")), Lookup::Missing);
	}

	#[test]
	fn lookup_reads_docker_style_keys_and_passwords_with_colons() {
		let doc = serde_json::json!({ "auths": {
			"https://index.docker.io/v1/": auth("me:tok:en"),
			"https://reg.example.com": auth("u:p"),
		}});
		assert_eq!(
			lookup_auth(&doc, &repo("docker.io/me/x")),
			Lookup::Found("me".into(), "tok:en".into())
		);
		assert_eq!(
			lookup_auth(&doc, &repo("reg.example.com/a/b")),
			Lookup::Found("u".into(), "p".into())
		);
	}

	#[test]
	fn lookup_reports_a_credential_helper() {
		let doc = serde_json::json!({ "auths": {}, "credHelpers": { "ghcr.io": "pass" } });
		assert_eq!(lookup_auth(&doc, &repo("ghcr.io/o/n")), Lookup::Helper("pass".into()));
		let doc = serde_json::json!({ "credsStore": "desktop" });
		assert_eq!(lookup_auth(&doc, &repo("ghcr.io/o/n")), Lookup::Helper("desktop".into()));
		// An inline entry beats a helper.
		let doc =
			serde_json::json!({ "auths": { "ghcr.io": auth("u:p") }, "credsStore": "desktop" });
		assert_eq!(lookup_auth(&doc, &repo("ghcr.io/o/n")), Lookup::Found("u".into(), "p".into()));
	}

	#[test]
	fn digest_is_the_sha256_of_the_bytes() {
		assert_eq!(
			digest_of(b""),
			"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
		);
	}
}
