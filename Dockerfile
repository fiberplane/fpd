# Setting ARCH allows us to use different base images for different architectures
ARG ARCH=
ARG BASE_IMAGE_VERSION=bullseye-slim

# Runtime image
FROM ${ARCH}debian:${BASE_IMAGE_VERSION}

RUN apt-get update \
 && apt-get install -y --force-yes --no-install-recommends ca-certificates \
 && apt-get clean \
 && apt-get autoremove \
 && rm -rf /var/lib/apt/lists/*

# This needs to be the path to the directory containing the providers
ARG PROVIDERS_PATH=providers
COPY ${PROVIDERS_PATH} /app/providers

# This needs to be the path to the proxy binary
ARG BIN_PATH=target/debug/proxy
COPY ${BIN_PATH} /app/proxy

ENV WASM_DIR=/app/providers
ENV RUST_LOG=proxy=info
ENV LISTEN_ADDRESS=127.0.0.1:3000

WORKDIR /app
ENTRYPOINT ["/app/proxy"]
