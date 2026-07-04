//! Container orchestration: spawn/stop/list/health-check replicas of a
//! service via the Docker daemon. Kept behind a trait so a Kubernetes or
//! Podman driver can be added later without touching the proxy/autoscaler.

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::Docker;
use lw_config::ServiceMeta;
use std::collections::HashMap;
use std::net::TcpListener;
use std::time::Duration;
use tracing::{info, warn};

/// A single running (or starting) replica of a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub container_id: String,
    pub host_port: u16,
    pub container_port: u16,
}

impl Instance {
    /// Base URL the proxy can forward requests to, e.g. "http://127.0.0.1:32891".
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }
}

#[async_trait]
pub trait ScalingDriver: Send + Sync {
    async fn spawn_instance(&self, spec: &ServiceMeta) -> Result<Instance>;
    async fn stop_instance(&self, instance: &Instance) -> Result<()>;
    async fn list_instances(&self, service_name: &str) -> Result<Vec<Instance>>;
    async fn health_check(&self, instance: &Instance, path: &str) -> bool;
}

/// Label used to tag containers we manage, so `list_instances` can find
/// them again after a restart and won't touch unrelated containers.
const MANAGED_LABEL: &str = "loadwright.managed";
const SERVICE_LABEL: &str = "loadwright.service";

pub struct DockerDriver {
    docker: Docker,
    http: reqwest::Client,
}

impl DockerDriver {
    /// Connects to the local Docker daemon via its default socket.
    pub fn connect() -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("connecting to docker daemon")?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .context("building health-check http client")?;
        Ok(Self { docker, http })
    }

    /// Finds a free ephemeral port on the host by briefly binding to :0.
    /// There's a small race between releasing this port and Docker binding
    /// it, but it's fine for a v1 scaffold — swap for a reserved port range
    /// or a retry loop if that ever bites in production.
    fn free_host_port() -> Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("finding a free port")?;
        Ok(listener.local_addr()?.port())
    }
}

#[async_trait]
impl ScalingDriver for DockerDriver {
    async fn spawn_instance(&self, spec: &ServiceMeta) -> Result<Instance> {
        let host_port = Self::free_host_port()?;
        let container_port_key = format!("{}/tcp", spec.port);

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            container_port_key.clone(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(host_port.to_string()),
            }]),
        );

        let mut labels = HashMap::new();
        labels.insert(MANAGED_LABEL.to_string(), "true".to_string());
        labels.insert(SERVICE_LABEL.to_string(), spec.name.clone());

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            ..Default::default()
        };

        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(container_port_key, HashMap::new());

        let config = Config {
            image: Some(spec.image.clone()),
            exposed_ports: Some(exposed_ports),
            host_config: Some(host_config),
            labels: Some(labels),
            ..Default::default()
        };

        // Container names must be unique; suffix with the chosen host port.
        let name = format!("loadwright-{}-{}", spec.name, host_port);
        let options = CreateContainerOptions {
            name: name.as_str(),
            platform: None,
        };

        let created = self
            .docker
            .create_container(Some(options), config)
            .await
            .with_context(|| format!("creating container for service '{}'", spec.name))?;

        self.docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("starting container {}", created.id))?;

        info!(
            service = %spec.name,
            container_id = %created.id,
            host_port,
            "spawned instance"
        );

        Ok(Instance {
            container_id: created.id,
            host_port,
            container_port: spec.port,
        })
    }

    async fn stop_instance(&self, instance: &Instance) -> Result<()> {
        self.docker
            .stop_container(&instance.container_id, Some(StopContainerOptions { t: 10 }))
            .await
            .with_context(|| format!("stopping container {}", instance.container_id))?;

        self.docker
            .remove_container(
                &instance.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("removing container {}", instance.container_id))?;

        info!(container_id = %instance.container_id, "stopped instance");
        Ok(())
    }

    async fn list_instances(&self, service_name: &str) -> Result<Vec<Instance>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", SERVICE_LABEL, service_name)],
        );

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false, // only running containers
                filters,
                ..Default::default()
            }))
            .await
            .context("listing containers")?;

        let mut instances = Vec::new();
        for c in containers {
            let Some(id) = c.id else { continue };
            let Some(ports) = c.ports else { continue };
            let Some(binding) = ports.into_iter().find(|p| p.public_port.is_some()) else {
                continue;
            };
            instances.push(Instance {
                container_id: id,
                host_port: binding.public_port.unwrap(),
                container_port: binding.private_port,
            });
        }
        Ok(instances)
    }

    async fn health_check(&self, instance: &Instance, path: &str) -> bool {
        let url = format!("{}{}", instance.base_url(), path);
        match self.http.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!(container_id = %instance.container_id, error = %e, "health check failed");
                false
            }
        }
    }
}
