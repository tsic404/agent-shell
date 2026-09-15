# company-03 GNOME 50 环境验收（backend 不可用）

## 环境信息

| 项 | 值 |
|----|----|
| 主机 | company-03（hostname `tsic-pc`，tailscale `100.107.109.109`） |
| 用户 | tsic |
| DE | GNOME Shell 50.4 |
| 会话类型 | Wayland (gdm-wayland-session) |
| 系统 | Arch Linux |
| 访问 | `ssh tsic@100.107.109.109`（免密已配置） |
| 图形 bus | session bus（gdm 自动注入） |

## 状态（2026-09-15 实测）

⚠ **GNOME backend 不可用**：company-03 部署的 `agent-shell` 为 0.1.0，不含 mutter extension bridge（`agent-shell --help` 无 `extension` 子命令），`agent-shell doctor` 无法装配 mutter 后端。须先部署含 extension bridge 的二进制并 `agent-shell extension install && enable`，本环境验收方可执行。

## doctor 实测基准（0.1.0）

```bash
ssh tsic@100.107.109.109
agent-shell doctor
```

实测输出：

```
✓ DE 检测        : Unknown (unknown-session, none)
✗ 合成器         : unavailable in this session
⚠ AT-SPI         : unavailable (org.a11y.atspi.Registry not reachable)
⚠ 截图捕获        : 候选 portal-screencast → portal-screenshot（Wayland 下 portal 未就绪/未授权，无可用后端）
```

## env 注入（当前必需）

SSH 登录后无图形会话环境变量（`XDG_SESSION_TYPE=tty`、`XDG_CURRENT_DESKTOP` 为空）。0.1.0 的 DE 检测不依赖 `org.gnome.Shell` D-Bus 兜底——`busctl --user status org.gnome.Shell` 可达（PID 可查），但 doctor 仍报 Unknown；须手动注入 `XDG_CURRENT_DESKTOP=GNOME` 才识别为 GNOME：

```bash
ssh tsic@100.107.109.109
export XDG_CURRENT_DESKTOP=GNOME
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
agent-shell doctor
```

注入后 DE 识别为 GNOME，但合成器仍 unavailable（extension bridge 未部署）：

```
✓ DE 检测        : GNOME (unknown-session, XDG_CURRENT_DESKTOP)
✗ 合成器         : unavailable in this session
```

## GNOME 关键验证命令（extension bridge 部署后）

### 会话 bus 上的核心服务

```bash
# org.gnome.Shell + Mutter.DisplayConfig 必须在 session bus 上
busctl --user list | grep -E 'org.gnome.Shell$|org.gnome.Mutter.DisplayConfig'

# 版本
gnome-shell --version   # 预期 GNOME Shell 50.4
```

### Extension 路径（GNOME 47+，Eval 已移除）

```bash
# 安装/启用 agent-shell-bridge extension（当前 0.1.0 无此命令，须先部署新二进制）
agent-shell extension install
agent-shell extension enable
# 探测 agent-shell-bridge extension 是否注册
gdbus introspect --session --dest org.gnome.Shell --object-path /org/gnome/Shell 2>&1 | head -10
busctl --user list | grep -i 'AGENTSHELL\|agent-shell'
```

### DisplayConfig 读显示器状态

```bash
gdbus introspect --session --dest org.gnome.Mutter.DisplayConfig \
  --object-path /org/gnome/Mutter/DisplayConfig 2>&1 | grep -E 'GetCurrentState|ApplyMonitorsConfig'
```

## 版本兼容断言（extension bridge 部署后）

| 断言 | GNOME 50 预期 |
|------|--------------|
| 版本边界 | GNOME 50 ≥ 47 → 走 Extension 路径，Eval 路径不可用（已移除） |
| `org.gnome.Shell` | session bus 可达 |
| `org.gnome.Mutter.DisplayConfig` | session bus 可达 |
| 窗口操作 | 受限（无 move/resize/workspace，GNOME 最受限后端） |
| 窗口枚举 | 依赖 Extension 注册的 D-Bus 接口（`GetWindows` 等） |

## 环境准备

1. tsic 用户已登录 GNOME 图形会话（`gdm` 自动拉起 `gnome-shell --mode=user`）
2. SSH 登录后 session bus 可直接访问（`DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus`）
3. agent-shell 二进制已安装到 PATH（须含 mutter extension bridge；当前 0.1.0 不含，待部署）


