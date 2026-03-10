use thiserror::Error;

/// Represents application-specific errors.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A fallback error type when no variant matches
    #[error("Generic error: {message}{}", source.as_ref().map(|e| format!(": {e}")).unwrap_or_default())]
    Generic {
        /// The error message
        message: String,
        /// The wrapped error if available.
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("I/O error: {message}: {source}")]
    Io {
        message: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Task join error: {message}: {source}")]
    Join {
        message: String,
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("Image pull error for '{image_ref}': {message}{}", source.as_ref().map(|e| format!(": {}", e)).unwrap_or_default())]
    ImagePull {
        image_ref: String,
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Image configuration error: {message}{}", source.as_ref().map(|e| format!(": {}", e)).unwrap_or_default())]
    ImageConfig {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("OCI archive build error: {message}{}", source.as_ref().map(|e| format!(": {}", e)).unwrap_or_default())]
    OciArchive {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Cache error: {message}{}", source.as_ref().map(|e| format!(": {}", e)).unwrap_or_default())]
    Cache {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Invalid path specified: {message}")]
    InvalidPath { message: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io {
            message: "An I/O error occurred".to_string(),
            source: err,
        }
    }
}

impl Error {
    /// Checks if this error is due to a manifest not being found in the registry.
    /// This is used to determine if we should retry with a fallback base image.
    pub fn is_manifest_not_found(&self) -> bool {
        match self {
            Error::ImagePull { source, .. } => {
                if let Some(source) = source {
                    // Try to downcast to OciDistributionError
                    if let Some(oci_err) =
                        source.downcast_ref::<oci_client::errors::OciDistributionError>()
                    {
                        return is_oci_manifest_not_found(oci_err);
                    }
                }
                false
            }
            _ => false,
        }
    }
}

/// Helper function to check if an OciDistributionError indicates a manifest not found
fn is_oci_manifest_not_found(error: &oci_client::errors::OciDistributionError) -> bool {
    match error {
        oci_client::errors::OciDistributionError::RegistryError { envelope, .. } => {
            // Check if any of the errors in the envelope is ManifestUnknown
            envelope
                .errors
                .iter()
                .any(|e| matches!(e.code, oci_client::errors::OciErrorCode::ManifestUnknown))
        }
        oci_client::errors::OciDistributionError::ImageManifestNotFoundError(_) => true,
        _ => false,
    }
}
