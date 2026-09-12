# Static musl build for agent-shell's pure-Rust binaries.
#
# `agent-shell` (CLI), `agent-shell-rootd`, and `agent-shell-mcp` have no native
# C dependency beyond libc, so a fully static musl binary runs on any glibc.
# This fixes the DDE 20 (glibc 2.28) / DDE 25 (glibc 2.38) load failure where a
# binary linked against a modern glibc required the GLIBC_2.29/2.30/2.32/2.33/
# 2.34/2.39 symbol versions those hosts do not provide.
#
# `agent-shell-daemon` is deliberately NOT built here: it links libpipewire and
# libwayland-client (C libraries) at runtime and must instead be built against
# the target host's glibc. See packaging/musl/README.md for the matrix.

FROM rust:1.98-slim AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rustup target add x86_64-unknown-linux-musl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Scope with -p to the three pure-Rust packages: the daemon's C deps
# (pipewire / wayland) cannot be statically linked for musl.
RUN cargo build --release --target x86_64-unknown-linux-musl \
    -p agent-shell-cli \
    -p agent-shell-rootd \
    -p mcp

FROM scratch AS export
COPY --from=builder \
    /src/target/x86_64-unknown-linux-musl/release/agent-shell \
    /src/target/x86_64-unknown-linux-musl/release/agent-shell-rootd \
    /src/target/x86_64-unknown-linux-musl/release/agent-shell-mcp \
    /
