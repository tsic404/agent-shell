# DDE 后端测试用例

## TC-101: 窗口列表（委托 KWin）
**步骤**: `agent-shell windows`  
**预期**: 返回窗口列表，委托 KWin 成功

## TC-102: DDE 服务名探测（DDE25）
**步骤**: `busctl --user list | grep org.deepin.dde`  
**预期**: 服务可达

## TC-103: DDE 服务名探测（DDE20）
**步骤**: `busctl --user list | grep -E 'com.deepin.daemon|org.deepin.dde'`  
**预期**: 以 com.deepin.daemon.* 为主，少量 org.deepin.dde.* 兼容存在

## TC-104: 音频控制（Sink 子对象）
**步骤**: 读取 DefaultSink → SetVolume(0.5)  
**预期**: DDE 20/25 均走 Sink 子对象

## TC-105: 系统服务
**步骤**: `agent-shell brightness get` / `notify send`  
**预期**: 亮度返回有效值，通知弹出

## TC-106: 版本兼容报告
**步骤**: `agent-shell doctor` 查看版本报告  
**预期**: 正确显示探测到的服务命名方案
