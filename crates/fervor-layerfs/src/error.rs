/// Underlying failure carried in an error's `source`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum HostTreeError {
    #[error("`{0}` is not a directory")]
    NotADirectory(String),
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl HostTreeError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure {
            context: context.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{context}")]
pub struct BuildError {
    pub context: String,
    #[source]
    pub source: BoxError,
}

impl BuildError {
    pub fn new(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self {
            context: context.into(),
            source: source.into(),
        }
    }
}
