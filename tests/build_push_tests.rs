use bollard::container::{
    CreateContainerOptions, LogOutput, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    WaitContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use container_registry::ContainerRegistry;
use dockdash::{
    Arch, BlobCache, ClientProtocol, Image, Layer, ManifestSource, PullPolicy, PushOptions,
    RegistryAuth,
};
use dockdash::{Error, Result};
use futures_util::stream::StreamExt;
use futures_util::TryStreamExt;
use rand::distr::Alphanumeric;
use rand::{rng, Rng};
use sec::Secret;
use std::collections::HashMap;
use std::default::Default;
use std::sync::Arc;
use tempfile::{tempdir, TempDir};
use tracing::{debug, error, info};

use std::sync::Once;

use dockdash::test_utils;

static TEST_SETUP: Once = Once::new();

// Setup function for test environment
fn setup_test_environment() {
    TEST_SETUP.call_once(|| {
        let _ = tracing_subscriber::fmt().try_init();
        info!("Test environment setup complete.");
    });
}

// Helper to set up a temporary blob cache
fn setup_blob_cache() -> Result<(BlobCache, TempDir)> {
    let temp_cache_dir = tempdir().map_err(|e| Error::Io {
        message: e.to_string(),
        source: e,
    })?;
    info!(
        "Using temporary blob cache for test at: {}",
        temp_cache_dir.path().display()
    );
    let shared_blob_cache = BlobCache::with_path(temp_cache_dir.path().to_path_buf())?;
    Ok((shared_blob_cache, temp_cache_dir))
}

// Helper to pull an image using Bollard
async fn pull_image_with_bollard(
    docker: &Docker,
    local_registry_host: &str,
    target_image_repo_namespaced: &str,
    image_tag: &str,
) -> Result<()> {
    info!(
        "Pulling image {}/{}_tag:{} using Bollard...",
        local_registry_host, target_image_repo_namespaced, image_tag
    );

    let registry_creds = bollard::auth::DockerCredentials {
        serveraddress: Some(format!("http://{}", local_registry_host)),
        ..Default::default()
    };

    // Construct the correct from_image for Bollard, including the registry host
    let bollard_from_image = format!("{}/{}", local_registry_host, target_image_repo_namespaced);

    let mut pull_stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: bollard_from_image,
            tag: image_tag.to_string(),
            ..Default::default()
        }),
        None,
        Some(registry_creds),
    );

    while let Some(pull_result) = pull_stream.next().await {
        match pull_result {
            Ok(info_msg) => {
                if let Some(status) = info_msg.status {
                    debug!("Pull status: {}", status);
                }
                if let Some(progress) = info_msg.progress {
                    debug!("Pull progress: {}", progress);
                }
            }
            Err(e) => {
                error!("Error during image pull: {:?}", e);
                return Err(Error::ImagePull {
                    image_ref: format!("{}/{}", local_registry_host, target_image_repo_namespaced),
                    message: format!("Bollard image pull failed for tag {}: {}", image_tag, e),
                    source: Some(Box::new(e)),
                });
            }
        }
    }
    info!(
        "Image {}/{}:{} pulled successfully via Bollard.",
        local_registry_host, target_image_repo_namespaced, image_tag
    );
    Ok(())
}

// Guard for managing Bollard container lifecycle
struct BollardContainerGuard<'a> {
    docker: &'a Docker,
    id: String,
    name: String,
}

impl<'a> BollardContainerGuard<'a> {
    async fn new(
        docker: &'a Docker,
        image_ref: &str,
        container_name: String,
        platform_arch: &str,
        env_param: Option<Vec<String>>,
        override_entrypoint: Option<Vec<String>>,
    ) -> Result<Self> {
        info!(
            "Creating container '{}' from image '{}' with env: {:?}, entrypoint override: {:?}",
            container_name, image_ref, env_param, override_entrypoint
        );

        let processed_env_for_config: Option<Vec<&str>> =
            env_param.as_ref().map(|actual_env_vec_string| {
                actual_env_vec_string
                    .iter()
                    .map(|s_string| s_string.as_str())
                    .collect::<Vec<&str>>()
            });

        let processed_entrypoint_for_config: Option<Vec<&str>> =
            override_entrypoint
                .as_ref()
                .map(|actual_entrypoint_vec_string| {
                    actual_entrypoint_vec_string
                        .iter()
                        .map(|s_string| s_string.as_str())
                        .collect::<Vec<&str>>()
                });

        let container_config = bollard::container::Config {
            image: Some(image_ref),
            tty: Some(true),
            env: processed_env_for_config,
            entrypoint: processed_entrypoint_for_config,
            ..Default::default()
        };

        let create_options = Some(CreateContainerOptions {
            name: container_name.clone(),
            platform: Some(platform_arch.to_string()),
        });

        let response = docker
            .create_container(create_options, container_config)
            .await
            .map_err(|e| Error::Generic {
                message: format!("Failed to create container '{}': {}", container_name, e),
                source: Some(Box::new(e)),
            })?;
        let container_id_str = response.id;
        info!(
            "Container '{}' created with ID: {}",
            container_name, container_id_str
        );

        info!("Starting container ID: {}...", container_id_str);
        docker
            .start_container(&container_id_str, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| Error::Generic {
                message: format!("Failed to start container '{}': {}", container_id_str, e),
                source: Some(Box::new(e)),
            })?;
        info!("Container {} started.", container_id_str);

        Ok(Self {
            docker,
            id: container_id_str,
            name: container_name,
        })
    }

