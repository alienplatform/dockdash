#![cfg(feature = "test-utils")]

use dockdash::{test_utils, Arch, BlobCache, ClientProtocol, Image, Layer, PullPolicy};
use oci_client::{
    client::{Client, ClientConfig},
    secrets::RegistryAuth,
    Reference,
};
use std::{collections::HashMap, fs::File, io::Read};
#[cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[cfg(unix)]
fn set_cache_writable(path: &Path, writable: bool) {
    if path.is_dir() {
        // Restore the directory before its children so writes can resume afterward.
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if writable { 0o755 } else { 0o555 }),
        )
        .unwrap();
        for entry in fs::read_dir(path).unwrap() {
            set_cache_writable(&entry.unwrap().path(), writable);
        }
    } else {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if writable { 0o644 } else { 0o444 }),
        )
        .unwrap();
    }
}

fn blobs(image: &Image) -> HashMap<String, Vec<u8>> {
    tar::Archive::new(File::open(image.path()).unwrap())
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            (name, data)
        })
        .collect()
}

#[tokio::test]
async fn repeated_pulls_preserve_registry_manifest_config_and_layers() {
    let (_registry, host) = test_utils::setup_local_registry().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cache = BlobCache::with_path(directory.path().to_path_buf()).unwrap();
    let layer = Layer::builder()
        .unwrap()
        .data("/app/data", b"image identity", None)
        .unwrap()
        .build()
        .await
        .unwrap();
    let (source, _) = Image::builder()
        .platform("linux", &Arch::ARM64)
        .layer(layer)
        .entrypoint(vec!["/app/data".to_string()])
        .build()
        .await
        .unwrap();
    let reference = format!("{host}/identity/shared:latest");
    source
        .push(&reference, &test_utils::test_push_options())
        .await
        .unwrap();
    let client = Client::new(ClientConfig {
        protocol: ClientProtocol::Http,
        ..Default::default()
    });
    let reference_parsed = Reference::try_from(reference.as_str()).unwrap();
    let (raw, digest) = client
        .pull_manifest_raw(
            &reference_parsed,
            &RegistryAuth::Anonymous,
            &["application/vnd.oci.image.manifest.v1+json"],
        )
        .await
        .unwrap();
    let original = blobs(&source);
    for (attempt, policy) in [PullPolicy::Always, PullPolicy::Always, PullPolicy::Missing]
        .into_iter()
        .enumerate()
    {
        // Exercise cold, valid warm, and corrupted transfer caches in that order.
        if attempt == 2 {
            cache
                .put_blob(&format!("raw-manifest:{digest}"), b"corrupt")
                .await
                .unwrap();
        }
        #[cfg(unix)]
        if attempt == 1 {
            set_cache_writable(directory.path(), false);
        }
        let result = Image::builder()
            .from(&reference)
            .platform("linux", &Arch::ARM64)
            .protocol(ClientProtocol::Http)
            .pull_policy(policy)
            .blob_cache(cache.clone())
            .build()
            .await;
        #[cfg(unix)]
        if attempt == 1 {
            set_cache_writable(directory.path(), true);
        }
        let (pulled, diagnostics) = result.unwrap();
        assert_eq!(diagnostics.resolved_manifest_digest, digest);
        let archive = blobs(&pulled);
        assert_eq!(
            archive[&format!("blobs/sha256/{}", digest.strip_prefix("sha256:").unwrap())],
            raw
        );
        let registry_manifest: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let descriptors = std::iter::once(&registry_manifest["config"])
            .chain(registry_manifest["layers"].as_array().unwrap());
        for descriptor in descriptors {
            let path = format!(
                "blobs/sha256/{}",
                descriptor["digest"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("sha256:")
                    .unwrap()
            );
            assert_eq!(
                archive[&path], original[&path],
                "registry blob {path} was rewritten"
            );
        }
        assert_eq!(pulled.config_digest(), source.config_digest());
        assert_eq!(
            pulled.read_file("/app/data").await.unwrap().unwrap(),
            b"image identity"
        );
    }
}
