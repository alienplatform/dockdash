//! Push a synthetic OCI archive to a local HTTP registry for byte-level validation.
use dockdash::{ClientProtocol, Image, PushOptions};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: cache-evidence-push ARCHIVE LOCAL_REGISTRY_REFERENCE".into());
    }
    let image = Image::from_tarball(&args[1])?;
    let digest = image
        .push(
            &args[2],
            &PushOptions {
                protocol: ClientProtocol::Http,
                ..Default::default()
            },
        )
        .await?;
    println!("{digest}");
    Ok(())
}
