# Compatible glibc builder for agent-shell-daemon (DDE 25 coverage).
#
# The daemon dynamically links libpipewire-0.3 (ScreenCast) and dlopens
# libwayland-client, so it cannot be musl-static like the pure-Rust binaries
# (see packaging/musl/). It must be built against a glibc old enough for the
# target host.
#
# Constraint: the `pipewire`/`libspa` crates (0.10.1) require SPA headers >=
# 0.3.65 (spa_video_info_raw.flags, SPA_PARAM_Latency, 10-bit video formats,
# …), so the builder needs pipewire >= 0.3.65. Debian 12 (bookworm) is the
# oldest still-supported distro satisfying BOTH:
#   * glibc 2.36  (<= DDE 25's 2.38 — the binary loads on DDE 25)
#   * pipewire 0.3.65
#
# (Debian 11's pipewire 0.3.19 is too old to compile the crates; Debian 13's
# glibc 2.41 is too new for DDE 25.)
#
# DDE 20 (glibc 2.28) is NOT covered here: UOS 20 / deepin 20 ship pipewire
# 0.3.15 (< 0.3.65), so even a native build is blocked by the same SPA >= 0.3.65
# requirement (needs a newer host pipewire or a pipewire-crate downgrade).
# Full matrix: packaging/musl/README.md.
FROM rust:1.98-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        clang \
        libclang-dev \
        libpipewire-0.3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Build only the daemon: libspa-sys needs bindgen (libclang) + libpipewire-0.3
# headers, both installed above.
RUN cargo build --release -p agent-shell-daemon

FROM scratch AS export
COPY --from=builder /src/target/release/agent-shell-daemon /
