//! GNOME Shell 版本探测（设计文档 §8.4 `version.rs`）。
//!
//! 探测路径：`org.gnome.Shell` 根对象的 `ShellVersion` 只读属性（§3.5：
//! 版本稳定，与 Eval 无关、始终可读）。解析失败时回退环境变量
//! `GNOME_SHELL_VERSION`（测试注入用），再失败按 47+ 处理——47+ 是
//! 当前主流，Extension 路径是推荐生产路径。
//!
//! 版本边界（§8.1）：GNOME 47 起 Eval 默认禁用（unsafe-mode 收紧），
//! 47 以下 Eval 可用但可能被 gsettings `developer-tools` 关闭。

use serde::{Deserialize, Serialize};
use zbus::Connection;

use crate::error::{MutterError, Result, SHELL_PATH, SHELL_SERVICE};

/// GNOME 主版本归类（决定双路径选择）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GnomeMajor {
    /// GNOME < 47：Eval 路径可用（默认开启）。
    Pre47,
    /// GNOME ≥ 47：Eval 默认禁用，走 Extension 路径。
    V47Plus,
}

/// GNOME Shell 版本信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GnomeVersion {
    /// 完整版本字符串（如 "47.0"）；探测失败时为 "unknown"。
    pub full: String,
    /// 主版本归类。
    pub major: GnomeMajor,
}

impl GnomeVersion {
    /// 是否为 GNOME 47+（Eval 受限边界）。
    pub fn is_47_plus(&self) -> bool {
        matches!(self.major, GnomeMajor::V47Plus)
    }

    /// 按版本归类选初始路径（§8.4 双路径选择）。
    pub fn preferred_path(&self) -> GnomeMajor {
        self.major
    }
}

/// 从 ShellVersion 字符串解析版本（"47.0"、"45.2" → major 归类）。
///
/// GNOME Shell 的版本号形如 `MAJOR.MINOR(.MICRO)?`，偶有 "40.alpha"
/// 这类后缀——取首个数字段为主版本，无数字前缀返回 None。
pub fn parse_shell_version(raw: &str) -> Option<GnomeVersion> {
    let token = raw.split_whitespace().next()?;
    let major: u32 = token.split('.').next()?.parse().ok()?;
    let major = if major >= 47 {
        GnomeMajor::V47Plus
    } else {
        GnomeMajor::Pre47
    };
    Some(GnomeVersion {
        full: token.to_string(),
        major,
    })
}

/// 通过 session bus 探测 GNOME Shell 版本。
///
/// 读 `org.gnome.Shell.ShellVersion` 属性；环境变量 `GNOME_SHELL_VERSION`
/// 仅在属性读取失败时作为兜底（测试注入用）。
pub async fn detect_version(conn: &Connection) -> Result<GnomeVersion> {
    #[zbus::proxy(
        default_service = "org.gnome.Shell",
        default_path = "/org/gnome/Shell",
        interface = "org.freedesktop.DBus.Properties"
    )]
    trait Properties {
        fn get(&self, interface: &str, name: &str) -> zbus::Result<zbus::zvariant::OwnedValue>;
    }

    let props = PropertiesProxy::new(conn)
        .await
        .map_err(|e| MutterError::Version(format!("{SHELL_SERVICE} unreachable: {e}")))?;
    match props.get(SHELL_SERVICE, "ShellVersion").await {
        Ok(v) => {
            let raw: String = v
                .try_into()
                .map_err(|e| MutterError::Version(format!("ShellVersion not a string: {e}")))?;
            parse_shell_version(&raw)
                .ok_or_else(|| MutterError::Version(format!("unrecognized ShellVersion `{raw}`")))
        }
        Err(e) => {
            // 属性不可达（服务未起 / 权限收紧）：环境变量兜底。
            if let Ok(raw) = std::env::var("GNOME_SHELL_VERSION") {
                return parse_shell_version(&raw).ok_or_else(|| {
                    MutterError::Version(format!("unrecognized GNOME_SHELL_VERSION `{raw}`"))
                });
            }
            Err(MutterError::Version(format!(
                "{SHELL_PATH} ShellVersion read failed: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pre47_version() {
        let v = parse_shell_version("45.2").unwrap();
        assert_eq!(v.full, "45.2");
        assert!(!v.is_47_plus());
        assert!(matches!(v.preferred_path(), GnomeMajor::Pre47));
    }

    #[test]
    fn parses_47plus_version() {
        let v = parse_shell_version("47.0").unwrap();
        assert_eq!(v.full, "47.0");
        assert!(v.is_47_plus());
    }

    #[test]
    fn boundary_is_forty_seven() {
        assert!(!parse_shell_version("46.5").unwrap().is_47_plus());
        assert!(parse_shell_version("47").unwrap().is_47_plus());
        assert!(parse_shell_version("48.beta").unwrap().is_47_plus());
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(parse_shell_version("alpha").is_none());
        assert!(parse_shell_version("").is_none());
    }
}
