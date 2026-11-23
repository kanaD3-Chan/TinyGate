//! Product mainline: services assembled around the event bus.
//!
//! Layering rule (2026-11-12, reaffirmed): **C components own all hardware
//! I/O — Rust is business logic only.** Drivers in `drivers/`, `radio/`,
//! `platform/` are thin FFI facades over `tg_*` C functions; nothing in
//! `gateway/` touches a hardware API directly. Identity and command storage
//! use the shared ESP-IDF NVS service for persistent business data.
//!
//! Each service owns one C driver (single-task serialization contract),
//! runs as an `embassy_executor` task, and publishes typed observations on
//! the shared [`bus`]. The UI task is the only screen writer.
pub mod access;
pub mod bus;
pub mod command;
pub mod device;
pub mod event;
pub mod identity;
pub mod presence;
