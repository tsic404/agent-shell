//! CLI 命令行参数定义（设计文档 §17.2）。
//!
//! 子命令与 QA 用户旅程测试场景一一对应：doctor / windows / input /
//! screenshot / a11y / events / info / launch / system。

use agent_shell_rpc::keys::{Key, KeyCombo, KeyName, ModifierMask};
use clap::{Args as ClapArgs, Parser, Subcommand};

/// agent-shell——桌面自动化 CLI 入口。
#[derive(Parser, Debug)]
#[command(name = "agent-shell", version, about)]
pub struct Cli {
    /// 输出格式（table | json）
    #[arg(long = "output-format", global = true, default_value = "table")]
    pub output_format: OutputFormat,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// 输出格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}
#[derive(Subcommand, Debug)]
pub enum Command {
    /// 后端健康诊断（§16.3 doctor 诊断输出）
    Doctor,

    /// DE/后端/能力报告
    Info,

    /// 窗口管理（singular 别名 `window` 兼容设计文档 §17.2 命名）
    #[command(subcommand, alias = "window")]
    Windows(WindowsCommand),

    /// 工作区管理（singular 别名 `workspace`）
    #[command(subcommand, alias = "workspace")]
    Workspaces(WorkspacesCommand),

    /// 输入注入（XTest；XWayland 下不可用并明确报错，§6.3）
    #[command(subcommand)]
    Input(InputCommand),
    /// 截图捕获（X11 GetImage 路径；Wayland 会话待 capture 组件 T2b）
    Screenshot(ScreenshotCommand),
    /// 事件订阅/回放（§22.5 D4）
    #[command(subcommand)]
    Events(EventsCommand),
    /// daemon 管理（§22.2）
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// 输入法（§22.8 D7）
    #[command(subcommand)]
    Ime(ImeCommand),
    /// 亮度
    Brightness(BrightnessCommand),
    /// 文件操作
    #[command(subcommand)]
    File(FileCommand),
    /// MIME 默认应用
    #[command(subcommand)]
    Mime(MimeCommand),
    /// 蓝牙
    #[command(subcommand)]
    Bluetooth(BluetoothCommand),
    /// 软件管理
    #[command(subcommand)]
    Software(SoftwareCommand),
    /// 触控板
    #[command(subcommand)]
    Touchpad(TouchpadCommand),
    /// 键盘布局
    #[command(subcommand)]
    Kbd(KbdCommand),
    /// 密钥存储
    #[command(subcommand)]
    Secret(SecretCommand),
    /// 快捷键
    #[command(subcommand)]
    Shortcut(ShortcutCommand),
    /// 定时器
    #[command(subcommand)]
    Timer(TimerCommand),
    /// AT-SPI 无障碍（a11y bus 探测 + 语义查询）
    #[command(subcommand)]
    A11y(A11yCommand),
}

// ───────────────────────── windows ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum WindowsCommand {
    /// 列出当前窗口
    List {
        /// 按 app_id 过滤
        #[arg(long)]
        filter: Option<String>,
    },
    /// 显示窗口详情
    Info { target: String },
    /// 聚焦窗口
    Focus { target: String },
    /// 移动窗口
    Move { target: String, x: i32, y: i32 },
    /// 缩放窗口
    Resize {
        target: String,
        width: i32,
        height: i32,
    },
    /// 最小化窗口
    Minimize { target: String },
    /// 关闭窗口
    Close { target: String },
}

// ───────────────────────── workspaces ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum WorkspacesCommand {
    /// 列出工作区
    List,
    /// 切换工作区
    Switch { id: String },
}

// ───────────────────────── input ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum InputCommand {
    /// 发送按键组合（如 "ctrl+c"、"meta+t"、"Return"）
    Key {
        /// 按键组合：'+' 分隔修饰键与按键
        combo: String,
    },
    /// 键入文本
    Type { text: String },
    /// 鼠标点击
    Click {
        /// 鼠标按键（left/middle/right/back/forward）
        #[arg(long, default_value = "left")]
        button: String,
        /// 点击坐标 X,Y（缺省为当前位置）
        #[arg(long)]
        at: Option<String>,
    },
    /// 鼠标滚动
    Scroll { dx: i32, dy: i32 },
}

