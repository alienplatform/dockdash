use crate::{ClientProtocol, PushOptions, RegistryAuth, Result};
use container_registry::test_support::RunningRegistry;
use container_registry::ContainerRegistry;
use tracing::info;

// Helper to set up and run a local container registry
// Returns the running registry guard and the host string (e.g., "localhost:12345")
pub async fn setup_local_registry() -> Result<(RunningRegistry, String)> {
    info!("Setting up and running local container registry in background...");
    let running_registry = ContainerRegistry::builder()
        .build_for_testing()
        .run_in_background();
    let local_registry_addr = running_registry.bound_addr();
    let local_registry_host = format!("localhost:{}", local_registry_addr.port());
    info!(
        "Local container registry listening on: {}",
        local_registry_host
    );
    Ok((running_registry, local_registry_host))
}

/// Returns default PushOptions for interacting with the local test registry.
pub fn test_push_options() -> PushOptions {
    PushOptions {
        auth: RegistryAuth::Anonymous,
        protocol: ClientProtocol::Http,
        ..Default::default()
    }
}
