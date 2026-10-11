//! Builder application services and adapters. See ARCHITECTURE.md for dependency rules.
pub mod agent;
pub mod code_index;
pub mod completion;
pub mod doctor;
pub mod embedding;
pub mod input;
pub mod scheduler;
pub mod ui;

pub mod memory;
mod presentation;

pub mod remote;
// Compatibility facade for the outbound remote adapter.
pub use remote::connection as remote_connect;
pub mod research;
pub mod retrieval_eval;
// Compatibility facade; delegated sessions are owned by the agent.
pub use agent::subagent;