    fn id(&self) -> &str {
        &self.id
    }

    #[allow(dead_code)] // Potentially useful for other tests
    fn name(&self) -> &str {
        &self.name
    }

    async fn logs(&self) -> Result<Vec<String>> {
        info!("Fetching logs for container {}...", self.id);
        let logs_options = Some(LogsOptions::<String> {
            follow: true, // Set to false if you only want logs up to current point
            stdout: true,
            stderr: true,
            timestamps: false,
            ..Default::default()
        });

        let mut log_stream = self.docker.logs(&self.id, logs_options);
        let mut log_lines: Vec<String> = Vec::new();

        // If follow is true, this might block indefinitely if the container keeps running.
        // For a test that expects termination, this should be fine.
        // Or set follow: false and call logs after wait_for_completion.
        // For this specific test, the container echoes and exits, so follow: true is okay.
        while let Some(log_entry_result) = log_stream.next().await {
            match log_entry_result {
                Ok(log_entry) => {
                    let log_message = match log_entry {
                        LogOutput::StdOut { message } => {
                            String::from_utf8_lossy(&message).to_string()
                        }
                        LogOutput::StdErr { message } => {
                            String::from_utf8_lossy(&message).to_string()
                        }
                        LogOutput::Console { message } => {
                            String::from_utf8_lossy(&message).to_string()
                        }
                        _ => continue,
                    };
                    info!("Log: {}", log_message.trim());
                    log_lines.push(log_message.trim().to_string());
                }
                Err(e) => {
                    error!("Error streaming logs: {:?}", e);
                    // Decide if to return error or just log
                }
            }
        }
        info!("Finished fetching logs for container {}.", self.id);
        Ok(log_lines)
    }

    async fn wait_for_completion(&self) -> Result<()> {
        info!("Waiting for container {} to complete...", self.id);
        let wait_options = Some(WaitContainerOptions {
            condition: "not-running",
        });
        let mut wait_stream = self.docker.wait_container(&self.id, wait_options);
        let wait_result = wait_stream.try_next().await.map_err(|e| Error::Generic {
            message: format!("Failed to wait for container '{}': {}", self.id, e),
            source: Some(Box::new(e)),
        })?;

        if let Some(response) = wait_result {
            info!(
                "Container {} exited with status code: {}",
                self.id, response.status_code
            );
            assert_eq!(
                response.status_code, 0,
                "Container did not exit cleanly. Logs might provide more details."
            );
            if let Some(error) = response.error {
                error!("Container exit error: {:?}", error.message);
                return Err(Error::Generic {
                    message: format!(
                        "Container exited with error: {}",
                        error.message.unwrap_or_else(|| "Unknown error".to_string())
                    ),
                    source: None,
                });
            }
        } else {
            return Err(Error::Generic {
                message: format!("Did not receive container exit status for {}.", self.id),
                source: None,
            });
        }
        info!("Container {} completed.", self.id);
        Ok(())
    }

    async fn cleanup(self) -> Result<()> {
        info!("Removing container {} (ID: {})...", self.name, self.id);
        self.docker
            .remove_container(
                &self.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| Error::Generic {
                message: format!("Failed to remove container '{}': {}", self.id, e),
                source: Some(Box::new(e)),
            })?;
        info!("Container {} (ID: {}) removed.", self.name, self.id);
        Ok(())
    }
}

