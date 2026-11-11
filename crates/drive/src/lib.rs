//! Kimono Drive as a folder on this computer: on Windows a Cloud Files sync root (files appear
//! at once, download when opened, and stay if pinned), on Linux a FUSE mount.

pub mod engine;
pub mod mirror;

#[cfg(windows)]
pub mod windows;

pub use engine::{Backend, Engine, Status};
