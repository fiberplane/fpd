# Define the base image version of Debian
ARG BASE_IMAGE_VERSION=11-slim

# Runtime image
FROM debian:${BASE_IMAGE_VERSION}

# This needs to be the path to the directory containing the providers
ARG PROVIDERS_PATH=providers
COPY ${PROVIDERS_PATH} /app/providers

# This needs to be the path to the proxy binary
ARG PROXY_PATH=target/debug/proxy
COPY ${PROXY_PATH} /app/proxy

ENV WASM_DIR=/app/providers
ENV RUST_LOG=proxy=info
ENV LISTEN_ADDRESS=127.0.0.1:3000
WORKDIR /app
ENTRYPOINT ["/app/proxy"]
