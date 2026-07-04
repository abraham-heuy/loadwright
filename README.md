# loadwright

**Container-native load balancing and autoscaling for self-hosted apps.**

loadwright sits in front of your containerized app and does two jobs: it
spreads incoming traffic across however many copies of your app are
currently running, and it decides — automatically, based on real load —
when to run more copies and when to shut extras down.

It's built for the gap between "single Docker container, works fine most
of the time" and "full Kubernetes cluster, more infrastructure than the
problem needs." If you're self-hosting on your own servers and your app
falls over during predictable spikes — enrollment day, a product launch,
an exam release — loadwright is meant to be the small, focused tool that
fixes exactly that, without asking you to adopt an entire orchestration
platform first.

## Why this exists

This started from a concrete problem: a university's self-hosted 
portal would slow to a crawl or crash whenever students hit it at once —
downloading an exam card sounds trivial, but a few hundred simultaneous
large downloads is real load, and a single server has no way to absorb it.
The fix isn't "buy a bigger server" (it'll still fall over at the next
spike) — it's running multiple copies of the app and distributing load
across them, scaling that number up before the spike and back down after.

That's a generic problem, not a university-specific one, so loadwright is
built as a standalone open-source tool: point it at a container image and
a config file, and it handles the proxying and scaling for any app, not
just this one.

## What it actually does

- **Reverse proxy** — routes requests to a pool of running containers
  (round robin to start(assumes all the servers are healthy and strong enough to handle requests); the load-balancing strategy is pluggable).
  Streams both request and response bodies, so one large, slow download
  doesn't block or slow down everyone else's requests.
- **Reactive autoscaling** — watches p95 latency, active connections, or
  requests/sec per backend, and spawns or stops containers when you cross
  configured thresholds.
- **Scheduled pre-warming** — if you *know* when a spike is coming (eg: an
  exam card download window  from 8:00am, a flash sale at noon), tell it in the config and it'll
  scale up ahead of time instead of reacting after things already slowed
  down.
- **Docker-native** — works with any image; no changes to your app
  required beyond exposing a health-check endpoint.

## How it helps

- **No more manual scaling.** Nobody has to notice load climbing and SSH
  in to spin up another container at 8:50am.
- **No over-provisioning.** You don't need to permanently run enough
  capacity for your worst-case spike — scale up for it, then back down.
- **Works on infrastructure you already have.** Self-hosted servers,
  Docker already installed — no cloud-provider lock-in, no Kubernetes
  cluster to stand up and maintain first.
- **Not tied to one app.** Anything you can containerize and give a
  health-check endpoint to can sit behind loadwright.

## Quick start

```bash
git clone <this-repo>
cd loadwright
cargo build
cargo run -- up examples/exam-portal.toml
```

That spawns your configured minimum number of containers, waits for them
to report healthy, and starts the proxy + autoscaler. Requests to the
configured `listen_port` get load-balanced across whatever's currently
running; the autoscaler adjusts that pool in the background based on your
`[scaling]` config. See [`RUNNING.md`](./RUNNING.md) for the full
walkthrough — config reference, how to watch it scale, logging, and
current limitations.

A minimal config looks like:

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

## Project status

Early and functional, not yet battle-tested. The core pieces — config
parsing, the Docker driver, the streaming proxy, and the reactive +
scheduled autoscaler — are implemented, and the scaling-decision logic has
unit test coverage. Known gaps (CPU-based scaling, a `/metrics` endpoint,
a non-Docker process driver, multi-service support in one process) are
tracked as open work — see [`RUNNING.md`](./RUNNING.md#whats-next).

Expect breaking changes before a 1.0.

## Architecture, at a glance

```
requests → proxy (load balancer) → container pool
                ↑                        │
          backend list             active conns / latency
                │                        │
          autoscaler loop ───────────────┘
                │
          orchestrator (docker) → spawn/stop containers
```

Four crates, each doing one job: `lw-config` (parse + decide),
`lw-orchestrator` (talk to Docker), `lw-proxy` (route traffic, collect
metrics), `lw-autoscaler` (the control loop tying the other three
together). Full crate layout is in [`RUNNING.md`](./RUNNING.md).

## Contributing

Contributions are genuinely welcome — this is a small enough codebase that
a first PR doesn't require understanding the whole thing first.

**Good places to start:**
- Add a load-balancing strategy (least-connections, weighted, consistent
  hashing) — implement the `LoadBalancer` trait in `lw-proxy`.
- Implement the CPU metric (poll Docker's stats API per container) — the
  slot for it already exists in `MetricsRegistry::value_for`.
- Add a `process` driver (spawn a subprocess instead of a container) —
  implement the `ScalingDriver` trait in `lw-orchestrator`.
- Add a `/metrics` endpoint in Prometheus format.
- Write more test coverage, especially around the orchestrator/proxy
  crates (currently only `lw-config`'s pure logic has unit tests).

**Before opening a PR:**
1. Check open issues (or open one) to avoid duplicate work on anything
   non-trivial — a quick heads-up saves everyone time.
2. Keep new orchestrator/proxy behavior behind the existing `ScalingDriver`
   / `LoadBalancer` traits where it applies, rather than special-casing.
3. Add tests for anything with pure logic (decision-making, parsing,
   config validation) — that's the part of the codebase that's easiest to
   verify without a live Docker daemon, and where regressions are easiest
   to catch early.
4. Run `cargo fmt` and `cargo clippy` before submitting.


## License

