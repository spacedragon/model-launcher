//! M0 risk spikes (S1). This crate is disposable.
//!
//! - [`fake_upstream`]: hyper-based fake SSE upstream used by spike tests.
//! - [`forwarder`]: Spike A — transparent SSE forwarding (client disconnect
//!   must cancel the upstream task and release the upstream connection).

pub mod fake_upstream;
pub mod forwarder;
pub mod port_race;
pub mod subprocess;
