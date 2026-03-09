#!/usr/bin/env bash
# Build and push workflow-engine image to a public Docker registry (e.g. Docker Hub).
# Builds multi-platform image (linux/amd64, linux/arm64) so it runs on both
# x86_64 servers (e.g. Ubuntu on AWS) and ARM (e.g. Apple Silicon).
#
# Usage:
#   ./scripts/docker-build-push.sh [IMAGE[:TAG]]
#
# Examples:
#   ./scripts/docker-build-push.sh myuser/workflow-engine           # build & push as myuser/workflow-engine:latest
#   ./scripts/docker-build-push.sh myuser/workflow-engine:v0.1.0    # build & push with tag v0.1.0
#   DOCKER_IMAGE=myuser/workflow-engine ./scripts/docker-build-push.sh
#
# Prerequisites:
#   - docker login   (to Docker Hub or your registry)
#   - docker buildx  (create builder once: docker buildx create --use)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

IMAGE="${DOCKER_IMAGE:-${1:-}}"
if [[ -z "$IMAGE" ]]; then
  echo "Usage: $0 <IMAGE[:TAG]>"
  echo "   or: DOCKER_IMAGE=username/workflow-engine $0"
  echo "Example: $0 myuser/workflow-engine:latest"
  exit 1
fi

# If IMAGE has no tag, default to latest
if [[ "$IMAGE" != *:* ]]; then
  IMAGE="${IMAGE}:latest"
fi

# Multi-platform so image works on linux/amd64 (e.g. Ubuntu) and linux/arm64 (e.g. Mac M1/M2)
PLATFORMS="${DOCKER_PLATFORMS:-linux/amd64,linux/arm64}"

echo "Building $IMAGE for $PLATFORMS ..."
docker buildx build \
  --platform "$PLATFORMS" \
  --tag "$IMAGE" \
  --push \
  "$REPO_ROOT"

echo "Done. Image pushed: $IMAGE (platforms: $PLATFORMS)"
