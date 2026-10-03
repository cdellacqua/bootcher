//! Shared scaffolding for the VM end-to-end tests — the LAN lifecycle
//! (`e2e_vm.rs`), the registry-mode signing path (`e2e_registry_sign.rs`), the
//! plain registry lifecycle + signing enrollment (`e2e_registry.rs`), and the
//! LAN→registry origin switch (`e2e_lan_to_registry.rs`). Holds the environment
//! probe, the ssh-agent wrapper, keypair generation, the throwaway-registry guard,
//! and the small filesystem/process helpers the harnesses share; each test keeps
//! its own `Harness` (the flows differ).
//!
//! Not a test binary itself — it's a module each test file pulls in with
//! `mod common;`. `dead_code` is allowed because not every helper is used by every
//! test that includes it.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use assert_cmd::Command as AssertCommand;
use bootcher_core::context::Arch;
use bootcher_core::progress::Scope;
use bootcher_core::qemu;

/// The provisioned admin `authorized_keys` file (mirrors the scaffold sshd config).
pub(crate) const ADMIN_KEYS: &str = "/etc/ssh/authorized_keys.d/admin";
/// The ssh user the scaffold provisions.
pub(crate) const VM_USER: &str = "admin";
/// Generous boot/ssh budget — a cold first boot of a real bootc disk, even on KVM.
pub(crate) const SSH_TIMEOUT: Duration = Duration::from_mins(10);
/// Root for the throwaway run dir *and* the podman store — disk-backed `/var/tmp`,
/// not tmpfs `/tmp` (might be too small for a ~2GB bootc image) and not the user's home.
pub(crate) const SCRATCH_BASE: &str = "/var/tmp";
/// The image-baked sentinel the e2es read back after an upgrade. Under `/usr` so an
/// upgrade swaps it atomically; [`SENTINEL_REL`] is the same path inside the sysroot
/// overlay (see [`set_sentinel`]).
pub(crate) const SENTINEL_PATH: &str = "/usr/lib/bootcher-e2e-sentinel";
/// Sysroot-relative form of [`SENTINEL_PATH`] (what gets baked into the image).
pub(crate) const SENTINEL_REL: &str = "usr/lib/bootcher-e2e-sentinel";

// ----------------------------------------------------------------- ssh-agent

/// A spawned `ssh-agent`, killed on drop. Carries the socket bootcher and `ssh-add`
/// talk to.
pub(crate) struct Agent {
	sock: PathBuf,
	pid: u32,
}

impl Agent {
	pub(crate) fn start(home: &Path) -> Self {
		let sock = home.join("agent.sock");
		// `-a <sock>`: bind a known socket and daemonize; stdout prints the PID.
		let out = duct::cmd!("ssh-agent", "-a", &sock)
			.stdout_capture()
			.run()
			.expect("ssh-agent must be installed for the e2e");
		let stdout = String::from_utf8_lossy(&out.stdout);
		let pid = stdout
			.lines()
			.find_map(|l| l.trim().strip_prefix("SSH_AGENT_PID="))
			.and_then(|v| v.trim_end_matches(';').split(';').next())
			.and_then(|v| v.trim().parse().ok())
			.expect("parse ssh-agent PID");
		Self { sock, pid }
	}

	pub(crate) fn sock(&self) -> &Path {
		&self.sock
	}

	pub(crate) fn add(&self, key: &Path) {
		duct::cmd!("ssh-add", key)
			.env("SSH_AUTH_SOCK", &self.sock)
			.run()
			.unwrap_or_else(|_| panic!("ssh-add of {} failed", key.display()));
	}
}

impl Drop for Agent {
	fn drop(&mut self) {
		let _ = duct::cmd!("kill", self.pid.to_string()).unchecked().run();
	}
}

// ----------------------------------------------------------------- prereqs

/// The host facts a VM e2e needs. [`Prereqs::probe`] checks what every VM e2e
/// needs; [`Prereqs::probe_disk_build`] additionally checks what building a disk
/// image needs.
pub(crate) struct Prereqs {
	pub arch: Arch,
	pub firmware: PathBuf,
}

