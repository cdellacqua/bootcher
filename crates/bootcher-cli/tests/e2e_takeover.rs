//! End-to-end takeover test: boot a **stock Debian 13 (trixie) generic cloud
//! image**, run `bootcher takeover` against it, and prove it reboots into a bootc
//! system with the provisioned admin key — the one pass that exercises the whole
//! `takeover` chain (host pre-flight over SSH → image transfer → `bootc install
//! to-existing-root` → staged-`/etc` secret injection → reboot → verify) against a
//! real foreign distro rather than any single seam.
//!
//! Like the other VM e2es it is **opt-in** (gated by the `e2e` Cargo feature; plain
//! `cargo test` compiles but `#[ignore]`s it) and probes its prerequisites.
//!
//! Scope: the **LAN** backend, which is self-contained — `takeover` ships the image
//! to the guest over the same `-R`-tunnelled loopback registry `upgrade` uses, so no
//! external registry is needed. amd64 only for now (matches the host's build arch);
//! aarch64 is left for later.
//!
//! ## The guest
//!
//! A stock Debian generic cloud qcow2, cached + sha512-verified on first use via
//! [`fetch::download`] (the new [`Checksum::Sha512`] path — Debian publishes a
//! `SHA512SUMS`, not a sha256). The image is hybrid (BIOS boot partition + ESP), so
//! the same base runs the takeover twice: once under **UEFI** (OVMF) and once under
//! **legacy BIOS** (`SeaBIOS`), proving `bootc install to-existing-root` (and takeover's
//! pre-flight) work on both firmwares. A cloud-init `NoCloud` seed gives the stock `debian` user our admin key and installs `podman` (takeover
//! never installs it — the host must already have it), mirroring `bootcher-core`'s
//! vm builder seed.
//!
//! ## The two identities, mirrored
//!
//! The initial connection is `debian@` (the stock cloud user, resolved by the
//! remote's `takeover_login`); after the reboot it's `admin@` with the injected key
//! — the same key, so one keypair drives both. The forwarded loopback port rides in
//! the `ssh://…:<port>` remote URL.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use bootcher_core::fetch::{self, Checksum};
use bootcher_core::progress::Scope;
use bootcher_core::qemu::{self, Vm, VmConfig};

mod common;
use common::{
	ADMIN_KEYS, Agent, Prereqs, SSH_TIMEOUT, Ssh, StoreGuard, VM_USER, keygen, read_pub_key,
};

/// Stock cloud login on the Debian generic image — takeover's *initial* connection
/// (before the image's `admin` user, [`VM_USER`], replaces it).
const STOCK_USER: &str = "debian";

/// Pinned Debian 13 (trixie) generic amd64 cloud image — the takeover target. The
/// sha512 is the published digest from
/// `https://cdimage.debian.org/cdimage/cloud/trixie/20260615-2510/SHA512SUMS`
/// (Debian ships sha512, exercising the `Checksum::Sha512` download path).
const DEBIAN_URL: &str = "https://cdimage.debian.org/cdimage/cloud/trixie/20260615-2510/debian-13-generic-amd64-20260615-2510.qcow2";
const DEBIAN_SHA512: &str = "e6ec0864d0b9c32ee60669cfe189e06baaaba26921ec0e8aeba6815a664275f13ef39b3a175af131e7d32889abd2e07f72c0c53893cb3b33d422f7b2a6834866";

/// Persistent download cache for the multi-hundred-MB Debian base — out of the
/// temp dir so the fetch happens once across runs. Wiped only by hand.
const CACHE_DIR: &str = "/var/tmp/bootcher-e2e-takeover-cache";
/// Persistent podman stores, pointed at via `XDG_DATA_HOME` (out of the user's real
/// store), one per firmware variant so the two tests can run in parallel. Wiped on
/// teardown unless `BOOTCHER_E2E_KEEP_STORE` is set.
const UEFI_STORE_DIR: &str = "/var/tmp/bootcher-e2e-takeover-store";
const BIOS_STORE_DIR: &str = "/var/tmp/bootcher-e2e-takeover-bios-store";

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --features=e2e")]
fn lan_takeover_converts_a_stock_debian_host() {
	let env = Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	// Boot OVMF (UEFI); `Prereqs` already located the host-arch firmware.
	run_takeover(Some(env.firmware.clone()), UEFI_STORE_DIR);
}

