use crate::blobcache;
use crate::error::{Error, Result};
use crate::layer::Layer;
use async_trait::async_trait;

use oci_client::{
    client::{
        Client, ClientConfig, ClientProtocol, Config as OciClientConfig, ImageData as OciImageData,
        ImageLayer as OciImageLayer,
    },
    errors::OciDistributionError,
    manifest::{ImageIndexEntry, OciImageManifest},
    secrets::RegistryAuth,
    Reference, RegistryOperation,
};

/// OCI media type for zstd-compressed tar layers
const IMAGE_LAYER_ZSTD_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
use oci_spec::image::Arch;
use oci_spec::image::{ImageConfiguration, ImageManifest as SpecImageManifest};
use ocipkg::image::Image as _;
use ocipkg::image::OciArtifact;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fs as std_fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tracing::{debug, info, instrument, warn};

/// Runtime metadata extracted from an OCI image configuration.
#[derive(Debug, Clone)]
pub struct ImageMetadata {
    /// Entrypoint from the image config
    pub entrypoint: Option<Vec<String>>,
    /// Cmd from the image config
    pub cmd: Option<Vec<String>>,
    /// Working directory from the image config
    pub working_dir: Option<String>,
}

impl ImageMetadata {
    /// Gets the full runtime command (entrypoint + cmd concatenated).
    /// This is what should actually be executed.
    pub fn runtime_command(&self) -> Vec<String> {
        let mut command = Vec::new();
        if let Some(ref entrypoint) = self.entrypoint {
            command.extend(entrypoint.iter().cloned());
        }
        if let Some(ref cmd) = self.cmd {
            command.extend(cmd.iter().cloned());
        }
        command
    }
}

/// Progress information for image push operations
#[derive(Debug, Clone)]
pub struct PushProgressInfo {
    /// Current operation being performed
    pub operation: String,
    /// Number of layers uploaded so far
    pub layers_uploaded: usize,
    /// Total number of layers to upload
    pub total_layers: usize,
    /// Bytes uploaded so far
    pub bytes_uploaded: u64,
    /// Total bytes to upload
    pub total_bytes: u64,
}

/// Trait for receiving progress updates during image push operations
#[async_trait]
pub trait PushProgressCallback: Send + Sync {
    /// Called when progress is updated
    async fn on_progress(&self, progress: PushProgressInfo);
}

/// Policy for determining whether to use monolithic push.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MonolithicPushPolicy {
    /// Automatically determine based on the registry hostname.
    /// Uses monolithic push for registries known to require it (e.g., Google Artifact Registry).
    #[default]
    Auto,
    /// Always use monolithic push.
    Always,
    /// Never use monolithic push (use chunked upload).
    Never,
}

/// Options for pushing an image.
pub struct PushOptions {
    /// The authentication details for the registry.
    pub auth: RegistryAuth,
    /// The protocol to use for communicating with the registry.
    pub protocol: ClientProtocol,
    /// The policy for determining whether to use monolithic push.
    pub monolithic_push: MonolithicPushPolicy,
    /// Optional progress callback
    pub progress_callback: Option<Box<dyn PushProgressCallback>>,
}

impl std::fmt::Debug for PushOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushOptions")
            .field("auth", &self.auth)
            .field("protocol", &self.protocol)
            .field("monolithic_push", &self.monolithic_push)
            .field("progress_callback", &self.progress_callback.is_some())
            .finish()
    }
}

impl Clone for PushOptions {
    fn clone(&self) -> Self {
        Self {
            auth: self.auth.clone(),
            protocol: self.protocol.clone(),
            monolithic_push: self.monolithic_push.clone(),
            progress_callback: None, // Can't clone trait objects, so we set to None
        }
    }
}

impl Default for PushOptions {
    fn default() -> Self {
        Self {
            auth: RegistryAuth::Anonymous,
            protocol: ClientProtocol::Https,
            monolithic_push: MonolithicPushPolicy::Auto,
            progress_callback: None,
        }
    }
}

impl PushOptions {
    /// Sets the monolithic push policy to always use monolithic push.
    pub fn with_monolithic_push(mut self) -> Self {
        self.monolithic_push = MonolithicPushPolicy::Always;
        self
    }

    /// Sets the monolithic push policy to never use monolithic push (use chunked upload).
    pub fn with_chunked_upload(mut self) -> Self {
        self.monolithic_push = MonolithicPushPolicy::Never;
        self
    }

    /// Sets the monolithic push policy to automatically determine based on the registry.
    pub fn with_auto_push_mode(mut self) -> Self {
        self.monolithic_push = MonolithicPushPolicy::Auto;
        self
    }

    /// Sets a progress callback to receive push progress updates.
    pub fn with_progress_callback(mut self, callback: Box<dyn PushProgressCallback>) -> Self {
        self.progress_callback = Some(callback);
        self
    }
}

/// Defines the policy for pulling an image manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PullPolicy {
    /// Always attempt to pull the image manifest from the registry.
    Always,
    /// Pull the image manifest only if it's not available in the local cache.
    #[default]
    Missing,
}

/// Indicates the source from which an image manifest was obtained during a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    /// The manifest was successfully loaded from the local cache.
    FromCache,
    /// The manifest was pulled from the remote registry.
    FromRegistry,
    /// Not applicable - building from scratch without a base image.
    NotApplicable,
}

/// Contains diagnostic information about the image build process.
#[derive(Debug, Clone)]
pub struct BuildDiagnostics {
    /// How the base image manifest was obtained.
    pub manifest_source: ManifestSource,
    /// The digest of the resolved manifest that was used for the build.
    pub resolved_manifest_digest: String,
}

/// Options for pulling and extracting an image.
#[derive(Debug, Clone, Default)]
pub struct PullAndExtractOptions {
    /// The platform OS to pull (e.g., "linux")
    pub platform_os: Option<String>,
    /// The platform architecture to pull (e.g., Arch::Amd64)
    pub platform_arch: Option<Arch>,
    /// Pull policy for the image manifest
    pub pull_policy: PullPolicy,
    /// Optional blob cache to use
    pub blob_cache: Option<blobcache::BlobCache>,
    /// Optional authentication for the registry
    pub auth: Option<RegistryAuth>,
}

/// Represents a built OCI image stored as an OCI layout tarball.
/// The temporary directory holding the tarball is cleaned up when this struct is dropped.
#[derive(Debug)]
pub struct Image {
    oci_archive_path: PathBuf,
    config_digest: String,
    // Holds the temporary directory to ensure it's cleaned up on drop,
    // if the image was built into a temporary location.
    // If None, the oci_archive_path points to a user-specified persistent location.
    _temp_dir_manager: Option<TempDir>,
}