impl Prereqs {
	/// Probe for qemu + KVM, UEFI firmware, podman and the ssh tools — enough for an
	/// e2e that boots a VM but never builds a disk (e.g. takeover, which only builds
	/// the container rootlessly).
	pub(crate) fn probe() -> Result<Self> {
		let Some(arch) = Arch::host() else {
			bail!("unrecognised host arch");
		};

		for bin in [arch.qemu_system_bin(), "qemu-img", "podman", "ssh", "ssh-agent", "ssh-keygen"]
		{
			if !is_binary_available(bin) {
				bail!("missing `{bin}`");
			}
		}
		if !Path::new("/dev/kvm").exists() {
			bail!("/dev/kvm not present (KVM required)");
		}
		let Some(firmware) = bootcher_core::qemu::find_uefi_firmware(arch) else {
			bail!("no UEFI firmware (install edk2-ovmf / AAVMF)");
		};
		Ok(Self { arch, firmware })
	}

	/// [`Prereqs::probe`] plus passwordless `sudo sh`, for e2es that build a disk image.
	pub(crate) fn probe_disk_build() -> Result<Self> {
		let env = Self::probe()?;
		// image-builder runs a privileged container, and bootcher opens it through one `sudo sh`
		// root session (see bootcher_core::sudo). The e2e needs passwordless
		// `sudo sh` specifically since while it's running there's no TTY
		// for a password prompt.
		let sudo_ok = duct::cmd!("sudo", "-n", "sh", "-c", "true")
			.stdout_null()
			.stderr_null()
			.unchecked()
			.run()
			.is_ok_and(|o| o.status.success());
		if !sudo_ok {
			bail!(
				"passwordless `sudo sh` not available. bootcher's privileged \
				 image build runs `sudo sh` — \
				 grant e.g. `{user} ALL=(ALL) NOPASSWD: ALL` (dev box) in sudoers",
				user = std::env::var("USER").unwrap_or_else(|_| "<you>".into()),
			);
		}
		Ok(env)
	}
}

/// Host facts the **cross-arch** builder e2e needs, or `None` (with a reason) to
/// skip. Unlike [`Prereqs`] this targets the *foreign* arch: the image-builder step runs in
/// a foreign Fedora Cloud builder VM under TCG (no KVM), and the produced disk is
/// booted under TCG too — so it needs the foreign `qemu-system-<arch>` and foreign
/// UEFI firmware, qemu-user emulation for the cross-arch `podman build`, and
/// network for the builder's one-time cloud-image + base-image pulls. It does *not*
/// need `/dev/kvm` (everything foreign is emulated) or passwordless host `sudo`
/// (the privileged image-builder step runs inside the builder VM, not in-process).
pub(crate) struct CrossPrereqs {
	pub host: Arch,
	pub cross: Arch,
	/// UEFI firmware for the *cross* arch, to boot the produced disk.
	pub cross_firmware: PathBuf,
}

impl CrossPrereqs {
	pub(crate) fn probe() -> Result<Self> {
		let Some(host) = Arch::host() else {
			bail!("unrecognised host arch");
		};
		// The cross target is whichever arch the host is not.
		let cross = match host {
			Arch::X86_64 => Arch::Aarch64,
			Arch::Aarch64 => Arch::X86_64,
		};

		for bin in [cross.qemu_system_bin(), "qemu-img", "podman", "ssh", "ssh-keygen"] {
			if !is_binary_available(bin) {
				bail!("missing `{bin}` (needed to build/boot a {cross} guest)");
			}
		}
		// The cross-arch container build (`[builder] build = "local"`) runs under
		// qemu-user via binfmt_misc — without a registered handler `podman build
		// --platform` fails. Probe for *any* registered qemu binfmt entry.
		if !qemu_user_registered() {
			bail!(
				"no qemu-user binfmt_misc handler registered — the cross-arch `podman build` needs \
				 one (install qemu-user-static, e.g. `podman run --rm --privileged \
				 docker.io/multiarch/qemu-user-static --reset -p yes`)"
			);
		}
		let Some(cross_firmware) = bootcher_core::qemu::find_uefi_firmware(cross) else {
			bail!("no UEFI firmware for {cross} (install edk2-ovmf / edk2-aarch64 / AAVMF)");
		};
		Ok(Self { host, cross, cross_firmware })
	}
}

/// True if any `qemu-*` handler is registered in `binfmt_misc` — a cheap proxy for
/// "cross-arch `podman build` can emulate the foreign userspace".
fn qemu_user_registered() -> bool {
	let Ok(entries) = std::fs::read_dir("/proc/sys/fs/binfmt_misc") else {
		return false;
	};
	entries.flatten().any(|e| {
		e.file_name().to_string_lossy().starts_with("qemu-")
			&& std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("enabled"))
	})
}

