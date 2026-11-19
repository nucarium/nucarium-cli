//! The code behind both binaries: `nucleate`, the workspace CLI, and
//! `nucleated`, the box daemon. They share the workspace model, the event
//! envelope and the Watch credential.

pub mod agent;
pub mod config;
pub mod credentials;
pub mod daemon;
pub mod envelope;
pub mod git;
pub mod github;
pub mod health;
pub mod hook;
pub mod housekeeping;
pub mod login;
pub mod mcp;
pub mod notifier;
pub mod paths;
pub mod pause;
pub mod reporter;
pub mod sessions;
pub mod settings;
pub mod spool;
pub mod status;
pub mod watch;
pub mod workspace;
