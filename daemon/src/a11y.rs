//! daemon 侧 a11y/AT-SPI 探测（§23.3：AT-SPI 连接归属 daemon 域）。

use agent_shell_a11y::atspi_bridge::{
    BUS_SERVICE, REGISTRY_SERVICE, ROOT_PATH, STATUS_IFACE, STATUS_PATH,
};

/// AT-SPI Registry 可达性报告行（✓/⚠ 前缀；doctor 与 a11y.status 共用）。
///
/// 探测与 core `dbus_service_exists` 同口径（busctl）；busctl 缺失视为
/// 不可达并注明原因。可达时附 [`a11y_detail`]：只报「Registry 可达」会与
/// 实际枚举结果矛盾——无障碍未启用时 Registry 可达但应用树为空。
pub fn atspi_line() -> String {
    for addr in a11y_bus_addresses() {
        let out = std::process::Command::new("busctl")
            .args(["--address", &addr, "status", REGISTRY_SERVICE])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                return format!(
                    "✓ AT-SPI         : enabled (a11y bus up, Registry reachable{})",
                    a11y_detail(&addr)
                )
            }
            Ok(_) => continue,
            Err(e) => return format!("⚠ AT-SPI         : unavailable (busctl: {e})"),
        }
    }
    format!("⚠ AT-SPI         : unavailable ({REGISTRY_SERVICE} not reachable)")
}

/// 状态行附加细节（尽力而为：读取失败即省略对应片段）：会话 AT-SPI 启用态 +
/// Registry 已注册节点数。
///
/// 「Registry 可达但应用数为 0」正是无障碍未启用的形态——工具包（Qt/GTK）在
/// `org.a11y.Status` 为假时不向 a11y bus 注册，`a11y query` 因此恒空。把启用位
/// 与节点数一并报出，操作者无需二次排查即可定位。
fn a11y_detail(a11y_addr: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    match busctl_property(&[
        "--user",
        "get-property",
        BUS_SERVICE,
        STATUS_PATH,
        STATUS_IFACE,
        "IsEnabled",
    ])
    .as_deref()
    {
        Some("true") => parts.push("AT-SPI on".to_string()),
        Some(_) => parts.push("AT-SPI off (a11y query enables it on demand)".to_string()),
        None => {}
    }
    if let Some(count) = busctl_property(&[
        "--address",
        a11y_addr,
        "get-property",
        REGISTRY_SERVICE,
        ROOT_PATH,
        "org.a11y.atspi.Accessible",
        "ChildCount",
    ]) {
        parts.push(format!("{count} nodes registered"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("; {}", parts.join(", "))
    }
}

/// 读一个 D-Bus 属性（busctl），返回其值文本（如 `true` / `23`）。
///
/// `busctl get-property` 输出 `<签名> <值>`；解析失败或调用失败返回 `None`。
fn busctl_property(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("busctl")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

/// a11y bus 地址候选：环境变量 → `$XDG_RUNTIME_DIR/at-spi/bus_0`（现代
/// at-spi2 标准 socket 名）→ `$XDG_RUNTIME_DIR/at-spi/bus`（旧版兜底）。
///
/// 旧实现只探测 `at-spi/bus`，而实际运行环境（及组件桥接
/// `atspi_bridge.rs` 文档）使用 `at-spi/bus_0`，导致 doctor 恒报
/// 「unavailable (Registry not reachable)」假阴性。
fn a11y_bus_addresses() -> Vec<String> {
    let mut addrs = Vec::new();
    if let Ok(a) = std::env::var("AT_SPI_BUS_ADDRESS") {
        addrs.push(a);
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        addrs.push(format!("unix:path={runtime}/at-spi/bus_0"));
        addrs.push(format!("unix:path={runtime}/at-spi/bus"));
    }
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_env_first_then_runtime_dir() {
        unsafe { std::env::set_var("AT_SPI_BUS_ADDRESS", "unix:path=/tmp/custom-bus") };
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        let addrs = a11y_bus_addresses();
        assert_eq!(addrs[0], "unix:path=/tmp/custom-bus");
        assert!(addrs.contains(&"unix:path=/run/user/1000/at-spi/bus_0".to_string()));
        assert!(addrs.contains(&"unix:path=/run/user/1000/at-spi/bus".to_string()));
        unsafe { std::env::remove_var("AT_SPI_BUS_ADDRESS") };
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[test]
    fn atspi_line_never_panics_without_bus() {
        // 无 a11y bus 的环境（CI/容器）下返回 ⚠ 行而非 panic；可达时保留
        // doctor/a11y.status 基准前缀（QA 以该前缀判定「bus up、Registry
        // 可达」），附加细节只允许出现在其后。
        let line = atspi_line();
        assert!(line.starts_with('✓') || line.starts_with('⚠'), "{line}");
        if line.starts_with('✓') {
            assert!(
                line.starts_with("✓ AT-SPI         : enabled (a11y bus up, Registry reachable"),
                "{line}"
            );
        }
    }
}
