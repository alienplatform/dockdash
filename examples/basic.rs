use dockdash::{Arch, Image, Layer, PushOptions, Result};
use rand::{distr::Alphanumeric, rng, Rng};
use std::path::PathBuf;
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // 1. Create a layer with your application binary
    let layer = Layer::builder()?
        .file("./target/release/my-app", "./my-app", Some(0o755))?
        .build()
        .await?;
    info!(diff_id = %layer.diff_id(), blob_digest = %layer.blob_digest(), "Layer created.");

    // 2. Build the OCI image
    info!("Building image...");

    let (image, _) = Image::builder()
        .from("ubuntu:latest")
        .platform("linux", &Arch::ARM64)
        .layer(layer)
        .entrypoint(vec!["/my-app".to_string()])
        .output_to(PathBuf::from("image.oci.tar"))
        .build()
        .await?;
    info!(path = %image.path().display(), config_digest = %image.config_digest(), "Image built.");

    // 3. Push to ttl.sh (anonymous, ephemeral registry — great for testing)
    info!("Pushing image to ttl.sh...");

    let tag: String = rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();
    let image_name = format!("ttl.sh/dockdash-example-{}:1h", tag);

    let pushed = image.push(&image_name, &PushOptions::default()).await?;

    info!("Image pushed to {} (expires in ~1 hour)", pushed);
    Ok(())
}
