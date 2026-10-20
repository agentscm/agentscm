//! Finite model review observations. These records grant no admission authority.
#[cfg(unix)]
mod claude_code;
mod contracts;
mod controller;
mod findings;
mod http;
mod wire;

#[cfg(unix)]
pub use claude_code::{ClaudeCodeTransport, observed_models};
pub use contracts::*;
pub use controller::ReviewController;
pub(crate) use controller::validate_replayed_record;
pub use findings::*;
pub use http::{LocalHttpConfig, LocalHttpTransport};

/// Accepts only aliases with an explicit observed-family rule or a full Claude
/// identifier that must match the provider-reported identity exactly.
pub(crate) fn validate_requested_model_selector(requested: &str) -> crate::Result<()> {
    contracts::validate_identifier(requested)?;
    if matches!(requested, "sonnet" | "haiku" | "opus" | "fable")
        || requested
            .strip_prefix("claude-")
            .is_some_and(|suffix| !suffix.is_empty())
    {
        Ok(())
    } else {
        Err(crate::Error::Invalid(
            "Claude review model must be sonnet, haiku, opus, fable, or a full claude-* identifier"
                .into(),
        ))
    }
}
