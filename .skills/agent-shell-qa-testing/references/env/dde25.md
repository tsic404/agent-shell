# Dev2-deepin (192.168.122.216) DDE 25 环境验收

## 环境信息

| 项 | 值 |
|----|----|
| 主机 | 192.168.122.216（静态 hostname `uos-PC`，与 Dev2-107X 同名，以 IP 区分） |
| 用户 | uos |
| DE | DDE 25 (Deepin 25) X11 |
| dde-daemon | 6.1.84 / KWin 6.1.17 |

## doctor 基准

预期输出：
```
✓ DE 检测        : DDE (X11, <source>)
✓ 合成器         : dde-compositor
✓ DDE 版本     : DDE 25 (6/6 service families resolved)
✓ deepin-kwin   : org.kde.KWin v6.1.17
✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; /Scripting introspected)
⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）
✓ DDE 服务     : appearance, audio, display, lock, notification, power
✓ AT-SPI         : enabled (a11y bus up, Registry reachable)
✓ 截图捕获        : portal-screencast → portal-screenshot → x11（选中 <backend>）
✓ 输入后端        : libei → ydotool → xdotool（选中 <backend>）
```

## 关键验证命令

```bash
# 服务名探测（DDE25 主名）
busctl --user list | grep -E 'org.deepin.dde' | head -10
# 音频 Sink 子对象
gdbus introspect --session --dest org.deepin.dde.Audio1
# 显示
gdbus introspect --session --dest org.deepin.dde.Display1
# 应用启动
which dde-am
```

## 版本兼容断言

| 断言 | DDE 25 预期 |
|------|------------|
| 服务命名 | org.deepin.dde.* 可达 |
| 音频控制 | Sink 子对象 |
| 应用启动 | dde-am 存在 |
| 区域监控 | com.deepin.api.XEventMonitor 可用 |