#[test]
#[cfg_attr(not(feature = "e2e"), ignore = "slow VM e2e; opt in with --features=e2e")]
fn lan_takeover_converts_a_legacy_bios_debian_host() {
	Prereqs::probe().expect("prerequisites not satisfied by the current environment");
	// No firmware blob: qemu boots its default `SeaBIOS`, so the guest has no
	// `/sys/firmware/efi` — the legacy-BIOS path through the whole takeover.
	run_takeover(None, BIOS_STORE_DIR);
}

/// The shared takeover scenario, booting the guest on `firmware` (`None` = legacy
/// BIOS) with its own podman store at `store_dir`.
fn run_takeover(firmware: Option<PathBuf>, store_dir: &'static str) {
	let scope = Scope::standalone();

	let h = Harness::setup(firmware, store_dir);
	let _vm = h.boot(&scope);

	// 1. Reachable as the stock `debian` user with our admin key, and cloud-init done
	//    (so `podman` — the takeover prerequisite — is installed).
	h.wait_for_ssh(STOCK_USER, &scope, "Debian guest never answered ssh as debian");
	h.wait_cloud_init();
	// Guard the variant's premise: the guest really is on the intended firmware.
	let efi = h.ssh_out(STOCK_USER, "[ -d /sys/firmware/efi ] && echo yes || echo no");
	assert_eq!(
		efi.as_deref().map(str::trim),
		Some(if h.firmware.is_some() { "yes" } else { "no" }),
		"guest booted on the wrong firmware"
	);

	// 2. Take it over: build the bootc image and convert the live host in place. `-y`
	//    skips the destructive confirmation (non-TTY); the remote's `takeover_login`
	//    supplies the stock login.
	eprintln!("e2e/takeover: converting the live Debian host (slow: build + install + reboot)");
	h.bootcher(&["takeover", "--ssh-key", h.admin_key.to_str().unwrap(), "-y"]).assert().success();

	// 3. It rebooted into bootc as `admin@`. `takeover` already waited + checked
	//    `bootc status` internally; re-assert independently that the host is bootc and
	//    the injected admin key is live.
	h.wait_for_ssh(VM_USER, &scope, "host never came back as admin after takeover");
	let status =
		h.ssh_out(VM_USER, "sudo bootc status").expect("`bootc status` over admin@ after takeover");
	assert!(
		status.contains("localhost/") || status.to_lowercase().contains("booted"),
		"host does not look like a booted bootc system:\n{status}"
	);
	let keys = h.ssh_out(VM_USER, &format!("cat {ADMIN_KEYS}")).expect("read injected admin keys");
	assert!(keys.contains(&read_pub_key(&h.admin_key)), "injected admin key missing from host");

	eprintln!("e2e/takeover: passed");
}

// ----------------------------------------------------------------- harness

/// Everything a takeover run needs: the throwaway project + `$HOME`, the admin
/// keypair, the ssh-agent, the boot overlay, and the forwarded port.
struct Harness {
	/// UEFI blob for `-bios`; `None` boots qemu's default legacy BIOS (`SeaBIOS`).
	firmware: Option<PathBuf>,
	store_dir: &'static str,
	home: tempfile::TempDir,
	proj: PathBuf,
	port: u16,
	agent: Agent,
	admin_key: PathBuf,
	overlay: PathBuf,
	serial_log: PathBuf,
	_store: StoreGuard,
}

