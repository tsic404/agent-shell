//! agent-shell-rootd：特权薄代理层（设计文档 §23.2 / §23.4.3）。
//!
//! 职责边界（§23.2「无业务逻辑，薄代理层」）：
//! - 仅实现白名单方法（§23.4.3 D-Bus 接口的方法名即操作名）
//! - 每个方法独立 polkit action（`com.agentshell.*`，§23.4.2）
//! - 无状态、无缓存、无用户域业务逻辑
//!
//! 传输：D-Bus system bus（systemd system unit 启动）。本 crate 提供
//! 方法分派与参数校验核心（可单测），D-Bus 服务注册在 main 中完成。
//! 五组核心场景（doctor/windows/input/screenshot/a11y）全部归属 daemon
//! 域（§23.3 矩阵）——rootd 不承载它们。

use serde_json::{json, Value};

/// 白名单方法执行结果。
pub type RootResult = Result<Value, String>;

/// 分派一个 rootd 方法调用（方法名 = §23.4.3 D-Bus 接口方法名）。
///
/// 非白名单方法一律拒绝——rootd 的安全模型是「默认拒绝 + 显式白名单」。
pub fn dispatch(method: &str, args: &[Value]) -> RootResult {
    match method {
        "Hello" => hello(),
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceEnable" | "ServiceDisable"
        | "ServiceReload" => service_control(method, args),
        "DaemonReload" => daemon_reload(),
        "JournalQuery" => journal_query(args),
        "SysctlGet" => sysctl_get(args),
        "SysctlSet" => sysctl_set(args),
        _ => Err(format!("method not in whitelist: {method}")),
    }
}

fn hello() -> RootResult {
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "security_model": 1, // §23.4.3 版本对账字段
    }))
}

fn str_arg(args: &[Value], idx: usize) -> Result<&str, String> {
    args.get(idx)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("arg[{idx}] must be a string"))
}

/// systemd system 单元控制。单元名做基本合法性校验（防 shell 注入面：
/// 实际执行走 systemd D-Bus API 而非 shell，但入口仍拒绝可疑字符）。
fn service_control(method: &str, args: &[Value]) -> RootResult {
    let unit = str_arg(args, 0)?;
    validate_unit_name(unit)?;
    // TODO(polkit): 经 org.freedesktop.systemd1 system bus 执行；
    // 当前骨架仅校验与记账，实际接线在 rootd D-Bus 服务任务落地。
    tracing::info!(
        method,
        unit,
        "service control requested (polkit action: com.agentshell.service.control)"
    );
    Ok(json!({ "accepted": true, "unit": unit, "action": method }))
}

fn validate_unit_name(unit: &str) -> Result<(), String> {
    if unit.is_empty() || unit.len() > 256 {
        return Err("invalid unit name length".into());
    }
    if !unit
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+'))
    {
        return Err(format!("illegal character in unit name: {unit:?}"));
    }
    Ok(())
}

fn daemon_reload() -> RootResult {
    tracing::info!("daemon-reload requested (polkit action: com.agentshell.systemd.manage)");
    Ok(json!({ "accepted": true }))
}

fn journal_query(args: &[Value]) -> RootResult {
    let filter = str_arg(args, 0)?;
    // 过滤表达式必须是合法 JSON 对象（结构化查询，非自由文本拼接）。
    let parsed: Value = serde_json::from_str(filter)
        .map_err(|e| format!("journal filter must be JSON object: {e}"))?;
    if !parsed.is_object() {
        return Err("journal filter must be a JSON object".into());
    }
    tracing::info!("journal query requested (polkit action: com.agentshell.system-log.view)");
    Ok(json!({ "accepted": true }))
}

fn sysctl_get(args: &[Value]) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    let value = std::fs::read_to_string(&path).map_err(|e| format!("sysctl read {key}: {e}"))?;
    Ok(json!({ "key": key, "value": value.trim() }))
}

fn sysctl_set(args: &[Value]) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let value = args.get(1).ok_or("arg[1] (value) required")?;
    tracing::info!(
        key,
        ?value,
        "sysctl set requested (polkit action: com.agentshell.sysctl.set)"
    );
    Ok(json!({ "accepted": true, "key": key }))
}

/// sysctl key 只允许 `a-z0-9.-_/`（防路径穿越 `/proc/sys/...`）。
fn validate_sysctl_key(key: &str) -> Result<(), String> {
    if key.contains("..") || key.starts_with('/') {
        return Err(format!("illegal sysctl key: {key:?}"));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_' | '/'))
    {
        return Err(format!("illegal character in sysctl key: {key:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_reports_version_and_security_model() {
        let r = dispatch("Hello", &[]).expect("hello");
        assert_eq!(r["security_model"], 1);
        assert!(r["version"].as_str().is_some());
    }

    #[test]
    fn non_whitelisted_method_is_rejected() {
        let e = dispatch("PackageInstall", &[json!(["curl"])]).unwrap_err();
        assert!(e.contains("not in whitelist"), "{e}");
        // 五组核心场景方法不属于 rootd（§23.3 daemon 域）。
        assert!(dispatch("windows.list", &[]).is_err());
    }

    #[test]
    fn service_control_validates_unit_name() {
        assert!(dispatch("ServiceStart", &[json!("nginx.service")]).is_ok());
        assert!(dispatch("ServiceStop", &[json!("evil; rm -rf /")]).is_err());
        assert!(dispatch("ServiceRestart", &[json!("../escape")]).is_err());
        assert!(dispatch("ServiceEnable", &[json!("")]).is_err());
    }

    #[test]
    fn journal_query_requires_json_object() {
        assert!(dispatch("JournalQuery", &[json!(r#"{"unit":"nginx"}"#)]).is_ok());
        assert!(dispatch("JournalQuery", &[json!("free text")]).is_err());
    }

    #[test]
    fn sysctl_key_validation_blocks_traversal() {
        assert!(dispatch("SysctlGet", &[json!("net.ipv4.ip_forward")]).is_ok());
        assert!(dispatch("SysctlSet", &[json!("../../etc/passwd"), json!(1)]).is_err());
        assert!(dispatch("SysctlSet", &[json!("/abs/path"), json!(1)]).is_err());
    }
}
