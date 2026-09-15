# company-04 DDE 20 Wayland 环境验收

## 环境信息

| 项 | 值 |
|----|----|
| 主机 | company-04 (100.113.128.117) |
| 用户 | uos |
| DE | DDE 20 (UOS Desktop 20 Professional) |
| 会话类型 | Wayland（startdde-wayland + kwin_wayland 5.27.2） |
| 系统 | UOS Desktop 20 Professional (arm64, dde-daemon 5.18.20.4) |
| 访问 | `ssh uos@100.113.128.117`（免密已配置） |
| 图形 bus | session bus（startdde-wayland 自动注入） |

## doctor 基准

```bash
ssh uos@100.113.128.117
agent-shell doctor
```

预期输出：

```
✓ DE 检测        : DDE (Wayland, <source>)
✓ 合成器         : dde-compositor
✓ DDE 版本     : DDE 20 (6/6 service families resolved)
✓ Wayland 协议 : 3/3 globals bound (window_mgmt v<N>, fake_input v5+, vd_mgmt bound)
✓ deepin-kwin   : org.kde.KWin v5.27.2
✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; /Scripting introspected)
✓ 输入注入   : fake_input authenticated ✓
⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）
✓ DDE 服务     : appearance, audio, display, lock, notification, power
✓ AT-SPI         : enabled (a11y bus up, Registry reachable)
✓ 截图捕获        : portal-screencast → portal-screenshot → x11（选中 <backend>）
✓ 输入后端        : libei → ydotool → xdotool（选中 <backend>）
```

> ⚠ SSH 非图形会话注意：SSH 登录后 `XDG_SESSION_TYPE=tty`、`XDG_CURRENT_DESKTOP` 为空，但 session bus 指向 startdde-wayland 所在会话（kwin_wayland PID 可经 `busctl --user status org.kde.KWin` 确认）。DE 检测依赖 `org.kde.KWin` D-Bus 探测兜底。

## DDE 20 关键验证命令

### 会话 bus 上的核心服务

```bash
# KWin + DDE 服务必须在 session bus 上
busctl --user list | grep -E 'org.kde.KWin|com.deepin|org.deepin.dde' | head -15
```

### KWin 版本

```bash
qdbus org.kde.KWin /KWin supportInformation 2>/dev/null | grep -i "KWin version"
# 预期：5.27.2
```

### DDE 20 服务命名（com.deepin.*）

```bash
# DDE 20 走 com.deepin.daemon.* 旧名（非 org.deepin.dde.*）
busctl --user list | grep -E 'com.deepin.daemon' | head -10
gdbus introspect --session --dest com.deepin.dde.Audio1 --object-path /com/deepin/dde/Audio1 2>&1 | head -5
```

## 版本兼容断言

| 断言 | DDE 20 预期 |
|------|------------|
| 服务命名 | `com.deepin.daemon.*`（DDE 20 旧名，非 org.deepin.dde.*） |
| 合成器 | kwin_wayland（deepin-kwin）5.27.2，org.kde.KWin 可达 |
| 会话类型 | Wayland（startdde-wayland） |
| 音频控制 | Sink 子对象（DDE 20/25 相同） |
| 区域监控 | `com.deepin.api.XEventMonitor` 可用 |

## 环境准备

1. uos 用户已登录 DDE Wayland 图形会话（startdde-wayland 拉起）
2. SSH 登录后 session bus 可直接访问
3. agent-shell 二进制已安装到 PATH
