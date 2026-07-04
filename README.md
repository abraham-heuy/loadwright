# loadwright

> **Motivation:** My first project in my distributed systems learning path using Rust.

**Container-native load balancing and autoscaling for self-hosted applications.**

`loadwright` is a lightweight reverse proxy and autoscaler designed for self-hosted, containerized applications. It distributes incoming traffic across multiple running instances of your application and automatically scales those instances based on real traffic and performance metrics.

The goal is to bridge the gap between running a single Docker container and adopting a full orchestration platform like Kubernetes. If your application experiences predictable traffic spikes but Kubernetes is more infrastructure than you need, `loadwright` provides a focused solution that is simple to deploy and easy to understand.

---

# Why This Project Exists

This project started from a real-world problem.

A university's self-hosted student portal would become extremely slow or even crash whenever thousands of students attempted to access it simultaneously(this used to annoy me, alooot!). A common example was students downloading examination cards. While each request was simple, hundreds of concurrent downloads created enough load to overwhelm a single application instance.

Simply upgrading the server was not a sustainable solution. Traffic spikes would eventually exceed the new hardware as well.

The correct solution was to:

- run multiple instances of the application,
- distribute requests between them,
- automatically increase capacity during peak periods,
- reduce capacity when demand falls.

Although inspired by a university portal, this is a common problem for many self-hosted applications. `loadwright` was built as a reusable open-source tool that works with any containerized service.

---

# Features

## Reverse Proxy

- Routes requests across multiple running containers.
- Uses **Round Robin** load balancing by default (assuming healthy backend instances).
- Load-balancing strategies are designed to be pluggable.
- Streams request and response bodies, allowing large downloads without blocking other clients.

## Reactive Autoscaling

Automatically monitors backend performance using configurable metrics such as:

- p95 latency
- Active connections
- Requests per second

When configured thresholds are exceeded, new containers are started automatically. Idle containers are stopped once demand decreases.

## Scheduled Pre-Warming

Some traffic spikes are predictable.

For example:

- examination result releases
- student registration periods
- flash sales
- product launches

Instead of waiting for latency to increase, `loadwright` can pre-scale the application before the expected surge begins.

## Docker Native

`loadwright` works directly with Docker images.

Your application only needs to expose a health check endpoint. No application code changes are required.

---

# Benefits

- Automatic scaling without manual intervention.
- Reduced infrastructure costs by avoiding permanent over-provisioning.
- Works with existing Docker deployments.
- No dependency on cloud providers.
- No Kubernetes cluster required.
- Supports any containerized application with a health endpoint.

---

# Quick Start

Clone the repository:

```bash
git clone <repository-url>
cd loadwright
```

Build the project:

```bash
cargo build
```

Run using an example configuration:

```bash
cargo run -- up examples/exam-portal.toml
```

`loadwright` will:

1. Start the configured minimum number of containers.
2. Wait until each container becomes healthy.
3. Launch the reverse proxy.
4. Begin monitoring application load.
5. Automatically scale containers according to your configuration.

For a complete deployment guide and configuration reference, see **RUNNING.md**.

---

# Example Configuration

```toml
[service]
name = "exam-portal"
image = "ghcr.io/university/portalservice:latest"
port = 8080
health_check = "/healthz"
listen_port = 9000

[scaling]
min_instances = 1
max_instances = 10
metric = "p95_latency_ms"
scale_up_threshold = 800
scale_down_threshold = 200
```

---

# Current Status

`loadwright` is currently in an early but functional stage.

Implemented features include:

- Configuration parsing
- Docker orchestration
- Streaming reverse proxy
- Reactive autoscaling
- Scheduled autoscaling
- Unit-tested scaling decision logic

Planned improvements include:

- CPU-based scaling
- Prometheus `/metrics` endpoint
- Process-based orchestration driver
- Multi-service support
- Additional load-balancing algorithms

Breaking changes should be expected before the first stable `1.0` release.

---

# Architecture

```text
                  Incoming Requests
                          │
                          ▼
                Reverse Proxy / Load Balancer
                          │
         ┌────────────────┴────────────────┐
         ▼                                 ▼
 Backend Container 1               Backend Container N
         ▲                                 ▲
         └────────────────┬────────────────┘
                          │
                    Metrics Collection
                          │
                          ▼
                     Autoscaler Loop
                          │
                          ▼
                 Docker Orchestrator
               (Start / Stop Containers)
```

The project is divided into four focused crates:

| Crate | Responsibility |
|-------|----------------|
| `lw-config` | Configuration parsing and scaling decisions |
| `lw-proxy` | Reverse proxy and request routing |
| `lw-orchestrator` | Docker integration and container lifecycle |
| `lw-autoscaler` | Scaling control loop |

---

# Contributing

Contributions are welcome.

The project has been intentionally organized into small, focused crates to make it easier for new contributors to get started.

## Good First Contributions

- Implement additional load-balancing algorithms
  - Least Connections
  - Weighted Round Robin
  - Consistent Hashing

- Add CPU-based autoscaling using Docker statistics.

- Implement a `ProcessDriver` for running local processes instead of Docker containers.

- Add a Prometheus-compatible `/metrics` endpoint.

- Expand unit and integration test coverage.

---

## Before Opening a Pull Request

Please:

1. Check existing issues before beginning large changes.
2. Keep new functionality behind the existing abstraction traits where appropriate.
3. Add tests for new decision-making or parsing logic.
4. Run formatting and linting before submitting.

```bash
cargo fmt
cargo clippy
cargo test
```

---

