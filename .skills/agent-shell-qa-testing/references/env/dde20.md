# Dev2-107X DDE 20 环境验收

## 环境信息

| 项 | 值 |
|----|----|
| 主机 | Dev2-107X (192.168.122.57) |
| 用户 | uos |
| DE | DDE 20 (UOS 20 Pro) X11 |
| dde-daemon | 5.19.16 / KWin 5.27.2 |

## doctor 基准

预期输出：
```
✓ DE 检测        : DDE (X11, <source>)
✓ 合成器         : dde-compositor
✓ DDE 版本     : DDE 20 (6/6 service families resolved)
✓ deepin-kwin   : org.kde.KWin v5.27.2
✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; /Scripting introspected)
⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）
✓ DDE 服务     : appearance, audio, display, lock, notification, power
✓ AT-SPI         : enabled (a11y bus up, Registry reachable)
✓ 截图捕获        : portal-screencast → portal-screenshot → x11（选中 <backend>）
✓ 输入后端        : libei → ydotool → xdotool（选中 <backend>）
```

## 关键验证命令

```bash
# 服务名：以 com.deepin.daemon.* 为主（约 24~25 项），少量 org.deepin.dde.* 兼容存在（约 2 项）
busctl --user list | grep -cE 'com.deepin.daemon'    # 预期 24~25（主）
busctl --user list | grep -cE 'org.deepin.dde'       # 预期 2（兼容）
busctl --user list | grep -E 'com.deepin.daemon' | head -10
# 音频 Sink（与 DDE25 一致）
gdbus introspect --session --dest com.deepin.daemon.Audio
# KWayland 服务（DDE20 独有）
gdbus introspect --session --dest com.deepin.daemon.KWayland
# 屏保独立服务
gdbus introspect --session --dest com.deepin.daemon.ScreenSaver
```

## daemon 重启

> ⚠ 禁止 `pkill -f agent-shell-daemon`：`-f` 按完整命令行匹配，远程执行该命令的 SSH 会话自身 argv 也含 `agent-shell-daemon`，会连带杀死连接（exit 255，无输出）。

> 本机 daemon **未纳入 systemd**：`systemctl --user list-unit-files | grep agent-shell` 无结果（2026-09-13 实测），故 `systemctl --user restart agent-shell-daemon` 会报 unit 不存在，勿用。daemon 为 ELF 二进制（非 shebang 脚本），kill 后无 supervisor 自动拉起，须手动重新启动。

```bash
# ✓ 先取 PID 再 kill——pidof 按完整进程名匹配，不受 comm 截断影响
kill $(pidof agent-shell-daemon)

# ✓ 等价：pkill -x 按 comm（截断到 15 字符）精确匹配；daemon 的 comm 实测为 agent-shell-dae
pkill -x agent-shell-dae

# 重新拉起：daemon 以 stdin 为唯一协议通道，EOF 即退出（main.rs:108 "stdin closed; exiting"）。
# stdin 受限（/dev/null、重定向、后台）会立即退出，须保持 stdin 打开。非交互可行写法：
# 二进制定位：~/agent-shell-<TSI>/target/release/agent-shell-daemon（QA 实测 ~/agent-shell-3085/target/release/agent-shell-daemon）
# --foreground 为 no-op（见 TSI-3096），省略亦可：
tail -f /dev/null | ~/agent-shell-<TSI>/target/release/agent-shell-daemon
```

> ⚠ 手动常驻期间 `agent-shell doctor` 会因单实例锁报 `daemon already running`（lock wait timed out after 30s）；跑 doctor 基线前须先 `pkill -x agent-shell-dae` 停掉常驻实例，再由 CLI 自行 fork/exec 拉起——本节「kill 后手动重启」与技能「doctor 是测试前置条件」互斥，常驻仅用于非 doctor 的现场排障。

> 设备实测闭环（Dev2-107X / uos-PC，2026-09-13）：`file agent-shell-daemon` → ELF（comm 截断成立，排除 shebang 解释器陷阱）；`ps -o comm= -p $(pidof agent-shell-daemon)` → `agent-shell-dae`；`pgrep -x agent-shell-daemon` 无命中、`pgrep -x agent-shell-dae` 命中；`pidof agent-shell-daemon` 命中。

## 版本兼容断言

| 断言 | DDE 20 预期 |
|------|------------|
| 服务命名 | 以 com.deepin.daemon.* 为主，少量 org.deepin.dde.* 兼容存在 |
| 音频控制 | Sink 子对象（与 DDE25 一致） |
| KWayland 服务 | 存在（DDE20 独有） |
| 屏保 | com.deepin.daemon.ScreenSaver 独立服务 |