// ----------------------------------------------------------------- helpers

pub(crate) fn is_binary_available(bin: &str) -> bool {
	duct::cmd(bin, ["--version"])
		.stdout_null()
		.stderr_null()
		.unchecked()
		.run()
		.is_ok_and(|o| o.status.success())
		|| duct::cmd("sh", ["-c", &format!("command -v {bin}")])
			.stdout_null()
			.stderr_null()
			.unchecked()
			.run()
			.is_ok_and(|o| o.status.success())
}

/// Generate an unencrypted ed25519 keypair `<dir>/<name>` (+ `.pub`), returning the
/// private-key path.
pub(crate) fn keygen(dir: &Path, name: &str) -> PathBuf {
	let key = dir.join(name);
	duct::cmd!("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", &key)
		.run()
		.unwrap_or_else(|_| panic!("ssh-keygen for {name} failed"));
	key
}

pub(crate) fn pub_key_path(key: &Path) -> PathBuf {
	let mut s = key.as_os_str().to_owned();
	s.push(".pub");
	PathBuf::from(s)
}

/// The base64 body of a public key (the field sshd actually stores), for substring
/// assertions against the on-device `authorized_keys`.
pub(crate) fn read_pub_key(key: &Path) -> String {
	std::fs::read_to_string(pub_key_path(key))
		.unwrap()
		.split_whitespace()
		.nth(1)
		.unwrap()
		.to_owned()
}

/// First file named `name` anywhere under `root`.
pub(crate) fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		for entry in std::fs::read_dir(&dir).ok()?.flatten() {
			let path = entry.path();
			if path.is_dir() {
				stack.push(path);
			} else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
				return Some(path);
			}
		}
	}
	None
}

// ----------------------------------------------------------------- registry

/// A throwaway `registry:2` container for the registry-mode e2e tests, removed on
/// drop. Both build host and guest pull a signed/unsigned image from it under the
/// **same** reference string (see [`host_primary_ip`]). Named per test so two
/// registry e2e binaries don't collide on the container name.
pub(crate) struct RegistryGuard {
	name: String,
}

impl RegistryGuard {
	/// Start `registry:2` named `name`, published on host `port`, and block until
	/// it accepts connections. A prior container of the same name is force-removed
	/// first so a crashed earlier run doesn't wedge this one.
	pub(crate) fn start(name: &str, port: u16) -> Self {
		// A prior container of this name (a crashed earlier run) would wedge the
		// `podman run` below; reap it — and its anonymous blob volume — first.
		bootcher_core::podman::reap_container(name);
		duct::cmd!(
			"podman",
			"run",
			"-d",
			"--name",
			name,
			"-p",
			&format!("{port}:5000"),
			"docker.io/library/registry:2"
		)
		.run()
		.expect("could not start the throwaway registry");
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			if TcpStream::connect(("127.0.0.1", port)).is_ok() {
				break;
			}
			assert!(Instant::now() < deadline, "registry did not start within 10s");
			std::thread::sleep(Duration::from_millis(100));
		}
		Self { name: name.to_owned() }
	}
}

impl Drop for RegistryGuard {
	fn drop(&mut self) {
		bootcher_core::podman::reap_container(&self.name);
	}
}

/// A `registries.conf` drop-in marking the throwaway registry (`<ip>:<port>`)
/// plain-HTTP, so both the build host's podman and the guest accept it without TLS.
pub(crate) fn insecure_registries_conf(reg_addr: &str) -> String {
	format!("[[registry]]\nlocation = \"{reg_addr}\"\ninsecure = true\n")
}

/// The host's primary (default-route source) IPv4 — the address both the build
/// host (locally) and the guest (via qemu user-net NAT) reach the registry at.
/// Addressing the registry by this IP (not `localhost`) keeps the reference string
/// identical on both sides, which a cosign `matchRepository` identity requires.
pub(crate) fn host_primary_ip() -> String {
	let out = duct::cmd!("ip", "-4", "route", "get", "1.1.1.1")
		.stdout_capture()
		.run()
		.expect("`ip route get` to discover the host IP");
	let text = String::from_utf8_lossy(&out.stdout);
	// "... src <ip> ..." — the source address the kernel would use.
	text.split_whitespace()
		.skip_while(|w| *w != "src")
		.nth(1)
		.map(str::to_owned)
		.expect("no `src` IP in `ip route get` output")
}

// ----------------------------------------------------------------- run dir

