// The mark, in the docs.rs sidebar and its tab. Absolute URLs because rustdoc
// does not copy local files into the generated output — docs.rs would serve a
// dead link to a path that only exists in this checkout.
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/icon.svg",
    html_favicon_url = "https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/icon.svg"
)]
// The README is the crate's front page on docs.rs as well as on crates.io: this
// is an application, so what a reader needs first is what zoetrope does, not the
// module list. Its images use absolute URLs for the same reason the logo above
// does. What follows is the part that only concerns someone depending on the
// library rather than running the binary.
#![doc = include_str!("../README.md")]
//!
//! ## The library
//!
//! The portable core behind both the native frontend
//! (the `zoe` terminal app, `src/main.rs`) and the browser frontend (the
//! `zoetrope-web` crate at `web/wasm/`, which depends on this one with default
//! features off).
//!
//! Portable everywhere: the domain [`state`] (model + unified replay/live
//! [`timeline`](state::timeline) + flow-graph projection), the [`ui`] rendering,
//! and [`transcript`] parsing. The wire types and pure replay assembly live in
//! [`tailer`]; its live file-tailing + the terminal loop ([`tui`]) and input
//! ([`handler`]) are native-only (they pull tokio/crossterm/fs) and `cfg`-gated
//! behind the `native` feature.

pub mod i18n;
pub mod state;
pub mod tailer;
pub mod transcript;
pub mod ui;

// The native frontend: terminal loop + crossterm input.
#[cfg(feature = "native")]
pub mod autopilot;
#[cfg(feature = "native")]
pub mod handler;
#[cfg(feature = "native")]
pub mod tui;
