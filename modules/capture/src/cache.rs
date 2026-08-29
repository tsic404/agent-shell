//! 变化检测缓存（设计文档 §13.4）。
//!
//! 像素级差分检测：轮询截图场景下，跳过内容未变化的帧，
//! 避免上层重复上传/分析。语义：
//!
//! - 首帧（无历史帧）必返回 `true`；
//! - 长度不同视为变化；
//! - RGB 任一通道差 > 10 记为一个变化像素（忽略 alpha）；
//! - 变化像素比例超过 threshold（默认 0.01 = 1%）才判定画面有变。

/// 变化检测缓存：保存上一帧并做快速差分。
pub struct CaptureCache {
    last_frame: Option<Vec<u8>>,
    threshold: f64,
}

impl Default for CaptureCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureCache {
    /// 创建缓存，threshold 默认 0.01（1% 像素变化）。
    pub fn new() -> Self {
        Self::with_threshold(0.01)
    }

    /// 创建缓存并指定变化阈值（0.0–1.0）。
    pub fn with_threshold(threshold: f64) -> Self {
        Self {
            last_frame: None,
            threshold,
        }
    }

    /// 快速像素级差分检测。
    ///
    /// 输入为 RGBA（每像素 4 字节）原始帧；首帧返回 `true` 并记录。
    /// 判定为变化后同样更新历史帧。
    pub fn has_changed(&mut self, new_frame: &[u8]) -> bool {
        let changed = match &self.last_frame {
            None => true,
            Some(last) => {
                if last.len() != new_frame.len() {
                    true
                } else {
                    let total = (last.len() / 4).max(1);
                    let (last_px, _last_tail) = last.as_chunks::<4>();
                    let (new_px, _new_tail) = new_frame.as_chunks::<4>();
                    let changed = last_px
                        .iter()
                        .zip(new_px.iter())
                        .filter(|(a, b)| {
                            a[0].abs_diff(b[0]) > 10
                                || a[1].abs_diff(b[1]) > 10
                                || a[2].abs_diff(b[2]) > 10
                        })
                        .count();
                    (changed as f64 / total as f64) > self.threshold
                }
            }
        };
        if changed {
            self.last_frame = Some(new_frame.to_vec());
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 n 个相同 RGBA 像素的帧。
    fn frame(n: usize, rgba: [u8; 4]) -> Vec<u8> {
        rgba.iter().copied().cycle().take(n * 4).collect()
    }

    #[test]
    fn first_frame_is_always_changed() {
        let mut cache = CaptureCache::new();
        assert!(cache.has_changed(&frame(10, [1, 2, 3, 255])));
    }

    #[test]
    fn identical_second_frame_is_unchanged() {
        let mut cache = CaptureCache::new();
        let f = frame(100, [10, 20, 30, 255]);
        assert!(cache.has_changed(&f));
        assert!(!cache.has_changed(&f));
    }

    #[test]
    fn length_change_counts_as_changed() {
        let mut cache = CaptureCache::new();
        assert!(cache.has_changed(&frame(10, [0, 0, 0, 255])));
        assert!(cache.has_changed(&frame(11, [0, 0, 0, 255])));
    }

    #[test]
    fn sub_threshold_noise_is_ignored() {
        // 1% 阈值：100 像素中仅 1 像素大改 → 未过阈值。
        let mut cache = CaptureCache::new();
        let base = frame(100, [100, 100, 100, 255]);
        assert!(cache.has_changed(&base));
        let mut next = base.clone();
        next[0] = 200; // 单像素 R 通道差 100 > 10
        assert!(
            !cache.has_changed(&next),
            "1/100 pixel change must be below default threshold"
        );
    }

    #[test]
    fn over_threshold_change_detected() {
        let mut cache = CaptureCache::new();
        let base = frame(100, [0, 0, 0, 255]);
        assert!(cache.has_changed(&base));
        // 5% 像素整体变白 → 超过 1% 阈值。
        let mut next = base.clone();
        for px in next.as_chunks_mut::<4>().0.iter_mut().step_by(20) {
            px[0] = 250;
            px[1] = 250;
            px[2] = 250;
        }
        assert!(cache.has_changed(&next));
    }

    #[test]
    fn small_deltas_below_tolerance_are_not_changes() {
        // 通道差 ≤ 10 不算变化像素（噪声容限）。
        let mut cache = CaptureCache::new();
        let base = frame(50, [100, 100, 100, 255]);
        assert!(cache.has_changed(&base));
        let next = frame(50, [108, 105, 110, 255]);
        assert!(!cache.has_changed(&next));
    }

    #[test]
    fn alpha_channel_only_change_is_ignored() {
        let mut cache = CaptureCache::new();
        let base = frame(50, [7, 8, 9, 255]);
        assert!(cache.has_changed(&base));
        let next = frame(50, [7, 8, 9, 128]);
        assert!(!cache.has_changed(&next));
    }

    #[test]
    fn custom_threshold_is_honored() {
        let mut cache = CaptureCache::with_threshold(0.0);
        let f = frame(10, [0, 0, 0, 255]);
        assert!(cache.has_changed(&f));
        // threshold=0 时任何单像素差异都判定变化。
        let mut next = f.clone();
        next[3] = 250; // alpha 差异不计——换 RGB
        next[0] = 20;
        assert!(cache.has_changed(&next));
    }
}