// ───────────────────────── screenshot ─────────────────────────

#[derive(ClapArgs, Debug)]
pub struct ScreenshotCommand {
    /// 截取指定窗口（native id）
    #[arg(long)]
    pub window: Option<String>,
    /// 截取区域 X,Y,W,H
    #[arg(long, num_args = 4, value_names = ["X", "Y", "W", "H"])]
    pub area: Option<Vec<i32>>,
    /// 输出文件路径
    #[arg(short = 'f', long = "file", default_value = "screenshot.ppm")]
    pub output: String,
}

// ───────────────────────── a11y ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum A11yCommand {
    /// a11y bus 与 AT-SPI Registry 可达性报告
    Status,
    /// 语义查询（role/name 过滤）
    Query {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
}

// ───────────────────────── events ─────────────────────────

/// 事件订阅/回放命令。
#[derive(Subcommand, Debug)]
pub enum EventsCommand {
    /// 订阅事件流（按类型过滤）
    Subscribe {
        #[arg(long)]
        filter: Option<String>,
    },
    /// 取消订阅
    Unsubscribe {
        #[arg(long)]
        id: String,
    },
    /// 回放历史事件
    Replay {
        #[arg(long)]
        filter: Option<String>,
    },
}

// ───────────────────────── daemon ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// daemon 状态
    Status,
    /// portal 会话
    Sessions,
}

// ───────────────────────── ime ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum ImeCommand {
    /// 输入法引擎子命令
    #[command(subcommand)]
    Engine(ImeEngineCommand),
    /// 通过 IME 输入文本
    Type { text: String },
}

#[derive(Subcommand, Debug)]
pub enum ImeEngineCommand {
    /// 列出可用引擎
    List,
    /// 设置当前引擎
    Set { engine: String },
    /// 查询当前引擎
    Current,
}

// ───────────────────────── brightness ─────────────────────────

/// 亮度命令：无值则查询，有值则设置。
#[derive(ClapArgs, Debug)]
pub struct BrightnessCommand {
    /// 亮度百分比（0-100）；省略则查询当前亮度
    pub value: Option<u32>,
}

// ───────────────────────── file ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum FileCommand {
    /// 文件选择器
    Pick,
    /// 删除到回收站
    Trash { path: String },
    /// 打开目录
    OpenDirectory { path: String },
}

// ───────────────────────── mime ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum MimeCommand {
    /// 查询默认应用
    Get { mime: String },
    /// 设置默认应用
    Set { mime: String, app: String },
    /// 默认浏览器
    DefaultBrowser,
}

// ───────────────────────── bluetooth ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum BluetoothCommand {
    /// 扫描
    Scan,
    /// 连接
    Connect { address: String },
    /// 断开
    Disconnect { address: String },
    /// 列出已配对设备
    List,
}

// ───────────────────────── software ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum SoftwareCommand {
    /// Flatpak 管理
    #[command(subcommand)]
    Flatpak(FlatpakCommand),
    /// 软件更新检查
    Updates,
}

#[derive(Subcommand, Debug)]
pub enum FlatpakCommand {
    /// 列出已安装 Flatpak
    List,
    /// 安装 Flatpak
    Install { app_id: String },
}

// ───────────────────────── touchpad ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum TouchpadCommand {
    /// 触控板状态
    Status,
    /// 开启触控板
    On,
    /// 关闭触控板
    Off,
    /// 自然滚动开关（on/off）
    NaturalScroll { state: String },
}

// ───────────────────────── kbd ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum KbdCommand {
    /// 键盘布局子命令
    #[command(subcommand)]
    Layout(KbdLayoutCommand),
}

