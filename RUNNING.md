# Docker Setup

`loadwright` manages containers through the Docker API. To use it, **a running Docker daemon must be accessible** from the machine where `loadwright` is running.

---

## Prerequisites

Before starting, ensure you have:

- Docker Engine or Docker Desktop installed.
- A running Docker daemon.
- A user account with permission to communicate with the Docker socket.
- All required service images available locally or accessible from a container registry.

---

# Connecting to the Docker Daemon

## Linux / macOS (Default)

On Linux and macOS, Docker exposes a Unix socket at:

```text
/var/run/docker.sock
```

If Docker is installed and running, `loadwright` will connect automatically without additional configuration.

---

## Windows (Docker Desktop)

Docker Desktop exposes the Docker API through a Windows named pipe:

```text
//./pipe/docker_engine
```

Some Docker clients (including `hyper`) may not discover it automatically.

Set the `DOCKER_HOST` environment variable before starting `loadwright`.

### Command Prompt (CMD)

```cmd
set DOCKER_HOST=npipe:////./pipe/docker_engine
```

### PowerShell

```powershell
$env:DOCKER_HOST="npipe:////./pipe/docker_engine"
```

Then start `loadwright` normally.

> **Tip:** If you're using **WSL2**, run `loadwright` from inside your WSL environment. Docker Desktop exposes the standard Unix socket there, so no additional configuration is required.

---

## Remote Docker Daemon (Advanced)

You can also connect to a Docker daemon running on another machine.

```bash
export DOCKER_HOST=tcp://your-server-ip:2375
```

> **Security Warning**
>
> Never expose Docker on TCP port **2375** without authentication.
> In production, always enable **TLS** when exposing the Docker API remotely.

---

# Docker Images

The images referenced in your service configuration must be available to the Docker daemon.

This means they must either:

- already exist locally (`docker pull ...`), or
- be accessible from a container registry such as:
  - Docker Hub
  - GitHub Container Registry (GHCR)
  - Any private OCI-compatible registry

> **Note**
>
> `loadwright` **does not build Docker images**.
> It only instructs Docker to start containers from existing images.

---

# Running `loadwright` in Production

For production deployments, it is recommended to run `loadwright` on the same machine (or inside the same private network/VPC) as the containers it manages.

Ensure:

- Docker is installed.
- Docker is running.
- The service user has permission to access Docker.

On Linux this typically means adding the user to the `docker` group.

---

# Example systemd Service

Create the following file:

```text
/etc/systemd/system/loadwright.service
```

```ini
[Unit]
Description=loadwright autoscaler
After=docker.service

[Service]
ExecStart=/usr/local/bin/loadwright up /etc/loadwright/service.toml
User=loadwright
Group=docker
Restart=always

[Install]
WantedBy=multi-user.target
```

Enable and start the service:

```bash
sudo systemctl enable loadwright
sudo systemctl start loadwright
```

---

# Troubleshooting

| Problem | Possible Cause | Solution |
|----------|----------------|----------|
| `client error (Connect)` or `file not found` | Docker daemon is not running or the socket path is incorrect | Start Docker and verify the socket. On Windows, set `DOCKER_HOST`. |
| `permission denied: /var/run/docker.sock` | User lacks Docker permissions | Add the user to the `docker` group and log in again. |
| `image not found` | Image is unavailable locally or in the registry | Run `docker pull <image>` or verify registry credentials. |
| Containers fail to start due to timeout | Docker daemon is overloaded or networking issues | Check Docker resources and increase driver timeouts if necessary. |

---

# Next Steps

- Read the main **README** for an overview of the project.
- Contributions are welcome, especially around additional drivers (such as a `ProcessDriver`) and improved Windows support.

---