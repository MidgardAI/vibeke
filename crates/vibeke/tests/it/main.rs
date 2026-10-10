//! One integration test binary for the whole crate: one link step instead of dozens.
//! `chaos`, `chaos_gaps` and `timing` stay separate (they run in release mode in the nightly and weekly workflows).

mod support;

mod api_clients;
mod api_docs;
mod api_method_coverage;
mod api_schema_live;
mod api_surface_2a;
mod assist;
mod assist_2d;
mod auth;
mod browser;
mod browser_pane;
mod compat_herdr;
mod compat_herdr_diff;
mod compat_herdr_plugins_ext;
mod compat_herdr_review;
mod compat_herdr_slice2;
mod compat_herdr_surfaces;
mod desk;
mod gateway_bridge;
mod handoff_pickers;
mod harnesses;
mod headless;
mod idle_timers;
mod mouse_select;
mod onboarding;
mod orchestrate;
mod osc_marks;
mod parity;
mod plugin_native;
mod preview;
mod remote_machine;
mod review;
mod scrollback_forget;
mod security;
mod state_backup;
mod task_workspace;
mod tracking;
mod update;
mod v1_remainder;
