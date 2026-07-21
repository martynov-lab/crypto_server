# Build stage: full toolchain, cached dependency layer.
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Dependency layer: manifests only, so a source-only change does not rebuild
# the ~200 crates of the dependency graph.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY bin/ bin/
RUN cargo build --release -p server --locked \
    && strip target/release/arb-screener

# Runtime stage: no toolchain, no source.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home arb

WORKDIR /app
COPY --from=builder /build/target/release/arb-screener /usr/local/bin/arb-screener
# Settings::load() resolves `config/default` relative to the working directory.
COPY config/ config/

USER arb
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/arb-screener"]
