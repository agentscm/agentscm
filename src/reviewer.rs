//! Finite model review observations. These records grant no admission authority.
#[cfg(unix)]
mod claude_code;
mod contracts;
mod controller;
mod findings;
mod http;
mod wire;

#[cfg(unix)]
pub(crate) use claude_code::validate_requested_model_selector;
#[cfg(unix)]
pub use claude_code::{ClaudeCodeTransport, observed_models};
pub use contracts::*;
pub use controller::ReviewController;
pub use findings::*;
pub use http::{LocalHttpConfig, LocalHttpTransport};
