use oci_client::{
    client::{Client, ClientConfig},
    manifest::{
        ImageIndexEntry, OciDescriptor, OciImageIndex, OciImageManifest, OciManifest, Platform,
        IMAGE_CONFIG_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
    },
    secrets::RegistryAuth,
    Reference, RegistryOperation,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::fs;
use tracing::{debug, info, instrument};

use crate::image::{determine_use_monolithic_push, MonolithicPushPolicy};
use crate::{ClientProtocol, Error, Layer, Result};

/// A concrete operating-system and CPU-architecture target in an image index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemotePlatform {
    /// OCI operating system name, such as `linux`.
    pub os: String,
    /// OCI architecture name, such as `amd64` or `arm64`.
    pub architecture: String,
    /// Optional OCI architecture variant.
    pub variant: Option<String>,
}

impl RemotePlatform {
    /// Creates a Linux platform target.
    pub fn linux(architecture: impl Into<String>) -> Self {
        Self {
            os: "linux".to_string(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    fn matches(&self, platform: &Platform) -> bool {
        platform.os == self.os
            && platform.architecture == self.architecture
            && self.variant == platform.variant
    }

    fn as_oci_platform(&self) -> Platform {
        Platform {
            architecture: self.architecture.clone(),
            os: self.os.clone(),
            os_version: None,
            os_features: None,
            variant: self.variant.clone(),
            features: None,
        }
    }
}

/// Authentication and transport settings for registry-native image derivation.
#[derive(Clone, Debug)]
pub struct RemoteDeriveOptions {
    /// Credentials used to read the base image.
    pub source_auth: RegistryAuth,
    /// Credentials used to mount blobs and publish the derived image.
    pub target_auth: RegistryAuth,
    /// Registry transport protocol.
    pub protocol: ClientProtocol,
    /// Upload strategy for the new, small blobs.
    pub monolithic_push: MonolithicPushPolicy,
}

impl Default for RemoteDeriveOptions {
    fn default() -> Self {
        Self {
            source_auth: RegistryAuth::Anonymous,
            target_auth: RegistryAuth::Anonymous,
            protocol: ClientProtocol::Https,
            monolithic_push: MonolithicPushPolicy::Auto,
        }
    }
}

/// Publication result for a registry-native derived multi-platform image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDerivedImage {
    /// Target tag supplied by the caller.
    pub reference: String,
    /// Immutable digest of the published image index.
    pub digest: String,
    /// Number of base filesystem-layer bytes downloaded by this operation.
    /// This is always zero; only the small image configuration is read.
    pub base_layer_bytes_downloaded: u64,
    /// Number of new layer and image-config bytes uploaded to the target registry.
    /// Blobs already present in the target repository are not counted.
    pub bytes_uploaded: u64,
}

/// Derives a multi-platform image by mounting every base filesystem layer and
/// appending one small layer, without downloading any base filesystem layer.
///
/// The source and target registries must support OCI cross-repository blob
/// mounting when the repositories differ. Failure to mount is returned instead
/// of silently downloading a potentially large base image.
#[instrument(skip(layer, options), fields(base = base_reference, target = target_reference))]
pub async fn derive_remote_image(
    base_reference: &str,
    target_reference: &str,
    platforms: &[RemotePlatform],
    layer: &Layer,
    options: &RemoteDeriveOptions,
) -> Result<RemoteDerivedImage> {
    if platforms.is_empty() {
        return Err(generic_error("at least one target platform is required"));
    }

    let base = parse_reference(base_reference, "base")?;
    let target = parse_reference(target_reference, "target")?;
    let target_tag = target
        .tag()
        .ok_or_else(|| generic_error("target reference must contain a tag"))?;
    if target.digest().is_some() {
        return Err(generic_error("target reference cannot contain a digest"));
    }

    let client = Client::new(ClientConfig {
        protocol: options.protocol.clone(),
        use_monolithic_push: determine_use_monolithic_push(&options.monolithic_push, &target),
        ..Default::default()
    });
    client
        .auth(&base, &options.source_auth, RegistryOperation::Pull)
        .await
        .map_err(|error| registry_error("authenticate to the base registry", error))?;
    client
        .auth(&target, &options.target_auth, RegistryOperation::Push)
        .await
        .map_err(|error| registry_error("authenticate to the target registry", error))?;

    let (base_manifest, _) = client
        .pull_manifest(&base, &options.source_auth)
        .await
        .map_err(|error| registry_error("read the base image manifest", error))?;
    let layer_bytes = fs::read(layer.path()).await.map_err(|source| Error::Io {
        message: format!("Failed to read derived layer {}", layer.path().display()),
        source,
    })?;
    let layer_descriptor = OciDescriptor {
        media_type: layer.media_type().to_string(),
        digest: layer.blob_digest().to_string(),
        size: i64::try_from(layer_bytes.len())
            .map_err(|_| generic_error("derived layer is too large for an OCI descriptor"))?,
        urls: None,
        annotations: None,
    };
    let mut uploaded_bytes =
        ensure_blob(&client, &target, &target, &layer_descriptor, &layer_bytes).await?;

    let mut derived_entries = Vec::with_capacity(platforms.len());
    for platform in platforms {
        let base_image =
            resolve_platform_manifest(&client, &base, &base_manifest, platform, options).await?;
        for descriptor in &base_image.layers {
            mount_base_blob(&client, &target, &base, descriptor).await?;
        }

        let mut base_config = Vec::new();
        client
            .pull_blob(&base, &base_image.config, &mut base_config)
            .await
            .map_err(|error| registry_error("read the base image config", error))?;
        let derived_config = append_diff_id(&base_config, layer.diff_id())?;
        let config_media_type = if base_image.config.media_type.is_empty() {
            IMAGE_CONFIG_MEDIA_TYPE
        } else {
            &base_image.config.media_type
        };
        let config_descriptor = descriptor_for_bytes(config_media_type, &derived_config)?;
        uploaded_bytes += ensure_blob(
            &client,
            &target,
            &target,
            &config_descriptor,
            &derived_config,
        )
        .await?;

        let mut layers = base_image.layers;
        layers.push(layer_descriptor.clone());
        let derived_manifest = OciImageManifest {
            schema_version: 2,
            media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
            config: config_descriptor,
            layers,
            subject: None,
            artifact_type: None,
            annotations: base_image.annotations,
        };
        let architecture_reference = parse_reference(
            &format!(
                "{}/{}:{}-{}-{}{}",
                target.registry(),
                target.repository(),
                target_tag,
                platform.os,
                platform.architecture,
                platform
                    .variant
                    .as_ref()
                    .map(|variant| format!("-{variant}"))
                    .unwrap_or_default()
            ),
            "derived architecture",
        )?;
        let derived_manifest = OciManifest::Image(derived_manifest);
        let manifest_bytes = canonical_manifest_bytes(&derived_manifest)?;
        let digest = format!("sha256:{:x}", Sha256::digest(&manifest_bytes));
        client
            .push_manifest_raw(
                &architecture_reference,
                manifest_bytes.clone(),
                derived_manifest
                    .content_type()
                    .parse()
                    .expect("OCI media type is valid"),
            )
            .await
            .map_err(|error| registry_error("publish a derived architecture manifest", error))?;
        derived_entries.push(ImageIndexEntry {
            media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
            digest,
            size: i64::try_from(manifest_bytes.len()).map_err(|_| {
                generic_error("derived manifest is too large for an OCI descriptor")
            })?,
            platform: Some(platform.as_oci_platform()),
            annotations: None,
        });
    }

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: derived_entries,
        artifact_type: None,
        annotations: None,
    };
    let index = OciManifest::ImageIndex(index);
    let index_bytes = canonical_manifest_bytes(&index)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&index_bytes));
    client
        .push_manifest_raw(
            &target,
            index_bytes,
            index
                .content_type()
                .parse()
                .expect("OCI media type is valid"),
        )
        .await
        .map_err(|error| registry_error("publish the derived image index", error))?;

    info!(%digest, uploaded_bytes, "Published registry-native derived image");
    Ok(RemoteDerivedImage {
        reference: target_reference.to_string(),
        digest,
        base_layer_bytes_downloaded: 0,
        bytes_uploaded: uploaded_bytes,
    })
}

