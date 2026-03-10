//! # Dockdash
//!
//! Build and push OCI container images from Rust — no Docker daemon required.
//!
//! Dockdash lets you programmatically create OCI-compliant container images,
//! add layers from files or raw bytes, and push them to any OCI registry.
//! It works anywhere Rust runs: CI pipelines, serverless functions, CLIs, or
//! embedded tooling.
//!
//! ## Quick Start
//!
//! ```no_run
//! use dockdash::{Arch, Image, Layer, PushOptions, Result};
//! use std::path::PathBuf;
//!
//! # async fn example() -> Result<()> {
//! let layer = Layer::builder()?
//!     .file("./my-binary", "./app", Some(0o755))?
//!     .build()
//!     .await?;
//!
//! let (image, _) = Image::builder()
//!     .from("ubuntu:latest")
//!     .platform("linux", &Arch::ARM64)
//!     .layer(layer)
//!     .entrypoint(vec!["/app".to_string()])
//!     .build()
//!     .await?;
//!
//! image.push("registry.example.com/my-app:latest", &PushOptions::default()).await?;
//! # Ok(())
//! # }
//! ```

/// OCI media type for zstd-compressed tar layers
pub(crate) const IMAGE_LAYER_ZSTD_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

mod error;
pub use error::*;

mod layer;
pub use layer::*;

mod image;
pub use image::*;

mod blobcache;
pub use blobcache::*;

pub use oci_client::client::ClientProtocol;
pub use oci_client::secrets::RegistryAuth;
pub use oci_spec::image::Arch;

#[cfg(feature = "test-utils")]
pub mod test_utils;
