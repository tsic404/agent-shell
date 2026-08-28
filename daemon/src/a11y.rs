//! daemon 侧 a11y/AT-SPI 探测（§23.3：AT-SPI 连接归属 daemon 域）。

/// AT-SPI Registry 可达性报告行（✓/⚠ 前缀；doctor 与 a11y.status 共用）。
///
/// 探测与 core `dbus_service_exists` 同口径（busctl）；busctl 缺失视为
/// 不可达并注明原因。
pub fn atspi_line() -> String {
    for addr in a11y_bus_addresses() {
        let out = std::process::Command::new("busctl")
            .args(["--address", &addr, "status", "org.a11y.atspi.Registry"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                return "✓ AT-SPI         : enabled (a11y bus up, Registry reachable)".into()
            }
            Ok(_) => continue,
            Err(e) => return format!("⚠ AT-SPI         : unavailable (busctl: {e})"),
        }
    }
    "⚠ AT-SPI         : unavailable (org.a11y.atspi.Registry not reachable)".into()
}

/// a11y bus 地址候选：环境变量 → `$XDG_RUNTIME_DIR/at-spi/bus_0`（现代
/// at-spi2 标准 socket 名）→ `$XDG_RUNTIME_DIR/at-spi/bus`（旧版兜底）。
///
/// TSI-2486：旧实现只探测 `at-spi/bus`，而实际运行环境（及组件桥接
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
        // 无 a11y bus 的环境（CI/容器）下返回 ⚠ 行而非 panic。
        let line = atspi_line();
        assert!(line.starts_with('✓') || line.starts_with('⚠'), "{line}");
    }
}
