//! In-crate integration tests.
//!
//! These exercise larger seams (the manifest-list assembly and the LAN
//! mini-registry) end-to-end with podman, but reach for crate-internal helpers,
//! so they live inside the crate (`#[cfg(test)]`) rather than under `tests/` —
//! that keeps the helpers they touch `pub(crate)` instead of forcing a wider
//! public surface just to test them.

mod manifest_list;
mod registry;