/// Tests the basic end-to-end flow of building an image with a single layer,
/// pushing it to a local registry, pulling it back using Bollard, running it as a container,
/// and verifying its output logs.
#[tokio::test]
async fn test_build_push_pull_run_image() -> Result<()> {
    setup_test_environment();

    // 1. Setup local container registry using the utility
    let (_running_registry, local_registry_host) = test_utils::setup_local_registry().await?;
    // _running_registry is kept to ensure the registry runs for the duration of the test

    // Setup image naming
    let unique_repo_name = generate_unique_image_name();
    let image_tag = "latest";
    let image_namespace = "testns";
    let target_image_repo_namespaced = format!("{}/{}", image_namespace, unique_repo_name);
    let target_image_ref = format!(
        "{}/{}:{}",
        local_registry_host, target_image_repo_namespaced, image_tag
    );
    info!("Target image for test: {}", target_image_ref);

    // Setup custom BlobCache for this test
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;
    // _temp_cache_dir is kept to ensure the cache directory exists for the duration of the test

    // 2. Create a simple layer
    let layer_content = "Hello from Dockdash!";
    let layer_file_path_in_container = "/app/hello.txt";

    info!("Creating layer with content: '{}'", layer_content);
    let layer = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_file_path_in_container, layer_content.as_bytes(), None)?
        .build()
        .await?;
    info!(
        "Layer created successfully. Diff ID: {}, Blob Digest: {}",
        layer.diff_id(),
        layer.blob_digest()
    );

    // 3. Build the image
    let base_image =
        "ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55";
    let platform_os = "linux";
    let platform_arch = Arch::ARM64;
    let entrypoint_cmd = format!(
        "echo 'Container says hi!' && cat {} && echo 'Container finished.'",
        layer_file_path_in_container
    );
    let entrypoint = vec!["/bin/sh".to_string(), "-c".to_string(), entrypoint_cmd];

    info!(
        "Building image from base '{}' with entrypoint: {:?}",
        base_image, entrypoint
    );
    let (image, _) = Image::builder()
        .from(base_image)
        .platform(platform_os, &platform_arch)
        .layer(layer)
        .entrypoint(entrypoint.clone())
        .blob_cache(shared_blob_cache)
        .build()
        .await?;
    info!(
        "Image built successfully. OCI Archive Path: {:?}, Config Digest: {}",
        image.path(),
        image.config_digest()
    );

    // 4. Push the image to local registry
    info!("Pushing image to {}...", target_image_ref);
    let push_opts = test_utils::test_push_options();
    let pushed_image_ref = image.push(&target_image_ref, &push_opts).await?;
    assert_eq!(pushed_image_ref, target_image_ref);
    info!("Image pushed successfully to: {}", pushed_image_ref);

    // 5. Bollard: Connect to Docker
    info!("Connecting to Docker daemon via Bollard...");
    let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Generic {
        message: format!("Failed to connect to Docker: {}", e),
        source: Some(Box::new(e)),
    })?;
    info!(
        "Connected to Docker version: {:?}",
        docker.version().await.map_err(|e| Error::Generic {
            message: format!("Failed to get Docker version: {}", e),
            source: Some(Box::new(e))
        })?
    );

    // 6. Bollard: Pull the image
    pull_image_with_bollard(
        &docker,
        &local_registry_host,
        &target_image_repo_namespaced,
        image_tag,
    )
    .await?;

    // 7. Bollard: Create, start, and manage the container
    let container_name = format!("{}-container", unique_repo_name);
    let container_guard = BollardContainerGuard::new(
        &docker,
        &target_image_ref, // Use the full image ref for creating container
        container_name,
        &platform_arch.to_string(),
        None, // Add None for env
        None, // Add None for override_entrypoint
    )
    .await?;

    // 8. Fetch logs & verify
    let log_lines = container_guard.logs().await?;

    // 9. Bollard: Wait for container to complete
    container_guard.wait_for_completion().await?;

    // 10. Verify logs
    let full_logs = log_lines.join("\n");
    info!("Collected logs:\n{}", full_logs);

    assert!(
        full_logs.contains("Container says hi!"),
        "Log verification failed: 'Container says hi!' not found. Logs: {}",
        full_logs
    );
    assert!(
        full_logs.contains(layer_content),
        "Log verification failed: Layer content '{}' not found. Logs: {}",
        layer_content,
        full_logs
    );
    assert!(
        full_logs.contains("Container finished."),
        "Log verification failed: 'Container finished.' not found. Logs: {}",
        full_logs
    );
    info!("Log content verified successfully.");

    // 11. Bollard: Clean up - remove container (handled by guard's cleanup)
    container_guard.cleanup().await?;

    // _running_registry and _temp_cache_dir will be dropped here,
    // shutting down the server and cleaning up temp storage.
    info!("Local container registry server will shut down and temp cache dir will be removed as guards are dropped.");

    info!("Integration test completed successfully!");
    Ok(())
}

