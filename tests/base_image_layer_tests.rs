use dockdash::{test_utils, Arch, BlobCache, Image, Layer, PullPolicy, Result};
use oci_spec::image::ImageConfiguration;
use ocipkg::image::{Image as _, OciArtifact};
use std::io::Read;
use tempfile::tempdir;

/// Keeps the base's entries except whatever holds id 1000 or the name `sandbox`, makes sure
/// `root` exists, and appends `sandbox` at 1000. `id_field` is the passwd/group id column.
fn merge_identity(base: &str, root_line: &str, sandbox_line: &str) -> String {
    let id_field = 2;
    let mut out = String::new();
    if !base.lines().any(|l| l.starts_with("root:")) {
        out.push_str(root_line);
        out.push('\n');
    }
    for line in base.lines().filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first() == Some(&"sandbox") || fields.get(id_field) == Some(&"1000") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(sandbox_line);
    out.push('\n');
    out
}

fn image_config(image: &Image) -> ImageConfiguration {
    let mut archive = OciArtifact::from_oci_archive(image.path()).unwrap();
    let (_, bytes) = archive.get_config().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// (path, uid, gid, mode) of every entry in the image's top layer.
fn top_layer_headers(image: &Image) -> Vec<(String, u64, u64, u32)> {
    let mut archive = OciArtifact::from_oci_archive(image.path()).unwrap();
    let manifest = archive.get_manifest().unwrap();
    let top = manifest.layers().last().unwrap();
    let blob = archive
        .get_blob(&ocipkg::Digest::from_descriptor(top).unwrap())
        .unwrap();
    let mut tar = tar::Archive::new(zstd::Decoder::new(blob.as_slice()).unwrap());
    tar.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let h = e.header().clone();
            let mut sink = Vec::new();
            e.read_to_end(&mut sink).unwrap();
            (
                e.path()
                    .unwrap()
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_string(),
                h.uid().unwrap(),
                h.gid().unwrap(),
                h.mode().unwrap(),
            )
        })
        .collect()
}

