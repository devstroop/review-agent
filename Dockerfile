# syntax=docker/dockerfile:1
# review-agent — multi-stage Docker build
#
# Stage 1: Build a statically-linked binary via musl (no glibc dependency).
# Stage 2: Copy into distroless/static (~20 MB final image).
#
# See ADR-018 for rationale on static linking + distroless.

# ── Builder ──────────────────────────────────────────────────────────────────
FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && apt-get install -y \
    musl-tools \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Cache dependencies by copying manifests first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src/ prompts/ && \
    echo "fn main() {}" > src/main.rs && \
    touch src/lib.rs && \
    cargo build --release --target x86_64-unknown-linux-musl 2>/dev/null || true
RUN rm -rf src/ prompts/

# Now copy the real source and rebuild (layer cache is warm).
COPY src/ src/
COPY prompts/ prompts/

RUN cargo build --release --target x86_64-unknown-linux-musl

# ── Runtime ──────────────────────────────────────────────────────────────────
FROM gcr.io/distroless/static:nonroot

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/review-agent /review-agent

USER nonroot:nonroot
ENTRYPOINT ["/review-agent"]
