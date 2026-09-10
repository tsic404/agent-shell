# Vendored `libspa-sys` (compatibility patch)

This is a vendored copy of `libspa-sys 0.10.1` (pipewire-rs), pinned via
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

`pipewire` crate `0.10` depends on `libspa-sys 0.10.1`, whose `build.rs`
compiles `src/type-info.c`. That file unconditionally references SPA type
table symbols (`spa_type_device_event_id`, `spa_type_device_event`,
`spa_type_prop_float_array`, …) that only exist in newer system pipewire.

UOS 20 Pro (deepin 20 / Debian 10 base) ships `libpipewire-0.3-dev 0.3.15`,
whose `/usr/include/spa-0.2/spa/type.h` lacks those symbols. The C compile of
`type-info.c` therefore fails, blocking `agent-shell-daemon` (and every backend
that links `agent-shell-capture` → `pipewire`) on UOS 20.

`pipewire` crate `0.6`+ all share this `type-info.c`; `0.5` and earlier use
pure bindgen but drop the `spa::param::video::VideoInfoRaw` /
`spa::param::format::FormatProperties` / `spa::pod` type-object APIs that
`modules/capture` relies on. A plain version downgrade is therefore not viable;
the version-compatible fix is this vendored patch.

## Patch

`src/type-info.c` now wraps the post-0.3.0 symbols in `#if PW_CHECK_VERSION(…)`
guards (matching the existing `wrapper.h` version guards):
| Symbol | Guard |
| --- | --- |
| `spa_type_param_availability` | `0.3.8` |
| `spa_type_device_event_id`, `spa_type_device_event` | `0.3.20` |
| `spa_type_prop_channel_map` | `0.3.20` |
| `spa_type_bluetooth_audio_codec` | `0.3.25` |
| `spa_type_prop_float_array` | `0.3.28` |
| `spa_type_param_latency`, `spa_type_param_process_latency` | `0.3.30` |
| `spa_type_prop_iec958_codec`, `spa_type_audio_iec958_codec` | `0.3.34` |
| `spa_type_param_bitorder` | `0.3.40` |

These are the symbols introduced after pipewire 0.3.0; the rest have been
present since 0.3.0 and are left unguarded. The guards are a no-op on any
system pipewire >= 0.3.40 (all conditions true).

The `extern` declarations in `src/type_info.rs` are left unguarded: an unused
`extern static` produces no link-time symbol reference, and `libspa 0.10.1`
only references symbols available since pipewire 0.3.x.

## Updating

When `pipewire` is upgraded, re-sync this directory from the corresponding
`libspa-sys` release and re-apply the `PW_CHECK_VERSION` guards in
`src/type-info.c`. The guards are a no-op on any system pipewire >= 0.3.40.

## Verification

Two CI checks keep this patch honest (see `.github/workflows/pr-check.yml`):

- `scripts/check-libspa-sys-compat.sh` — compiles `type-info.c` against the
  real deepin `libspa-0.2-dev`/`libpipewire-0.3-dev` 0.3.15.1 headers and
  asserts both that the gated file compiles and that the ungated file fails.
- `scripts/check-libspa-sys-vendor.sh` — diffs this directory against the
  upstream crates.io `libspa-sys` release, allowing only the patched files
  (`src/type-info.c`, `src/type_info.rs`, `README.md`) to differ.
