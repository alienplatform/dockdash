use crate::blobcache;
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::File,
    io::{self},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use tokio::task;
use tracing::{debug, info, instrument, warn};

use crate::IMAGE_LAYER_ZSTD_MEDIA_TYPE;

/// Represents a single layer in an OCI image.
///
/// A layer consists of a zstd-compressed tarball of files and a corresponding diff_id.
/// The temporary file holding the compressed tarball is automatically deleted when
/// the `Layer` instance is dropped.
pub struct Layer {
    diff_id: String,     // Digest of the uncompressed tar (for image config diffIds)
    blob_digest: String, // Digest of the compressed tar (for OCI blob storage/cache key)
    media_type: String,  // Media type of the blob, typically IMAGE_LAYER_ZSTD_MEDIA_TYPE
    compressed_layer_file: NamedTempFile, // The temporary file for this layer instance
}

impl Layer {
    /// Creates a new `LayerBuilder` to construct a layer.
    pub fn builder() -> Result<LayerBuilder> {
        LayerBuilder::new()
    }

    /// Returns the diff_id of the layer (e.g., "sha256:deadbeef..."), hash of uncompressed tar.
    pub fn diff_id(&self) -> &str {
        &self.diff_id
    }

    /// Returns the blob_digest of the layer (e.g., "sha256:abc123..."), hash of compressed tar.
    pub fn blob_digest(&self) -> &str {
        &self.blob_digest
    }

    /// Returns the media type of the layer's blob.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the path to the temporary file containing the compressed tarball for this layer.
    pub fn path(&self) -> &Path {
        self.compressed_layer_file.path()
    }
}

/// Metadata about a file added to the layer, used for input-based cache key calculation.
#[derive(Debug, Clone)]
struct FileMetadata {
    /// Path in the archive
    archive_path: PathBuf,
    /// Size of the file
    size: u64,
    /// Modification time (seconds since epoch)
    mtime: u64,
    /// File mode (permissions)
    mode: u32,
    /// Whether this is a directory
    is_dir: bool,
    /// Optional content hash for in-memory data (where mtime is meaningless)
    content_hash: Option<[u8; 32]>,
}

/// Builder for creating `Layer` instances.
///
/// Allows for incrementally adding content (directories, files, in-memory data)
/// to an uncompressed tar archive stored in a temporary file. The `build` method
/// finalizes this tar archive, computes its diff_id, zstd-compresses it into another
/// temporary file, and returns a `Layer`.
pub struct LayerBuilder {
    /// Kept alive so the temp file isn't deleted while the tar builder writes to it.
    _uncompressed_tar_tmpfile: NamedTempFile,
    tar_writer: tar::Builder<File>,
    uncompressed_tar_path: PathBuf,
    blob_cache: Option<blobcache::BlobCache>,
    created_archive_dirs: HashSet<PathBuf>,
    /// Metadata about files added to this layer, used for input-based cache key.
    file_metadata: Vec<FileMetadata>,
}

impl LayerBuilder {
    #[instrument(level = "info", skip_all, fields(uncompressed_tar_path))]
    fn new() -> Result<Self> {
        info!("Creating new LayerBuilder.");
        let uncompressed_tar_tmpfile = NamedTempFile::new().map_err(|e| {
            warn!(error = %e, "Failed to create temporary file for uncompressed tar.");
            Error::Io {
                source: e,
                message: "Failed to create temporary file for uncompressed tar".to_string(),
            }
        })?;
        let file_for_builder = uncompressed_tar_tmpfile.reopen().map_err(|e| {
            warn!(error = %e, "Failed to reopen temporary file for tar builder.");
            Error::Io {
                source: e,
                message: "Failed to reopen temporary file for tar builder".to_string(),
            }
        })?;
        let mut tar_writer = tar::Builder::new(file_for_builder);
        tar_writer.follow_symlinks(false);
        let uncompressed_tar_path = uncompressed_tar_tmpfile.path().to_path_buf();
        tracing::Span::current().record(
            "uncompressed_tar_path",
            uncompressed_tar_path.display().to_string(),
        );
        debug!(path = %uncompressed_tar_path.display(), "LayerBuilder initialized with temporary tar file.");
        Ok(Self {
            _uncompressed_tar_tmpfile: uncompressed_tar_tmpfile,
            tar_writer,
            uncompressed_tar_path,
            blob_cache: None,
            created_archive_dirs: HashSet::new(),
            file_metadata: Vec::new(),
        })
    }

    /// Sets a specific BlobCache instance to be used by the LayerBuilder.
    /// If not called, a default BlobCache will be created when `build()` is invoked.
    pub fn blob_cache(mut self, cache: blobcache::BlobCache) -> Self {
        self.blob_cache = Some(cache);
        self
    }

    /// Calculate a cache key from input file metadata.
    /// This is much faster than hashing the tar content because it only hashes
    /// the metadata (paths, sizes, mtimes, modes) rather than file contents.
    /// The key is deterministic: same input files → same tar → same key.
    fn calculate_input_key(&self) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();

