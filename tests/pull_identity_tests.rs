#![cfg(feature = "test-utils")]

use dockdash::{test_utils, Arch, BlobCache, ClientProtocol, Image, Layer, PullPolicy};
use oci_client::{
    client::{Client, ClientConfig},
    secrets::RegistryAuth,
    Reference,
};
use std::{collections::HashMap, fs::File, io::Read};

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
    for policy in [PullPolicy::Always, PullPolicy::Always, PullPolicy::Missing] {
        // A corrupt cached transfer must be replaced with registry bytes.
        cache
            .put_blob(&format!("raw-manifest:{digest}"), b"corrupt")
            .await
            .unwrap();
        let (pulled, diagnostics) = Image::builder()
            .from(&reference)
            .platform("linux", &Arch::ARM64)
            .protocol(ClientProtocol::Http)
            .pull_policy(policy)
            .blob_cache(cache.clone())
            .build()
            .await
            .unwrap();
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