/// Tests a scenario where an image is pushed that shares one layer with a previously pushed image.
/// It verifies that the shared layer is effectively mounted (not re-uploaded) by the registry
/// and that the final image contains the correct combination of shared and new layers.
#[tokio::test]
async fn test_push_with_partial_existing_layers() -> Result<()> {
    setup_test_environment();
    info!("Starting test_push_with_partial_existing_layers");

    let (_running_registry, local_registry_host) = test_utils::setup_local_registry().await?;
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    let platform_os = "linux";
    let platform_arch = Arch::ARM64;
    let image_tag = "latest";
    let image_namespace = "testpartial";

    // Common Layer
    let common_layer_content = "This is a common layer across images.";
    let common_layer_path = "/app/common.txt";
    info!("Creating common layer...");
    let common_layer = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(common_layer_path, common_layer_content.as_bytes(), None)?
        .build()
        .await?;
    info!("Common layer created: {}", common_layer.diff_id());

    // Image A specific layer
    let image_a_specific_content = "Content specific to Image A.";
    let image_a_specific_path = "/app/image_a_only.txt";
    info!("Creating Image A specific layer...");
    let image_a_layer = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(
            image_a_specific_path,
            image_a_specific_content.as_bytes(),
            None,
        )?
        .build()
        .await?;
    info!(
        "Image A specific layer created: {}",
        image_a_layer.diff_id()
    );

    // Build Image A
    let image_a_repo = generate_unique_image_name();
    let image_a_repo_namespaced = format!("{}/{}", image_namespace, image_a_repo);
    let image_a_ref = format!(
        "{}/{}:{}",
        local_registry_host, image_a_repo_namespaced, image_tag
    );
    info!("Building Image A: {}", image_a_ref);
    let (image_a, _) = Image::builder()
        .from("ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55")
        .platform(platform_os, &platform_arch)
        .layer(common_layer)
        .layer(image_a_layer)
        .entrypoint(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo \'--- Image A files ---\' && ls -la /app && echo \'--- Image A common content ---\' && cat {} && echo \'--- Image A specific content ---\' && cat {}", common_layer_path, image_a_specific_path),
        ])
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;
    info!("Image A built. Pushing to {}", image_a_ref);
    image_a
        .push(&image_a_ref, &test_utils::test_push_options())
        .await?;
    info!("Image A pushed.");

    // Image B specific layer
    let image_b_specific_content = "Fresh content for Image B.";
    let image_b_specific_path = "/app/image_b_only.txt";
    info!("Creating Image B specific layer...");
    let image_b_layer = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(
            image_b_specific_path,
            image_b_specific_content.as_bytes(),
            None,
        )?
        .build()
        .await?;
    info!(
        "Image B specific layer created: {}",
        image_b_layer.diff_id()
    );

    // Rebuild common_layer for Image B to ensure identical logical layer
    info!("Rebuilding common layer for Image B...");
    let common_layer_for_b = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(common_layer_path, common_layer_content.as_bytes(), None)?
        .build()
        .await?;
    info!(
        "Common layer for Image B created: {}",
        common_layer_for_b.diff_id()
    );

    // Build Image B (reuses common_layer logic)
    let image_b_repo = generate_unique_image_name();
    let image_b_repo_namespaced = format!("{}/{}", image_namespace, image_b_repo);
    let image_b_ref = format!(
        "{}/{}:{}",
        local_registry_host, image_b_repo_namespaced, image_tag
    );
    info!("Building Image B: {}", image_b_ref);
    let image_b_entrypoint_cmd = format!(
        "echo \\\'Image B running\\\' && echo \\\'--- Image B files ---\\\' && ls -la /app && echo \\\'--- Image B common content ---\\\' && cat {} && echo \\\'--- Image B specific content ---\\\' && cat {} && echo \\\'--- End of Image B content ---\\\'",
        common_layer_path,
        image_b_specific_path
    );
    let (image_b, _) = Image::builder()
        .from("ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55")
        .platform(platform_os, &platform_arch)
        .layer(common_layer_for_b) // Use the rebuilt common layer for image_b
        .layer(image_b_layer)
        .entrypoint(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            image_b_entrypoint_cmd.clone(),
        ])
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;
    info!(
        "Image B built. Pushing to {} (expecting common layer to be mounted)",
        image_b_ref
    );
    image_b
        .push(&image_b_ref, &test_utils::test_push_options())
        .await?;
    info!("Image B pushed.");

    // Verification for Image B
    let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Generic {
        message: format!("Failed to connect to Docker: {}", e),
        source: Some(Box::new(e)),
    })?;
    pull_image_with_bollard(
        &docker,
        &local_registry_host,
        &image_b_repo_namespaced,
        image_tag,
    )
    .await?;

    let container_name_b = format!("{}-container", image_b_repo);
    let guard_b = BollardContainerGuard::new(
        &docker,
        &image_b_ref,
        container_name_b,
        &platform_arch.to_string(),
        None,
        None,
    )
    .await?;
    let logs_b = guard_b.logs().await?;
    guard_b.wait_for_completion().await?;
    guard_b.cleanup().await?;

    let full_logs_b = logs_b.join("\n");
    info!("Image B logs: {}", full_logs_b);
    assert!(
        full_logs_b.contains(common_layer_content),
        "Image B logs should contain common layer content. Logs: {}",
        full_logs_b
    );
    assert!(
        full_logs_b.contains(image_b_specific_content),
        "Image B logs should contain its specific content. Logs: {}",
        full_logs_b
    );
    assert!(
        !full_logs_b.contains(image_a_specific_content),
        "Image B logs should NOT contain Image A specific content. Logs: {}",
        full_logs_b
    );

    info!("test_push_with_partial_existing_layers completed successfully!");
    Ok(())
}

