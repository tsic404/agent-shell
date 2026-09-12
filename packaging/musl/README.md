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

`agent-shell-daemon` 运行时动态链接 `libpipewire-0.3.so` 与 `libwayland-client.so`
（合成器 / ScreenCast 截图必需），这些是 C 库，无法用 musl 静态链接。daemon
只能通过**在目标 glibc 版本上构建**来降低 glibc 门槛：

- DDE 25（glibc 2.38）：在 Debian 11（glibc 2.31，含 pipewire 0.3）或更旧的
  glibc 构建容器里产出即可覆盖。
- DDE 20（glibc 2.28）：无主流发行版同时提供 glibc 2.28 与 pipewire 0.3 头文件，
  需在 UOS 20 / deepin 20 本机（自带 pipewire 0.3.15）构建。

验收关注的 `agent-shell doctor` / `--version` 走 CLI 二进制，静态 musl 已覆盖
DDE 20/25；`doctor` 的后端诊断由 daemon 承载，daemon 需按上表在目标环境构建。
