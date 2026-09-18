# agent-shell

Linux desktop agent shell：窗口管理、输入注入、截图捕获与无障碍集成，覆盖
KDE、GNOME、DDE、Hyprland、Sway 与 X11。

## 构建

```sh
cargo build --release --workspace
```

默认构建启用 `portal-screencast`（portal ScreenCast 的 PipeWire 流式截图路径），
需要 PipeWire dev 头（`libpipewire-0.3-dev` >= 0.3.37）。旧发行版——**DDE 20 /
UOS 20（libspa 0.3.15）**、Debian bullseye 0.3.19——自带的头缺
`spa_type_param_bitorder` 等符号，默认特性直编会失败，改用无 portal 路径：

```sh
cargo build --release --workspace --no-default-features
```

该命令关闭 `portal-screencast`，capture 回退到 portal Screenshot 与 X11 降级链
（`modules/capture/Cargo.toml` 的 `portal-screencast` feature）。

## 打包

- `packaging/debian/build-deb.sh` — dpkg-deb（agent-shell + agent-shell-rootd）
- `packaging/arch/PKGBUILD` — Arch Linux
- `packaging/nix/flake.nix` — Nix flakes

三个脚本默认走 BE（`--no-default-features`）：`PKGBUILD`（Arch）恒走 BE——
portal-screencast 依赖 libspa-sys 0.10.1 的 `_libspa_rs` shim 符号与 release LTO
交互，在部分 Arch 类机器链接期报 undefined `spa_*_libspa_rs`（pre-existing 上游
问题，与 libpipewire 版本无关），故不启用。`build-deb.sh` 仅在 `libpipewire-0.3`
>= 0.3.37 时启用 `portal-screencast`；arm64（company-04 UOS）头版本上报不可靠，
无视门槛强制 BE。`flake.nix` 不探测 host 头，按 nixpkgs 自带 pipewire 的版本门槛
判定（nixos-unstable 为 1.x，恒启用 `portal-screencast`）。