/// Set this (to any value) to *keep* a test's throwaway run dir (`$HOME`, project,
/// VM disk overlay, serial log) after the run, for post-mortem inspection of a
/// failure. By default it's removed on teardown.
pub(crate) const KEEP_RUN_ENV: &str = "BOOTCHER_E2E_KEEP_RUN";

/// Create a test's throwaway run dir under [`SCRATCH_BASE`] — disk-backed, since a
/// bootc disk image overflows tmpfs `/tmp` — removed on drop unless [`KEEP_RUN_ENV`]
/// is set (then its path is printed so it can be found).
pub(crate) fn run_dir(prefix: &str) -> tempfile::TempDir {
	let keep = std::env::var_os(KEEP_RUN_ENV).is_some();
	let dir = tempfile::Builder::new()
		.prefix(prefix)
		.disable_cleanup(keep)
		.tempdir_in(SCRATCH_BASE)
		.expect("tempdir under /var/tmp");
	if keep {
		eprintln!("e2e: keeping run dir {}", dir.path().display());
	}
	dir
}

// ----------------------------------------------------------------- store cleanup

/// Set this (to any value) to *keep* a test's persistent podman store after the
/// run — trading disk for a warm cache (cached base image / built layers) on the
/// next run. By default the store is wiped on teardown so repeated e2e runs don't
/// pile up gigabytes of accumulated images under `/var/tmp/bootcher-e2e-*-store`.
pub(crate) const KEEP_STORE_ENV: &str = "BOOTCHER_E2E_KEEP_STORE";

/// Owns a test's persistent podman store dir (the one its commands point
/// `XDG_DATA_HOME` at). Constructing it readies the dir; dropping it removes the
/// dir — wiping the accumulated images — unless [`KEEP_STORE_ENV`] is set. Held as
/// a `Harness` field so the wipe runs at end of test, after the last podman call.
pub(crate) struct StoreGuard {
	dir: PathBuf,
}

impl StoreGuard {
	/// Create (if needed) the podman store dir at `dir` and guard it for cleanup.
	pub(crate) fn new(dir: &str) -> Self {
		std::fs::create_dir_all(dir).expect("create podman store dir");
		Self { dir: PathBuf::from(dir) }
	}
}

impl Drop for StoreGuard {
	fn drop(&mut self) {
		if std::env::var_os(KEEP_STORE_ENV).is_some() {
			return;
		}
		// Go through `podman unshare`: it enters the user namespace where the
		// rootless store's subuid-owned overlay files map to root, so a plain
		// `rm -rf` can remove them (a bare `std::fs::remove_dir_all` would hit
		// EACCES on those files wherever podman uses a subuid range).
		bootcher_core::exec::best_effort(&duct::cmd!("podman", "unshare", "rm", "-rf", &self.dir));
	}
}

// ----------------------------------------------------------------- project + image

/// Scaffold a throwaway project with `bootcher init -y <name>` under `hp`, returning
/// the project dir. Each harness then overwrites `bootcher.toml` with its own manifest.
pub(crate) fn scaffold_project(hp: &Path, name: &str) -> PathBuf {
	AssertCommand::cargo_bin("bootcher")
		.unwrap()
		.args(["init", "-y", name])
		.current_dir(hp)
		.env("HOME", hp)
		.assert()
		.success();
	hp.join(name)
}

/// Write `content` to `<proj>/sysroot/<rel>` (parent dirs created) — the image overlay
/// that gets baked into the next build.
pub(crate) fn write_sysroot(proj: &Path, rel: &str, content: &str) {
	let p = proj.join("sysroot").join(rel);
	std::fs::create_dir_all(p.parent().unwrap()).unwrap();
	std::fs::write(p, content).unwrap();
}

/// Bake the sentinel value (newline-terminated) into the sysroot overlay at
/// [`SENTINEL_REL`].
pub(crate) fn set_sentinel(proj: &Path, value: &str) {
	write_sysroot(proj, SENTINEL_REL, &format!("{value}\n"));
}

/// The image-builder-produced `disk.qcow2` under `<proj>/output`.
pub(crate) fn built_disk(proj: &Path) -> PathBuf {
	let output = proj.join("output");
	find_file(&output, "disk.qcow2")
		.unwrap_or_else(|| panic!("no disk.qcow2 under {}", output.display()))
}

