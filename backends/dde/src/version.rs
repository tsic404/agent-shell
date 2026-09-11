//! DDE 版本探测与 20/25 服务名路由（设计文档 §10.4 `version.rs`、§21.36）。
//!
//! 探测策略（§21.36.4）：**不把版本假设写死**——按能力逐个探测服务名，
//! 先试 DDE25 主名 `org.deepin.dde.*`，失败退 DDE20 名
//! `com.deepin.daemon.*`。[`DdeVersion`] 只做两件事：
//!
//! 1. 汇总各服务族的探测命中结果，给出整体归类（Dde25 / Dde20 / Mixed /
//!    Unknown）供 doctor 输出；
//! 2. 为「DDE25 主名 + DDE20 别名并存」的服务族提供统一的候选名序。

use serde::{Deserialize, Serialize};

/// DDE25 主服务名前缀。
pub const DDE25_PREFIX: &str = "org.deepin.dde.";
/// DDE20 服务名前缀。
pub const DDE20_PREFIX: &str = "com.deepin.daemon.";

/// 各服务族的 DDE25/DDE20 候选名表（先新后旧，§21.36 兼容矩阵）。
///
/// 显示在 system bus；音频/通知在 session bus；电源/锁屏在 system bus。
/// 探测方按自身 bus 归属调用 [`candidates_for`] 后自行探测。
pub const SERVICE_FAMILIES: [(&str, [&str; 2]); 6] = [
    (
        "display",
        ["org.deepin.dde.Display1", "com.deepin.daemon.Display"],
    ),
    (
        "audio",
        ["org.deepin.dde.Audio1", "com.deepin.daemon.Audio"],
    ),
    (
        "power",
        ["org.deepin.dde.Power1", "com.deepin.daemon.Power"],
    ),
    (
        "notification",
        [
            "org.deepin.dde.Notification1",
            "com.deepin.daemon.Notification",
        ],
    ),
    (
        "lock",
        [
            "org.deepin.dde.LockService1",
            "com.deepin.daemon.LockService",
        ],
    ),
    (
        "appearance",
        ["org.deepin.dde.Appearance1", "com.deepin.daemon.Appearance"],
    ),
];

/// 按服务族名取候选服务名（先 DDE25 新名后 DDE20 旧名）。
pub fn candidates_for(family: &str) -> Option<&'static [&'static str; 2]> {
    SERVICE_FAMILIES
        .iter()
        .find(|(f, _)| *f == family)
        .map(|(_, names)| names)
}

/// DDE 版本归类。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DdeMajor {
    /// DDE 25：`org.deepin.dde.*` 命中。
    V25,
    /// 仅 `com.deepin.daemon.*` 旧名命中（纯净旧名环境；DDE20 混入少量
    /// `org.deepin.dde.*` 时归为 [`DdeMajor::Mixed`]）。
    V20,
    /// 两代名混合命中（DDE25 双名并存 / DDE20 少量 `org.deepin.dde.*`
    /// 兼容存在，均可能出现）——调用点仍以各自探测命中的名字为准，
    /// 归类仅用于 doctor 展示。
    Mixed,
    /// 无任何命中（非 DDE 环境 / 服务全缺席）。
    #[default]
    Unknown,
}

impl DdeMajor {
    /// doctor 展示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::V25 => "DDE 25",
            Self::V20 => "DDE 20",
            Self::Mixed => "DDE (mixed 20/25 names)",
            Self::Unknown => "unknown",
        }
    }
}

/// DDE 版本探测结果：各服务族实际命中的服务名。
///
/// 「版本」不是一次性判定的全局常量而是**逐族探测的命中记录**——
/// DDE25 双名并存环境下不同族可能落在不同代际的名字上（§21.36.1 实测：
/// 方法集一致，命中哪个名都能用）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DdeVersion {
    /// family → 实际命中的服务名；未命中不出现在表中。
    pub resolved: std::collections::BTreeMap<String, String>,
}

impl DdeVersion {
    /// 由逐族探测结果构造。
    ///
    /// `probes`：(family, 命中的服务名) 序列；未命中的族不传即可。
    pub fn from_probes(probes: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            resolved: probes.into_iter().collect(),
        }
    }

    /// 取某服务族实际命中的服务名。
    pub fn resolved_name(&self, family: &str) -> Option<&str> {
        self.resolved.get(family).map(String::as_str)
    }

    /// 整体归类：任一新名命中且存在旧名命中 → Mixed；只有新名 → V25；
    /// 只有旧名 → V20；无命中 → Unknown。
    pub fn major(&self) -> DdeMajor {
        let has_new = self.resolved.values().any(|n| n.starts_with(DDE25_PREFIX));
        let has_old = self.resolved.values().any(|n| n.starts_with(DDE20_PREFIX));
        match (has_new, has_old) {
            (true, true) => DdeMajor::Mixed,
            (true, false) => DdeMajor::V25,
            (false, true) => DdeMajor::V20,
            (false, false) => DdeMajor::Unknown,
        }
    }

    /// 已解析的服务族数（doctor「N/M reachable」分子）。
    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_order_is_new_then_old() {
        let c = candidates_for("display").unwrap();
        assert_eq!(c[0], "org.deepin.dde.Display1");
        assert_eq!(c[1], "com.deepin.daemon.Display");
        assert_eq!(
            candidates_for("nonexistent"),
            None,
            "未知族必须返回 None，不允许猜名字"
        );
    }

    #[test]
    fn major_classifies_by_prefixes() {
        let v25 = DdeVersion::from_probes([("display".into(), "org.deepin.dde.Display1".into())]);
        assert_eq!(v25.major(), DdeMajor::V25);

        let v20 = DdeVersion::from_probes([("audio".into(), "com.deepin.daemon.Audio".into())]);
        assert_eq!(v20.major(), DdeMajor::V20);

        let mixed = DdeVersion::from_probes([
            ("display".into(), "org.deepin.dde.Display1".into()),
            ("audio".into(), "com.deepin.daemon.Audio".into()),
        ]);
        assert_eq!(mixed.major(), DdeMajor::Mixed);
        assert_eq!(mixed.resolved_count(), 2);
        assert_eq!(
            mixed.resolved_name("audio"),
            Some("com.deepin.daemon.Audio")
        );

        assert_eq!(DdeVersion::default().major(), DdeMajor::Unknown);
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(DdeMajor::V25.label(), "DDE 25");
        assert_eq!(DdeMajor::V20.label(), "DDE 20");
        assert_eq!(DdeMajor::Unknown.label(), "unknown");
    }
}