/// Tests a scenario where an image is pushed that consists entirely of layers already present
/// in the registry (from a previous identical push). It verifies that this results in a
/// manifest-only push, and the pulled image is correct.
#[tokio::test]
async fn test_push_with_all_existing_layers() -> Result<()> {
    setup_test_environment();
    info!("Starting test_push_with_all_existing_layers");

    let (_running_registry, local_registry_host) = test_utils::setup_local_registry().await?;
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    let platform_os = "linux";
    let platform_arch = Arch::ARM64;
    let image_tag = "latest";
    let image_namespace = "testall";

    // Layers for the image
    let layer_x_content = "Content for Layer X";
    let layer_x_path = "/app/layer_x.txt";
    let layer_x = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_x_path, layer_x_content.as_bytes(), None)?
        .build()
        .await?;

    let layer_y_content = "Content for Layer Y";
    let layer_y_path = "/app/layer_y.txt";
    let layer_y = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_y_path, layer_y_content.as_bytes(), None)?
        .build()
        .await?;

    // Build and push Image C
    let image_c_repo = generate_unique_image_name();
    let image_c_repo_namespaced = format!("{}/{}", image_namespace, image_c_repo);
    let image_c_ref = format!(
        "{}/{}:{}",
        local_registry_host, image_c_repo_namespaced, image_tag
    );
    let image_c_entrypoint_cmd = format!(
        "echo \\\'Image C running\\\' && echo \\\'--- Image C files ---\\\' && ls -la /app && echo \\\'--- Layer X content ---\\\' && cat {} && echo \\\'--- Layer Y content ---\\\' && cat {} && echo \\\'--- End of Image C content ---\\\'",
        layer_x_path, layer_y_path
    );

    info!("Building Image C: {}", image_c_ref);
    let (image_c, _) = Image::builder()
        .from("ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55")
        .platform(platform_os, &platform_arch)
        .layer(layer_x)
        .layer(layer_y)
        .entrypoint(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            image_c_entrypoint_cmd.clone(),
        ])
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;
    info!("Image C built. Pushing to {}", image_c_ref);
    image_c
        .push(&image_c_ref, &test_utils::test_push_options())
        .await?;
    info!("Image C pushed.");

    // Attempt to push Image C again (or an identical one)
    info!(
        "Building Image C Prime (identical to Image C): {}",
        image_c_ref
    );
    // Rebuild layers for image_c_prime to ensure they are new Layer objects but logically identical
    let layer_x_prime = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_x_path, layer_x_content.as_bytes(), None)?
        .build()
        .await?;
    let layer_y_prime = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_y_path, layer_y_content.as_bytes(), None)?
        .build()
        .await?;

    let (image_c_prime, _) = Image::builder() // Rebuild to ensure it's a new "build" artifact
        .from("ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55")
        .platform(platform_os, &platform_arch)
        .layer(layer_x_prime) // Use rebuilt layer_x_prime
        .layer(layer_y_prime) // Use rebuilt layer_y_prime
        .entrypoint(vec!["/bin/sh".to_string(), "-c".to_string(), image_c_entrypoint_cmd.clone()])
        .blob_cache(shared_blob_cache.clone()) // Use same cache
        .build()
        .await?;
    info!("Image C Prime built. Re-pushing to {} (expecting all layers to be mounted, manifest only push)", image_c_ref);
    image_c_prime
        .push(&image_c_ref, &test_utils::test_push_options())
        .await?;
    info!("Image C Prime pushed (re-push).");

    // Verification
    let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Generic {
        message: format!("Failed to connect to Docker: {}", e),
        source: Some(Box::new(e)),
    })?;
    pull_image_with_bollard(
        &docker,
        &local_registry_host,
        &image_c_repo_namespaced,
        image_tag,
    )
    .await?;

    let container_name_c = format!("{}-container", image_c_repo);
    let guard_c = BollardContainerGuard::new(
        &docker,
        &image_c_ref,
        container_name_c,
        &platform_arch.to_string(),
        None,
        None,
    )
    .await?;
    let logs_c = guard_c.logs().await?;
    guard_c.wait_for_completion().await?;
    guard_c.cleanup().await?;

    let full_logs_c = logs_c.join("\n");
    info!("Image C (after re-push) logs: {}", full_logs_c);
    assert!(
        full_logs_c.contains(layer_x_content),
        "Logs should contain Layer X content. Logs: {}",
        full_logs_c
    );
    assert!(
        full_logs_c.contains(layer_y_content),
        "Logs should contain Layer Y content. Logs: {}",
        full_logs_c
    );

    info!("test_push_with_all_existing_layers completed successfully!");
    Ok(())
}

