//! `bootcher-core`: the engine behind the `bootcher` CLI.
//!
//! The public surface is two tiers. The high-level driver API the binary runs —
//! [`context`], [`jobs`], [`pipelines`], [`cache`], [`progress`]'s
//! [`Scope`](progress::Scope), [`signals`], [`podman::sweep_working_containers`],
//! and [`ssh`]'s [`Ssh`](ssh::Ssh) type (which leaks through [`context::ToSsh`])
//! — plus a lower-level building-block tier the end-to-end test harness uses to
//! stand up VMs and fixtures: [`qemu`], [`fetch`], [`exec`], and
//! [`podman::reap_container`]. Everything else — the image builders, the fleet
//! fan-out, lifecycle hooks, pre-flight checks, the LAN mini-registry, and the
//! sudo session — is internal plumbing behind `pub(crate)`. `unreachable_pub`
//! keeps that boundary honest: a `pub` item the outside can't reach is a
//! warning, so the qualifiers mean what they say (the `unreachable_pub` lint is
//! enabled workspace-wide in `Cargo.toml`).

pub(crate) mod builder;
pub mod cache;
pub mod context;
pub mod exec;
pub mod fetch;
pub(crate) mod fleet;
pub(crate) mod hooks;
pub mod jobs;
pub mod pipelines;
pub mod podman;
pub(crate) mod preflight;
pub mod progress;
pub mod qemu;
pub(crate) mod registry;
pub mod signals;
pub mod ssh;
pub(crate) mod sudo;

#[cfg(test)]
mod tests;