impl Harness {
	fn setup(firmware: Option<PathBuf>, store_dir: &'static str) -> Self {
		let home = common::run_dir("bootcher-e2e-takeover-");
		let store = StoreGuard::new(store_dir);
		let hp = home.path();

		// One keypair drives both identities: its public half is seeded onto the stock
		// `debian` user *and* injected as the new `admin` user; its private half is the
		// post-reboot identity. Loaded into an agent so bootcher's ambient ssh uses it
		// for the initial `debian@` connection.
		let keydir = hp.join(".ssh");
		std::fs::create_dir_all(&keydir).unwrap();
		let admin_key = keygen(&keydir, "admin");
		let agent = Agent::start(hp);
		agent.add(&admin_key);

		// Throwaway project, scaffolded with `bootcher init -y` then pointed at the
		// guest: the steady-state remote is `admin@` over the forwarded loopback port
		// (in the `ssh://` URL), with `takeover_login = "debian"` for the initial
		// connection and known-hosts pinned to /dev/null for the throwaway host key.
		let proj = common::scaffold_project(hp, "e2e");

		let port = qemu::free_port().expect("free port");
		let manifest_path = proj.join("bootcher.toml");
		let manifest = std::fs::read_to_string(&manifest_path).unwrap();
		std::fs::write(
			&manifest_path,
			manifest.replace(
				"remotes = []",
				&format!(
					"remotes = [{{ remote = \"ssh://{VM_USER}@127.0.0.1:{port}\", \
					 ssh_opts = [\"-o\", \"UserKnownHostsFile=/dev/null\"], \
					 takeover_login = \"{STOCK_USER}\" }}]"
				),
			),
		)
		.unwrap();

		Self {
			firmware,
			store_dir,
			proj,
			port,
			agent,
			admin_key,
			overlay: hp.join("disk-overlay.qcow2"),
			serial_log: hp.join("serial.log"),
			home,
			_store: store,
		}
	}

	/// Boot a writable, grown overlay on the cached Debian base, attaching the
	/// cloud-init seed, under UEFI (OVMF) or legacy BIOS per `firmware`. Returns the
	/// running guest.
	fn boot(&self, scope: &Scope) -> Vm {
		let base = ensure_debian_base(scope);
		// CoW overlay grown to 20G so growpart + the bootc install have room (the stock
		// cloud image is small); the base stays pristine for the next run.
		common::make_overlay(&base, &self.overlay, Some("20G"));

		let seed = self.home.path().join("seed.img");
		write_seed(&seed, &read_pub_contents(&self.admin_key)).expect("writing cloud-init seed");

		Vm::spawn(
			&VmConfig {
				arch: bootcher_core::context::Arch::X86_64,
				disk: &self.overlay,
				seed: Some(&seed),
				firmware: self.firmware.as_deref(),
				port: self.port,
				log: &self.serial_log,
				accel: "kvm",
				mem_mib: "2048",
				smp: "2",
			},
			scope,
		)
		.expect("spawning qemu")
	}

	/// Block until cloud-init has fully applied on the guest (so `podman` is present),
	/// over the stock `debian@` login. A generous timeout — apt fetches over user-net.
	fn wait_cloud_init(&self) {
		eprintln!("e2e/takeover: waiting for cloud-init (installs podman)…");
		let argv =
			qemu::ssh(STOCK_USER, self.port, &self.admin_key).argv(&[], "cloud-init status --wait");
		let (program, rest) = argv.split_first().unwrap();
		let ok = Command::new(program)
			.args(rest)
			.env("HOME", self.home.path())
			.env("SSH_AUTH_SOCK", self.agent.sock())
			.status()
			.is_ok_and(|s| s.success());
		assert!(ok, "cloud-init did not finish cleanly on the guest");
	}

	/// Wait for the guest to answer ssh as `user` with the admin key; on timeout dump
	/// the serial log and panic with `what`.
	fn wait_for_ssh(&self, user: &str, scope: &Scope, what: &str) {
		common::wait_for_ssh(
			user,
			self.port,
			&self.admin_key,
			SSH_TIMEOUT,
			scope,
			&self.serial_log,
			"takeover",
			what,
		);
	}

