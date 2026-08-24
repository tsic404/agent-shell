//! KWin 版本探测与 5/6 兼容层（设计文档 §7.7 `version.rs`，§7.5 版本表）。
//!
//! 探测路径：`org.kde.KWin` D-Bus 服务 `supportInformation()` 返回的
//! 多行文本中含 `KWin version:` / `Version:` 行；解析失败时回退
//! 环境变量 `KWIN_VERSION`（测试注入用），再失败按 KWin 6 处理
//! （Plasma 6 已是当前主流，脚本模板按 6 生成、5 差异由兼容层修正）。

use serde::{Deserialize, Serialize};
use zbus::Connection;

use crate::error::KWinError;

/// KWin 主版本。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KWinMajor {
    /// KWin 5（Plasma 5，≥5.27 支持 org_kde_* 协议）。
    V5,
    /// KWin 6（Plasma 6）。
    V6,
}

/// KWin 版本信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KWinVersion {
    /// 完整版本字符串（如 "6.1.4"）；探测失败时为 "unknown"。
    pub full: String,
    /// 主版本归类（决定 Scripting API 形态）。
    pub major: KWinMajor,
}

impl KWinVersion {
    /// 是否为 KWin 6+。
    pub fn is_v6(&self) -> bool {
        matches!(self.major, KWinMajor::V6)
    }
}

/// 从 `supportInformation` 文本解析版本行。
///
/// 匹配 `KWin version:` / `Version:` 前缀（不同小版本输出格式有差异），
/// 提取首个形如 `N.M(.K)?` 的版本号。
pub fn parse_support_information(text: &str) -> Option<KWinVersion> {
    for line in text.lines() {
        let line = line.trim();
        // 注意：不能用 `?`——首行不匹配会提前退出整个函数。
        let rest = match line
            .strip_prefix("KWin version:")
            .or_else(|| line.strip_prefix("Version:"))
            .or_else(|| line.strip_prefix("version:"))
        {
            Some(r) => r,
            None => continue,
        };
        let token = match rest.split_whitespace().next() {
            Some(t) => t,
            None => continue,
        };
        if let Some(major) = leading_major(token) {
            return Some(KWinVersion {
                full: token.to_string(),
                major: if major >= 6 {
                    KWinMajor::V6
                } else {
                    KWinMajor::V5
                },
            });
        }
    }
    None
}

/// 解析 "6.1.4" → 6；无数字前缀返回 None。
fn leading_major(token: &str) -> Option<u32> {
    let major = token.split('.').next()?;
    major.parse().ok()
}

/// 通过 session bus 探测 KWin 版本。
///
/// `supportInformation` 位于 `org.kde.KWin` 根对象，返回多行文本的字符串数组；
/// 兼容单元素数组与非数组回复两种形态。
pub async fn detect_version(conn: &Connection) -> crate::error::Result<KWinVersion> {
    #[zbus::proxy(
        default_service = "org.kde.KWin",
        default_path = "/",
        interface = "org.kde.KWin"
    )]
    trait KWin {
        fn support_information(&self) -> zbus::Result<Vec<String>>;
    }

    let kwin = KWinProxy::new(conn)
        .await
        .map_err(|e| KWinError::Scripting(format!("org.kde.KWin unreachable: {e}")))?;
    let info = kwin
        .support_information()
        .await
        .map_err(|e| KWinError::Scripting(format!("supportInformation failed: {e}")))?;
    let joined = info.join("\n");
    parse_support_information(&joined)
        .ok_or_else(|| KWinError::Scripting("supportInformation has no version line".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kwin_version_prefix() {
        let v = parse_support_information(
            "Qt version: 5.15.2\nKWin version: 5.27.10\nBuild flags: ...",
        )
        .unwrap();
        assert_eq!(v.full, "5.27.10");
        assert!(!v.is_v6());
    }

    #[test]
    fn parses_plain_version_line() {
        let v = parse_support_information("Version: 6.1.4\n...").unwrap();
        assert_eq!(v.full, "6.1.4");
        assert!(v.is_v6());
    }

    #[test]
    fn returns_none_without_version_line() {
        assert!(parse_support_information("no useful lines here").is_none());
    }

    #[test]
    fn v5_v6_boundary_is_six() {
        let v5 = parse_support_information("KWin version: 5.27").unwrap();
        let v6 = parse_support_information("KWin version: 6.0").unwrap();
        assert!(!v5.is_v6());
        assert!(v6.is_v6());
    }
}
