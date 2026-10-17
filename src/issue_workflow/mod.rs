//! Bounded, provider-neutral issue intake over already collected observations.
//! This module performs no provider I/O and grants no write authority.

mod claims;
mod contracts;
mod intake;

pub use claims::*;
pub use contracts::*;
pub use intake::evaluate;
