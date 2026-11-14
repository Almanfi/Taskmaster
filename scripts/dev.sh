#!/usr/bin/env bash
set -euo pipefail

IMAGE="taskmaster-dev"

if [[ "${REBUILD:-0}" == "1" ]] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build --platform linux/amd64 -f Dockerfile.dev -t "$IMAGE" .
fi

exec docker run -it --rm \
  --platform linux/amd64 \
  --cap-add SYS_ADMIN \
  --cap-add SYS_PTRACE \
  --security-opt seccomp=unconfined \
  -v "$PWD":/workspace \
  -w /workspace \
  "$IMAGE" bash
