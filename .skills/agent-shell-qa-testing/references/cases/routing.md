# 语义路由与降级链测试用例

## TC-501: 语义目标解析（Active）
**步骤**: resolve_target(Active)  
**预期**: 返回当前活动窗口

## TC-502: 语义目标解析（ByAppId）
**步骤**: resolve_target(ByAppId("firefox"))  
**预期**: 找到匹配窗口

## TC-503: 标题匹配模式
**步骤**: 四种模式（Substring/Exact/Regex/Glob）逐一测试  
**预期**: 均正确匹配

## TC-504: 输入语义降级
**步骤**: TypeText + AT-SPI 不可用 → 键盘模拟  
**预期**: 文本最终输入成功

## TC-505: 等待命令
**步骤**: windows wait <app_id> --timeout <duration>  
**预期**: 窗口出现后返回 Success；超时返回 Timeout

## TC-506: 后端降级链
**步骤**: doctor 查看后端降级  
**预期**: KDE→kwin→at-spi→x11；DDE→dde→at-spi