impl Image {
    /// Returns a builder to construct an `Image`.
    pub fn builder() -> ImageBuilder {
        ImageBuilder::default()
    }

    /// Loads an existing OCI tarball from disk.
    ///
    /// This is useful for pushing pre-built images without rebuilding them.
    ///
    /// # Arguments
    /// * `tarball_path` - Path to the OCI tarball file
    ///
    /// # Returns
    /// A loaded Image instance on success
    #[instrument(fields(tarball_path = %tarball_path.as_ref().display()))]
    pub fn from_tarball(tarball_path: impl AsRef<Path>) -> Result<Self> {
        let tarball_path = tarball_path.as_ref();

        if !tarball_path.exists() {
            return Err(Error::Generic {
                message: format!("OCI tarball not found at {}", tarball_path.display()),
                source: None,
            });
        }

        info!("Loading OCI image from tarball: {}", tarball_path.display());

        // Load the OCI artifact to get the config digest
        let mut archive =
            OciArtifact::from_oci_archive(tarball_path).map_err(|e| Error::OciArchive {
                message: format!(
                    "Failed to load OCI artifact from {}",
                    tarball_path.display()
                ),
                source: Some(e.into()),
            })?;

        let (config_desc, _config_bytes) = archive.get_config().map_err(|e| Error::OciArchive {
            message: "Failed to get config from OCI artifact".to_string(),
            source: Some(e.into()),
        })?;

        Ok(Self {
            oci_archive_path: tarball_path.to_path_buf(),
            config_digest: config_desc.digest().to_string(),
            _temp_dir_manager: None, // Not owned by us, user manages the tarball
        })
    }

    /// Pulls an OCI image from a registry and extracts it to a directory.
    ///
    /// This is a convenience method that combines pulling and extracting.
    ///
    /// # Arguments
    /// * `image_ref` - Image reference (e.g., "ghcr.io/user/image:tag")
    /// * `target_dir` - Directory where the image should be extracted
    /// * `options` - Pull and extract options
    ///
    /// # Returns
    /// Tuple of (extracted_path, metadata) on success
    #[instrument(skip(options), fields(image_ref = %image_ref, target_dir = %target_dir.as_ref().display()))]
    pub async fn pull_and_extract(
        image_ref: &str,
        target_dir: impl AsRef<Path>,
        options: PullAndExtractOptions,
    ) -> Result<(PathBuf, ImageMetadata)> {
        info!("Pulling and extracting image.");

        // Build the image (pulls from registry, uses cache)
        let mut builder = Image::builder()
            .from(image_ref)
            .pull_policy(options.pull_policy);

        if let Some(os) = options.platform_os {
            if let Some(arch) = options.platform_arch {
                builder = builder.platform(&os, &arch);
            }
        }

        if let Some(cache) = options.blob_cache {
            builder = builder.blob_cache(cache);
        }

        if let Some(auth) = options.auth {
            builder = builder.auth(auth);
        }

        let (image, _diagnostics) = builder.build().await?;

        // Extract the image
        image.extract(target_dir).await
    }

    /// Returns the path to the OCI archive tarball.
    pub fn path(&self) -> &Path {
        &self.oci_archive_path
    }

    /// Returns the digest of the image configuration.
    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    /// Gets runtime metadata from the OCI image config.
    ///
    /// Extracts the entrypoint, cmd, and working directory from the image configuration.
    /// If you're planning to extract the image anyway, use `extract()` instead which
    /// returns metadata without an extra tar read.
    ///
    /// # Returns
    /// ImageMetadata containing entrypoint, cmd, and working_dir
    pub fn get_metadata(&self) -> Result<ImageMetadata> {
        Self::read_metadata_from_oci_archive(self.path())
    }

    /// Internal helper to read metadata from an OCI archive.
    fn read_metadata_from_oci_archive(path: &Path) -> Result<ImageMetadata> {
        let mut archive = OciArtifact::from_oci_archive(path).map_err(|e| Error::OciArchive {
            message: format!("Failed to load OCI artifact from {}", path.display()),
            source: Some(e.into()),
        })?;

        let (_config_desc, config_bytes) = archive.get_config().map_err(|e| Error::OciArchive {
            message: "Failed to get config from OCI artifact".to_string(),
            source: Some(e.into()),
        })?;

        let config: ImageConfiguration =
            serde_json::from_slice(&config_bytes).map_err(|e| Error::ImageConfig {
                message: "Failed to parse image configuration".to_string(),
                source: Some(e.into()),
            })?;

        let entrypoint;
        let cmd;
        let working_dir;

        if let Some(process_config) = config.config() {
            entrypoint = process_config.entrypoint().clone();
            cmd = process_config.cmd().clone();
            working_dir = process_config.working_dir().clone();
        } else {
            entrypoint = None;
            cmd = None;
            working_dir = None;
        }

        Ok(ImageMetadata {
            entrypoint,
            cmd,
            working_dir,
        })
    }

