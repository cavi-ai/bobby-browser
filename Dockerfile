# syntax=docker/dockerfile:1

# --- builder -----------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /build

# openssl-sys + native-tls (in Cargo.lock) link against system OpenSSL.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release -p bobby-browser -p mcp-gateway \
    && mkdir -p /out \
    && cp target/release/bobby target/release/mcp-gateway /out/

# --- runtime -------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation \
        tini \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system bobby \
    && useradd --system --gid bobby --uid 10001 --no-create-home \
        --shell /usr/sbin/nologin bobby \
    && mkdir -p /var/lib/bobby /etc/bobby \
    && chown -R bobby:bobby /var/lib/bobby /etc/bobby

COPY --from=builder /out/bobby /usr/local/bin/bobby
COPY --from=builder /out/mcp-gateway /usr/local/bin/mcp-gateway
COPY deploy/docker/config.toml /etc/bobby/config.toml
COPY deploy/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh \
    && chown bobby:bobby /etc/bobby/config.toml

# Managed headless Chromium: point bobby at the apt-installed binary, force
# the managedChromium engine (default engine preference is Firefox, which
# has no companion inside this image), and disable Chrome's setuid sandbox
# since the container has no privileged user namespace and runs as non-root
# (crates/worker-pool/src/chromium.rs honors this env var explicitly).
ENV BOBBY_CHROME_EXECUTABLE=/usr/bin/chromium
ENV BOBBY_CHROME_NO_SANDBOX=1
ENV AUTOMATION_RUNTIME_BROWSER_SELECTION="{\"preference\":{\"mode\":\"managedChromium\"}}"
ENV BOBBY_BROWSER_CONFIG=/etc/bobby/config.toml
ENV BOBBY_BROWSER_BOOTSTRAP_ENV=/var/lib/bobby/bootstrap.env
ENV HOME=/var/lib/bobby

USER bobby
WORKDIR /var/lib/bobby
EXPOSE 7777

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
