//! `lite-harness-tui` (architecture Phase 8) -- an interactive, persistent
//! Harness Protocol client, unlike `lite-harness` (`lh-cli`) which sends
//! exactly one prompt and exits. All the actual connection/state/rendering
//! logic lives here so it's reachable from `tests/*.rs` integration tests
//! (a `[[bin]]`-only crate's modules aren't visible to those); `src/main.rs`
//! is just the real terminal's startup wiring and event loop, matching the
//! same split `lh-daemon` and `lh-web-backend` already use for exactly this
//! reason (in-process testability, not just code organization).

pub mod app;
pub mod client;
pub mod dispatch;
pub mod input;
pub mod ui;