    /// Extracts the OCI image to a directory and returns runtime metadata.
    ///
    /// All layers are extracted in order, with later layers overwriting earlier ones.
    /// This creates a merged filesystem view of the container image.
    ///
    /// # Arguments
    /// * `target_dir` - Directory where the image should be extracted
    ///
    /// # Returns
    /// Tuple of (extracted_path, metadata) on success
    #[instrument(skip(self), fields(image_path = %self.oci_archive_path.display(), target_dir = %target_dir.as_ref().display()))]
    pub async fn extract(&self, target_dir: impl AsRef<Path>) -> Result<(PathBuf, ImageMetadata)> {
        let target_dir = target_dir.as_ref();
        info!("Starting image extraction.");

        // Create target directory if it doesn't exist
        std_fs::create_dir_all(target_dir).map_err(|e| {
            warn!(error = %e, "Failed to create target directory.");
            Error::Io {
                message: format!("Failed to create target directory {}", target_dir.display()),
                source: e,
            }
        })?;

        // Load OCI artifact from the archive (single tar read for both metadata and layers)
        info!(path = %self.path().display(), "Loading OCI artifact for extraction.");
        let mut archive = OciArtifact::from_oci_archive(self.path()).map_err(|e| {
            warn!(path = %self.path().display(), error = %e, "Failed to load OCI artifact.");
            Error::OciArchive {
                message: format!("Failed to load OCI artifact from {}", self.path().display()),
                source: Some(e.into()),
            }
        })?;

        // Extract metadata from config before extracting layers
        let (_config_desc, config_bytes) = archive.get_config().map_err(|e| {
            warn!(error = %e, "Failed to get config from OCI artifact.");
            Error::OciArchive {
                message: "Failed to get config from OCI artifact".to_string(),
                source: Some(e.into()),
            }
        })?;

        let config: ImageConfiguration =
            serde_json::from_slice(&config_bytes).map_err(|e| Error::ImageConfig {
                message: "Failed to parse image configuration".to_string(),
                source: Some(e.into()),
            })?;

        let metadata = if let Some(process_config) = config.config() {
            ImageMetadata {
                entrypoint: process_config.entrypoint().clone(),
                cmd: process_config.cmd().clone(),
                working_dir: process_config.working_dir().clone(),
            }
        } else {
            ImageMetadata {
                entrypoint: None,
                cmd: None,
                working_dir: None,
            }
        };

        // Get all layers
        let layers = archive.get_layers().map_err(|e| {
            warn!(error = %e, "Failed to get layers from OCI artifact.");
            Error::OciArchive {
                message: "Failed to get layers from OCI artifact".to_string(),
                source: Some(e.into()),
            }
        })?;

        info!(
            num_layers = layers.len(),
            "Extracting layers to target directory."
        );

        // Extract each layer in order (first layer first, last layer last)
        // Later layers overwrite earlier ones, simulating a union filesystem
        for (idx, (desc, layer_data)) in layers.iter().enumerate() {
            debug!(
                layer_idx = idx,
                layer_digest = %desc.digest(),
                layer_size = layer_data.len(),
                "Extracting layer"
            );

            // Clone data for the blocking task
            let layer_data_vec = layer_data.to_vec();
            let target_dir_clone = target_dir.to_path_buf();
            let layer_digest = desc.digest().to_string();

            // Extract in a blocking task since tar extraction is CPU-intensive
            tokio::task::spawn_blocking(move || -> Result<()> {
                use std::io::Cursor;
                use tar::Archive;

                // Decompress zstd
                let cursor = Cursor::new(layer_data_vec);
                let decoder = zstd::Decoder::new(cursor).map_err(|e| {
                    warn!(
                        layer_digest = %layer_digest,
                        error = %e,
                        "Failed to create zstd decoder"
                    );
                    Error::Io {
                        message: format!(
                            "Failed to create zstd decoder for layer {}",
                            layer_digest
                        ),
                        source: e,
                    }
                })?;

                // Extract tar
                let mut tar_archive = Archive::new(decoder);
                tar_archive.unpack(&target_dir_clone).map_err(|e| {
                    warn!(
                        layer_digest = %layer_digest,
                        error = %e,
                        "Failed to extract tar archive"
                    );
                    Error::Io {
                        message: format!("Failed to extract layer {}", layer_digest),
                        source: e,
                    }
                })?;

                debug!(layer_digest = %layer_digest, "Layer extracted successfully");
                Ok(())
            })
            .await
            .map_err(|e| {
                warn!(error = %e, "Task join error during layer extraction.");
                Error::Generic {
                    message: "Task join error during layer extraction".to_string(),
                    source: Some(Box::new(e)),
                }
            })??;
        }

        info!(
            target_dir = %target_dir.display(),
            num_layers = layers.len(),
            "Image extraction completed successfully"
        );

        Ok((target_dir.to_path_buf(), metadata))
    }

