# 截图/录屏子系统测试用例

> 输出编码由捕获后端决定，与 `-f` 路径扩展名无关：X11/portal-screencast 像素路径写入 Netpbm PPM（P6），portal-screenshot 路径落盘 PNG。示例以 X11 目标为准，统一用 `.ppm`。

## TC-301: 全屏截图
**步骤**: `agent-shell screenshot -f /tmp/screen.ppm`  
**预期**: 文件生成，内容为当前屏幕

## TC-302: 窗口截图
**步骤**: `agent-shell screenshot --window <id> -f /tmp/win.ppm`  
**说明**: `<id>` 为十进制 X11 window id（window capture 为 X11-only）；`windows list` 的 `native_id`：X11 `0x{hex}` 需换算为十进制；Wayland `{uuid}` 无对应 X11 id、报 `invalid window id`  
**预期**: 仅包含目标窗口（P6 PPM）

## TC-303: 区域截图
**步骤**: `agent-shell screenshot --area 100 100 400 300 -f /tmp/area.ppm`  
**预期**: 指定区域截图

## TC-304: 截图降级链
**步骤**: `agent-shell doctor` 查看截图捕获  
**预期**: Wayland→portal-screencast；X11→x11

