# Project Operating Notes

## Docker Storage Incident Prevention
- Docker storage work is not complete after a one-time restart or cache prune. Confirm `/System/Volumes/Data` free space, `Docker.raw` allocated size, Docker Build Cache, container logs, and large non-Docker growth sources before reporting completion.
- Docker Desktop cannot be guaranteed to keep running when the host filesystem has no writable space. If the user asks for Docker to stay running, create or verify a guardrail that preserves host free space before Docker reaches write failure.
- Treat `no space left on device` under `Library/Containers/com.docker.docker/Data/log/vm/` as a host free-space failure affecting Docker Desktop startup and runtime stability.