    /// Pushes the OCI image to a remote registry.
    ///
    /// - `target_image_ref_str`: The full reference of the target image (e.g., "ghcr.io/user/image:tag").
    /// - `options`: Push options including authentication and protocol.
    ///
    /// Returns the pushed image reference string on success.
    #[instrument(skip(self, options), fields(image_path = %self.oci_archive_path.display(), target_image_ref = %target_image_ref_str, protocol = ?options.protocol))]
    pub async fn push(&self, target_image_ref_str: &str, options: &PushOptions) -> Result<String> {
        info!("Starting image push.");

        // Helper function to report progress
        let report_progress = |progress: PushProgressInfo| async {
            if let Some(ref callback) = options.progress_callback {
                callback.on_progress(progress).await;
            }
        };

        // Initial progress report
        report_progress(PushProgressInfo {
            operation: "Starting push".to_string(),
            layers_uploaded: 0,
            total_layers: 0,
            bytes_uploaded: 0,
            total_bytes: 0,
        })
        .await;

        // 1. Parse the target reference
        let push_ref = Reference::try_from(target_image_ref_str).map_err(|e| {
            warn!(error = %e, "Invalid target image reference format.");
            Error::Generic {
                message: format!(
                    "Invalid target image reference format '{}': {}",
                    target_image_ref_str, e
                ),
                source: Some(Box::new(e)),
            }
        })?;
        debug!(push_reference = %push_ref, "Parsed target image reference.");

        // 2. Determine monolithic push setting based on policy and registry
        let use_monolithic_push =
            determine_use_monolithic_push(&options.monolithic_push, &push_ref);

        let push_client_config = ClientConfig {
            protocol: options.protocol.clone(),
            use_monolithic_push,
            ..Default::default()
        };

        let oci_client = Client::new(push_client_config);
        debug!(
            use_monolithic_push = use_monolithic_push,
            "OCI client for push created."
        );

        // 3. Load OCI artifact from self.oci_archive_path
        info!(path = %self.path().display(), "Loading OCI artifact for push.");
        let mut archive = OciArtifact::from_oci_archive(self.path()).map_err(|e| {
            warn!(path = %self.path().display(), error = %e, "Failed to load OCI artifact.");
            Error::OciArchive {
                message: format!("Failed to load OCI artifact from {}", self.path().display()),
                source: Some(e.into()),
            }
        })?;

        // 4. Convert manifest (ocipkg -> oci_spec -> json -> oci_client)
        debug!("Converting OCI manifest for client.");
        let spec_mani: SpecImageManifest = archive.get_manifest().map_err(|e| {
            warn!(error = %e, "Failed to get manifest from OCI artifact.");
            Error::OciArchive {
                message: "Failed to get manifest from OCI artifact".to_string(),
                source: Some(e.into()),
            }
        })?;
        let mani_json_bytes = serde_json::to_vec(&spec_mani).map_err(|e| {
            warn!(error = %e, "Failed to serialize spec manifest to JSON.");
            Error::ImageConfig {
                message: "Failed to serialize spec manifest to JSON".to_string(),
                source: Some(e.into()),
            }
        })?;
        let dist_mani: OciImageManifest =
            serde_json::from_slice(&mani_json_bytes).map_err(|e| {
                warn!(error = %e, "Failed to deserialize OCI client manifest from JSON.");
                Error::ImageConfig {
                    message: "Failed to deserialize OCI client manifest from JSON".to_string(),
                    source: Some(e.into()),
                }
            })?;
        debug!("OCI manifest converted.");
        debug!(
            num_dist_mani_layers = dist_mani.layers.len(),
            dist_mani_layers_digests = ?dist_mani.layers.iter().map(|l| l.digest.as_str()).collect::<Vec<_>>(),
            "Details of dist_mani (the manifest to be pushed)"
        );

        // 5. Prepare config for oci_client
        debug!("Preparing image config for OCI client.");
        let (cfg_desc, cfg_bytes) = archive.get_config().map_err(|e| {
            warn!(error = %e, "Failed to get config from OCI artifact.");
            Error::OciArchive {
                message: "Failed to get config from OCI artifact".to_string(),
                source: Some(e.into()),
            }
        })?;
        let cfg_for_push = OciClientConfig {
            data: cfg_bytes,
            media_type: cfg_desc.media_type().to_string(),
            annotations: cfg_desc
                .annotations()
                .clone()
                .map(|h| h.into_iter().collect()),
        };
        debug!(config_media_type = %cfg_for_push.media_type, "Image config prepared.");

        // 6. Authenticate
        info!(target_registry = %push_ref.registry(), "Authenticating with registry.");
        oci_client
            .auth(&push_ref, &options.auth, RegistryOperation::Push)
            .await
            .map_err(|e| {
                warn!(registry = %push_ref.registry(), error = %e, "Authentication failed for push.");
                Error::Generic {
                    message: format!("Authentication failed for push to {}: {}", push_ref, e),
                    source: Some(Box::new(e)),
                }
            })?;
        info!("Authentication successful.");

        // 7. Retrieve artifact layers and try to mount them
        debug!("Retrieving artifact layers for mounting check.");
        let artifact_layers_result = archive.get_layers().map_err(|e| {
            warn!(error = %e, "Failed to get layers from OCI artifact.");
            Error::OciArchive {
                message: "Failed to get layers from OCI artifact".to_string(),
                source: Some(e.into()),
            }
        });
        let artifact_layers = artifact_layers_result?;
        debug!(
            num_artifact_layers = artifact_layers.len(),
            artifact_layers_digests = ?artifact_layers.iter().map(|(d, _)| d.digest()).collect::<Vec<_>>(),
            "Details of artifact_layers (layers to be processed for mount/upload)"
        );

        let mut mounted_digests = HashSet::new();
        info!("Attempting to mount or verify existing layers to skip upload.");

        for (desc, _layer_data_from_artifact) in &artifact_layers {
            let digest_str = desc.digest().to_string();
            let mut should_skip_upload = false;

            debug!(layer_digest = %digest_str, "Attempting to mount layer.");
            match oci_client
                .mount_blob(&push_ref, &push_ref, &digest_str)
                .await
            {
                Ok(_) => {
                    info!(layer_digest = %digest_str, "Layer successfully mounted (OCI 201).");
                    should_skip_upload = true;
                }
                Err(e) => {
                    debug!(layer_digest = %digest_str, error = %e, "Layer mount failed. Will upload.");
                }
            }

            if should_skip_upload {
                mounted_digests.insert(digest_str.clone());
            }
        }
        info!(
            num_layers_skipped = mounted_digests.len(),
            "Finished attempting to mount/verify layers."
        );

        // 8. Prepare layers for push (filter out mounted ones)
        let layers_to_push: Vec<OciImageLayer> = artifact_layers
            .into_iter() // Consumes artifact_layers
            .filter(|(d, _)| !mounted_digests.contains(d.digest()))
            .map(|(d, data)| OciImageLayer {
                data: data.to_vec(), // ocipkg returns bytes::Bytes, oci_client expects Vec<u8>
                media_type: d.media_type().to_string(),
                annotations: d.annotations().clone().map(|h| h.into_iter().collect()),
            })
            .collect();

        let total_push_size_bytes: usize = layers_to_push.iter().map(|l| l.data.len()).sum();
        info!(
            num_layers_to_push = layers_to_push.len(),
            num_total_layers = mounted_digests.len() + layers_to_push.len(),
            total_push_size_mb = total_push_size_bytes / (1024 * 1024),
            "Preparing to push layers."
        );

        // Report progress with total bytes and layers
        let total_layers = mounted_digests.len() + layers_to_push.len();
        report_progress(PushProgressInfo {
            operation: "Uploading layers".to_string(),
            layers_uploaded: mounted_digests.len(),
            total_layers,
            bytes_uploaded: 0,
            total_bytes: total_push_size_bytes as u64,
        })
        .await;

        // Skip push when everything was mounted
        if layers_to_push.is_empty() {
            info!("All layers already exist in the registry. Pushing config and manifest.");

            // Report progress for config and manifest upload
            report_progress(PushProgressInfo {
                operation: "Uploading config and manifest".to_string(),
                layers_uploaded: total_layers,
                total_layers,
                bytes_uploaded: total_push_size_bytes as u64,
                total_bytes: total_push_size_bytes as u64,
            })
            .await;

            // Use the standard push method with empty layers to ensure config blob is uploaded
            oci_client
                .push(
                    &push_ref,
                    &Vec::new(), // Empty layers since they're all already mounted
                    cfg_for_push,
                    &options.auth,
                    Some(dist_mani),
                )
                .await
                .map_err(|e| {
                    warn!(error = %e, "OCI client push failed.");
                    Error::Generic {
                        message: format!("OCI client push to {} failed: {}", push_ref, e),
                        source: Some(Box::new(e)),
                    }
                })?;

            // Final progress report
            report_progress(PushProgressInfo {
                operation: "Push completed".to_string(),
                layers_uploaded: total_layers,
                total_layers,
                bytes_uploaded: total_push_size_bytes as u64,
                total_bytes: total_push_size_bytes as u64,
            })
            .await;

            info!(image_ref = %target_image_ref_str, "Image push successful (config and manifest only).");
            return Ok(target_image_ref_str.to_string());
        }

        // 9. Push layers individually with progress reporting
        info!("Pushing image (layers, config, manifest).");

        // Report that we're about to start the actual upload
        let operation_text = if layers_to_push.is_empty() {
            "All layers cached".to_string()
        } else if total_push_size_bytes > 10 * 1024 * 1024 {
            format!(
                "Uploading {:.1} MB in {} layers",
                total_push_size_bytes as f64 / (1024.0 * 1024.0),
                layers_to_push.len()
            )
        } else {
            format!("Uploading {} layers", layers_to_push.len())
        };

        report_progress(PushProgressInfo {
            operation: operation_text,
            layers_uploaded: mounted_digests.len(),
            total_layers,
            bytes_uploaded: 0,
            total_bytes: total_push_size_bytes as u64,
        })
        .await;

        let mut uploaded_bytes = 0u64;
        let mut uploaded_layers = mounted_digests.len();

        // Upload each layer individually with progress reporting
        for (i, layer) in layers_to_push.iter().enumerate() {
            let digest = format!("sha256:{:x}", sha2::Sha256::digest(&layer.data));
            info!(
                "Uploading layer {}/{}: {}",
                i + 1,
                layers_to_push.len(),
                digest
            );

            oci_client
                .push_blob(&push_ref, &layer.data, &digest)
                .await
                .map_err(|e| {
                    warn!(error = %e, "Failed to push layer {}", digest);
                    Error::Generic {
                        message: format!("Failed to push layer {}: {}", digest, e),
                        source: Some(Box::new(e)),
                    }
                })?;

            uploaded_bytes += layer.data.len() as u64;
            uploaded_layers += 1;

            report_progress(PushProgressInfo {
                operation: String::new(),
                layers_uploaded: uploaded_layers,
                total_layers,
                bytes_uploaded: uploaded_bytes,
                total_bytes: total_push_size_bytes as u64,
            })
            .await;
        }

        // Upload config blob
        info!("Uploading config blob");
        report_progress(PushProgressInfo {
            operation: "Uploading config".to_string(),
            layers_uploaded: uploaded_layers,
            total_layers,
            bytes_uploaded: uploaded_bytes,
            total_bytes: total_push_size_bytes as u64,
        })
        .await;

        oci_client
            .push_blob(&push_ref, &cfg_for_push.data, &dist_mani.config.digest)
            .await
            .map_err(|e| {
                warn!(error = %e, "Failed to push config blob");
                Error::Generic {
                    message: format!("Failed to push config blob: {}", e),
                    source: Some(Box::new(e)),
                }
            })?;

        // Upload manifest
        info!("Uploading manifest");
        report_progress(PushProgressInfo {
            operation: "Uploading manifest".to_string(),
            layers_uploaded: uploaded_layers,
            total_layers,
            bytes_uploaded: uploaded_bytes,
            total_bytes: total_push_size_bytes as u64,
        })
        .await;

        oci_client
            .push_manifest(&push_ref, &dist_mani.into())
            .await
            .map_err(|e| {
                warn!(error = %e, "Failed to push manifest");
                Error::Generic {
                    message: format!("Failed to push manifest: {}", e),
                    source: Some(Box::new(e)),
                }
            })?;

        // Final progress report
        report_progress(PushProgressInfo {
            operation: "Push completed".to_string(),
            layers_uploaded: total_layers,
            total_layers,
            bytes_uploaded: total_push_size_bytes as u64,
            total_bytes: total_push_size_bytes as u64,
        })
        .await;

        // 10. Return success
        info!(image_ref = %target_image_ref_str, "Image push successful.");
        Ok(target_image_ref_str.to_string())
    }
}

