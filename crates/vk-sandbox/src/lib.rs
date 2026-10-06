//! Execution isolation for Vibeke (spec 13): the `sandbox` level (macOS Seatbelt; Linux
//! bubblewrap + Landlock + seccomp), the host-side egress proxy with network profiles,
//! credential projection, the [`runner::Runner`] abstraction (05 §14), the `container` level
//! (per-task boxes, devcontainers) and the in-box helpers.
//!
//! The crate is server-agnostic: `vk-server` owns task/pane state, Interactions and the per-pane
//! broker; this crate generates profiles, wraps spawn commands and enforces egress.

pub mod config;
pub mod container;
pub mod creds;
pub mod devcontainer;
pub mod env;
pub mod exec;
pub mod linux;
pub mod net;
pub mod policy;
pub mod proxy;
pub mod runner;
pub mod seatbelt;

pub use net::{EgressPolicy, NetworkProfile};
pub use policy::{GitLayout, NetMode, Policy, SandboxSpec};
pub use runner::{
    HostRunner, PreparedSpawn, Runner, RunnerError, SandboxRunner, SandboxSetup, SpawnRequest,
    VmRunner,
};
pub use vk_proto::model::IsolationLevel;
