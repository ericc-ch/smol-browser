FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
        perl \
        make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation by copying manifests first
COPY Cargo.toml Cargo.lock ./
COPY crates/tinybrowser-dom/Cargo.toml       crates/tinybrowser-dom/Cargo.toml
COPY crates/tinybrowser-net/Cargo.toml       crates/tinybrowser-net/Cargo.toml
COPY crates/tinybrowser-core/Cargo.toml   crates/tinybrowser-core/Cargo.toml
COPY crates/tinybrowser-cdp/Cargo.toml       crates/tinybrowser-cdp/Cargo.toml
COPY crates/tinybrowser-js/Cargo.toml        crates/tinybrowser-js/Cargo.toml
COPY crates/tinybrowser-cli/Cargo.toml       crates/tinybrowser-cli/Cargo.toml
COPY crates/tinybrowser-lib/Cargo.toml           crates/tinybrowser-lib/Cargo.toml

# Create stub src files so cargo can resolve the dependency graph
RUN for crate in tinybrowser-dom tinybrowser-net tinybrowser-core tinybrowser-cdp tinybrowser-js tinybrowser-lib; do \
        mkdir -p crates/$crate/src && echo "// stub" > crates/$crate/src/lib.rs; \
    done && \
    mkdir -p crates/tinybrowser-cli/src && \
    echo "fn main() {}" > crates/tinybrowser-cli/src/main.rs

RUN cargo build --release --bin tinybrowser 2>/dev/null || true

ARG TINYBROWSER_VERSION

# Copy real sources and build
COPY crates/ crates/
RUN echo "Building tinybrowser version ${TINYBROWSER_VERSION:-from Cargo.toml}" && \
    touch crates/*/src/*.rs && cargo build --release --bin tinybrowser

# ---

# distroless/cc: glibc + libgcc + CA certs only — no shell, no package manager
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/tinybrowser /tinybrowser

EXPOSE 9222

# Bind to 0.0.0.0 so the port is reachable via `docker run -p 9222:9222`.
# Native binary still defaults to 127.0.0.1 (loopback only) — this override
# is just for the container.
ENTRYPOINT ["/tinybrowser"]
CMD ["serve", "--port", "9222", "--host", "0.0.0.0"]
