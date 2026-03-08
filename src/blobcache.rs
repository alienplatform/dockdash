use crate::error::{Error, Result};
use std::path::PathBuf;
use tracing::{debug, info, warn};

const DEFAULT_CACHE_SUBDIR: &str = ".dockdash/cache/blobs";

/// Manages access to a content-addressable blob cache on disk.
///
/// The cache is used to avoid re-uploading or re-compressing layers that
/// have already been built. By default, the cache lives at
/// `~/.dockdash/cache/blobs`, but you can point it at any directory with
/// [`BlobCache::with_path`].
///
/// # Examples
///
/// ```no_run
/// use dockdash::BlobCache;
///
/// // Use the default location (~/.dockdash/cache/blobs)
/// let cache = BlobCache::new().unwrap();
///
/// // Use a custom directory
/// let cache = BlobCache::with_path("/tmp/my-cache".into()).unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct BlobCache {
    cache_path: PathBuf,
}

impl BlobCache {
    /// Creates a new `BlobCache` using the default cache location
    /// (`~/.dockdash/cache/blobs`, or `/tmp/.dockdash/cache/blobs` in
    /// constrained environments).
    pub fn new() -> Result<Self> {
        let path = if let Some(home) = dirs::home_dir() {
            let home_cache_path = home.join(DEFAULT_CACHE_SUBDIR);
            if let Some(parent) = home_cache_path.parent() {
                match std::fs::create_dir_all(parent) {
                    Ok(_) => home_cache_path,
                    Err(e) => {
                        warn!(
                            "Home directory is not writable ({}), falling back to /tmp",
                            e
                        );
                        PathBuf::from("/tmp").join(DEFAULT_CACHE_SUBDIR)
                    }
                }
            } else {
                PathBuf::from("/tmp").join(DEFAULT_CACHE_SUBDIR)
            }
        } else {
            warn!("Could not determine home directory, using /tmp");
            PathBuf::from("/tmp").join(DEFAULT_CACHE_SUBDIR)
        };

        Self::init(path)
    }

    /// Creates a new `BlobCache` at the given directory path.
    ///
    /// The directory will be created if it does not exist. The path is used
    /// as-is — no subdirectory is appended.
    ///
    /// This is useful when you want full control over the cache location, for
    /// example to share a cache across tools or to place it alongside project
    /// artifacts.
    ///
    /// ```no_run
    /// # use dockdash::BlobCache;
    /// let cache = BlobCache::with_path("/var/cache/my-tool/blobs".into()).unwrap();
    /// ```
    pub fn with_path(path: PathBuf) -> Result<Self> {
        info!(cache_path = %path.display(), "Initializing BlobCache with custom path.");
        Self::init(path)
    }

    fn init(path: PathBuf) -> Result<Self> {
        debug!("Cache directory: {}", path.display());
        if !path.exists() {
            info!("Creating cache directory at: {}", path.display());
            std::fs::create_dir_all(&path).map_err(|e| Error::Io {
                message: format!("Failed to create cache directory: {}", path.display()),
                source: e,
            })?;
        }
        Ok(Self { cache_path: path })
    }

    /// Retrieves a blob from the cache by its digest.
    ///
    /// Returns `Ok(Some(data))` if the blob is found, `Ok(None)` if not.
    pub async fn get_blob(&self, digest: &str) -> Result<Option<Vec<u8>>> {
        debug!(blob_digest = %digest, cache_path = %self.cache_path.display(), "Looking up blob in cache.");
        match cacache::read(&self.cache_path, digest).await {
            Ok(data) => {
                info!(blob_digest = %digest, "Blob found in cache.");
                Ok(Some(data))
            }
            Err(cacache::Error::EntryNotFound(_, _)) => Ok(None),
            Err(e) => {
                warn!(blob_digest = %digest, error = %e, "Failed to read blob from cache.");
                Err(Error::Cache {
                    message: format!(
                        "Failed to read blob '{}' from cache at {}",
                        digest,
                        self.cache_path.display()
                    ),
                    source: Some(Box::new(e)),
                })
            }
        }
    }

    /// Stores a blob in the cache.
    pub async fn put_blob(&self, digest: &str, data: &[u8]) -> Result<()> {
        debug!(blob_digest = %digest, size = data.len(), "Storing blob in cache.");
        cacache::write(&self.cache_path, digest, data)
            .await
            .map_err(|e| {
                warn!(blob_digest = %digest, error = %e, "Failed to write blob to cache.");
                Error::Cache {
                    message: format!(
                        "Failed to write blob '{}' to cache at {}",
                        digest,
                        self.cache_path.display()
                    ),
                    source: Some(Box::new(e)),
                }
            })?;
        Ok(())
    }

    /// Removes a blob from the cache.
    ///
    /// Returns `Ok(())` even if the blob was not found.
    pub async fn remove_blob(&self, digest: &str) -> Result<()> {
        debug!(blob_digest = %digest, "Removing blob from cache.");
        match cacache::remove(&self.cache_path, digest).await {
            Ok(_) => Ok(()),
            Err(cacache::Error::EntryNotFound(_, _)) => Ok(()),
            Err(e) => {
                warn!(blob_digest = %digest, error = %e, "Failed to remove blob from cache.");
                Err(Error::Cache {
                    message: format!(
                        "Failed to remove blob '{}' from cache at {}",
                        digest,
                        self.cache_path.display()
                    ),
                    source: Some(Box::new(e)),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_put_get_blob() -> Result<()> {
        let temp_dir = tempdir().map_err(|e| Error::Io {
            message: "Failed to create temp dir".to_string(),
            source: e,
        })?;
        let cache = BlobCache::with_path(temp_dir.path().to_path_buf())?;

        let digest = "sha256:testdigest123";
        let data = b"hello cache data";

        cache.put_blob(digest, data).await?;

        let retrieved_data = cache.get_blob(digest).await?.expect("Blob should be found");
        assert_eq!(retrieved_data, data);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_non_existent_blob() -> Result<()> {
        let temp_dir = tempdir().map_err(|e| Error::Io {
            message: "Failed to create temp dir".to_string(),
            source: e,
        })?;
        let cache = BlobCache::with_path(temp_dir.path().to_path_buf())?;

        let digest = "sha256:nonexistent123";
        let retrieved_data = cache.get_blob(digest).await?;
        assert!(retrieved_data.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_put_remove_get_blob() -> Result<()> {
        let temp_dir = tempdir().map_err(|e| Error::Io {
            message: "Failed to create temp dir".to_string(),
            source: e,
        })?;
        let cache = BlobCache::with_path(temp_dir.path().to_path_buf())?;

        let digest = "sha256:toberemoved456";
        let data = b"this will be removed";

        cache.put_blob(digest, data).await?;
        let retrieved_data_before_remove = cache
            .get_blob(digest)
            .await?
            .expect("Blob should be found before remove");
        assert_eq!(retrieved_data_before_remove, data);

        cache.remove_blob(digest).await?;
        let retrieved_data_after_remove = cache.get_blob(digest).await?;
        assert!(retrieved_data_after_remove.is_none());

        // Removing again should be fine
        cache.remove_blob(digest).await?;

        Ok(())
    }
}