/// Reproduces, without running anything inside the base, a Dockerfile that copies a
/// merged /etc/passwd and /etc/group, a root-owned binary and a 1000-owned 0700 directory
/// onto a public base, then sets ENV, EXPOSE, USER and ENTRYPOINT. Needs network access
/// to Docker Hub.
#[tokio::test]
async fn layers_onto_a_public_base_with_owner_modes_and_config() -> Result<()> {
    let cache_dir = tempdir().unwrap();
    let cache = BlobCache::with_path(cache_dir.path().to_path_buf())?;

    let (base, diagnostics) = Image::builder()
        .from("alpine:latest")
        .platform("linux", &Arch::Amd64)
        .blob_cache(cache.clone())
        .build()
        .await?;
    let base_passwd = String::from_utf8(base.read_file("/etc/passwd").await?.unwrap()).unwrap();
    let base_group = String::from_utf8(base.read_file("/etc/group").await?.unwrap()).unwrap();
    assert!(base_passwd.starts_with("root:x:0:0:"), "{base_passwd}");
    assert!(base.read_file("/etc/no-such-file").await?.is_none());
    let base_env = image_config(&base)
        .config()
        .as_ref()
        .and_then(|c| c.env().clone())
        .unwrap_or_default();
    let base_path = base_env
        .iter()
        .find(|e| e.starts_with("PATH="))
        .expect("alpine sets PATH")
        .clone();

    let passwd = merge_identity(
        &base_passwd,
        "root:x:0:0:root:/root:/sbin/nologin",
        "sandbox:x:1000:1000::/sandbox:/sbin/nologin",
    );
    let group = merge_identity(&base_group, "root:x:0:", "sandbox:x:1000:");
    let binary = b"#!/bin/sh\necho stand-in\n";

    let layer = Layer::builder()?
        .blob_cache(cache.clone())
        .data_with_owner("/etc/passwd", passwd.as_bytes(), 0o644, 0, 0)?
        .data_with_owner("/etc/group", group.as_bytes(), 0o644, 0, 0)?
        .data_with_owner("/usr/local/bin/app-agent", binary, 0o755, 0, 0)?
        .empty_directory("/sandbox", 0o700, 1000, 1000)?
        .build()
        .await?;

    // Pinning the base by the digest the first build resolved keeps both builds on one base.
    let base_by_digest = format!(
        "docker.io/library/alpine@{}",
        diagnostics.resolved_manifest_digest
    );
    let (derived, _) = Image::builder()
        .from(&base_by_digest)
        .platform("linux", &Arch::Amd64)
        .blob_cache(cache.clone())
        .layer(layer)
        .env("APP_ROOT", "/sandbox")
        .env("APP_PORT", "8080")
        .expose_port("8080/tcp")
        .user("1000:1000")
        .entrypoint(vec!["/usr/local/bin/app-agent".to_string()])
        .build()
        .await?;

    let (_registry, host) = test_utils::setup_local_registry().await?;
    let target = format!("{host}/derived/app:test");
    derived
        .push(&target, &test_utils::test_push_options())
        .await?;

    let pull_cache_dir = tempdir().unwrap();
    let (pulled, _) = Image::builder()
        .from(&target)
        .platform("linux", &Arch::Amd64)
        .protocol(dockdash::ClientProtocol::Http)
        .pull_policy(PullPolicy::Always)
        .blob_cache(BlobCache::with_path(pull_cache_dir.path().to_path_buf())?)
        .build()
        .await?;

    let config = image_config(&pulled);
    assert_eq!(config.architecture().to_string(), "amd64");
    let process = config.config().as_ref().unwrap();
    let env = process.env().clone().unwrap();
    assert!(env.contains(&base_path), "base PATH kept: {env:?}");
    assert!(env.contains(&"APP_ROOT=/sandbox".to_string()), "{env:?}");
    assert!(env.contains(&"APP_PORT=8080".to_string()), "{env:?}");
    assert_eq!(process.user().as_deref(), Some("1000:1000"));
    assert!(process
        .exposed_ports()
        .as_ref()
        .unwrap()
        .contains(&"8080/tcp".to_string()));
    assert_eq!(
        process.entrypoint().as_deref(),
        Some(&["/usr/local/bin/app-agent".to_string()][..])
    );
    assert_eq!(process.cmd(), &None);

    let pulled_passwd = String::from_utf8(pulled.read_file("/etc/passwd").await?.unwrap()).unwrap();
    assert_eq!(pulled_passwd, passwd);
    assert!(pulled_passwd.starts_with("root:x:0:0:"));
    assert!(pulled_passwd.ends_with("sandbox:x:1000:1000::/sandbox:/sbin/nologin\n"));
    for base_line in base_passwd.lines().filter(|l| !l.is_empty()) {
        assert!(
            pulled_passwd.contains(base_line),
            "base user kept: {base_line}"
        );
    }
    let pulled_group = String::from_utf8(pulled.read_file("/etc/group").await?.unwrap()).unwrap();
    assert!(
        pulled_group.ends_with("sandbox:x:1000:\n"),
        "{pulled_group}"
    );
    assert_eq!(
        pulled.read_file("/usr/local/bin/app-agent").await?.unwrap(),
        binary
    );
    // A base file the layer did not touch is still read from the base's own layers.
    assert!(pulled.read_file("/etc/alpine-release").await?.is_some());
    // Alpine's /etc/os-release is a symlink, which read_file refuses rather than follows.
    assert!(matches!(
        pulled.read_file("/etc/os-release").await,
        Err(dockdash::Error::InvalidPath { .. })
    ));

    let headers = top_layer_headers(&pulled);
    let owner_mode = |path: &str| {
        let hits: Vec<_> = headers.iter().filter(|h| h.0 == path).collect();
        assert_eq!(hits.len(), 1, "one entry for {path}: {headers:?}");
        (hits[0].1, hits[0].2, hits[0].3)
    };
    assert_eq!(owner_mode("etc/passwd"), (0, 0, 0o644));
    assert_eq!(owner_mode("etc/group"), (0, 0, 0o644));
    assert_eq!(owner_mode("usr/local/bin/app-agent"), (0, 0, 0o755));
    assert_eq!(owner_mode("sandbox"), (1000, 1000, 0o700));
    Ok(())
}
