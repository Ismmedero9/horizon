//! A Simple Voice Chat client for Horizon.
pub mod config;
pub mod groups;
pub mod connection;
pub mod engine;
pub mod mixer;
pub mod resample;
pub mod net;
pub mod opus;
pub mod secret;
pub mod service;
pub mod wire;

#[cfg(feature = "audio")]
mod audio;
