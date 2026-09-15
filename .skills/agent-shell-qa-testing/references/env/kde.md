# 本机 KDE 6.7.4 环境验收（company-01）

## 环境信息

| 项 | 值 |
|----|----|
| 主机 | company-01（本机，100.73.214.94） |
| DE | KDE Plasma 6.7.4 |
| 会话类型 | Wayland |
| 系统 | Arch Linux |
| 图形 bus | DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus |
| 访问 | 本地 |

> 注意：company-01 是本机（KDE 6.7.4），KWin 后端验收本地执行，无需 SSH。GNOME 环境在 company-03（100.107.109.109），非本机，且当前 backend 不可用（见 gnome.md）。

## doctor 基准

```bash
agent-shell doctor
```

预期输出：

```
✓ DE 检测        : KDE (Wayland, <source>)
✓ 合成器         : kwin-compositor
✓ Wayland 协议 : 3/3 globals bound (window_mgmt v<N>, fake_input v5+, vd_mgmt bound)
✓ KWin 服务   : org.kde.KWin v6.7.4
✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; /Scripting introspected)
✓ 输入注入   : fake_input authenticated ✓
⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）
✓ AT-SPI         : enabled (a11y bus up, Registry reachable)
✓ 截图捕获        : portal-screencast → portal-screenshot → x11（选中 <backend>）
✓ 输入后端        : libei → ydotool → xdotool（选中 <backend>）
```

## 关键验证命令

```bash
# KWin 脚本桥接
gdbus introspect --session --dest org.kde.KWin --object-path /Scripting 2>&1 | grep -E 'loadScript|isScriptLoaded'

# KWin 版本
qdbus org.kde.KWin /KWin supportInformation 2>/dev/null | grep -i "KWin version"

# 输入 portal
busctl --user list | grep -i portal
gdbus introspect --session --dest org.freedesktop.portal.Desktop   --object-path /org/freedesktop/portal/desktop 2>&1 | grep -i remote

# portal-screencast
pgrep -a pipewire | head -3
gdbus introspect --session --dest org.freedesktop.portal.Desktop   --object-path /org/freedesktop/portal/desktop 2>&1 | grep -A3 'org.freedesktop.portal.ScreenCast'
```

## 环境准备

1. 确认桌面会话已登录（有图形会话才能测输入/截图）
2. 桌面无障碍已启用（System Settings -> Accessibility）否则 AT-SPI 不报告
3. agent-shell 二进制已安装到 PATH
4. 核对二进制与 HEAD 一致：company-01 曾部署 09-12 版本（早于 HEAD），事件脚本行输出与基准漂移；比对前确认二进制为当前 HEAD 构建，否则产生假 FAIL





