<p align="center">
  <h1 align="center">dockdash</h1>
  <p align="center">
    Build and push OCI container images from Rust — no Docker daemon required.
  </p>
</p>

<p align="center">
  <a href="https://crates.io/crates/dockdash"><img src="https://img.shields.io/crates/v/dockdash.svg" alt="crates.io"></a>
  <a href="https://docs.rs/dockdash"><img src="https://docs.rs/dockdash/badge.svg" alt="docs.rs"></a>
  <a href="https://github.com/alienplatform/dockdash/actions"><img src="https://github.com/alienplatform/dockdash/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
</p>

---

**Dockdash** is a Rust library for building and pushing OCI-compliant container images — without needing Docker installed. It's fast, lightweight, and works anywhere Rust runs: CI pipelines, serverless functions, CLIs, or embedded tooling.

## Why Dockdash?

- **No Docker daemon** — build images in environments where Docker isn't available (serverless, sandboxed CI, etc.)
- **Fast** — native Rust performance with zstd layer compression and content-addressable blob caching
- **Simple API** — intuitive builder pattern to create layers, assemble images, and push to any OCI registry
- **Multi-arch support** — build images for `amd64`, `arm64`, and other architectures
- **Layer caching** — content-addressable local blob cache for fast incremental builds
- **Registry push** — push directly to Docker Hub, ECR, GCR, ACR, GitHub Container Registry, or any OCI-compliant registry
- **Authentication** — supports anonymous, basic auth, and token-based registry authentication

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
dockdash = "0.1"
tokio = { version = "1", features = ["full"] }
```

Build and push an image:

```rust
use dockdash::{Arch, Image, Layer, PushOptions, Result};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Create a layer from a local binary
    let layer = Layer::builder()?
        .file("./target/release/my-app", "./my-app", Some(0o755))?
        .build()
        .await?;

    // Build the OCI image
    let (image, _) = Image::builder()
        .from("ubuntu:latest")
        .platform("linux", &Arch::ARM64)
        .layer(layer)
        .entrypoint(vec!["/my-app".to_string()])
        .output_to(PathBuf::from("image.oci.tar"))
        .build()
        .await?;

    // Push to a registry
    image.push("my-registry.com/my-app:latest", &PushOptions::default()).await?;

    Ok(())
}
```

## Features

### Layer Builder

Create layers from files or raw data:

```rust
// From a file on disk
let layer = Layer::builder()?
    .file("./my-binary", "./app/my-binary", Some(0o755))?
    .build()
    .await?;

// From raw bytes
let layer = Layer::builder()?
    .data("./app/config.toml", config_bytes, None)?
    .build()
    .await?;

// Multiple files in one layer
let layer = Layer::builder()?
    .file("./binary", "./app/binary", Some(0o755))?
    .file("./config.toml", "./app/config.toml", None)?
    .data("./app/version.txt", b"1.0.0", None)?
    .build()
    .await?;
```

### Image Builder

Compose images from a base and custom layers:

```rust
let (image, diagnostics) = Image::builder()
    .from("alpine:3.19")
    .platform("linux", &Arch::Amd64)
    .layer(app_layer)
    .layer(config_layer)
    .entrypoint(vec!["/app/server".to_string()])
    .working_dir("/app")
    .build()
    .await?;
```

### Blob Caching

Speed up repeated builds with content-addressable caching:

```rust
use dockdash::BlobCache;

// Default location: ~/.dockdash/cache/blobs
let cache = BlobCache::new()?;

// Or use a custom cache directory (path is used as-is)
let cache = BlobCache::with_path("/my/custom/cache".into())?;

let layer = Layer::builder()?
    .blob_cache(cache.clone())
    .file("./my-binary", "./app/my-binary", Some(0o755))?
    .build()
    .await?;

let (image, _) = Image::builder()
    .from("alpine:latest")
    .blob_cache(cache)
    .layer(layer)
    .build()
    .await?;
```

### Registry Authentication

```rust
use dockdash::{RegistryAuth, PushOptions, ClientProtocol};

// Anonymous (e.g., ttl.sh)
let opts = PushOptions::default();

// Basic auth
let opts = PushOptions {
    auth: RegistryAuth::Basic("user".into(), "pass".into()),
    ..Default::default()
};

// HTTP (for local registries)
let opts = PushOptions {
    protocol: ClientProtocol::Http,
    ..Default::default()
};
```

## Use Cases

- **CI/CD pipelines** — build container images without Docker-in-Docker or privileged containers
- **Serverless functions** — dynamically build and push images from Lambda, Cloud Functions, etc.
- **CLI tools** — embed container image building into your Rust CLI
- **Platform tooling** — build images as part of a deployment platform without requiring Docker on the host
- **Edge computing** — build images on resource-constrained devices

## Testing

```bash
# Unit tests
cargo test

# Integration tests (requires Docker for Bollard-based tests)
cargo test --features test-utils
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
