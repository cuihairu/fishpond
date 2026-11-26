# fishpond deployment image (F029): builds the workspace with the pinned
# MSRV toolchain and ships the `fishpond` CLI (doctor / migrate).
# Build from the repository root:
#   docker build -f deploy/Dockerfile -t fishpond:local .
#
# The long-running app services (fishpond-server, fishpond-worker) join
# this image in a later task once their binaries exist; compose already
# wires the database, the one-shot migrate job and the doctor check.

FROM rust:1.88-slim AS build
WORKDIR /build
# The manifest set first so dependency downloads cache across source edits.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
RUN cargo build --release -p fishpond-cli

FROM debian:bookworm-slim
RUN useradd --system --no-create-home fishpond
COPY --from=build /build/target/release/fishpond /usr/local/bin/fishpond
USER fishpond
ENTRYPOINT ["fishpond"]