        // Sort metadata by archive path for deterministic ordering
        let mut sorted_metadata = self.file_metadata.clone();
        sorted_metadata.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));

        for meta in &sorted_metadata {
            // Hash: path | size | mtime | mode | is_dir | content_hash
            hasher.update(meta.archive_path.to_string_lossy().as_bytes());
            hasher.update(meta.size.to_le_bytes());
            hasher.update(meta.mtime.to_le_bytes());
            hasher.update(meta.mode.to_le_bytes());
            hasher.update([meta.is_dir as u8]);
            if let Some(ref hash) = meta.content_hash {
                hasher.update(hash);
            }
        }

        format!("layer-input-{:x}", hasher.finalize())
    }

    /// Calculate a content-based cache key from a finalized tar file path.
    /// This is a fallback when input-based caching doesn't produce a hit.
    fn calculate_content_key(tar_path: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();

        // Hash all entries in the tar in order
        let mut file = File::open(tar_path).map_err(|e| Error::Io {
            source: e,
            message: format!(
                "Failed to open tar file for content key calculation: {}",
                tar_path.display()
            ),
        })?;

        std::io::copy(&mut file, &mut hasher).map_err(|e| Error::Io {
            source: e,
            message: "Failed to read tar file for content key calculation".to_string(),
        })?;

        Ok(format!("layer-content-{:x}", hasher.finalize()))
    }

    /// Adds an entire directory (recursively) from the local filesystem to the layer.
    ///
    /// - `disk_path`: Path to the source directory on the local filesystem.
    /// - `archive_path`: Path under which the directory contents will be placed within the layer's tar archive.
    #[instrument(level = "info", skip(self), fields(disk_path = %disk_path.as_ref().display(), archive_path = %archive_path.as_ref().display()))]
    pub fn directory(
        mut self,
        disk_path: impl AsRef<Path>,
        archive_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let dp_ref = disk_path.as_ref();
        let ap_ref = archive_path.as_ref();
        info!(
            "Adding directory to layer. Original archive path: {}",
            ap_ref.display()
        );

        let normalized_ap = normalize_archive_path(ap_ref);
        debug!(
            "Normalized archive path for directory: {}",
            normalized_ap.display()
        );

        self.tar_writer
            .append_dir_all(&normalized_ap, dp_ref)
            .map_err(|e| {
                warn!(error = %e, "Failed to append directory to tar.");
                Error::Io {
                    source: e,
                    message: format!(
                        "Failed to append directory {} (as {}) to tar",
                        dp_ref.display(),
                        normalized_ap.display()
                    ),
                }
            })?;

        // Walk the source directory and record metadata for every file,
        // so the input-based cache key captures actual directory contents.
        for entry in walkdir::WalkDir::new(dp_ref).sort_by_file_name() {
            let entry = entry.map_err(|e| Error::Io {
                source: e.into(),
                message: format!(
                    "Failed to walk directory {} for cache key metadata",
                    dp_ref.display()
                ),
            })?;
            let entry_metadata = entry.metadata().map_err(|e| Error::Io {
                source: e.into(),
                message: format!("Failed to get metadata for {}", entry.path().display()),
            })?;
            let relative = entry.path().strip_prefix(dp_ref).unwrap_or(entry.path());
            let archive_entry_path = normalized_ap.join(relative);

            let mtime = entry_metadata
                .modified()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                })
                .unwrap_or(0);

            let mode = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    entry_metadata.permissions().mode()
                }
                #[cfg(not(unix))]
                {
                    if entry_metadata.is_dir() {
                        0o755
                    } else {
                        0o644
                    }
                }
            };

            self.file_metadata.push(FileMetadata {
                archive_path: archive_entry_path,
                size: entry_metadata.len(),
                mtime,
                mode,
                is_dir: entry_metadata.is_dir(),
                content_hash: None,
            });
        }

        debug!("Successfully added directory to layer.");
        Ok(self)
    }

    /// Adds a single file from the local filesystem to the layer.
    ///
    /// - `disk_path`: Path to the source file on the local filesystem.
    /// - `archive_path`: Path where the file will be placed within the layer's tar archive.
    ///   If absolute (e.g. "/foo/bar.txt"), it will be treated as relative to the layer root (e.g. "foo/bar.txt").
    ///   An archive_path of "/" is invalid for a file.
    /// - `mode`: Optional. The file mode (permissions) to set. If `None`, uses the source file's mode.
    #[instrument(level = "info", skip(self), fields(disk_path = %disk_path.as_ref().display(), archive_path = %archive_path.as_ref().display(), mode))]
    pub fn file(
        mut self,
        disk_path: impl AsRef<Path>,
        archive_path: impl AsRef<Path>,
        mode: Option<u32>,
    ) -> Result<Self> {
        let dp_ref = disk_path.as_ref();
        let ap_ref = archive_path.as_ref();
        info!(
            "Adding file to layer. Original archive path: {}",
            ap_ref.display()
        );

        let normalized_ap = normalize_archive_path(ap_ref);
        debug!(
            "Normalized archive path for file: {}",
            normalized_ap.display()
        );

        if normalized_ap.as_os_str().is_empty() {
            warn!("Attempted to add file with an empty archive path (derived from root '/')");
            return Err(Error::InvalidPath {
                message: format!(
                    "Archive path for file '{}' cannot be the root directory ('/') or empty.",
                    dp_ref.display()
                ),
            });
        }

        self.ensure_parent_dirs_exist(&normalized_ap)?;

        let mut f = File::open(dp_ref).map_err(|e| Error::Io {
            source: e,
            message: format!("Failed to open file {} for tar", dp_ref.display()),
        })?;

        // Get file metadata for input-based cache key
        let file_metadata = f.metadata().map_err(|e| Error::Io {
            source: e,
            message: format!("Failed to get metadata for file {}", dp_ref.display()),
        })?;

        let file_mode = mode.unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file_metadata.permissions().mode()
            }
            #[cfg(not(unix))]
            {
                0o644
            }
        });

        // Track file metadata for input-based cache key calculation
        self.file_metadata.push(FileMetadata {
            archive_path: normalized_ap.clone(),
            size: file_metadata.len(),
            mtime: file_metadata
                .modified()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                })
                .unwrap_or(0),
            mode: file_mode,
            is_dir: false,
            content_hash: None,
        });

        if let Some(new_mode) = mode {
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&file_metadata); // Copies size, perms, mtime, etc.
            header.set_mode(new_mode); // Override mode
            header.set_mtime(0); // Consistent mtime for reproducibility if desired
            header.set_uid(0);
            header.set_gid(0);

            self.tar_writer
                .append_data(&mut header, &normalized_ap, &mut f)
                .map_err(|e| Error::Io {
                    source: e,
                    message: format!(
                        "Failed to append file {} (as {}) with custom mode to tar",
                        dp_ref.display(),
                        normalized_ap.display()
                    ),
                })?;
        } else {
            self.tar_writer
                .append_file(&normalized_ap, &mut f)
                .map_err(|e| Error::Io {
                    source: e,
                    message: format!(
                        "Failed to append file {} (as {}) to tar",
                        dp_ref.display(),
                        normalized_ap.display()
                    ),
                })?;
        }
        Ok(self)
    }

    /// Adds data from an in-memory byte slice to the layer as a file.
    ///
    /// - `archive_path`: Path where the data will be stored within the layer's tar archive.
    ///   If absolute (e.g. "/foo/bar.txt"), it will be treated as relative to the layer root (e.g. "foo/bar.txt").
    ///   An archive_path of "/" is invalid for data.
    /// - `content`: The byte slice containing the file content.
    /// - `mode`: The file mode (permissions) to set for the data in the tar archive. Defaults to `0o644` if `None`.
    #[instrument(level = "info", skip(self, content), fields(archive_path = %archive_path.as_ref().display(), content_len = content.len(), mode))]
    pub fn data(
        mut self,
        archive_path: impl AsRef<Path>,
        content: &[u8],
        mode: Option<u32>,
    ) -> Result<Self> {
        let ap_ref = archive_path.as_ref();
        info!(
            "Adding data to layer. Original archive path: {}",
            ap_ref.display()
        );

        let normalized_ap = normalize_archive_path(ap_ref);
        debug!(
            "Normalized archive path for data: {}",
            normalized_ap.display()
        );

        if normalized_ap.as_os_str().is_empty() {
            warn!("Attempted to add data with an empty archive path (derived from root '/')");
            return Err(Error::InvalidPath {
                message: "Archive path for data cannot be the root directory ('/') or empty."
                    .to_string(),
            });
        }

        self.ensure_parent_dirs_exist(&normalized_ap)?;

        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(mode.unwrap_or(0o644));
        // `append_data` sets the path in the header and calculates checksum.
        self.tar_writer
            .append_data(&mut header, &normalized_ap, content)
            .map_err(|e| {
                warn!(error = %e, "Failed to append data to tar.");
                Error::Io {
                    source: e,
                    message: format!(
                        "Failed to append data as {} to tar",
                        normalized_ap.display()
                    ),
                }
            })?;

        // Hash in-memory content so the input cache key distinguishes
        // different data at the same path/size (mtime is always 0 here).
        let content_hash: [u8; 32] = {
            use sha2::Digest;
            Sha256::digest(content).into()
        };

        self.file_metadata.push(FileMetadata {
            archive_path: normalized_ap,
            size: content.len() as u64,
            mtime: 0,
            mode: mode.unwrap_or(0o644),
            is_dir: false,
            content_hash: Some(content_hash),
        });

        debug!("Successfully added data to layer.");
        Ok(self)
    }

    /// Finalizes the layer construction.
    ///
    /// This method completes the uncompressed tar archive, calculates its diff_id (SHA256 hash),
    /// zstd-compresses the archive into a new temporary file, and returns the resulting `Layer`.
    /// This is an asynchronous operation due to potentially blocking I/O for hashing and compression.
    #[instrument(level = "info", skip_all, fields(uncompressed_tar_path = %self.uncompressed_tar_path.display()))]
    pub async fn build(mut self) -> Result<Layer> {
        info!("Building layer.");

        // Get or create cache
        let cache = match self.blob_cache.take() {
            Some(c) => c,
            None => blobcache::BlobCache::new()?,
        };

        // Try input-based cache key FIRST (before finalizing tar)
        // This is much faster than hashing tar content for large binaries
        let input_key = self.calculate_input_key();
        debug!(input_key = %input_key, "Calculated layer input key from file metadata");
        if let Some(cached_metadata) = cache.get_blob(&input_key).await? {
            if let Ok(metadata_str) = std::str::from_utf8(&cached_metadata) {
                if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata_str) {
                    if let (Some(diff_id), Some(blob_digest)) = (
                        metadata.get("diff_id").and_then(|v| v.as_str()),
                        metadata.get("blob_digest").and_then(|v| v.as_str()),
                    ) {
                        if let Some(compressed_data) = cache.get_blob(blob_digest).await? {
                            info!(
                                input_key = %input_key,
                                blob_digest = %blob_digest,
                                "Found layer in cache via input key, skipping tar finalization"
                            );

                            let compressed_layer_file =
                                NamedTempFile::new().map_err(|e| Error::Io {
                                    source: e,
                                    message: "Failed to create temporary file for cached layer"
                                        .to_string(),
                                })?;

                            tokio::fs::write(compressed_layer_file.path(), &compressed_data)
                                .await
                                .map_err(|e| Error::Io {
                                    source: e,
                                    message: "Failed to write cached layer to temp file"
                                        .to_string(),
                                })?;

                            return Ok(Layer {
                                diff_id: diff_id.to_string(),
                                blob_digest: blob_digest.to_string(),
                                media_type: IMAGE_LAYER_ZSTD_MEDIA_TYPE.to_string(),
                                compressed_layer_file,
                            });
                        }
                    }
                }
            }
        }

        // Input-based cache miss - finalize the tar and try content-based cache
        debug!("Input-based cache miss, finalizing tar");

        // Finalize the tar writer, ensuring all data is flushed and the file is closed.
        let uncompressed_tar_file_writer = self.tar_writer.into_inner().map_err(|e| {
            warn!(error = %e, "Failed to get underlying writer from tar builder.");
            Error::Io {
                source: e,
                message: "Failed to get underlying writer from tar builder".to_string(),
            }
        })?;

        uncompressed_tar_file_writer.sync_all().map_err(|e| {
            warn!(error = %e, "Failed to sync uncompressed tar file to disk.");
            Error::Io {
                source: e,
                message: "Failed to sync uncompressed tar file to disk".to_string(),
            }
        })?;
        drop(uncompressed_tar_file_writer); // Explicitly drop to release file handle before hashing/gzipping
        debug!("Uncompressed tar finalized and synced.");

        // Calculate content-based cache key from the uncompressed tar
        let content_key = Self::calculate_content_key(&self.uncompressed_tar_path)?;
        debug!(content_key = %content_key, "Calculated layer content key");

        // Check if we have a cached blob for this content
        if let Some(cached_metadata) = cache.get_blob(&content_key).await? {
            // Parse the cached metadata (it's a JSON string with diff_id and blob_digest)
            if let Ok(metadata_str) = std::str::from_utf8(&cached_metadata) {
                if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata_str) {
                    if let (Some(diff_id), Some(blob_digest)) = (
                        metadata.get("diff_id").and_then(|v| v.as_str()),
                        metadata.get("blob_digest").and_then(|v| v.as_str()),
                    ) {
                        // Try to get the actual compressed blob
                        if let Some(compressed_data) = cache.get_blob(blob_digest).await? {
                            info!(
                                content_key = %content_key,
                                blob_digest = %blob_digest,
                                "Found layer in cache, skipping compression"
                            );

                            // Write the cached data to a temp file
                            let compressed_layer_file =
                                NamedTempFile::new().map_err(|e| Error::Io {
                                    source: e,
                                    message: "Failed to create temporary file for cached layer"
                                        .to_string(),
                                })?;

                            tokio::fs::write(compressed_layer_file.path(), &compressed_data)
                                .await
                                .map_err(|e| Error::Io {
                                    source: e,
                                    message: "Failed to write cached layer to temp file"
                                        .to_string(),
                                })?;

                            return Ok(Layer {
                                diff_id: diff_id.to_string(),
                                blob_digest: blob_digest.to_string(),
                                media_type: IMAGE_LAYER_ZSTD_MEDIA_TYPE.to_string(),
                                compressed_layer_file,
                            });
                        }
                    }
                }
            }
        }

        info!("Layer not in cache, building from scratch");

        // Calculate diff_id (SHA256 of the uncompressed tar file)
        let path_for_hashing = self.uncompressed_tar_path.clone();
        info!(path = %path_for_hashing.display(), "Calculating diff_id (SHA256 of uncompressed tar).");
        let diff_id_hex = task::spawn_blocking(move || -> Result<String> {
            let mut file = File::open(&path_for_hashing).map_err(|e| {
                warn!(path = %path_for_hashing.display(), error = %e, "Hashing: Failed to open uncompressed tar.");
                Error::Io {
                    source: e,
                    message: format!(
                        "Hashing: Failed to open uncompressed tar {}",
                        path_for_hashing.display()
                    ),
                }
            })?;
            let mut hasher = Sha256::new();
            io::copy(&mut file, &mut hasher).map_err(|e| {
                warn!(path = %path_for_hashing.display(), error = %e, "Hashing: Failed to read uncompressed tar.");
                Error::Io {
                    source: e,
                    message: format!(
                        "Hashing: Failed to read uncompressed tar {}",
                        path_for_hashing.display()
                    ),
                }
            })?;
            let hash_bytes = hasher.finalize();
            Result::Ok(format!("{:x}", hash_bytes)) // Ensure this Ok is crate::error::Result
        })
        .await
        .map_err(|e| {
            warn!(error = %e, "Task join error during hashing.");
            Error::Join { // JoinError
                source: e,
                message: "Task join error during hashing".to_string(),
            }
        })??; // Outer Result for JoinError, inner Result for hashing logic
        info!(diff_id = %format!("sha256:{}", diff_id_hex), "Calculated diff_id.");

        // Compress the uncompressed tar with zstd into a new temporary file
        let compressed_layer_file = NamedTempFile::new().map_err(|e| {
            warn!(error = %e, "Failed to create temporary file for compressed layer.");
            Error::Io {
                source: e,
                message: "Failed to create temporary file for compressed layer".to_string(),
            }
        })?;
        debug!(compressed_layer_path = %compressed_layer_file.path().display(), "Created temporary file for compressed layer.");

        let compressed_writer_file_reopened = compressed_layer_file.reopen().map_err(|e| {
            warn!(error = %e, "Compression: Failed to reopen temporary file for zstd writer.");
            Error::Io {
                source: e,
                message: "Compression: Failed to reopen temporary file for zstd writer".to_string(),
            }
        })?;

        let path_for_compressing = self.uncompressed_tar_path.clone(); // path to uncompressed tar
        info!(source_path = %path_for_compressing.display(), dest_path = %compressed_layer_file.path().display(), "Compressing uncompressed tar with zstd.");
        task::spawn_blocking(move || -> Result<()> {
            let mut uncompressed_reader =
                File::open(&path_for_compressing).map_err(|e| {
                    warn!(path = %path_for_compressing.display(), error = %e, "Compression: Failed to open uncompressed tar.");
                    Error::Io {
                        source: e,
                        message: format!(
                            "Compression: Failed to open uncompressed tar {}",
                            path_for_compressing.display()
                        ),
                    }
                })?;
            // Use zstd compression level 3 for good balance of speed and compression
            let mut zstd_encoder = zstd::Encoder::new(compressed_writer_file_reopened, 3).map_err(|e| {
                warn!(error = %e, "Failed to create zstd encoder.");
                Error::Io {
                    source: e,
                    message: "Failed to create zstd encoder".to_string(),
                }
            })?;
            io::copy(&mut uncompressed_reader, &mut zstd_encoder).map_err(|e| {
                warn!(error = %e, "Failed to compress tar data with zstd.");
                Error::Io {
                    source: e,
                    message: "Failed to compress tar data with zstd".to_string(),
                }
            })?;
            zstd_encoder.finish().map_err(|e| {
                warn!(error = %e, "Failed to finalize zstd stream.");
                Error::Io {
                    source: e,
                    message: "Failed to finalize zstd stream".to_string(),
                }
            })?;
            Result::Ok(())
        })
        .await
        .map_err(|e| {
            warn!(error = %e, "Task join error during compression.");
            Error::Join { // JoinError
                source: e,
                message: "Task join error during compression".to_string(),
            }
        })??;
        info!("Compression completed.");

        // Read the compressed file to calculate its digest and store in cache
        info!(path = %compressed_layer_file.path().display(), "Reading compressed layer file for digest calculation and caching.");
        let compressed_data = tokio::fs::read(compressed_layer_file.path()).await.map_err(|e| {
            warn!(path = %compressed_layer_file.path().display(), error = %e, "Failed to read compressed layer file for caching.");
            Error::Io {
                message: format!("Failed to read compressed layer file for caching: {}", compressed_layer_file.path().display()),
                source: e,
            }
        })?;

        let blob_digest_sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(&compressed_data);
            format!("sha256:{:x}", hasher.finalize())
        };
        info!(blob_digest = %blob_digest_sha256, "Calculated blob digest for compressed layer.");

        // Put into blob cache
        info!(blob_digest = %blob_digest_sha256, "Storing compressed layer in blob cache.");
        cache
            .put_blob(&blob_digest_sha256, &compressed_data)
            .await?;
        info!(blob_digest = %blob_digest_sha256, "Compressed layer stored in blob cache.");

        // Store metadata mapping from content_key to {diff_id, blob_digest}
        // This allows future builds with the same content to skip compression
        let metadata = serde_json::json!({
            "diff_id": format!("sha256:{}", diff_id_hex),
            "blob_digest": blob_digest_sha256
        });
        let metadata_bytes = serde_json::to_vec(&metadata).map_err(|e| Error::Io {
            source: std::io::Error::other(e),
            message: "Failed to serialize layer metadata".to_string(),
        })?;
        cache.put_blob(&content_key, &metadata_bytes).await?;
        info!(content_key = %content_key, "Stored layer metadata for future cache hits");

        // Also store input-based key mapping for faster future cache lookups
        // This allows future builds with the same input files to skip tar hashing entirely
        cache.put_blob(&input_key, &metadata_bytes).await?;
        info!(input_key = %input_key, "Stored layer metadata with input key for faster cache lookups");

        info!(diff_id = %format!("sha256:{}", diff_id_hex), blob_digest = %blob_digest_sha256, "Layer built successfully.");
        Ok(Layer {
            diff_id: format!("sha256:{}", diff_id_hex),
            blob_digest: blob_digest_sha256,
            media_type: IMAGE_LAYER_ZSTD_MEDIA_TYPE.to_string(),
            compressed_layer_file,
        })
    }

    #[instrument(level = "debug", skip(self), fields(path_in_archive = %path_in_archive.as_ref().display()))]
    fn ensure_parent_dirs_exist(&mut self, path_in_archive: impl AsRef<Path>) -> Result<()> {
        let path_in_archive = path_in_archive.as_ref();
        if let Some(parent) = path_in_archive.parent() {
            if parent.as_os_str().is_empty() {
                return Ok(()); // No parent directory (e.g., file in root)
            }

            let mut paths_to_create = Vec::new();
            let mut current_ancestor = parent;
            // Collect all ancestor paths up to the root
            loop {
                let s = current_ancestor.to_string_lossy();
                if !s.is_empty() && s != "/" {
                    paths_to_create.push(current_ancestor.to_path_buf());
                }
                if let Some(p) = current_ancestor.parent() {
                    current_ancestor = p;
                } else {
                    break; // Reached the top or an empty path
                }
            }

            // Create directories from top-level downwards (e.g., "app", then "app/foo")
            for p_to_create in paths_to_create.iter().rev() {
                if !self.created_archive_dirs.contains(p_to_create) {
                    debug!(
                        "Ensuring directory {} exists in archive layer",
                        p_to_create.display()
                    );
                    let mut header = tar::Header::new_gnu();
                    // tar paths should be relative and use '/'
                    header.set_path(p_to_create).map_err(|e| Error::Io {
                        message: format!(
                            "Failed to set path for directory header: {}",
                            p_to_create.display()
                        ),
                        source: e,
                    })?;
                    header.set_size(0); // Directories have zero size
                    header.set_mtime(0); // Consistent mtime, e.g., for reproducibility
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_mode(0o755); // Standard directory permissions
                    header.set_entry_type(tar::EntryType::Directory);
                    // Checksum is handled by append

                    // Append the header with an empty data stream for a directory
                    let empty_data: &[u8] = &[];
                    self.tar_writer
                        .append_data(&mut header, p_to_create, empty_data)
                        .map_err(|e| Error::Io {
                            message: format!(
                                "Failed to append directory header for: {}",
                                p_to_create.display()
                            ),
                            source: e,
                        })?;
                    self.created_archive_dirs.insert(p_to_create.clone());
                }
            }
        }
        Ok(())
    }
}