/// Tests the ability to push an image to a local registry using basic HTTP authentication.
/// It verifies that the push operation succeeds with valid credentials and that the
/// resulting image can be pulled and run, containing the correct content.
#[tokio::test]
async fn test_push_with_basic_authentication() -> Result<()> {
    setup_test_environment();
    info!("Starting test_push_with_basic_authentication");

    // --- Manually set up registry with Basic Auth ---
    info!("Setting up and running local container registry with Basic Auth...");
    let mut users = HashMap::new();
    users.insert(
        "testuser".to_string(),
        Secret::new("testpassword".to_string()),
    );
    let auth_provider = Arc::new(users);

    let mut testing_registry = ContainerRegistry::builder()
        .auth_provider(auth_provider)
        .build_for_testing();
    // Explicitly bind to 127.0.0.1:0 to get a random available port
    testing_registry.bind(([127, 0, 0, 1], 0).into());
    let _running_registry = testing_registry.run_in_background(); // Keep the guard
    let local_registry_addr = _running_registry.bound_addr();
    info!(
        "Local container registry with Basic Auth listening on: {}",
        local_registry_addr
    );
    // --- End of manual registry setup ---

    let local_registry_host = format!("localhost:{}", local_registry_addr.port());
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    let platform_os = "linux";
    let platform_arch = Arch::ARM64;
    let image_tag = "auth";
    let image_namespace = "testauth";

    let layer_auth_content = "Authenticated push content!";
    let layer_auth_path = "/app/auth_content.txt";
    let layer_auth = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(layer_auth_path, layer_auth_content.as_bytes(), None)?
        .build()
        .await?;

    let image_d_repo = generate_unique_image_name();
    let image_d_repo_namespaced = format!("{}/{}", image_namespace, image_d_repo);
    let image_d_ref = format!(
        "{}/{}:{}",
        local_registry_host, image_d_repo_namespaced, image_tag
    );
    let image_d_entrypoint_cmd = format!("echo \'Auth Image Running\' && echo \'--- Auth Content ---\' && cat {} && echo \'--- End Auth Content ---\'", layer_auth_path);

    info!("Building Image D for auth test: {}", image_d_ref);
    let (image_d, _) = Image::builder()
        .from("ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55")
        .platform(platform_os, &platform_arch)
        .layer(layer_auth)
        .entrypoint(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            image_d_entrypoint_cmd.clone(),
        ])
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;

    info!(
        "Image D built. Pushing with basic authentication to {}",
        image_d_ref
    );
    let push_opts_auth = PushOptions {
        auth: RegistryAuth::Basic("testuser".to_string(), "testpassword".to_string()),
        protocol: ClientProtocol::Http,
        ..Default::default()
    };
    image_d.push(&image_d_ref, &push_opts_auth).await?;
    info!("Image D pushed with basic auth.");

    // Verification
    let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Generic {
        message: format!("Failed to connect to Docker: {}", e),
        source: Some(Box::new(e)),
    })?;
    // Pull requires auth now because the test registry is configured with it.
    // Modify pull_image_with_bollard to accept credentials or pull directly here.

    info!(
        "Pulling image {}/{}_tag:{} using Bollard with Auth...",
        local_registry_host, image_d_repo_namespaced, image_tag
    );
    let bollard_registry_creds = bollard::auth::DockerCredentials {
        username: Some("testuser".to_string()),
        password: Some("testpassword".to_string()),
        serveraddress: Some(format!("http://{}", local_registry_host)),
        ..Default::default()
    };
    let bollard_from_image = format!("{}/{}", local_registry_host, image_d_repo_namespaced);
    let mut pull_stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: bollard_from_image,
            tag: image_tag.to_string(),
            ..Default::default()
        }),
        None,
        Some(bollard_registry_creds),
    );
    while let Some(pull_result) = pull_stream.next().await {
        match pull_result {
            Ok(info_msg) => {
                if let Some(status) = info_msg.status {
                    debug!("Pull status: {}", status);
                }
                if let Some(progress) = info_msg.progress {
                    debug!("Pull progress: {}", progress);
                }
            }
            Err(e) => {
                error!("Error during image pull: {:?}", e);
                return Err(Error::ImagePull {
                    image_ref: format!("{}/{}", local_registry_host, image_d_repo_namespaced),
                    message: format!("Bollard image pull failed for tag {}: {}", image_tag, e),
                    source: Some(Box::new(e)),
                });
            }
        }
    }
    info!(
        "Image {}/{}:{} pulled successfully via Bollard with Auth.",
        local_registry_host, image_d_repo_namespaced, image_tag
    );

    let container_name_d = format!("{}-container", image_d_repo);
    let guard_d = BollardContainerGuard::new(
        &docker,
        &image_d_ref,
        container_name_d,
        &platform_arch.to_string(),
        None,
        None,
    )
    .await?;
    let logs_d = guard_d.logs().await?;
    guard_d.wait_for_completion().await?;
    guard_d.cleanup().await?;

    let full_logs_d = logs_d.join("\n");
    info!("Image D (auth test) logs: {}", full_logs_d);

    assert!(
        full_logs_d.contains(layer_auth_content),
        "Logs should contain auth test content. Logs: {}",
        full_logs_d
    );

    info!("test_push_with_basic_authentication completed successfully!");
    Ok(())
}

