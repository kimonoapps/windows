//! Kimono's API as the desktop app uses it: signing in, Drive, resumable uploads,
//! and uploading whole folder trees fast. No UI and nothing platform-specific here.

pub mod bulk;
pub mod client;
pub mod format;
pub mod models;
pub mod problem;
pub mod scan;
pub mod secrets;
mod signin;
pub mod tus;

pub use client::Client;
pub use models::*;
pub use problem::Problem;
pub use secrets::{MemoryStore, SecretStore};

#[cfg(feature = "fake")]
pub mod fake;