	/// A `bootcher` command rooted at the project, with the throwaway `$HOME`, the
	/// agent socket, and the e2e's own persistent podman store.
	fn bootcher(&self, args: &[&str]) -> AssertCommand {
		let mut c = common::bootcher_cmd(
			&self.proj,
			self.home.path(),
			self.store_dir,
			Some(self.agent.sock()),
		);
		c.args(args);
		c
	}

	/// An ssh view as `user` with the admin key, sharing the agent (the initial
	/// `debian@` login the remote's `takeover_login` resolves needs it).
	fn ssh<'a>(&'a self, user: &'a str) -> Ssh<'a> {
		Ssh {
			home: self.home.path(),
			user,
			port: self.port,
			key: &self.admin_key,
			agent_sock: Some(self.agent.sock()),
		}
	}

	/// Capture stdout of `cmd` over ssh as `user`, retrying through the transient
	/// post-reboot transport blips qemu's user-net can produce.
	fn ssh_out(&self, user: &str, cmd: &str) -> Option<String> {
		self.ssh(user).out(cmd)
	}
}

/// Ensure the pinned Debian base is present + sha512-verified in [`CACHE_DIR`],
/// returning its path. Downloads (atomic `.part` + checksum + rename) on first use,
/// trusts the cached file thereafter — the same criterion as the Fedora Cloud base.
/// Serialized so the parallel firmware variants don't race on the same `.part`.
fn ensure_debian_base(scope: &Scope) -> PathBuf {
	static FETCH: std::sync::Mutex<()> = std::sync::Mutex::new(());
	let _guard = FETCH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
	std::fs::create_dir_all(CACHE_DIR).expect("create download cache dir");
	let base = Path::new(CACHE_DIR).join("debian-13-generic-amd64.qcow2");
	if !base.is_file() {
		eprintln!("e2e/takeover: fetching the Debian base image (cached after first run)");
		fetch::download(scope, DEBIAN_URL, &base, Some(Checksum::Sha512(DEBIAN_SHA512)))
			.expect("download + verify the Debian base image");
	}
	base
}

/// The trimmed public-key line for `key` (`<key>.pub`), as seeded into the guest.
fn read_pub_contents(key: &Path) -> String {
	let mut pubp = key.as_os_str().to_owned();
	pubp.push(".pub");
	std::fs::read_to_string(PathBuf::from(pubp)).expect("read admin .pub").trim().to_owned()
}

/// Build the `NoCloud` seed: a 1 MiB FAT image labelled `CIDATA` holding
/// `user-data` (give the stock `debian` user our admin key + install podman) and
/// `meta-data`. Same shape as the vm builder's seed (cloud-init finds it by label).
fn write_seed(path: &Path, pubkey: &str) -> std::io::Result<()> {
	use std::io::Write as _;

	let user_data = format!(
		"#cloud-config\n\
		 users:\n\
		 \x20 - default\n\
		 ssh_authorized_keys:\n\
		 \x20 - {pubkey}\n\
		 package_update: true\n\
		 packages:\n\
		 \x20 - podman\n"
	);
	let meta_data = "instance-id: bootcher-takeover-e2e\nlocal-hostname: takeover-e2e\n";

	let file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(true)
		.open(path)?;
	file.set_len(1024 * 1024)?;
	let opts = fatfs::FormatVolumeOptions::new().volume_label(*b"CIDATA     ");
	fatfs::format_volume(&file, opts)?;
	let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new())?;
	{
		let root = fs.root_dir();
		for (name, body) in [("user-data", user_data.as_str()), ("meta-data", meta_data)] {
			let mut f = root.create_file(name)?;
			f.truncate().ok();
			f.write_all(body.as_bytes())?;
		}
	}
	fs.unmount().ok();
	Ok(())
}
