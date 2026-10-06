//! Errors returned by the resolver and package-contents reader.

use fervor_domain::environment::EnvironmentError;

/// Underlying failure carried by an `Infrastructure` error variant.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("the requirements cannot be satisfied:\n{0}")]
    Unsolvable(String),
    #[error(transparent)]
    Environment(#[from] EnvironmentError),
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl ResolveError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure { context: context.into(), source: source.into() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContentsError {
    #[error("`{package}` needs a python interpreter in the environment (it is noarch: python)")]
    MissingPython { package: String },
    #[error("downloaded archive of `{package}` does not match its sha256")]
    DigestMismatch { package: String },
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl ContentsError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure { context: context.into(), source: source.into() }
    }
}
