//! Product mainline: services assembled around the event bus.
//!
//! C components own hardware I/O. Rust accesses hardware through the
//! `drivers/` facades. Identity and command storage use the shared ESP-IDF
//! NVS service for persistent business data.
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
