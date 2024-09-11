# syntax=docker/dockerfile:1
ARG SOURCES_DIR=/usr/src/ipa
FROM rust:bookworm AS builder
ARG SOURCES_DIR

# Prepare helper binaries
WORKDIR "$SOURCES_DIR"
COPY . .
RUN set -eux; \
    cargo build --bin helper --release --no-default-features --features "web-app real-world-infra compact-gate"

RUN apt-get update &&  apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
