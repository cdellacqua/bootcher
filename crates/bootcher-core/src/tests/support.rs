//! Fixtures shared by tests in several modules.

use std::path::Path;

/// Serializes the tests that mutate the process-global `BOOTCHER_SIGN_PASSPHRASE`
/// env var. `cargo test` runs tests in parallel, so without this the podman
/// signing tests would race on the var — both its logical value (each uses a
/// different passphrase, read once at enroll and again at push) and `set_var`'s
/// own thread-safety contract.
pub(crate) static SIGN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A throwaway `registry:2` container published on `host_port`, removed on drop.
/// The sign/verify and promote tests need a *real* push-capable registry — the
/// read-only LAN registry ([`crate::registry`]) can't accept a signed push — so
/// this centralizes the `podman run`/teardown boilerplate they share.
/// Cleanup runs in `Drop`, so a panicking test still tears the container down.
pub(crate) struct TestRegistry {
	name: &'static str,
	host_port: u16,
}

impl TestRegistry {
	pub(crate) fn start(name: &'static str, host_port: u16) -> Self {
		// A prior container of this name (a crashed earlier run) would wedge the
		// `podman run` below; reap it — and its anonymous blob volume — first.
		crate::podman::reap_container(name);
		duct::cmd!(
			"podman",
			"run",
			"-d",
			"--name",
			name,
			"-p",
			&format!("{host_port}:5000"),
			"docker.io/library/registry:2"
		)
		.run()
		.expect("could not start the throwaway registry");
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
		loop {
			if std::net::TcpStream::connect(("127.0.0.1", host_port)).is_ok() {
				break;
			}
			assert!(std::time::Instant::now() < deadline, "registry did not start within 10s");
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
		Self { name, host_port }
	}
}

impl Drop for TestRegistry {
	fn drop(&mut self) {
		// Reap the container and its anonymous blob volume.
		crate::podman::reap_container(self.name);
		// These tests tag/build/pull images under `localhost:<port>/…` into the
		// *default* store (no XDG_DATA_HOME override here), so untag those refs or
		// every run leaves dangling `localhost:<port>/test/*` entries behind.
		crate::podman::untag_prefixed(&format!("localhost:{}/", self.host_port));

		// `ensure_push_attachments` (called by the verify test) writes a
		// `registries.d` drop-in into the *real* `~/.config/containers` — same
		// reason as the store: it reads `HOME` from the process env, which we
		// don't (and can't safely) override here. Remove the drop-ins it scoped to
		// this registry's `localhost:<port>` namespace so config doesn't pile up
		// either. The slug replaces `:`/`/` with `-`, so the file is named
		// `bootcher-localhost-<port>-<path>.yaml`.
		if let Some(home) = std::env::var_os("HOME") {
			let dir = Path::new(&home).join(".config/containers/registries.d");
			let prefix = format!("bootcher-localhost-{}-", self.host_port);
			if let Ok(entries) = std::fs::read_dir(&dir) {
				for name in entries.flatten().map(|e| e.file_name()) {
					if name.to_string_lossy().starts_with(&prefix) {
						let _ = std::fs::remove_file(dir.join(&name));
					}
				}
			}
		}
	}
}
