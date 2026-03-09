# Build stage
FROM rust:1-bookworm AS builder

WORKDIR /app

# Copy manifests and source
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY migrations ./migrations

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary and migrations from builder
COPY --from=builder /app/target/release/workflow-engine /app/workflow-engine
COPY --from=builder /app/migrations /app/migrations

EXPOSE 3000

ENV PORT=3000

CMD ["/app/workflow-engine"]