/// Builder for creating `Image` instances.
#[derive(Default)]
pub struct ImageBuilder {
    base_image_ref: Option<String>,
    platform_os: Option<String>,
    platform_arch: Option<Arch>,
    layers: Vec<Layer>,
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    working_dir: Option<String>,
    output_path: Option<PathBuf>,
    blob_cache: Option<blobcache::BlobCache>,
    output_image_name_and_tag: Option<String>,
    pull_policy: Option<PullPolicy>,
    auth: Option<RegistryAuth>,
}

impl ImageBuilder {
    /// Sets the base image reference (e.g., "marketplace.gcr.io/google/ubuntu2404:latest", "ghcr.io/user/image:tag").
    pub fn from(mut self, base_image_ref: &str) -> Self {
        self.base_image_ref = Some(base_image_ref.to_string());
        self
    }

    /// Sets the target platform for the image.
    pub fn platform(mut self, os: &str, arch: &Arch) -> Self {
        self.platform_os = Some(os.to_string());
        self.platform_arch = Some(arch.clone());
        self
    }

    /// Adds a `Layer` to be included in the image. Layers are applied in the order they are added.
    pub fn layer(mut self, layer: Layer) -> Self {
        self.layers.push(layer);
        self
    }

    /// Sets the entrypoint for the image. Overrides the entrypoint from the base image.
    pub fn entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = Some(entrypoint);
        self
    }

    /// Sets the command (Cmd) for the image. Overrides the command from the base image.
    pub fn cmd(mut self, cmd: Vec<String>) -> Self {
        self.cmd = Some(cmd);
        self
    }

    /// Sets the working directory for the image. Overrides the working directory from the base image.
    pub fn working_dir(mut self, working_dir: &str) -> Self {
        self.working_dir = Some(working_dir.to_string());
        self
    }

    /// Specifies the final path where the OCI archive tarball should be saved.
    /// If not set, the archive will be created in a temporary directory.
    pub fn output_to(mut self, path: PathBuf) -> Self {
        self.output_path = Some(path);
        self
    }

    /// Sets a specific BlobCache instance to be used by the ImageBuilder.
    /// If not called, a default BlobCache will be created when `build()` is invoked.
    pub fn blob_cache(mut self, cache: blobcache::BlobCache) -> Self {
        self.blob_cache = Some(cache);
        self
    }

    /// Sets the name and tag to be used for the image configuration within the OCI archive.
    /// If not set, it defaults to "<base_image_repository>:latest".
    pub fn output_name_and_tag(mut self, name_and_tag: &str) -> Self {
        self.output_image_name_and_tag = Some(name_and_tag.to_string());
        self
    }

    /// Sets the pull policy for the base image manifest.
    /// Defaults to `PullPolicy::Missing`.
    pub fn pull_policy(mut self, policy: PullPolicy) -> Self {
        self.pull_policy = Some(policy);
        self
    }

    /// Sets the authentication for pulling the base image.
    /// If not set, authentication is determined from environment variables (DOCKER_USERNAME/DOCKER_PASSWORD).
    pub fn auth(mut self, auth: RegistryAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Builds the image.
    /// This involves potentially using a cache for the base image, pulling it if necessary,
    /// applying new layers and configuration,
    /// and creating an OCI tarball layout in a temporary directory.
    #[instrument(skip_all, fields(
        base_image_ref = ?self.base_image_ref,
        platform_os = ?self.platform_os,
        platform_arch = ?self.platform_arch,
        num_layers_to_add = self.layers.len(),
        output_path = ?self.output_path
    ))]
    pub async fn build(mut self) -> Result<(Image, BuildDiagnostics)> {
        info!("Starting image build.");

        let target_os_for_build = self
            .platform_os
            .clone()
            .unwrap_or_else(|| "linux".to_string());
        let target_arch_for_build = self.platform_arch.clone().unwrap_or(Arch::Amd64);

        // Conditionally pull base image if specified
        let base_image_data: Option<OciImageData>;
        let manifest_source: ManifestSource;
        let resolved_manifest_digest_str: String;
        let default_image_name: String;

        if let Some(base_image_ref_str) = &self.base_image_ref {
            // Pull base image
            info!("Building from base image: {}", base_image_ref_str);

            let base_ref =
                Reference::try_from(base_image_ref_str.as_str()).map_err(|e| Error::ImagePull {
                    image_ref: base_image_ref_str.to_string(),
                    message: format!("Invalid base image reference format: {}", e),
                    source: Some(Box::new(e)),
                })?;

            // Determine authentication for pulling base image
            // Use provided auth if available, otherwise determine from environment variables
            let pull_auth = self
                .auth
                .take()
                .unwrap_or_else(|| determine_registry_auth(&base_ref));
            let pull_policy = self.pull_policy.take().unwrap_or_default(); // Get policy or default

            // Initialize cache instance ONCE.
            // If self.blob_cache is None, ImageBuilder creates its own.
            // If it's Some, it means ImageBuilder was configured with an external cache.
            let cache = match self.blob_cache.take() {
                // Take ownership from self
                Some(c) => c,
                None => {
                    debug!("No BlobCache provided to ImageBuilder, creating a default one.");
                    blobcache::BlobCache::new()?
                }
            };

            let mut client_cfg = ClientConfig::default();

            if let (Some(os_filter_val), Some(arch_filter_val)) =
                (&self.platform_os, &self.platform_arch)
            {
                let os_filter_cloned = os_filter_val.clone();
                let arch_filter_cloned = arch_filter_val.to_string();
                client_cfg.platform_resolver = Some(Box::new(
                    move |index_entries: &[ImageIndexEntry]| {
                        info!(target_os = %os_filter_cloned, target_arch = %arch_filter_cloned, num_index_entries = index_entries.len(), "Platform resolver: Attempting to find match.");
                        for entry in index_entries {
                            if let Some(p) = entry.platform.as_ref() {
                                if p.os == os_filter_cloned && p.architecture == arch_filter_cloned
                                {
                                    info!(resolver_selected_digest = %entry.digest, "Platform resolver: Found a match.");
                                    return Some(entry.digest.clone());
                                }
                            }
                        }
                        warn!(target_os = %os_filter_cloned, target_arch = %arch_filter_cloned, "Platform resolver: No match found.");
                        None
                    },
                ));
            } else {
                info!("Platform resolver not configured as os/arch were not explicitly provided to ImageBuilder. Default oci-client resolver will be used if necessary.");
                // If no platform is specified by the user, oci-client's default resolver (current_platform_resolver) will be used.
                // We don't need to explicitly set it to None here, as ClientConfig::default() already sets a default resolver.
            }

            let oci_client = Client::new(client_cfg);

            info!(base_image_ref = %base_image_ref_str, pull_policy = ?pull_policy, "Attempting to pull/load resolved base image manifest.");

            let pull_err_mapper = |e: OciDistributionError| {
                warn!(base_image_ref = %base_image_ref_str, error = %e, "Failed to pull and resolve base image manifest.");
                Error::ImagePull {
                    image_ref: base_image_ref_str.to_string(),
                    message: format!(
                        "Failed to pull/resolve base image manifest ({}): {}",
                        base_image_ref_str, e
                    ),
                    source: Some(Box::new(e)),
                }
            };

            let manifest_cache_key = format!("manifest-v1:{}", base_ref.whole());
            let manifest_source_temp: ManifestSource;

            let (base_image_manifest_resolved, resolved_manifest_digest_str_temp) = if pull_policy
                == PullPolicy::Missing
            {
                debug!(key = %manifest_cache_key, "PullPolicy::Missing. Attempting to load manifest from cache.");
                match cache.get_blob(&manifest_cache_key).await {
                    Ok(Some(cached_data)) => {
                        match serde_json::from_slice::<(OciImageManifest, String)>(&cached_data) {
                            Ok((manifest, digest)) => {
                                info!(key = %manifest_cache_key, resolved_digest = %digest, "Manifest cache hit and deserialized successfully.");
                                manifest_source_temp = ManifestSource::FromCache;
                                (manifest, digest)
                            }
                            Err(e) => {
                                warn!(key = %manifest_cache_key, error = %e, "Failed to deserialize cached manifest. Will pull from registry.");
                                // Fallback: Pull and cache
                                manifest_source_temp = ManifestSource::FromRegistry;
                                let (pulled_manifest, pulled_digest) = oci_client
                                    .pull_image_manifest(&base_ref, &pull_auth)
                                    .await
                                    .map_err(pull_err_mapper)?;
                                match serde_json::to_vec(&(
                                    pulled_manifest.clone(),
                                    pulled_digest.clone(),
                                )) {
                                    Ok(data_to_cache) => {
                                        if let Err(cache_err) = cache
                                            .put_blob(&manifest_cache_key, &data_to_cache)
                                            .await
                                        {
                                            warn!(key = %manifest_cache_key, error = %cache_err, "Failed to cache manifest after pull.");
                                        }
                                    }
                                    Err(ser_err) => {
                                        warn!(key = %manifest_cache_key, error = %ser_err, "Failed to serialize manifest for caching after pull.");
                                    }
                                }
                                (pulled_manifest, pulled_digest)
                            }
                        }
                    }
                    Ok(None) => {
                        // Cache miss
                        info!(key = %manifest_cache_key, "Manifest cache miss (no entry found). Will pull from registry.");
                        manifest_source_temp = ManifestSource::FromRegistry;
                        let (pulled_manifest, pulled_digest) = oci_client
                            .pull_image_manifest(&base_ref, &pull_auth)
                            .await
                            .map_err(pull_err_mapper)?;
                        match serde_json::to_vec(&(pulled_manifest.clone(), pulled_digest.clone()))
                        {
                            Ok(data_to_cache) => {
                                if let Err(cache_err) =
                                    cache.put_blob(&manifest_cache_key, &data_to_cache).await
                                {
                                    warn!(key = %manifest_cache_key, error = %cache_err, "Failed to cache manifest after pull.");
                                }
                            }
                            Err(ser_err) => {
                                warn!(key = %manifest_cache_key, error = %ser_err, "Failed to serialize manifest for caching after pull.");
                            }
                        }
                        (pulled_manifest, pulled_digest)
                    }
                    Err(e) => {
                        // Cache error
                        warn!(key = %manifest_cache_key, error = %e, "Error reading manifest from cache. Will pull from registry.");
                        manifest_source_temp = ManifestSource::FromRegistry;
                        let (pulled_manifest, pulled_digest) = oci_client
                            .pull_image_manifest(&base_ref, &pull_auth)
                            .await
                            .map_err(pull_err_mapper)?;
                        match serde_json::to_vec(&(pulled_manifest.clone(), pulled_digest.clone()))
                        {
                            Ok(data_to_cache) => {
                                if let Err(cache_err) =
                                    cache.put_blob(&manifest_cache_key, &data_to_cache).await
                                {
                                    warn!(key = %manifest_cache_key, error = %cache_err, "Failed to cache manifest after pull.");
                                }
                            }
                            Err(ser_err) => {
                                warn!(key = %manifest_cache_key, error = %ser_err, "Failed to serialize manifest for caching after pull.");
                            }
                        }
                        (pulled_manifest, pulled_digest)
                    }
                }
            } else {
                // PullPolicy::Always
                info!(key = %manifest_cache_key, "PullPolicy::Always. Pulling manifest from registry.");
                manifest_source_temp = ManifestSource::FromRegistry;
                let (pulled_manifest, pulled_digest) = oci_client
                    .pull_image_manifest(&base_ref, &pull_auth)
                    .await
                    .map_err(pull_err_mapper)?;

                match serde_json::to_vec(&(pulled_manifest.clone(), pulled_digest.clone())) {
                    Ok(data_to_cache) => {
                        if let Err(cache_err) =
                            cache.put_blob(&manifest_cache_key, &data_to_cache).await
                        {
                            warn!(key = %manifest_cache_key, error = %cache_err, "Failed to cache manifest after pull.");
                        } else {
                            debug!(key = %manifest_cache_key, "Successfully cached manifest after pull.");
                        }
                    }
                    Err(ser_err) => {
                        warn!(key = %manifest_cache_key, error = %ser_err, "Failed to serialize manifest for caching after pull.");
                    }
                }
                (pulled_manifest, pulled_digest)
            };

            info!(manifest_digest = %resolved_manifest_digest_str_temp, "Successfully obtained base ImageManifest (source: {:?}).", manifest_source_temp);

            // Fetch config blob using the resolved manifest
            let config_descriptor = &base_image_manifest_resolved.config;
            info!(config_digest = %config_descriptor.digest, "Fetching base image config blob.");
            let config_data = match cache.get_blob(&config_descriptor.digest).await? {
                Some(data) => {
                    info!(config_digest = %config_descriptor.digest, "Base image config blob found in cache.");
                    data
                }
                None => {
                    info!(config_digest = %config_descriptor.digest, "Base image config blob not in cache, pulling.");
                    let mut pulled_data = Vec::new();
                    oci_client
                        .pull_blob(&base_ref, config_descriptor, &mut pulled_data)
                        .await
                        .map_err(|e| Error::ImagePull {
                            image_ref: base_image_ref_str.to_string(),
                            message: format!(
                                "Failed to pull config blob {}",
                                config_descriptor.digest
                            ),
                            source: Some(Box::new(e)),
                        })?;
                    cache
                        .put_blob(&config_descriptor.digest, &pulled_data)
                        .await?;
                    pulled_data
                }
            };

            let oci_client_config_for_imagedata = OciClientConfig {
                data: config_data,
                media_type: config_descriptor.media_type.clone(),
                annotations: config_descriptor.annotations.clone(),
            };

            info!(
                num_base_layers = base_image_manifest_resolved.layers.len(),
                "Fetching base image layer blobs."
            );
            let mut oci_client_layers = Vec::new();
            for (idx, layer_descriptor) in base_image_manifest_resolved.layers.iter().enumerate() {
                let layer_data = match cache.get_blob(&layer_descriptor.digest).await? {
                    Some(data) => {
                        info!(layer_idx = idx, layer_digest = %layer_descriptor.digest, "Base layer blob found in cache.");
                        data
                    }
                    None => {
                        info!(layer_idx = idx, layer_digest = %layer_descriptor.digest, "Base layer blob not in cache, pulling.");
                        let mut pulled_data = Vec::new();
                        oci_client
                            .pull_blob(&base_ref, layer_descriptor, &mut pulled_data)
                            .await
                            .map_err(|e| Error::ImagePull {
                                image_ref: base_image_ref_str.to_string(),
                                message: format!(
                                    "Failed to pull layer blob {}",
                                    layer_descriptor.digest
                                ),
                                source: Some(Box::new(e)),
                            })?;
                        cache
                            .put_blob(&layer_descriptor.digest, &pulled_data)
                            .await?;
                        pulled_data
                    }
                };
                oci_client_layers.push(OciImageLayer {
                    data: layer_data,
                    media_type: layer_descriptor.media_type.clone(),
                    annotations: layer_descriptor.annotations.clone(),
                });
            }

            // Assign to outer scope variables
            manifest_source = manifest_source_temp;
            resolved_manifest_digest_str = resolved_manifest_digest_str_temp;

            base_image_data = Some(OciImageData {
                layers: oci_client_layers,
                digest: Some(resolved_manifest_digest_str.clone()),
                config: oci_client_config_for_imagedata,
                manifest: Some(base_image_manifest_resolved.clone()),
            });
            default_image_name = format!("{}:latest", base_ref.repository());
        } else {
            // Building from scratch (no base image)
            info!("Building from scratch (no base image)");
            base_image_data = None;
            manifest_source = ManifestSource::NotApplicable;
            resolved_manifest_digest_str = String::new();
            default_image_name = "scratch:latest".to_string();
        }

        let build_artifacts_dir = tempfile::tempdir().map_err(|e| Error::Io {
            message: "Failed to create temporary directory for image build artifacts".to_string(),
            source: e,
        })?;

        // Build image configuration (either from base or from scratch)
        let current_config: ImageConfiguration = if let Some(ref base_data) = base_image_data {
            // Start from base image config
            let mut config: ImageConfiguration = serde_json::from_slice(&base_data.config.data)
                .map_err(|e| Error::ImageConfig {
                    message: "Failed to parse base image configuration".to_string(),
                    source: Some(Box::new(e)),
                })?;

            // Add new layers to diff_ids
            let mut all_diff_ids: Vec<String> = config.rootfs().diff_ids().clone();
            for new_layer in &self.layers {
                all_diff_ids.push(new_layer.diff_id().to_string());
            }
            *config.rootfs_mut().diff_ids_mut() = all_diff_ids;

            // Update process config
            let mut proc_config = config.config().clone().unwrap_or_default();
            if let Some(entrypoint) = self.entrypoint {
                proc_config.set_entrypoint(Some(entrypoint));
            }
            if let Some(cmd) = self.cmd {
                proc_config.set_cmd(Some(cmd));
            } else {
                proc_config.set_cmd(Some(vec![]));
            }
            if let Some(working_dir) = self.working_dir {
                proc_config.set_working_dir(Some(working_dir));
            }
            config.set_os(target_os_for_build.as_str().into());
            config.set_architecture(target_arch_for_build.to_string().as_str().into());
            config.set_config(Some(proc_config));
            config
        } else {
            // Build from scratch - create minimal config
            use oci_spec::image::{ConfigBuilder, ImageConfigurationBuilder, RootFsBuilder};

            let diff_ids: Vec<String> = self
                .layers
                .iter()
                .map(|layer| layer.diff_id().to_string())
                .collect();

            let rootfs = RootFsBuilder::default()
                .typ("layers")
                .diff_ids(diff_ids)
                .build()
                .map_err(|e| Error::ImageConfig {
                    message: format!("Failed to build rootfs: {}", e),
                    source: Some(Box::new(e)),
                })?;

            let mut config_builder = ConfigBuilder::default();
            if let Some(entrypoint) = self.entrypoint {
                config_builder = config_builder.entrypoint(entrypoint);
            }
            if let Some(cmd) = self.cmd {
                config_builder = config_builder.cmd(cmd);
            } else {
                config_builder = config_builder.cmd(Vec::<String>::new());
            }
            if let Some(working_dir) = self.working_dir {
                config_builder = config_builder.working_dir(working_dir);
            }

            let proc_config = config_builder.build().map_err(|e| Error::ImageConfig {
                message: format!("Failed to build config: {}", e),
                source: Some(Box::new(e)),
            })?;

            ImageConfigurationBuilder::default()
                .os(target_os_for_build.as_str())
                .architecture(target_arch_for_build.to_string().as_str())
                .rootfs(rootfs)
                .config(proc_config)
                .build()
                .map_err(|e| Error::ImageConfig {
                    message: format!("Failed to build image configuration: {}", e),
                    source: Some(Box::new(e)),
                })?
        };

        let config_json_bytes =
            serde_json::to_vec(&current_config).map_err(|e| Error::ImageConfig {
                message: "Failed to serialize new image configuration".to_string(),
                source: Some(Box::new(e)),
            })?;
        let config_digest_sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(&config_json_bytes);
            format!("sha256:{:x}", hasher.finalize())
        };

        let mut oci_tar_builder = oci_tar_builder::Builder::default();

        // Add base layers if we have them
        if let Some(ref base_data) = base_image_data {
            for (idx, base_layer_oci) in base_data.layers.iter().enumerate() {
                let temp_layer_path = build_artifacts_dir
                    .path()
                    .join(format!("base_layer_{}.blob", idx));
                std_fs::write(&temp_layer_path, &base_layer_oci.data).map_err(|e| Error::Io {
                    message: format!("Failed to write base layer {} to temp file", idx),
                    source: e,
                })?;
                oci_tar_builder
                    .add_layer_with_media_type(&temp_layer_path, base_layer_oci.media_type.clone());
            }
        }

        // Add new layers
        for new_layer in &self.layers {
            oci_tar_builder.add_layer_with_media_type(
                &new_layer.path().to_path_buf(),
                IMAGE_LAYER_ZSTD_MEDIA_TYPE.to_string(),
            );
        }

        let image_name_and_tag_for_config = self
            .output_image_name_and_tag
            .clone()
            .unwrap_or(default_image_name);
        oci_tar_builder.add_config(current_config.clone(), image_name_and_tag_for_config);

        let (oci_archive_final_path, temp_dir_manager_for_image_struct) = if let Some(output_p) =
            self.output_path
        {
            if let Some(parent_dir) = output_p.parent() {
                if !parent_dir.exists() {
                    std_fs::create_dir_all(parent_dir).map_err(|e| Error::Io {
                        message: format!(
                            "Failed to create parent directory for output OCI archive: {}",
                            parent_dir.display()
                        ),
                        source: e,
                    })?;
                }
            }
            (output_p, None)
        } else {
            let final_oci_temp_dir = tempfile::tempdir().map_err(|e| Error::Io {
                message: "Failed to create temporary directory for final OCI archive".to_string(),
                source: e,
            })?;
            (
                final_oci_temp_dir.path().join("image.oci.tar"),
                Some(final_oci_temp_dir),
            )
        };

        let oci_archive_file =
            std_fs::File::create(&oci_archive_final_path).map_err(|e| Error::OciArchive {
                message: format!(
                    "Failed to create OCI archive file at {}",
                    oci_archive_final_path.display()
                ),
                source: Some(Box::new(e)),
            })?;

        oci_tar_builder
            .build(oci_archive_file)
            .map_err(|e| Error::OciArchive {
                message: format!("OCI tar builder failed: {}", e),
                source: Some(e.into()),
            })?;

        let diagnostics = BuildDiagnostics {
            manifest_source,
            resolved_manifest_digest: resolved_manifest_digest_str.clone(),
        };

        Ok((
            Image {
                oci_archive_path: oci_archive_final_path,
                config_digest: config_digest_sha256,
                _temp_dir_manager: temp_dir_manager_for_image_struct,
            },
            diagnostics,
        ))
    }
}