/// Base `bootcher` command rooted at the project with the env every harness shares —
/// the throwaway `$HOME`, the e2e's own podman store (`XDG_DATA_HOME`), and (when
/// `Some`) the ssh-agent socket. Callers chain `.args(...)` and any test-specific
/// `.env(...)` (signing passphrase, pull credential, cache home).
pub(crate) fn bootcher_cmd(
	proj: &Path,
	home: &Path,
	store: &str,
	agent_sock: Option<&Path>,
) -> AssertCommand {
	let mut c = AssertCommand::cargo_bin("bootcher").unwrap();
	c.current_dir(proj).env("HOME", home).env("XDG_DATA_HOME", store);
	if let Some(sock) = agent_sock {
		c.env("SSH_AUTH_SOCK", sock);
	}
	c
}

// ----------------------------------------------------------------- boot + serial

/// Create a copy-on-write qcow2 overlay on `base` at `overlay`, optionally grown to
/// `size` (e.g. `"20G"` for a small cloud image that must fit a `bootc install`). The
/// base stays pristine so a re-boot starts clean.
pub(crate) fn make_overlay(base: &Path, overlay: &Path, size: Option<&str>) {
	let mut args: Vec<&OsStr> =
		["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"].into_iter().map(OsStr::new).collect();
	args.push(base.as_os_str());
	args.push(overlay.as_os_str());
	if let Some(size) = size {
		args.push(OsStr::new(size));
	}
	duct::cmd("qemu-img", args).run().expect("creating boot overlay");
}

/// Copy the guest serial log out of the (about-to-be-removed) temp dir and print its
/// tail — the only window into a boot that never answered ssh. `tag` names the saved
/// file (`bootcher-e2e-<tag>-serial.log`) so parallel e2e binaries don't clobber it.
pub(crate) fn dump_serial(serial_log: &Path, tag: &str) {
	let dest = PathBuf::from(SCRATCH_BASE).join(format!("bootcher-e2e-{tag}-serial.log"));
	let _ = std::fs::copy(serial_log, &dest);
	eprintln!("--- guest serial log (saved to {}) ---", dest.display());
	if let Ok(s) = std::fs::read_to_string(serial_log) {
		for line in s.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
			eprintln!("{line}");
		}
	}
}

/// Wait for `user@127.0.0.1:port` to answer ssh with `key`; on timeout dump the serial
/// log (which the temp dir would otherwise take with it) and panic with `what`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wait_for_ssh(
	user: &str,
	port: u16,
	key: &Path,
	timeout: Duration,
	scope: &Scope,
	serial_log: &Path,
	tag: &str,
	what: &str,
) {
	if let Err(e) = qemu::ssh(user, port, key).wait_until_reachable(timeout, scope) {
		dump_serial(serial_log, tag);
		panic!("{what}: {e:#}");
	}
}

// ----------------------------------------------------------------- ssh

/// A borrow-only view of one ssh identity to the guest — the login user, forwarded
/// port, key, and whether to share an ssh-agent. Every harness's `ssh_*` helpers
/// funnel through this so the retry/transport handling lives in one place. Cheap to
/// build per call (all borrows), so a test that rotates keys or switches login users
/// just varies a field.
pub(crate) struct Ssh<'a> {
	pub home: &'a Path,
	pub user: &'a str,
	pub port: u16,
	pub key: &'a Path,
	/// `Some` to share an agent (e.g. takeover's initial login); `None` to offer
	/// *only* `-i key`, the basis for the rotation tests' "this key works / is
	/// rejected" assertions.
	pub agent_sock: Option<&'a Path>,
}

impl Ssh<'_> {
	fn command(&self, cmd: &str) -> Command {
		let argv = qemu::ssh(self.user, self.port, self.key).argv(&[], cmd);
		let (program, rest) = argv.split_first().unwrap();
		let mut c = Command::new(program);
		c.args(rest).env("HOME", self.home);
		match self.agent_sock {
			Some(sock) => {
				c.env("SSH_AUTH_SOCK", sock);
			}
			None => {
				c.env_remove("SSH_AUTH_SOCK");
			}
		}
		c
	}

	/// Run `cmd` once over a fresh login, capturing its output (stdin nulled).
	pub(crate) fn capture(&self, cmd: &str) -> Output {
		self.command(cmd).stdin(Stdio::null()).output().expect("ssh")
	}

	/// `true` iff a single login runs `cmd` to a zero exit.
	pub(crate) fn ok(&self, cmd: &str) -> bool {
		self.capture(cmd).status.success()
	}

	/// Capture stdout of `cmd`, retrying ~10s through a transient transport blip —
	/// `Connection timed out during banner exchange`, the guest's sshd answering
	/// slowly over qemu's user-net (SLIRP) right after a reboot, with no bearing on
	/// auth (the key is already proven). Used for reads we expect to succeed.
	pub(crate) fn out(&self, cmd: &str) -> Option<String> {
		for i in 0..20 {
			if i > 0 {
				std::thread::sleep(Duration::from_millis(500));
			}
			let out = self.capture(cmd);
			if out.status.success() {
				return Some(String::from_utf8_lossy(&out.stdout).into_owned());
			}
		}
		None
	}

	/// `true` iff a login succeeds within the same retry budget as [`Self::out`].
	pub(crate) fn reachable(&self) -> bool {
		(0..20).any(|i| {
			if i > 0 {
				std::thread::sleep(Duration::from_millis(500));
			}
			self.ok("true")
		})
	}
}

