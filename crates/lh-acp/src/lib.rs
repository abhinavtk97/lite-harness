//! ACP orchestration module (architecture §5, §11 phase 4): connects the
//! daemon to an external ACP-speaking coding agent subprocess (Claude Code,
//! via the `claude-agent-acp` adapter) as a delegated child session, with
//! that agent's tool calls and permission requests flowing through the
//! exact same `PermissionEngine`/`ExecutionPlane` native tool calls use --
//! no parallel trust boundary.

pub mod client;
pub mod delegate;
pub mod registry;
pub mod transport;

pub use client::HarnessAcpClient;
pub use registry::{AgentsFile, DelegatedAgentAdapter, SpawnSpec};
pub use transport::{AcpConnection, AcpTransportError};