// Helper function to normalize paths intended for the archive
// Converts absolute paths like "/foo/bar" to relative "foo/bar".
// A path like "/" becomes an empty path, suitable for archive root in some tar operations.
fn normalize_archive_path(path_in_layer: &Path) -> PathBuf {
    // On Windows, Path::is_absolute() requires a drive letter (e.g. C:\), so
    // Unix-style absolute paths like "/app/foo" are not detected as absolute.
    // We handle both cases: platform-native absolute paths via .components().skip(1),
    // and Unix-style leading '/' via byte/string stripping.
    if path_in_layer.is_absolute() {
        // .components() -> RootDir, "foo", "bar"
        // .skip(1) -> "foo", "bar"
        // .collect() -> "foo/bar"
        // If path_in_layer is just "/", components().skip(1) is empty, collect() -> ""
        path_in_layer.components().skip(1).collect::<PathBuf>()
    } else {
        let path_str = path_in_layer.to_string_lossy();
        if path_str.starts_with('/') {
            PathBuf::from(path_str.trim_start_matches('/'))
        } else {
            path_in_layer.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read as _};
    use tar::Archive;
    use tempfile::tempdir;

    // Helper to extract tar content for verification
    fn list_tar_contents(layer_path: &Path) -> Result<std::collections::HashMap<String, String>> {
        let compressed_file = File::open(layer_path).map_err(|e| Error::Io {
            message: "failed to open layer".into(),
            source: e,
        })?;
        let tar_reader = zstd::Decoder::new(compressed_file).map_err(|e| Error::Io {
            message: "failed to create zstd decoder".into(),
            source: e,
        })?;
        let mut archive = Archive::new(tar_reader);
        let mut entries_found = std::collections::HashMap::new();

        for entry_result in archive.entries().map_err(|e| Error::Io {
            message: "failed to get entries".into(),
            source: e,
        })? {
            let mut entry = entry_result.map_err(|e| Error::Io {
                message: "failed to get entry".into(),
                source: e,
            })?;
            let mut path_str = entry
                .path()
                .map_err(|e| Error::Io {
                    message: "failed to get path".into(),
                    source: e,
                })?
                .to_string_lossy()
                .into_owned();
            let mut content_str = String::new();
            let is_dir = entry.header().entry_type().is_dir();

            if is_dir {
                if !path_str.ends_with("/") {
                    path_str.push('/');
                }
            } else {
                entry
                    .read_to_string(&mut content_str)
                    .map_err(|e| Error::Io {
                        message: "failed to read to string".into(),
                        source: e,
                    })?;
            }
            entries_found.insert(path_str, content_str);
        }
        Ok(entries_found)
    }

    #[tokio::test]
    async fn test_layer_creation_empty() -> Result<()> {
        let layer = Layer::builder()?.build().await?;
        assert!(layer.diff_id().starts_with("sha256:"));
        assert!(layer.path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_data_absolute_path() -> Result<()> {
        let content = b"hello absolute world";
        let layer = Layer::builder()?
            .data("/app/hello_abs.txt", content, None)?
            .build()
            .await?;

        let entries = list_tar_contents(layer.path())?;
        assert_eq!(
            entries.get("app/hello_abs.txt").unwrap(),
            "hello absolute world"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_data_relative_path() -> Result<()> {
        let content = b"hello relative world";
        let layer = Layer::builder()?
            .data("app/hello_rel.txt", content, None)?
            .build()
            .await?;

        let entries = list_tar_contents(layer.path())?;
        assert_eq!(
            entries.get("app/hello_rel.txt").unwrap(),
            "hello relative world"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_data_root_path_error() -> Result<()> {
        let builder = Layer::builder()?;
        let result_of_data_call = builder.data("/", b"root data", None); // No '?' here, check this call's result

        assert!(
            result_of_data_call.is_err(),
            "Expected .data(\"/\", ...) to fail for root path data"
        );
        match result_of_data_call {
            Err(Error::InvalidPath { message }) => {
                assert!(message.contains("Archive path for data cannot be the root directory"));
            }
            Err(other_error) => {
                panic!(
                    "Expected Error::InvalidPath from .data(), but got {:?}",
                    other_error
                );
            }
            Ok(_) => {
                panic!("Expected .data(\"/\", ...) to fail, but it succeeded building a LayerBuilder instance.");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_file_absolute_path() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;
        let source_file_disk = temp_root.path().join("source_abs.txt");
        fs::write(&source_file_disk, "absolute file content").map_err(|e| Error::Io {
            message: "failed to write to source_abs.txt".to_string(),
            source: e,
        })?;

        let layer = Layer::builder()?
            .file(&source_file_disk, "/app/file_abs.txt", None)?
            .build()
            .await?;

        let entries = list_tar_contents(layer.path())?;
        assert_eq!(
            entries.get("app/file_abs.txt").unwrap(),
            "absolute file content"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_file_root_path_error() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;
        let source_file_disk = temp_root.path().join("source_root.txt");
        fs::write(&source_file_disk, "root file content").map_err(|e| Error::Io {
            message: "failed to write to source_root.txt".to_string(),
            source: e,
        })?;

        let builder = Layer::builder()?;
        let result_of_file_call = builder.file(&source_file_disk, "/", None); // No '?' here, check this call's result

        assert!(
            result_of_file_call.is_err(),
            "Expected .file(\"/\", ...) to fail for root path file"
        );
        match result_of_file_call {
            Err(Error::InvalidPath { message }) => {
                assert!(message.contains("Archive path for file"));
            }
            Err(other_error) => {
                panic!(
                    "Expected Error::InvalidPath from .file(), but got {:?}",
                    other_error
                );
            }
            Ok(_) => {
                panic!("Expected .file(\"/\", ...) to fail, but it succeeded building a LayerBuilder instance.");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_directory_absolute_path() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;
        let source_dir_disk = temp_root.path().join("my_dir_abs");
        fs::create_dir_all(source_dir_disk.join("sub")).map_err(|e| Error::Io {
            message: "failed to create_dir_all".to_string(),
            source: e,
        })?;
        fs::write(
            source_dir_disk.join("sub/file_in_dir.txt"),
            "dir content abs",
        )
        .map_err(|e| Error::Io {
            message: "failed to write to file_in_dir.txt".to_string(),
            source: e,
        })?;

        let layer = Layer::builder()?
            .directory(&source_dir_disk, "/archived_dir_abs")?
            .build()
            .await?;

        let entries = list_tar_contents(layer.path())?;
        assert!(entries.contains_key("archived_dir_abs/sub/"));
        assert_eq!(
            entries.get("archived_dir_abs/sub/file_in_dir.txt").unwrap(),
            "dir content abs"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_directory_root_path() -> Result<()> {
        // Adding a directory with archive_path "/" means its *contents* go to the layer's root.
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;
        let source_dir_disk = temp_root.path().join("my_dir_for_root");
        fs::create_dir_all(source_dir_disk.join("sub_at_root")).map_err(|e| Error::Io {
            message: "failed to create_dir_all".to_string(),
            source: e,
        })?;
        fs::write(source_dir_disk.join("file_at_root.txt"), "root dir content").map_err(|e| {
            Error::Io {
                message: "failed to write to file_at_root.txt".to_string(),
                source: e,
            }
        })?;
        fs::write(
            source_dir_disk.join("sub_at_root/nested.txt"),
            "nested root",
        )
        .map_err(|e| Error::Io {
            message: "failed to write to nested.txt".to_string(),
            source: e,
        })?;

        let layer = Layer::builder()?
            .directory(&source_dir_disk, "/")? // Contents of my_dir_for_root should be at archive root
            .build()
            .await?;

        let entries = list_tar_contents(layer.path())?;
        assert!(entries.contains_key("sub_at_root/"));
        assert_eq!(entries.get("file_at_root.txt").unwrap(), "root dir content");
        assert_eq!(
            entries.get("sub_at_root/nested.txt").unwrap(),
            "nested root"
        );
        // The directory "my_dir_for_root" itself should not appear in the path.
        assert!(!entries.contains_key(&format!(
            "{}/file_at_root.txt",
            source_dir_disk.file_name().unwrap().to_string_lossy()
        )));
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_data() -> Result<()> {
        let content = b"hello world";
        let layer = Layer::builder()?
            .data("hello.txt", content, Some(0o755))?
            .build()
            .await?;

        assert!(layer.diff_id().starts_with("sha256:"));
        assert!(layer.path().exists());

        // Verify tar content
        let compressed_file = File::open(layer.path()).map_err(|e| Error::Io {
            message: "failed to open layer path".to_string(),
            source: e,
        })?;
        let tar_reader = zstd::Decoder::new(compressed_file).map_err(|e| Error::Io {
            message: "failed to create zstd decoder".to_string(),
            source: e,
        })?;
        let mut archive = Archive::new(tar_reader);
        let mut found = false;
        for entry_result in archive.entries().map_err(|e| Error::Io {
            message: "failed to get archive entries".to_string(),
            source: e,
        })? {
            let mut entry = entry_result.map_err(|e| Error::Io {
                message: "failed to get entry".to_string(),
                source: e,
            })?;
            if entry
                .path()
                .map_err(|e| Error::Io {
                    message: "failed to get entry path".to_string(),
                    source: e,
                })?
                .to_string_lossy()
                == "hello.txt"
            {
                let mut s = String::new();
                entry.read_to_string(&mut s).map_err(|e| Error::Io {
                    message: "failed to read entry to string".to_string(),
                    source: e,
                })?;
                assert_eq!(s, "hello world");
                found = true;
                break;
            }
        }
        assert!(found, "File 'hello.txt' not found in layer");
        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_file_and_dir() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;

        // Create a directory on disk to add
        let source_dir_disk = temp_root.path().join("mydir_on_disk");
        fs::create_dir(&source_dir_disk).map_err(|e| Error::Io {
            message: "failed to create source_dir_disk".to_string(),
            source: e,
        })?;
        fs::write(source_dir_disk.join("file1.txt"), "content1").map_err(|e| Error::Io {
            message: "failed to write to file1.txt".to_string(),
            source: e,
        })?;
        fs::write(source_dir_disk.join("file2.txt"), "content2").map_err(|e| Error::Io {
            message: "failed to write to file2.txt".to_string(),
            source: e,
        })?;

        // Add a subdirectory with a file to test recursion
        let sub_dir_disk = source_dir_disk.join("my_subdir");
        fs::create_dir(&sub_dir_disk).map_err(|e| Error::Io {
            message: "failed to create sub_dir_disk".to_string(),
            source: e,
        })?;
        fs::write(sub_dir_disk.join("nested_file.txt"), "nested_content").map_err(|e| {
            Error::Io {
                message: "failed to write to nested_file.txt".to_string(),
                source: e,
            }
        })?;

        // Create an individual file on disk to add
        let source_file_disk = temp_root.path().join("individual_on_disk.txt");
        fs::write(&source_file_disk, "individual_content").map_err(|e| Error::Io {
            message: "failed to write to individual_on_disk.txt".to_string(),
            source: e,
        })?;

        let layer = Layer::builder()?
            .directory(&source_dir_disk, "archived_dir")? // Add dir to "archived_dir/" in layer
            .file(
                &source_file_disk,
                "archived_dir/renamed_individual.txt",
                None,
            )? // Add file into "archived_dir/"
            .data("root_file.txt", b"root data", None)? // Add data to root with default mode
            .build()
            .await?;

        assert!(layer.diff_id().starts_with("sha256:"));
        assert!(layer.path().exists());

        // Verify tar content
        let compressed_file = File::open(layer.path()).map_err(|e| Error::Io {
            message: "failed to open layer path".to_string(),
            source: e,
        })?;
        let tar_reader = zstd::Decoder::new(compressed_file).map_err(|e| Error::Io {
            message: "failed to create zstd decoder".to_string(),
            source: e,
        })?;
        let mut archive = Archive::new(tar_reader);
        let mut entries_found = std::collections::HashMap::new();

        for entry_result in archive.entries().map_err(|e| Error::Io {
            message: "failed to get archive entries".to_string(),
            source: e,
        })? {
            let mut entry = entry_result.map_err(|e| Error::Io {
                message: "failed to get entry".to_string(),
                source: e,
            })?;
            let path_str = entry
                .path()
                .map_err(|e| Error::Io {
                    message: "failed to get entry path".to_string(),
                    source: e,
                })?
                .to_string_lossy()
                .into_owned();
            let mut content = String::new();
            if !entry.header().entry_type().is_dir() {
                // Only read content for files
                entry.read_to_string(&mut content).map_err(|e| Error::Io {
                    message: "failed to read entry to string".to_string(),
                    source: e,
                })?;
            }
            entries_found.insert(path_str, content);
        }

        assert_eq!(
            entries_found.get("archived_dir/file1.txt").unwrap(),
            "content1"
        );
        assert_eq!(
            entries_found.get("archived_dir/file2.txt").unwrap(),
            "content2"
        );
        assert_eq!(
            entries_found
                .get("archived_dir/renamed_individual.txt")
                .unwrap(),
            "individual_content"
        );
        assert_eq!(entries_found.get("root_file.txt").unwrap(), "root data");
        assert!(entries_found.contains_key("archived_dir/")); // Check if directory entry itself exists
        assert_eq!(
            entries_found
                .get("archived_dir/my_subdir/nested_file.txt")
                .unwrap(),
            "nested_content"
        );

        // New assertion for the subdirectory entry
        let mut subdir_entry_found_in_archive = false;
        // Re-open archive for a fresh iteration if entries() consumes it, or clone if possible.
        // For simplicity, let's assume we can re-iterate or the previous loop collected all paths.
        // If not, this test needs restructuring to iterate once and check all conditions.
        // Given the test structure, it's better to check entries_found map directly.

        let subdir_path1 = "archived_dir/my_subdir";
        let subdir_path2 = "archived_dir/my_subdir/"; // Check with trailing slash as well

        if let Some(content) = entries_found.get(subdir_path1) {
            if content.is_empty() {
                // Directories usually have empty content string in this map setup
                // To be more robust, we'd need to store EntryType in the map or re-iterate the archive.
                // For now, assume if the key exists and content is empty, it might be a dir.
                // This isn't a perfect check for it being a directory based on entries_found alone.
                // The original test was better by iterating the archive directly for this check.
                // Let's revert to a direct archive iteration for this specific check for robustness.
                let compressed_file_recheck = File::open(layer.path()).map_err(|e| Error::Io {
                    message: "failed to open layer path".to_string(),
                    source: e,
                })?;
                let tar_reader_recheck =
                    zstd::Decoder::new(compressed_file_recheck).map_err(|e| Error::Io {
                        message: "failed to create zstd decoder".to_string(),
                        source: e,
                    })?;
                let mut archive_recheck = Archive::new(tar_reader_recheck);
                for entry_result in archive_recheck.entries().map_err(|e| Error::Io {
                    message: "failed to get archive entries".to_string(),
                    source: e,
                })? {
                    let entry = entry_result.map_err(|e| Error::Io {
                        message: "failed to get entry".to_string(),
                        source: e,
                    })?;
                    let path_cow = entry.path().map_err(|e| Error::Io {
                        message: "failed to get entry path".to_string(),
                        source: e,
                    })?;
                    if (path_cow == Path::new(subdir_path1) || path_cow == Path::new(subdir_path2))
                        && entry.header().entry_type().is_dir()
                    {
                        subdir_entry_found_in_archive = true;
                        break;
                    }
                }
            }
        } else if let Some(content) = entries_found.get(subdir_path2) {
            if content.is_empty() {
                let compressed_file_recheck = File::open(layer.path()).map_err(|e| Error::Io {
                    message: "failed to open layer path".to_string(),
                    source: e,
                })?;
                let tar_reader_recheck =
                    zstd::Decoder::new(compressed_file_recheck).map_err(|e| Error::Io {
                        message: "failed to create zstd decoder".to_string(),
                        source: e,
                    })?;
                let mut archive_recheck = Archive::new(tar_reader_recheck);
                for entry_result in archive_recheck.entries().map_err(|e| Error::Io {
                    message: "failed to get archive entries".to_string(),
                    source: e,
                })? {
                    let entry = entry_result.map_err(|e| Error::Io {
                        message: "failed to get entry".to_string(),
                        source: e,
                    })?;
                    let path_cow = entry.path().map_err(|e| Error::Io {
                        message: "failed to get entry path".to_string(),
                        source: e,
                    })?;
                    if (path_cow == Path::new(subdir_path1) || path_cow == Path::new(subdir_path2))
                        && entry.header().entry_type().is_dir()
                    {
                        subdir_entry_found_in_archive = true;
                        break;
                    }
                }
            }
        }
        assert!(
            subdir_entry_found_in_archive,
            "Directory entry 'archived_dir/my_subdir[/]' not found or not a directory"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_compressed_temp_file_deleted_on_drop() -> Result<()> {
        let layer_path_before_drop;
        {
            let layer = Layer::builder()?.build().await?;
            layer_path_before_drop = layer.path().to_path_buf();
            assert!(
                layer_path_before_drop.exists(),
                "Compressed temp file should exist while Layer is in scope"
            );
        } // Layer is dropped here

        assert!(
            !layer_path_before_drop.exists(),
            "Compressed temp file should be deleted after Layer is dropped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_directory_to_archive_root() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;

        // 1. Create a source directory on disk with some files
        let source_dir_on_disk = temp_root.path().join("my_app_files");
        fs::create_dir(&source_dir_on_disk).map_err(|e| Error::Io {
            message: "failed to create source_dir_on_disk".to_string(),
            source: e,
        })?;
        fs::write(source_dir_on_disk.join("app.js"), "console.log('app');").map_err(|e| {
            Error::Io {
                message: "failed to write to app.js".to_string(),
                source: e,
            }
        })?;
        fs::write(
            source_dir_on_disk.join("styles.css"),
            "body { color: blue; }",
        )
        .map_err(|e| Error::Io {
            message: "failed to write to styles.css".to_string(),
            source: e,
        })?;

        // 2. Build the layer, copying the directory to the archive root "."
        let layer = Layer::builder()?
            .directory(&source_dir_on_disk, ".")?
            .build()
            .await?;

        assert!(layer.diff_id().starts_with("sha256:"));
        assert!(layer.path().exists());

        // 3. Verify tar content
        let compressed_file = File::open(layer.path()).map_err(|e| Error::Io {
            message: "failed to open layer path".to_string(),
            source: e,
        })?;
        let tar_reader = zstd::Decoder::new(compressed_file).map_err(|e| Error::Io {
            message: "failed to create zstd decoder".to_string(),
            source: e,
        })?;
        let mut archive = Archive::new(tar_reader);
        let mut entries_found = std::collections::HashMap::new();

        for entry_result in archive.entries().map_err(|e| Error::Io {
            message: "failed to get archive entries".to_string(),
            source: e,
        })? {
            let mut entry = entry_result.map_err(|e| Error::Io {
                message: "failed to get entry".to_string(),
                source: e,
            })?;
            let path_str = entry
                .path()
                .map_err(|e| Error::Io {
                    message: "failed to get entry path".to_string(),
                    source: e,
                })?
                .to_string_lossy()
                .into_owned();
            let mut content = String::new();
            if !entry.header().entry_type().is_dir() {
                entry.read_to_string(&mut content).map_err(|e| Error::Io {
                    message: "failed to read entry to string".to_string(),
                    source: e,
                })?;
            }
            entries_found.insert(path_str, content);
        }

        // Check that files are at the root
        assert_eq!(entries_found.get("app.js").unwrap(), "console.log('app');");
        assert_eq!(
            entries_found.get("styles.css").unwrap(),
            "body { color: blue; }"
        );

        // Check that the source directory name itself is NOT part of the path in the archive
        assert!(
            !entries_found.contains_key("my_app_files/app.js"),
            "Path should not include source dir name"
        );
        assert!(
            !entries_found.contains_key("my_app_files/"),
            "Source directory itself should not be an entry when copied to root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_layer_with_file_mode_override() -> Result<()> {
        let temp_root = tempdir().map_err(|e| Error::Io {
            message: "failed to create tempdir".to_string(),
            source: e,
        })?;
        let source_file_disk = temp_root.path().join("test_mode.txt");
        fs::write(&source_file_disk, "mode test content").map_err(|e| Error::Io {
            message: "failed to write to test_mode.txt".to_string(),
            source: e,
        })?;

        // Ensure original file has known, non-execute permissions for the test
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&source_file_disk)
                .map_err(|e| Error::Io {
                    message: "failed to get metadata".to_string(),
                    source: e,
                })?
                .permissions();
            perms.set_mode(0o644); // rw-r--r--
            fs::set_permissions(&source_file_disk, perms).map_err(|e| Error::Io {
                message: "failed to set permissions".to_string(),
                source: e,
            })?;
        }
        // On non-unix, we rely on default file creation masks, the key is that it's unlikely to be 0o755 by default.

        let layer = Layer::builder()?
            .file(&source_file_disk, "app/executable.txt", Some(0o755))? // Override to rwxr-xr-x
            .file(&source_file_disk, "app/normal.txt", None)? // Keep original (0o644)
            .build()
            .await?;

        let compressed_file = File::open(layer.path()).map_err(|e| Error::Io {
            message: "failed to open layer path".to_string(),
            source: e,
        })?;
        let tar_reader = zstd::Decoder::new(compressed_file).map_err(|e| Error::Io {
            message: "failed to create zstd decoder".to_string(),
            source: e,
        })?;
        let mut archive = Archive::new(tar_reader);
        let mut modes = std::collections::HashMap::new();

        for entry_result in archive.entries().map_err(|e| Error::Io {
            message: "failed to get archive entries".to_string(),
            source: e,
        })? {
            let entry = entry_result.map_err(|e| Error::Io {
                message: "failed to get entry".to_string(),
                source: e,
            })?;
            let path_str = entry
                .path()
                .map_err(|e| Error::Io {
                    message: "failed to get entry path".to_string(),
                    source: e,
                })?
                .to_string_lossy()
                .into_owned();
            if path_str == "app/executable.txt" || path_str == "app/normal.txt" {
                modes.insert(
                    path_str,
                    entry.header().mode().map_err(|e| Error::Io {
                        message: "failed to get mode".to_string(),
                        source: e,
                    })?,
                );
            }
        }

        assert_eq!(
            modes.get("app/executable.txt").unwrap(),
            &0o755,
            "Mode for executable.txt should be overridden to 0o755"
        );
        // We set it to 0o644 explicitly above on Unix.
        // On non-Unix, the original permissions might vary, but the key is that it should NOT be 0o755 from the override.
        // For consistency and to make the assert more robust, let's check it's not 0o755 if not unix,
        // and specifically 0o644 if unix.
        if cfg!(unix) {
            assert_eq!(
                modes.get("app/normal.txt").unwrap() & 0o777,
                0o644,
                "Mode for normal.txt should be original 0o644 on unix (permission bits)"
            );
        } else {
            assert_ne!(
                modes.get("app/normal.txt").unwrap(),
                &0o755,
                "Mode for normal.txt should not be 0o755 on non-unix (was not overridden)"
            );
        }

        Ok(())
    }
}
