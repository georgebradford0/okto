//! Shared e2e test harness. Included by each test file via `mod common;`.
//!
//! - `mock_llm` — an Anthropic-SSE mock server with scriptable turns.
//! - `lair_proc` — spawns the real lair binary on a temp dir + ephemeral ports.
//! - `tunnel` — Noise transport + HTTP/WS client (the mobile flow).
//!
//! Each integration-test crate that includes this module only uses part of it,
//! so dead-code warnings are expected and silenced here.
#![allow(dead_code, unused_imports)]

pub mod lair_proc;
pub mod mock_llm;
pub mod tunnel;

pub use lair_proc::LairProcess;
pub use mock_llm::Turn;
pub use tunnel::{event_types, ChatWs};
