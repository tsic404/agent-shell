//! epoch 时间戳格式化（§21.34 systemd timer 触发时间、§21.22 portal 会话时间戳）。
//!
//! 工作区不引入日期库：civil-from-days 逆变换（Howard Hinnant 算法）即可覆盖
//! 1970 之后的 Unix 时间戳，输出 UTC ISO-8601（`YYYY-MM-DDTHH:MM:SSZ`）。
//! 时区依赖外部状态，不在此处转换——调用方需要本地时间时自行渲染。

/// epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
pub fn format_epoch_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// epoch 微秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
///
/// `0` 是 systemd 的「无下次触发」哨兵值，映射为空串而非 1970 年，避免消费者
/// 把「未安排」误读成「早已触发」。
pub fn format_epoch_usec(usec: u64) -> String {
    if usec == 0 {
        return String::new();
    }
    format_epoch_secs(usec / 1_000_000)
}

/// days since 1970-01-01 → (year, month, day)。
/// Algorithm from <http://howardhinnant.github.io/date_algorithms.html>。
pub fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epochs_in_utc() {
        assert_eq!(format_epoch_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(1_000_000_000), "2001-09-09T01:46:40Z");
        // 闰日：2024-02-29 与 2000-02-29（百年闰规则）。
        assert_eq!(format_epoch_secs(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_epoch_secs(951_782_400), "2000-02-29T00:00:00Z");
        // 2100 非闰年（整百年需被 400 整除）。
        assert_eq!(format_epoch_secs(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn formats_usec_and_keeps_zero_as_unscheduled() {
        // 实测 systemd LastTriggerUSec 样本：1790393550992423 → 2026-09-26T03:32:30Z。
        assert_eq!(
            format_epoch_usec(1_790_393_550_992_423),
            "2026-09-26T03:32:30Z"
        );
        assert_eq!(format_epoch_usec(1_000_000), "1970-01-01T00:00:01Z");
        assert_eq!(format_epoch_usec(0), "");
        // 亚秒部分截断（不四舍五入），避免「未到期」被渲染成已到期。
        assert_eq!(format_epoch_usec(1_999_999), "1970-01-01T00:00:01Z");
    }

    #[test]
    fn days_to_ymd_matches_civil_calendar_anchors() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(11_016), (2000, 2, 29));
        assert_eq!(days_to_ymd(19_782), (2024, 2, 29));
        assert_eq!(days_to_ymd(47_482), (2100, 1, 1));
    }
}