#[derive(Subcommand, Debug)]
pub enum KbdLayoutCommand {
    /// 列出布局
    List,
    /// 设置布局
    Set { layout: String },
}

// ───────────────────────── secret ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum SecretCommand {
    /// 存储密钥
    Set { key: String, value: String },
    /// 读取密钥
    Get { key: String },
}

// ───────────────────────── shortcut ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum ShortcutCommand {
    /// 绑定快捷键到动作
    Bind { combo: String, action: String },
    /// 触发已绑定快捷键
    Trigger { combo: String },
}

// ───────────────────────── timer ─────────────────────────

#[derive(Subcommand, Debug)]
pub enum TimerCommand {
    /// 列出定时器
    List,
    /// 查询下次触发
    Next { name: String },
}

// ───────────────────────── 解析辅助 ─────────────────────────

/// 解析 "ctrl+c" / "meta+t" 形式的按键组合。
///
/// 语法：`+` 分隔的段；`ctrl|alt|shift|meta|super|win` 为修饰键，
/// 其余单字符按 [`Key::Char`]、多字符按命名键 [`KeyName`] 解析。
/// 未识别的键名报错——QA 场景下静默吞错比失败更难排查。
pub fn parse_key_combo(spec: &str) -> Result<KeyCombo, String> {
    let mut modifiers = ModifierMask::default();
    let mut keys = Vec::new();
    for part in spec.split('+').filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "meta" | "super" | "win" => modifiers.meta = true,
            other => {
                let key = if other.chars().count() == 1 {
                    Key::Char(other.chars().next().expect("count == 1"))
                } else {
                    Key::Named(parse_key_name(other)?)
                };
                keys.push(key);
            }
        }
    }
    if keys.is_empty() {
        return Err(format!("no non-modifier key in combo: {spec}"));
    }
    Ok(KeyCombo { keys, modifiers })
}

fn parse_key_name(name: &str) -> Result<KeyName, String> {
    use KeyName::*;
    let key = match name.to_ascii_lowercase().as_str() {
        "return" | "enter" => Return,
        "escape" | "esc" => Escape,
        "backspace" => BackSpace,
        "tab" => Tab,
        "space" => Space,
        "left" => Left,
        "right" => Right,
        "up" => Up,
        "down" => Down,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "insert" => Insert,
        "delete" => Delete,
        "menu" => Menu,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        _ => return Err(format!("unknown key name: {name}")),
    };
    Ok(key)
}

/// 窗口目标解析：标题子串 / `id:<native_id>`（daemon 返回的 WindowEntry）。
///
/// 匹配顺序：显式 id 前缀 → app_id 精确/包含 → 标题精确 → 标题子串。
/// 未命中时错误信息列出候选（QA 定位失败的常见原因是不在目标会话内）。
pub fn resolve_target_entry<'a>(
    spec: &str,
    windows: &'a [agent_shell_rpc::WindowEntry],
) -> Result<&'a agent_shell_rpc::WindowEntry, String> {
    let matches: Vec<&agent_shell_rpc::WindowEntry> = match spec.strip_prefix("id:") {
        Some(id) => windows.iter().filter(|w| w.native_id == id).collect(),
        None => {
            let by_title_exact: Vec<_> = windows.iter().filter(|w| w.title == spec).collect();
            if !by_title_exact.is_empty() {
                by_title_exact
            } else {
                // 子串回退：标题或 app_id 包含。
                windows
                    .iter()
                    .filter(|w| w.title.contains(spec) || w.app_id.contains(spec))
                    .collect()
            }
        }
    };
    match matches.len() {
        0 => Err(format!(
            "window not found: {spec:?}; visible titles: {}",
            titles(windows.iter())
        )),
        1 => Ok(matches[0]),
        n => Err(format!(
            "{n} windows match {spec:?}, disambiguate with id:; candidates: {}",
            titles(matches.into_iter())
        )),
    }
}

