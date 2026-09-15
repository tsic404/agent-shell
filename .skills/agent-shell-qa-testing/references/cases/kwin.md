# KWin 后端测试用例

## TC-001: 窗口列表
**步骤**: `agent-shell windows`  
**预期**: 返回 WindowInfo[]，含 id/title/app_id/geometry

## TC-002: 聚焦窗口
**步骤**: `agent-shell window focus <known_id>`  
**预期**: 窗口被激活

## TC-003: 移动窗口
**步骤**: `agent-shell window move <known_id> 100 100`  
**预期**: 窗口移动到 (100, 100)

## TC-004: 工作区切换
**步骤**: `agent-shell workspace switch <ws_id>`  
**预期**: 工作区切换成功

## TC-005: 事件流
**前置条件**: doctor 显示 event script loaded  
**预期**: 打开新窗口时收到 WindowAdded 事件

## TC-006: KWin 版本兼容
**步骤**: `agent-shell doctor` 检查 KWin 版本行  
**预期**: 版本号正确解析，KWin 5/6 脚本路径均兼容