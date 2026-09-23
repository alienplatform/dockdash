use dockdash::{
    derive_remote_image, Arch, ClientProtocol, Image, Layer, PushOptions, RemoteDeriveOptions,
    RemotePlatform, Result,
};
use oci_client::{
    client::{Client, ClientConfig},
    manifest::{
        ImageIndexEntry, OciImageIndex, OciManifest, Platform, OCI_IMAGE_INDEX_MEDIA_TYPE,
        OCI_IMAGE_MEDIA_TYPE,
    },
    secrets::RegistryAuth,
    Reference, RegistryOperation,
};
use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct RegistryContainer(String);

impl Drop for RegistryContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn start_registry() -> (RegistryContainer, String) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let name = format!("dockdash-remote-test-{}", std::process::id());
    let status = Command::new("docker")
        .args([
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            &format!("127.0.0.1:{port}:5000"),
            "registry:2",
        ])
        .status()
        .expect("Docker is required for the remote derivation integration test");
    assert!(status.success(), "failed to start registry:2");
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "registry:2 did not become ready");
        thread::sleep(Duration::from_millis(50));
    }
    (RegistryContainer(name), format!("localhost:{port}"))
}

#[tokio::test]
async fn derives_multi_platform_image_without_copying_base_layers() -> Result<()> {
    let (_registry, host) = start_registry();
    let auth = RegistryAuth::Anonymous;
    let protocol = ClientProtocol::Http;
    let client = Client::new(ClientConfig {
        protocol: protocol.clone(),
        ..Default::default()
    });

    let base_repository = format!("{host}/operators/base");
    let mut entries = Vec::new();
    let mut base_layer_digest = None;
    for (architecture, arch) in [("amd64", Arch::Amd64), ("arm64", Arch::ARM64)] {
        let base_layer = Layer::builder()?
            .data("base.txt", b"large immutable operator layer", None)?
            .build()
            .await?;
        base_layer_digest = Some(base_layer.blob_digest().to_string());
        let reference = format!("{base_repository}:{architecture}");
        let (image, _) = Image::builder()
            .platform("linux", &arch)
            .layer(base_layer)
            .entrypoint(vec!["/operator".to_string()])
            .build()
            .await?;
        image
            .push(
                &reference,
                &PushOptions {
                    auth: auth.clone(),
                    protocol: protocol.clone(),
                    ..Default::default()
                },
            )
            .await?;
        let reference: Reference = reference.parse().unwrap();
        let (bytes, digest) = client
            .pull_manifest_raw(&reference, &auth, &[OCI_IMAGE_MEDIA_TYPE])
            .await
            .unwrap();
        entries.push(ImageIndexEntry {
            media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
            digest,
            size: bytes.len() as i64,
            platform: Some(Platform {
                architecture: architecture.to_string(),
                os: "linux".to_string(),
                os_version: None,
                os_features: None,
                variant: None,
                features: None,
            }),
            annotations: None,
        });
    }

    let base = format!("{base_repository}:release");
    let base_ref: Reference = base.parse().unwrap();
    client
        .auth(&base_ref, &auth, RegistryOperation::Push)
        .await
        .unwrap();
    client
        .push_manifest(
            &base_ref,
            &OciManifest::ImageIndex(OciImageIndex {
                schema_version: 2,
                media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
                manifests: entries,
                artifact_type: None,
                annotations: None,
            }),
        )
        .await
        .unwrap();

    let packaged_config = br#"{"display_name":"Acme","label_domain":"acme.example"}"#;
    let config_layer = Layer::builder()?
        .data(
            "/etc/alien/operator-config.json",
            packaged_config,
            Some(0o444),
        )?
        .build()
        .await?;
    let target = format!("{host}/operators/projects:acme-v1");
    let options = RemoteDeriveOptions {
        source_auth: auth.clone(),
        target_auth: auth.clone(),
        protocol,
        ..Default::default()
    };
    let platforms = [
        RemotePlatform::linux("amd64"),
        RemotePlatform::linux("arm64"),
    ];

    let first = derive_remote_image(&base, &target, &platforms, &config_layer, &options).await?;
    let second = derive_remote_image(&base, &target, &platforms, &config_layer, &options).await?;

    assert_eq!(
        first.digest, second.digest,
        "derivation must be reproducible"
    );
    assert_eq!(first.base_layer_bytes_downloaded, 0);
    assert!(first.bytes_uploaded > 0);
    assert_eq!(second.bytes_uploaded, 0, "all blobs should already exist");

    let target_ref: Reference = target.parse().unwrap();
    let (derived, _) = client.pull_manifest(&target_ref, &auth).await.unwrap();
    let OciManifest::ImageIndex(derived_index) = derived else {
        panic!("derived tag did not point to an image index");
    };
    assert_eq!(derived_index.manifests.len(), 2);

    for entry in derived_index.manifests {
        let manifest_ref = target_ref.clone_with_digest(entry.digest);
        let (manifest, _) = client
            .pull_image_manifest(&manifest_ref, &auth)
            .await
            .unwrap();
        assert_eq!(manifest.layers.len(), 2);
        assert_eq!(
            manifest.layers[0].digest,
            base_layer_digest.as_deref().unwrap()
        );
        assert_eq!(manifest.layers[1].digest, config_layer.blob_digest());

        let mut compressed = Vec::new();
        client
            .pull_blob(&target_ref, &manifest.layers[1], &mut compressed)
            .await
            .unwrap();
        let mut archive = tar::Archive::new(zstd::Decoder::new(compressed.as_slice()).unwrap());
        let mut found = None;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().as_ref()
                == std::path::Path::new("etc/alien/operator-config.json")
            {
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                found = Some(contents);
            }
        }
        assert_eq!(found.as_deref(), Some(packaged_config.as_slice()));
    }

    Ok(())
}
