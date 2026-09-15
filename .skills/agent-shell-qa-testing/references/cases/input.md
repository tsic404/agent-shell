# 输入子系统测试用例

## TC-201: 键盘输入
**步骤**: `agent-shell input key "ctrl+n"`  
**预期**: 编辑器收到按键

## TC-202: 文本输入
**步骤**: `agent-shell input type "hello agent-shell"`  
**预期**: 编辑器显示该文本

## TC-203: 鼠标点击
**步骤**: `agent-shell input click --at X,Y`  
**预期**: 鼠标移动到坐标并点击

## TC-204: 鼠标滚动
**步骤**: `agent-shell input scroll 0 3`  
**预期**: 页面向下滚动

## TC-205: 输入降级链
**步骤**: `agent-shell doctor` 查看输入后端  
**预期**: Wayland→libei→ydotool；X11→xdotool