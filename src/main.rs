use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lw_autoscaler::Autoscaler;
use lw_config::ServiceConfig;
use lw_orchestrator::{DockerDriver, ScalingDriver};
use lw_proxy::{BackendPool, MetricsRegistry, ProxyState, RoundRobin};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[derive(Parser)]
#[command(name = "loadwright", about = "Container-native load balancing & autoscaling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bring a service up: spawns min_instances and starts the proxy.
    Up {
        /// Path to the service's TOML config file.
        config_path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Up { config_path } => up(&config_path).await,
    }
}

async fn up(config_path: &str) -> Result<()> {
    let cfg = ServiceConfig::from_file(config_path)
        .with_context(|| format!("loading config from {config_path}"))?;
    info!(service = %cfg.service.name, "starting service");

    let driver: Arc<dyn ScalingDriver> =
        Arc::new(DockerDriver::connect().context("connecting to docker")?);
    let pool = BackendPool::new();
    let metrics = MetricsRegistry::new();

    // Bring up min_instances synchronously before serving any traffic. Once
    // the proxy and autoscaler are both running, all further scaling
    // (reactive or scheduled pre-warming) happens through the autoscaler
    // loop below — this initial batch is just "don't accept requests with
    // zero backends".
    for _ in 0..cfg.scaling.min_instances {
        let instance = driver
            .spawn_instance(&cfg.service)
            .await
            .context("spawning initial instance")?;
        wait_until_healthy(driver.as_ref(), &instance, &cfg.service.health_check).await;
        metrics.track(&instance.container_id);
        let mut current = pool.get().await;
        current.push(instance);
        pool.set(current).await;
    }

    info!(
        replicas = pool.len().await,
        listen_port = cfg.service.listen_port,
        "initial instances healthy, starting proxy + autoscaler"
    );

    let autoscaler = Autoscaler::new(
        driver.clone(),
        pool.clone(),
        metrics.clone(),
        cfg.service.clone(),
        cfg.scaling.clone(),
    );
    tokio::spawn(autoscaler.run());

    let lb = Arc::new(RoundRobin::default());
    let state = ProxyState::new(pool, lb, metrics)?;
    let app = lw_proxy::router(state);

    let addr = format!("0.0.0.0:{}", cfg.service.listen_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding proxy listener on {addr}"))?;
    info!(%addr, "proxy listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn wait_until_healthy(driver: &dyn ScalingDriver, instance: &lw_orchestrator::Instance, health_path: &str) {
    for attempt in 1..=30 {
        if driver.health_check(instance, health_path).await {
            info!(container_id = %instance.container_id, attempt, "instance healthy");
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(
        container_id = %instance.container_id,
        "instance did not report healthy after 15s, adding to pool anyway"
    );
}