/// Determines the RegistryAuth by trying environment variables and falling back to Anonymous.
fn determine_registry_auth(reference: &Reference) -> RegistryAuth {
    let host_for_logging = reference.resolve_registry(); // Still useful for logging

    let auth = match (env::var("DOCKER_USERNAME"), env::var("DOCKER_PASSWORD")) {
        (Ok(username), Ok(password)) if !username.is_empty() && !password.is_empty() => {
            info!(
                "Using Docker credentials from DOCKER_USERNAME/PASSWORD env vars for {}",
                host_for_logging
            );
            RegistryAuth::Basic(username, password)
        }
        _ => {
            info!(
                "DOCKER_USERNAME and/or DOCKER_PASSWORD not set or empty. Falling back to anonymous auth for {}.",
                host_for_logging
            );
            RegistryAuth::Anonymous
        }
    };

    auth
}

/// Determines whether to use monolithic push based on the policy and registry hostname.
fn determine_use_monolithic_push(policy: &MonolithicPushPolicy, reference: &Reference) -> bool {
    match policy {
        MonolithicPushPolicy::Always => {
            debug!("MonolithicPushPolicy::Always - using monolithic push");
            true
        }
        MonolithicPushPolicy::Never => {
            debug!("MonolithicPushPolicy::Never - using chunked upload");
            false
        }
        MonolithicPushPolicy::Auto => {
            let registry_host = reference.resolve_registry();
            let use_monolithic = is_registry_requiring_monolithic_push(registry_host);

            if use_monolithic {
                info!(
                    "Registry {} requires monolithic push - enabling monolithic push mode",
                    registry_host
                );
            } else {
                debug!(
                    "Registry {} supports chunked upload - using chunked upload mode",
                    registry_host
                );
            }

            use_monolithic
        }
    }
}

/// Checks if a registry requires monolithic push based on its hostname.
fn is_registry_requiring_monolithic_push(registry_host: &str) -> bool {
    // Google Artifact Registry and Container Registry require monolithic push
    registry_host.ends_with("-docker.pkg.dev") // Google Artifact Registry
        || registry_host == "gcr.io"
        || registry_host.ends_with(".gcr.io") // Google Container Registry
        || registry_host == "us.gcr.io"
        || registry_host == "eu.gcr.io"
        || registry_host == "asia.gcr.io"
}
