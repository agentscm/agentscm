//! Repository-scoped team authority, authenticated application operations and transport.
pub mod auth;
pub mod http;
pub mod publication;
pub mod service;
pub mod source;

use std::fmt;

#[derive(Debug)]
pub enum TeamError {
    Storage(crate::Error),
    Database(postgres::Error),
    Conflict(String),
    Expired,
    Indeterminate,
    Unauthorized,
    Denied,
    Unavailable,
}

pub type Result<T> = std::result::Result<T, TeamError>;

impl fmt::Display for TeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => e.fmt(f),
            Self::Database(e) => e.fmt(f),
            Self::Conflict(s) => write!(f, "conflict: {s}"),
            Self::Expired => write!(f, "publication lease expired"),
            Self::Indeterminate => write!(f, "outcome not retained for rejected generation"),
            Self::Unauthorized => write!(f, "authentication required"),
            Self::Denied => write!(f, "permission denied"),
            Self::Unavailable => write!(f, "required capability unavailable"),
        }
    }
}
impl std::error::Error for TeamError {}
impl From<crate::Error> for TeamError {
    fn from(e: crate::Error) -> Self {
        Self::Storage(e)
    }
}
impl From<postgres::Error> for TeamError {
    fn from(e: postgres::Error) -> Self {
        Self::Database(e)
    }
}
