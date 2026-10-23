//! Kairo's platform-independent core: data model, recurrence, server API, sync, `.ics`.
//!
//! The desktop app (GPUI) and the mobile app both build on this crate. Where the platforms
//! differ (config/data folders, secret storage) the code goes through [`platform`].

pub mod api;
pub mod ics;
pub mod import;
pub mod live;
pub mod members;
pub mod model;
pub mod platform;
pub mod sync;