// ----------------------------------------------------------------- registry client

/// The throwaway registry as the *build host* addresses it: bundles the podman env
/// (project dir, throwaway `$HOME`, dedicated store) and the `<ns>/<name>` reference
/// the registry-mode e2es query and push to. Every method shells out under that env so
/// the insecure-registry drop-in in `$HOME` applies. Built ad hoc per call (all borrows).
pub(crate) struct RegistryClient<'a> {
	pub proj: &'a Path,
	pub home: &'a Path,
	pub store: &'a str,
	/// `<host_ip>:<reg_port>/<repo>` — the namespace both build host and guest use.
	pub ns: &'a str,
	pub name: &'a str,
}

impl RegistryClient<'_> {
	/// `podman manifest inspect <ns>/<name>:<tag>`, returning its stdout (the OCI index
	/// JSON) on success or `None` when the tag is absent / unreachable.
	pub(crate) fn manifest_inspect(&self, tag: &str) -> Option<String> {
		let reference = format!("{}/{}:{tag}", self.ns, self.name);
		let out = duct::cmd!("podman", "manifest", "inspect", "--tls-verify=false", &reference)
			.dir(self.proj)
			.env("HOME", self.home)
			.env("XDG_DATA_HOME", self.store)
			.stderr_null()
			.unchecked()
			.stdout_capture()
			.run()
			.ok()?;
		out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
	}

	/// Whether `<ns>/<name>:<tag>` resolves in the registry (manifest-only, no blobs) —
	/// proof a push landed under the expected tag (and didn't silently create another).
	pub(crate) fn has_tag(&self, tag: &str) -> bool {
		self.manifest_inspect(tag).is_some()
	}

	/// A content-identifying digest for `<ns>/<name>:<tag>`, or `None` if absent — the
	/// first `sha256:<hex>` in the OCI index (the first arch member's digest), enough to
	/// compare two tags for pointing at the same (or distinct) images. Formatting-agnostic.
	pub(crate) fn digest(&self, tag: &str) -> Option<String> {
		let json = self.manifest_inspect(tag)?;
		let (_, rest) = json.split_once("sha256:")?;
		let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
		(!hex.is_empty()).then(|| format!("sha256:{hex}"))
	}

	/// Push the locally-built `localhost/<name>:latest` list to `<ns>/<name>:latest`
	/// **without** signing — the tamper case the signing/enrollment e2es assert is rejected.
	pub(crate) fn push_unsigned(&self) {
		let reg_ref = format!("{}/{}:latest", self.ns, self.name);
		let list = format!("localhost/{}:latest", self.name);
		duct::cmd!("podman", "manifest", "push", "--all", &list, &reg_ref)
			.dir(self.proj)
			.env("HOME", self.home)
			.env("XDG_DATA_HOME", self.store)
			.run()
			.unwrap_or_else(|_| panic!("unsigned push to {reg_ref} failed"));
	}
}

/// Tell the *build host's* podman the throwaway registry (`reg_addr`) is plain HTTP, by
/// writing a `registries.conf` into the test's throwaway `$HOME` (so the user's real
/// config is untouched). The guest gets the same via a baked/pushed sysroot drop-in.
pub(crate) fn write_host_insecure_registry(hp: &Path, reg_addr: &str) {
	let conf_dir = hp.join(".config/containers");
	std::fs::create_dir_all(&conf_dir).unwrap();
	std::fs::write(conf_dir.join("registries.conf"), insecure_registries_conf(reg_addr)).unwrap();
}
