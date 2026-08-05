### Builder ###
FROM rust:1.89-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Build both binaries in one cargo invocation so we share the dep graph.
RUN cargo build --release \
    -p indexer-service --bin indexer-service \
    -p indexer-web     --bin indexer-web

### Runtime ###
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --shell /usr/sbin/nologin indexer
USER indexer
WORKDIR /home/indexer

COPY --from=builder /build/target/release/indexer-service /usr/local/bin/indexer-service
COPY --from=builder /build/target/release/indexer-web     /usr/local/bin/indexer-web
COPY crates/indexer-resolver/data/overlay.yaml      /etc/indexer/overlay.yaml
COPY crates/indexer-resolver/data/known_names.yaml  /etc/indexer/known_names.yaml
# Static assets for the web UI must live alongside the binary at runtime.
COPY crates/indexer-web/static    /opt/indexer-web/static
COPY crates/indexer-web/templates /opt/indexer-web/templates

# No ENTRYPOINT: each compose service specifies the binary to run.
CMD ["/usr/local/bin/indexer-service", "--help"]
