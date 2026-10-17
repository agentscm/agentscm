use std::fmt;
#[derive(Debug)]
pub enum Error {
    Invalid(String),
    UnsupportedGitVersion {
        minimum: &'static str,
        found: Option<String>,
    },
    NotFound(String),
    MissingObject(String),
    SourceDenied,
    SourceIndeterminate,
    Conflict(String),
    Limit(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Sql(rusqlite::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid: {s}"),
            Self::UnsupportedGitVersion { minimum, found } => match found {
                Some(found) => write!(
                    f,
                    "unsupported Git version: requires Git {minimum} or newer (found {found})"
                ),
                None => write!(
                    f,
                    "unsupported Git version: requires Git {minimum} or newer (version output was not recognized)"
                ),
            },
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::MissingObject(id) => write!(f, "not found: object: {id}"),
            Self::SourceDenied => write!(f, "source read denied"),
            Self::SourceIndeterminate => write!(f, "source read indeterminate"),
            Self::Conflict(s) => write!(f, "conflict: {s}"),
            Self::Limit(s) => write!(f, "limit: {s}"),
            Self::Io(e) => e.fmt(f),
            Self::Json(e) => e.fmt(f),
            Self::Sql(e) => e.fmt(f),
        }
    }
}
impl Error {
    /// Typed callback failures and the controlled legacy local-store format.
    /// Provider messages must be normalized at the reader boundary first.
    pub fn missing_object_id(&self) -> Option<&str> {
        match self {
            Self::MissingObject(id) => Some(id),
            Self::NotFound(message) => message.strip_prefix("object: "),
            _ => None,
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
