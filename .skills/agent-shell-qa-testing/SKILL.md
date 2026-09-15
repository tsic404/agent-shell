---
name: agent-shell-qa-testing
description: agent-shell 验收测试方法论。执行 doctor 与后端/模块测试时加载。
tags: [agent-shell, qa, testing, verification]
---

# agent-shell QA 测试

## 测试方法论

验收测试按两个维度组织：

1. **按真机环境**：每台物理/虚拟机的测试步骤（`references/env/*.md`）
2. **按后端/模块**：每个适配器或功能模块的测试用例（`references/cases/*.md`）

### 验收标准

每个测试用例输出：
- `PASS` — 行为符合规格
- `FAIL` — 行为不符合规格
- `BLOCKED` — 环境/工具链问题导致无法测试

所有测试用例 PASS 后，Verity 输出 `QA_PASSED`。

### doctor 命令基准

`agent-shell doctor` 是所有测试的前置条件。必须输出完整的健康报告。基准以**绑定态**输出为准（portal 会话已建立、协议已绑定），典型 KWin Wayland 会话：

```
✓ DE 检测        : KDE (Wayland, <source>)
✓ 合成器         : kwin-compositor
✓ Wayland 协议 : 3/3 globals bound (window_mgmt v<N>, fake_input v5+, vd_mgmt bound)
✓ KWin 服务   : org.kde.KWin v<X.Y.Z>
✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; /Scripting introspected)
✓ 输入注入   : fake_input authenticated ✓
⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）
✓ AT-SPI         : enabled (a11y bus up, Registry reachable)
✓ 截图捕获        : portal-screencast → portal-screenshot → x11（选中 <backend>）
✓ 输入后端        : libei → ydotool → xdotool（选中 <backend>）
```

顶层行顺序：`DE 检测` → `合成器`（含合成器子行）→ `AT-SPI` → `截图捕获` → `输入后端`（GNOME 会话另有 `Shell 扩展`）。KWin 合成器子行顺序：`Wayland 协议` / `KWin 服务` / `D-Bus 桥接` / `输入注入` / `事件脚本`。不存在 `主后端` / `事件流` 标签。截图行绑定态为 `✓ …（选中 …）`，未绑定会话为 `⚠ … 候选 …`，括注四分支——Wayland 且候选链含 x11：「（Wayland 下 portal 未就绪/未授权，已拒绝 x11 兜底）」；纯 Wayland（候选链无 x11）：「（Wayland 下 portal 未就绪/未授权，无可用后端）」；原生 X11 且候选链含 portal：「（非交互探测未就绪，需 portal 交互授权）」；无任何可用后端：「（非交互探测失败，无可用后端）」；输入后端行仅 `✓ …（选中 …）`（无可用后端时为 `✗ … 不可用`）；`DE 检测` 第三段是检测信号来源（`XDG_CURRENT_DESKTOP` / `dbus:<service>` 等），非窗口管理器。

> ⚠ 真机二进制须与 HEAD 一致：实机部署的 `agent-shell` 二进制若早于 HEAD（如 company-01 的 09-12 版本、company-03 的 0.1.0），doctor 输出会与基准漂移（事件脚本行文案、DE 检测兜底差异），比对前先核对二进制版本，否则产生假 FAIL。

## 真机环境清单

> ⚠ Dev2-107X（192.168.122.57）与 Dev2-deepin（192.168.122.216）两台 LAN 真机静态主机名均为 `uos-PC`（`hostnamectl` 实测），无法按主机名区分，须以 IP 标识并核对，避免跨机误操作。

| 环境 | 目标 | 主机名 (tailscale) | IP | 访问 |
|------|------|------|------|------|
| 本机 KDE | KDE 6.7.4 Wayland (Arch) | company-01 | 100.73.214.94 | 本地 |
| Dev2-deepin | DDE 25 (Deepin 25) X11 | uos-PC（LAN，非 tailscale；与 Dev2-107X 同名） | 192.168.122.216 | `ssh uos@192.168.122.216`（via ProxyJump company-01） |
| Dev2-107X | DDE 20 (UOS 20 Pro) X11 | uos-PC（LAN，非 tailscale；与 Dev2-deepin 同名） | 192.168.122.57 | `ssh uos@192.168.122.57`（via ProxyJump company-01） |
| GNOME（backend 不可用） | GNOME Shell 50.4 Wayland (Arch) | company-03 | 100.107.109.109 | `ssh tsic@100.107.109.109`（免密）；见 gnome.md |
| company-04 | DDE 20 Wayland (UOS 20 Pro) | company-04 | 100.113.128.117 | `ssh uos@100.113.128.117`（免密） |

## 测试用例结构

每个测试用例包含：

```
### TC-NNN: 测试标题

**前置条件**: ...
**测试步骤**:
1. ...
2. ...
**预期结果**: ...
**实际结果**: PASS/FAIL/BLOCKED
```

## 验证策略

1. 先跑 `agent-shell doctor` 确认环境健康
2. 按后端/模块的顺序执行测试用例
3. 每次测试记录 `agent-shell doctor` 输出作为基准
4. 失败时记录 `FAIL` 原因

## 模块引用

**环境验收（references/env/）**：

| 文件 | 内容 |
|------|------|
| `references/env/kde.md` | 本机 KDE 6.7.4 环境验收步骤 |
| `references/env/dde25.md` | Dev2-deepin DDE 25 环境验收步骤 |
| `references/env/dde20.md` | Dev2-107X DDE 20 环境验收步骤 |
| `references/env/gnome.md` | GNOME 50 环境验收步骤（backend 不可用） |
| `references/env/dde20-wayland.md` | company-04 DDE 20 Wayland 环境验收步骤 |

**测试用例（references/cases/）**：

| 文件 | 内容 |
|------|------|
| `references/cases/kwin.md` | KWin 后端：窗口管理、工作区、事件流、版本兼容 (TC-001~006) |
| `references/cases/dde.md` | DDE 后端：DDE 专有接口、系统服务 (TC-101~106) |
| `references/cases/input.md` | 输入子系统：键盘、鼠标、滚动 (TC-201~205) |
| `references/cases/capture.md` | 截图/录屏子系统 (TC-301~304) |
| `references/cases/atspi.md` | AT-SPI 无障碍模块 (TC-401~404) |
| `references/cases/routing.md` | 语义路由与降级链 (TC-501~506) |

> **落地状态（2026-08-30）**：上表 `references/` 共 11 个文件均已落盘于工作区技能库（`multica skill files list` 可见 `references/env/*.md` 5 个 + `references/cases/*.md` 6 个），QA agent 加载本技能时可直接读取。提示词仓 `tsic404/multica-agent` 按仓库惯例仅版本化平铺的 `skill/agent-shell-qa-testing.md`，不单独存放 `references/` 目录；`agent-shell` 主仓不含本技能文件。