#[tokio::test]
async fn test_executable_permission_error() -> Result<()> {
    setup_test_environment();
    info!("Starting test_executable_permission_error");

    // 1. Setup local registry & blob cache
    let (_running_registry, local_registry_host) = test_utils::setup_local_registry().await?;
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    // Setup image naming
    let unique_repo_name = generate_unique_image_name();
    let image_tag = "no-exec-test";
    let image_namespace = "testnoexec";
    let target_image_repo_namespaced = format!("{}/{}", image_namespace, unique_repo_name);
    let target_image_ref = format!(
        "{}/{}:{}",
        local_registry_host, target_image_repo_namespaced, image_tag
    );
    info!("Target image for no-exec test: {}", target_image_ref);

    // 2. Create a simple script and a layer without execute permissions
    let script_content = "#!/bin/sh\necho \'Script executed!\'";
    let script_path_in_container = "/app/test_script.sh";

    info!("Creating layer with script, no execute permissions...");
    let layer_script = Layer::builder()?
        .blob_cache(shared_blob_cache.clone())
        .data(
            script_path_in_container,
            script_content.as_bytes(),
            Some(0o644),
        )? // Explicitly NO execute permission
        .build()
        .await?;
    info!(
        "Script layer created. Diff ID: {}, Blob Digest: {}",
        layer_script.diff_id(),
        layer_script.blob_digest()
    );

    // 3. Build the image
    let base_image =
        "ubuntu@sha256:736e224eff152057af468d34e0a495e57e7f70d274a012897d10461527f0ca55"; // Using a common base
    let platform_os = "linux";
    let platform_arch = Arch::ARM64; // Match other tests, ensure base image compat
    let entrypoint = vec![script_path_in_container.to_string()];

    info!(
        "Building image from base '{}' ({}/{}) with entrypoint: {:?}",
        base_image, platform_os, platform_arch, entrypoint
    );
    let (image, _) = Image::builder()
        .from(base_image)
        .platform(platform_os, &platform_arch)
        .layer(layer_script)
        .entrypoint(entrypoint.clone())
        .blob_cache(shared_blob_cache)
        .build()
        .await?;
    info!(
        "Image built. OCI Archive Path: {:?}, Config Digest: {}",
        image.path(),
        image.config_digest()
    );

    // 4. Push the image
    info!("Pushing image to {}...", target_image_ref);
    let push_opts = test_utils::test_push_options();
    image.push(&target_image_ref, &push_opts).await?;
    info!("Image pushed successfully to: {}", target_image_ref);

    // 5. Bollard: Connect and Pull
    info!("Connecting to Docker daemon via Bollard...");
    let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Generic {
        message: format!("Failed to connect to Docker: {}", e),
        source: Some(Box::new(e)),
    })?;
    pull_image_with_bollard(
        &docker,
        &local_registry_host,
        &target_image_repo_namespaced,
        image_tag,
    )
    .await?;

    // 6. Bollard: Attempt to run container and expect failure
    let container_name = format!("{}-no-exec-container", unique_repo_name);
    info!(
        "Attempting to create and start container '{}' (expected to fail or error)...",
        container_name
    );

    // We expect container creation/start to fail or the container to error out quickly.
    // The exact error might come during create_container, start_container, or container exit.
    // Let's try to create, then start, and then check logs/status.

    let container_config = bollard::container::Config {
        image: Some(target_image_ref.as_str()),
        tty: Some(true),
        entrypoint: Some(entrypoint.iter().map(|s| s.as_str()).collect()),
        ..Default::default()
    };
    let create_options = Some(CreateContainerOptions {
        name: container_name.clone(),
        platform: Some(platform_arch.to_string()),
    });

    let create_response_result = docker
        .create_container(create_options.clone(), container_config.clone())
        .await;

    // If create succeeds, we'll try to start and then look for runtime errors.
    // If create fails, the error might already indicate the problem.
    // Bollard might not always give a permission error on create, rather on start or from runtime.

    let container_id: String;
    if let Ok(response) = create_response_result {
        container_id = response.id;
        info!(
            "Container '{}' created with ID: {}",
            container_name, container_id
        );

        // Attempt to start the container
        let start_result = docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await;
        if let Err(e) = start_result {
            info!("Container start failed as expected: {:?}", e.to_string());
            // Docker might give an error like:
            // "OCI runtime create failed: ... container_linux.go:380: starting container process caused: exec: "/app/test_script.sh": permission denied: unknown"
            assert!(
                e.to_string().to_lowercase().contains("permission denied")
                    || e.to_string().to_lowercase().contains("exec format error")
            );
            // Clean up the created (but not started) container
            docker
                .remove_container(
                    &container_id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e_bollard| Error::Generic {
                    message: format!(
                        "Failed to remove container '{}' during cleanup: {}",
                        container_id, e_bollard
                    ),
                    source: Some(Box::new(e_bollard)),
                })?;
            info!("test_executable_permission_error completed successfully (failure on start).");
            return Ok(());
        }
        info!(
            "Container {} started (unexpectedly, will check logs/exit code).",
            container_id
        );

        // If it somehow starts, wait for it and check exit code and logs.
        let guard = BollardContainerGuard {
            docker: &docker,
            id: container_id.clone(),
            name: container_name.clone(),
        };

        // Wait for completion - expect non-zero exit code
        info!(
            "Waiting for container {} to complete (expecting error)...",
            guard.id()
        );
        let wait_options = Some(WaitContainerOptions {
            condition: "not-running",
        });
        let mut wait_stream = guard.docker.wait_container(&guard.id, wait_options);
        let wait_result = wait_stream.try_next().await;

        let logs = guard.logs().await.unwrap_or_default();
        let full_logs = logs.join("\n");
        info!("Container logs: {}", full_logs);

        // Process wait_result and logs BEFORE cleaning up the guard
        match wait_result {
            Ok(Some(response)) => {
                info!(
                    "Container {} exited with status code: {}",
                    guard.id(),
                    response.status_code
                );
                assert_ne!(
                    response.status_code, 0,
                    "Container should have failed (non-zero exit code). Logs: {}",
                    full_logs
                );
                // Check logs for permission denied or similar
                // The exact message can vary. "Permission denied" is common.
                // Sometimes it can be "exec format error" if the shebang is problematic and it tries to run it directly.
                let lower_logs = full_logs.to_lowercase();
                assert!(
                    lower_logs.contains("permission denied")
                        || lower_logs.contains("exec format error"),
                    "Expected 'permission denied' or 'exec format error' in logs. Found: {}",
                    full_logs
                );
            }
            Ok(None) => {
                panic!("Did not receive container exit status for {}.", guard.id());
            }
            Err(e) => {
                // This could be an error from the wait call itself, implies container problem.
                info!("Error waiting for container: {}. This can be an indication of the execution failure.", e);
                assert!(
                    e.to_string().to_lowercase().contains("permission denied")
                        || e.to_string().to_lowercase().contains("exec format error")
                );
            }
        }

        guard.cleanup().await?; // Cleanup guard AFTER all uses
    } else if let Err(e) = create_response_result {
        // Error during container creation itself
        info!("Container creation failed as expected: {:?}", e.to_string());
        // Example error: "container_linux.go:380: starting container process caused: exec: "/app/test_script.sh": permission denied"
        // This is less common for Bollard; usually the error is on start or from runtime.
        assert!(
            e.to_string().to_lowercase().contains("permission denied")
                || e.to_string().to_lowercase().contains("exec format error")
        );
    }

    info!("test_executable_permission_error completed successfully!");
    Ok(())
}

