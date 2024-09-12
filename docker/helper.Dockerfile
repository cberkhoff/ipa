# syntax=docker/dockerfile:1
# Builder
FROM rust:bookworm AS builder
WORKDIR /usr/src/ipa
COPY . .
RUN set -eux; \
    cargo install --no-default-features --features "web-app real-world-infra compact-gate" --path ipa-core

# Slimer runtime docker
FROM rust:slim-bookworm
RUN apt-get update &&  apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/helper /usr/local/bin/ipa-helper