fn titles<'a, I>(windows: I) -> String
where
    I: IntoIterator<Item = &'a agent_shell_rpc::WindowEntry>,
{
    windows
        .into_iter()
        .map(|w| format!("[{}] {}", w.native_id, w.title))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 点击坐标 "X,Y" → JSON 数组 [x, y]（经 RPC 载荷传递）。
pub fn parse_xy_json(s: &str) -> Result<serde_json::Value, String> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y, got {s:?}"))?;
    let x: i64 = a.trim().parse().map_err(|_| format!("bad X in {s:?}"))?;
    let y: i64 = b.trim().parse().map_err(|_| format!("bad Y in {s:?}"))?;
    Ok(serde_json::json!([x, y]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_rpc::keys::{Key, KeyName};

    #[test]
    fn parses_modifier_combo() {
        let c = parse_key_combo("ctrl+c").expect("parse");
        assert_eq!(c.keys, vec![Key::Char('c')]);
        assert!(c.modifiers.ctrl);
        assert!(!c.modifiers.meta);
    }

    #[test]
    fn parses_named_and_meta_aliases() {
        for alias in ["meta+t", "super+t", "win+t"] {
            let c = parse_key_combo(alias).expect("parse");
            assert_eq!(c.keys, vec![Key::Char('t')]);
            assert!(c.modifiers.meta);
        }
        let c = parse_key_combo("Return").expect("parse");
        assert_eq!(c.keys, vec![Key::Named(KeyName::Return)]);
    }

    #[test]
    fn all_function_keys_parse() {
        // F1..F12 全覆盖（F1/F2 曾被遗漏，审查发现）。
        let names = [
            ("f1", KeyName::F1),
            ("f2", KeyName::F2),
            ("f3", KeyName::F3),
            ("f4", KeyName::F4),
            ("f5", KeyName::F5),
            ("f6", KeyName::F6),
            ("f7", KeyName::F7),
            ("f8", KeyName::F8),
            ("f9", KeyName::F9),
            ("f10", KeyName::F10),
            ("f11", KeyName::F11),
            ("f12", KeyName::F12),
        ];
        for (name, expected) in names {
            let c = parse_key_combo(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.keys, vec![Key::Named(expected)]);
        }
        assert!(parse_key_combo("F5").is_ok());
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_key_combo("nosuchkey").is_err());
        assert!(parse_key_combo("ctrl+alt").is_err());
    }

    #[test]
    fn resolve_target_entry_matches_and_reports() {
        let wins = vec![
            entry("w1", "Editor — main.rs"),
            entry("w2", "Terminal"),
            entry("w3", "doc"),
            entry("w4", "doc"),
        ];
        // app_id 包含匹配。
        assert_eq!(
            resolve_target_entry("main.rs", &wins)
                .expect("found")
                .native_id,
            "w1"
        );
        // id 前缀。
        assert_eq!(
            resolve_target_entry("id:w2", &wins)
                .expect("found")
                .native_id,
            "w2"
        );
        // 未命中错误信息包含候选。
        let err = resolve_target_entry("nope", &wins).unwrap_err();
        assert!(err.contains("w1") && err.contains("w2"), "{err}");
        // 歧义要求 id: 消歧。
        let amb = resolve_target_entry("doc", &wins).unwrap_err();
        assert!(amb.contains("disambiguate"), "{amb}");
    }

    #[test]
    fn parse_xy_json_produces_pair() {
        let v = parse_xy_json("100, 200").expect("xy");
        assert_eq!(v, serde_json::json!([100, 200]));
        assert!(parse_xy_json("bad").is_err());
    }

    fn entry(native: &str, title: &str) -> agent_shell_rpc::WindowEntry {
        agent_shell_rpc::WindowEntry {
            native_id: native.into(),
            title: title.into(),
            app_id: "kate".into(),
            pid: 1000,
            x: None,
            y: None,
            width: None,
            height: None,
            workspace: None,
        }
    }
}