fn generate_unique_image_name() -> String {
    let random_string: String = rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    format!("dockdash-test-{}", random_string.to_lowercase())
}

#[tokio::test]
async fn test_pull_policy_missing_uses_cache() -> Result<()> {
    setup_test_environment();
    info!("Starting test_pull_policy_missing_uses_cache");

    // No local registry needed for this test, we pull from public Docker Hub.
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    let base_image_for_test = "alpine:latest";
    info!("Base image for test: {}", base_image_for_test);

    // 1. Perform a preliminary build with PullPolicy::Always to ensure the manifest is pulled from the registry and cached.
    info!("Performing a preliminary build with PullPolicy::Always to ensure manifest for {} is cached.", base_image_for_test);
    let (_pre_cache_image, pre_cache_diagnostics) = Image::builder()
        .from(base_image_for_test)
        .platform("linux", &Arch::Amd64) // Platform required by builder, choose common ones.
        .pull_policy(PullPolicy::Always)
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;
    assert_eq!(
        pre_cache_diagnostics.manifest_source,
        ManifestSource::FromRegistry,
        "Preliminary build should pull from registry"
    );
    info!(
        "Preliminary build complete. Manifest for {} should now be cached.",
        base_image_for_test
    );

    // 2. Build the image again with PullPolicy::Missing. This should use the cached manifest.
    info!(
        "Building image with PullPolicy::Missing, expecting cache hit for manifest: {}",
        base_image_for_test
    );
    let (image_missing_policy, missing_diagnostics) = Image::builder()
        .from(base_image_for_test)
        .platform("linux", &Arch::Amd64)
        .pull_policy(PullPolicy::Missing)
        .blob_cache(shared_blob_cache.clone()) // Use the same cache
        .build()
        .await?;
    info!(
        "Image with PullPolicy::Missing built. Config digest: {}",
        image_missing_policy.config_digest()
    );

    assert_eq!(
        missing_diagnostics.manifest_source,
        ManifestSource::FromCache,
        "PullPolicy::Missing should use cached manifest"
    );

    Ok(())
}

#[tokio::test]
async fn test_pull_policy_always_hits_registry() -> Result<()> {
    setup_test_environment();
    info!("Starting test_pull_policy_always_hits_registry");

    // No local registry needed, we pull from public Docker Hub.
    let (shared_blob_cache, _temp_cache_dir) = setup_blob_cache()?;

    let base_image_for_test = "alpine:latest";
    info!("Base image for test: {}", base_image_for_test);

    // 1. Perform a preliminary build to ensure the manifest *could* be in the cache.
    info!(
        "Performing a preliminary build to ensure manifest for {} could be cached.",
        base_image_for_test
    );
    let (_pre_cache_image, _pre_cache_diagnostics) = Image::builder() // Diagnostics not asserted here
        .from(base_image_for_test)
        .platform("linux", &Arch::Amd64)
        .pull_policy(PullPolicy::Missing) // Or Always, doesn't matter much for pre-caching
        .blob_cache(shared_blob_cache.clone())
        .build()
        .await?;
    info!(
        "Preliminary build complete. Manifest for {} might be cached.",
        base_image_for_test
    );

    // 2. Build the image again with PullPolicy::Always. This should ignore the cache and pull from the registry.
    info!(
        "Building image with PullPolicy::Always, expecting registry pull for manifest: {}",
        base_image_for_test
    );
    let (image_always_policy, always_diagnostics) = Image::builder()
        .from(base_image_for_test)
        .platform("linux", &Arch::Amd64)
        .pull_policy(PullPolicy::Always)
        .blob_cache(shared_blob_cache.clone()) // Use the same cache
        .build()
        .await?;
    info!(
        "Image with PullPolicy::Always built. Config digest: {}",
        image_always_policy.config_digest()
    );

    assert_eq!(
        always_diagnostics.manifest_source,
        ManifestSource::FromRegistry,
        "PullPolicy::Always should pull from registry"
    );

    Ok(())
}
