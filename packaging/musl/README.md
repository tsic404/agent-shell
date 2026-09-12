# 静态 musl 构建（glibc 兼容）

## 问题

`agent-shell` 在 glibc 较新的本机（如 Arch glibc 2.44）编译后，二进制引用了
`GLIBC_2.29 / 2.30 / 2.32 / 2.33 / 2.34 / 2.39` 符号版本，无法在 DDE 20
（glibc 2.28）与 DDE 25（glibc 2.38）真机加载：

```
/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found
```

根因：`agent-shell` / `agent-shell-rootd` / `agent-shell-mcp` 三个二进制是纯
Rust（无 libpipewire / libwayland 等 C 库依赖），但 Rust std 与 `libc` crate 在
构建机上按新 glibc 解析符号版本，把最低 glibc 门槛抬高了。

## 方案：静态链接 musl

这三个二进制没有原生 C 依赖，可以完全静态链接 musl，从而**不依赖宿主 glibc**，
在任何 glibc 上都能运行。用 `x86_64-unknown-linux-musl` 目标 + `musl-tools` 构建。

## 构建

```sh
./packaging/musl/build-musl.sh
```

产物输出到 `target/x86_64-unknown-linux-musl/release/`。

等价的手工命令（需 `rustup target add x86_64-unknown-linux-musl` + 安装
`musl-tools`）：

```sh
cargo build --release --target x86_64-unknown-linux-musl \
    -p agent-shell-cli \
    -p agent-shell-rootd \
    -p mcp
```

验证静态性：

```sh
file target/x86_64-unknown-linux-musl/release/agent-shell
# -> ELF 64-bit ... statically linked ...
ldd target/x86_64-unknown-linux-musl/release/agent-shell
# -> not a dynamic executable
```

## glibc 兼容矩阵

| 二进制 | 原生 C 依赖 | 静态 musl | glibc 构建门槛 |
|--------|-------------|:---------:|---------------|
| `agent-shell`（CLI） | 无 | ✅ | 任意（静态） |
| `agent-shell-rootd` | 无 | ✅ | 任意（静态） |
| `agent-shell-mcp` | 无 | ✅ | 任意（静态） |
| `agent-shell-daemon` | libpipewire、libwayland-client | ❌ | 需按目标 glibc 构建 |

### daemon 的说明

`agent-shell-daemon` 运行时动态链接 `libpipewire-0.3.so` 并 dlopen
`libwayland-client.so`（合成器 / ScreenCast 截图必需），这些是 C 库，无法用
musl 静态链接。daemon 只能在目标 glibc 兼容的构建环境里产出：

```sh
./packaging/debian/build-daemon.sh   # Debian 12（glibc 2.36）兼容构建器
```

注意：`pipewire`/`libspa` crate 0.10.1 要求系统 pipewire 头 **≥ 0.3.65**
（引用 `spa_video_info_raw.flags`、`SPA_PARAM_Latency`、10-bit 视频格式等新
符号），因此构建器不能选 Debian 11（pipewire 仅 0.3.19）。Debian 12 是同时
满足「glibc ≤ 2.38（DDE 25）且 pipewire ≥ 0.3.65」的最老仍受支持发行版：

- DDE 25（glibc 2.38）：`build-daemon.sh`（Debian 12 / glibc 2.36）产出即可覆盖
  （实测产物最高引用 `GLIBC_2.34`）。
- DDE 20（glibc 2.28）：无受维护镜像同时提供 glibc 2.28 与 pipewire ≥ 0.3.65
  头文件；UOS 20 / deepin 20 自带 pipewire 仅 0.3.15（< 0.3.65），同样无法满足
  libspa 0.10.1 的结构体布局要求。DDE 20 的 daemon 构建当前仍受此门槛阻塞，
  需升级目标机 pipewire 或降级 pipewire crate（超出本构建器范围）。

DDE 20/25；`doctor` 的后端诊断由 daemon 承载，daemon 按上表构建后即可在
DDE 25 完整执行；DDE 20 受 pipewire ≥ 0.3.65 门槛阻塞，暂无法产出 daemon。