async fn resolve_platform_manifest(
    client: &Client,
    base: &Reference,
    manifest: &OciManifest,
    platform: &RemotePlatform,
    options: &RemoteDeriveOptions,
) -> Result<OciImageManifest> {
    match manifest {
        OciManifest::Image(image) => {
            let actual = platform_from_image_config(client, base, image).await?;
            if &actual != platform {
                return Err(generic_error(format!(
                    "base image is {}/{}, not {}/{}",
                    actual.os, actual.architecture, platform.os, platform.architecture
                )));
            }
            Ok(image.clone())
        }
        OciManifest::ImageIndex(index) => {
            let entry = index
                .manifests
                .iter()
                .find(|entry| {
                    entry
                        .platform
                        .as_ref()
                        .is_some_and(|value| platform.matches(value))
                })
                .ok_or_else(|| {
                    generic_error(format!(
                        "base image has no {}/{} manifest",
                        platform.os, platform.architecture
                    ))
                })?;
            let reference = base.clone_with_digest(entry.digest.clone());
            let (manifest, _) = client
                .pull_image_manifest(&reference, &options.source_auth)
                .await
                .map_err(|error| registry_error("read a platform base manifest", error))?;
            Ok(manifest)
        }
    }
}

async fn platform_from_image_config(
    client: &Client,
    base: &Reference,
    manifest: &OciImageManifest,
) -> Result<RemotePlatform> {
    let mut bytes = Vec::new();
    client
        .pull_blob(base, &manifest.config, &mut bytes)
        .await
        .map_err(|error| registry_error("read a single-platform base config", error))?;
    let config: Value = serde_json::from_slice(&bytes).map_err(|source| Error::ImageConfig {
        message: "Failed to parse base image configuration".to_string(),
        source: Some(Box::new(source)),
    })?;
    Ok(RemotePlatform {
        os: json_string(&config, "os")?.to_string(),
        architecture: json_string(&config, "architecture")?.to_string(),
        variant: config
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn mount_base_blob(
    client: &Client,
    target: &Reference,
    source: &Reference,
    descriptor: &OciDescriptor,
) -> Result<()> {
    client
        .mount_blob(target, source, &descriptor.digest)
        .await
        .map_err(|error| {
            registry_error(
                format!(
                    "mount base layer {} from repository {}; registry-native derivation never downloads base layers",
                    descriptor.digest,
                    source.repository()
                ),
                error,
            )
        })?;
    debug!(digest = descriptor.digest, "Mounted existing base layer");
    Ok(())
}

async fn ensure_blob(
    client: &Client,
    target: &Reference,
    mount_source: &Reference,
    descriptor: &OciDescriptor,
    bytes: &[u8],
) -> Result<u64> {
    if client
        .mount_blob(target, mount_source, &descriptor.digest)
        .await
        .is_ok()
    {
        return Ok(0);
    }
    client
        .push_blob(target, bytes, &descriptor.digest)
        .await
        .map_err(|error| registry_error(format!("upload blob {}", descriptor.digest), error))?;
    Ok(bytes.len() as u64)
}

fn append_diff_id(base: &[u8], diff_id: &str) -> Result<Vec<u8>> {
    let mut config: Value = serde_json::from_slice(base).map_err(|source| Error::ImageConfig {
        message: "Failed to parse base image configuration".to_string(),
        source: Some(Box::new(source)),
    })?;
    let diff_ids = config
        .get_mut("rootfs")
        .and_then(|rootfs| rootfs.get_mut("diff_ids"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| generic_error("base image config has no rootfs.diff_ids array"))?;
    diff_ids.push(Value::String(diff_id.to_string()));
    let history = config
        .as_object_mut()
        .ok_or_else(|| generic_error("base image config is not a JSON object"))?
        .entry("history")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| generic_error("base image config history is not an array"))?;
    history.push(serde_json::json!({
        "created_by": "dockdash remote derived layer"
    }));
    serde_json::to_vec(&config).map_err(|source| Error::ImageConfig {
        message: "Failed to serialize derived image configuration".to_string(),
        source: Some(Box::new(source)),
    })
}

fn descriptor_for_bytes(media_type: &str, bytes: &[u8]) -> Result<OciDescriptor> {
    Ok(OciDescriptor {
        media_type: media_type.to_string(),
        digest: format!("sha256:{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len())
            .map_err(|_| generic_error("blob is too large for an OCI descriptor"))?,
        urls: None,
        annotations: None,
    })
}

fn canonical_manifest_bytes(manifest: &OciManifest) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut bytes, olpc_cjson::CanonicalFormatter::new());
    manifest
        .serialize(&mut serializer)
        .map_err(|source| Error::ImageConfig {
            message: "Failed to serialize OCI manifest".to_string(),
            source: Some(Box::new(source)),
        })?;
    Ok(bytes)
}

fn json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| generic_error(format!("base image config has no string field '{key}'")))
}

fn parse_reference(value: &str, kind: &str) -> Result<Reference> {
    value.parse().map_err(|source| Error::Generic {
        message: format!("Invalid {kind} image reference '{value}'"),
        source: Some(Box::new(source)),
    })
}

fn registry_error(
    operation: impl Into<String>,
    source: oci_client::errors::OciDistributionError,
) -> Error {
    Error::Generic {
        message: format!("Failed to {}", operation.into()),
        source: Some(Box::new(source)),
    }
}

fn generic_error(message: impl Into<String>) -> Error {
    Error::Generic {
        message: message.into(),
        source: None,
    }
}
