//! rustglance: a browser tool for AI agents.
//!
//! Chromium is driven over the DevTools protocol and the tools are served as
//! MCP over stdio. Extensions add their own tools by passing extra [`OpDef`]s
//! to [`cli::main_with`].

pub mod browser;
pub mod cdp;
pub mod cli;
pub mod config;
pub mod ctx;
pub mod journal;
pub mod js;
pub mod mcp;
pub mod ops;
mod ready;
pub mod refusal;
pub mod replay;
pub mod report;
pub mod script;
pub mod util;
pub mod view;

pub use ctx::Ctx;
pub use mcp::{OpDef, OpFuture, Output};
pub use refusal::{Class, Refusal};
