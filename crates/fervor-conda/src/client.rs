use reqwest_middleware::ClientWithMiddleware;

/// HTTP client for conda channels, shared by [`RattlerResolver`](crate::RattlerResolver)
/// and [`RattlerPackageContents`](crate::RattlerPackageContents): rustls plus
/// rattler's authentication middleware.
#[derive(Debug, Clone)]
pub struct CondaClient(ClientWithMiddleware);

/// Failure to set up the conda HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("failed to load conda channel credentials")]
    Credentials(#[source] rattler_networking::AuthenticationStorageError),
    #[error("failed to build the HTTP client")]
    Http(#[source] reqwest::Error),
}

impl CondaClient {
    /// Credentials come from `RATTLER_AUTH_FILE` and the default rattler auth stores.
    pub fn authenticated() -> Result<Self, ClientError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("fervor/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ClientError::Http)?;
        let auth = rattler_networking::AuthenticationMiddleware::from_env_and_defaults().map_err(ClientError::Credentials)?;
        Ok(Self(reqwest_middleware::ClientBuilder::new(client).with(auth).build()))
    }

    pub(crate) fn http(&self) -> &ClientWithMiddleware {
        &self.0
    }
}
