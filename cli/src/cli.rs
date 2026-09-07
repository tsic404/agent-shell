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

    /// 「daemon 连接提前关闭」时重建连接重试次数（§19 短退避）
    #[arg(long = "retry", global = true, default_value_t = 0)]
    pub retry: u32,

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
    /// 截图捕获（portal ScreenCast / Screenshot / X11 三级降级链）
    Screenshot(ScreenshotCommand),
    /// 事件订阅/回放（§22.5 D4）
    #[command(subcommand)]
    Events(EventsCommand),
    /// daemon 管理（§22.2）
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// 安全策略管理（status / grant / revoke / audit，§22.7 D6）
    #[command(subcommand)]
    Security(SecurityCommand),
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
    /// 系统服务控制（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Service(ServiceCommand),
    /// 文件系统挂载/卸载（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Fs(FsCommand),
    /// 系统日志查看（rootd 特权链路，§23.4）
    Log(LogCommand),
    /// 主机名管理（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Hostname(HostnameCommand),
    /// 进程管理（rootd 特权链路，§23.4）
    Kill {
        /// 目标进程 PID
        pid: i32,
        /// 信号编号（默认 15 = SIGTERM）
        #[arg(long, default_value = "15")]
        signal: i32,
    },
    /// Job 状态查询（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Job(JobCommand),
    /// 系统包管理（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Pkg(PkgCommand),
    /// 内核参数管理（rootd 特权链路，§23.4）
    #[command(subcommand)]
    Sysctl(SysctlCommand),
}

// ───────────────────────── windows ─────────────────────────

/// 窗口标题匹配模式（`windows list` / `windows wait` 的 `--match`）。
///
/// 与 core `TitleMatchMode` 一一对应，wire 值固定为小写 snake_case。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MatchMode {
    /// 子串包含（默认，兼容现有调用）
    Substring,
    /// 精确匹配
    Exact,
    /// 正则匹配
    Regex,
    /// Glob 通配符
    Glob,
}

impl MatchMode {
    /// 线格式字符串（与 core `TitleMatchMode` 的 snake_case serde 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            MatchMode::Substring => "substring",
            MatchMode::Exact => "exact",
            MatchMode::Regex => "regex",
            MatchMode::Glob => "glob",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum WindowsCommand {
    /// 列出当前窗口
    List {
        /// 按 app_id 或标题过滤
        #[arg(long)]
        filter: Option<String>,
        /// 标题匹配模式（默认 substring；须与 --filter 搭配）
        #[arg(long = "match", value_enum, requires = "filter")]
        match_mode: Option<MatchMode>,
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
    /// 等待 app_id/标题窗口出现（轮询 windows.list）
    Wait {
        /// 目标 app_id 或标题模式
        app_id: String,
        /// 标题匹配模式（默认 substring）
        #[arg(long = "match", value_enum)]
        match_mode: Option<MatchMode>,
        /// 超时毫秒（默认 15000）
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
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
        /// 点击坐标，格式 "X,Y"（如 100,200）；缺省为当前位置
        #[arg(long, value_name = "X,Y")]
        at: Option<String>,
    },
    /// 鼠标滚动（dx/dy 支持负数直传，如 `scroll -2 0` 向上滚动）
    Scroll {
        #[arg(allow_negative_numbers = true)]
        dx: i32,
        #[arg(allow_negative_numbers = true)]
        dy: i32,
    },
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
    /// 语义查询（role/name 过滤；--all 允许省略过滤查询全树）
    Query {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// 通配匹配：允许省略 role/name 查询全树；提供时仍作为过滤条件
        #[arg(long, default_value_t = false)]
        all: bool,
        /// 零命中时按失败处理（exit 2），供需要区分空结果的脚本调用方使用
        #[arg(short = 'c', long = "fail-on-empty", default_value_t = false)]
        fail_on_empty: bool,
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

// ───────────────────────── security ─────────────────────────

/// 安全策略管理（§22.7 D6）：策略面 RPC 的 CLI 透传。
#[derive(Subcommand, Debug)]
pub enum SecurityCommand {
    /// 当前安全配置真值（默认确认级别、白/黑名单、审计路径）
    Status,
    /// 授权 agent 到指定级别（L0..L4）
    Grant {
        /// agent 标识
        agent_id: String,
        /// 权限级别（L0..L4）
        level: String,
    },
    /// 撤销 agent 白名单
    Revoke {
        /// agent 标识
        agent_id: String,
    },
    /// 读回审计日志（可按 agent_id / op / decision / result 过滤）
    Audit {
        /// 按 agent 过滤
        #[arg(long)]
        agent_id: Option<String>,
        /// 按操作名过滤
        #[arg(long)]
        op: Option<String>,
        /// 按判定结果过滤（allow / confirm / deny）
        #[arg(long)]
        decision: Option<String>,
        /// 按执行态过滤（true = 仅已执行成功，false = 仅未执行/失败）
        #[arg(long)]
        result: Option<bool>,
    },
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

// ───────────────────────── service（rootd 特权链路） ─────────────────────────

/// 系统服务控制命令（rootd ServiceStart/Stop/Restart/Enable/Disable/Reload，§23.4）。
#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// 启动系统服务
    Start { unit: String },
    /// 停止系统服务
    Stop { unit: String },
    /// 重启系统服务
    Restart { unit: String },
    /// 启用系统服务（开机自启）
    Enable { unit: String },
    /// 禁用系统服务（取消开机自启）
    Disable { unit: String },
    /// 重载系统服务配置
    Reload { unit: String },
    /// 重载 systemd 管理器配置（daemon-reload）
    DaemonReload,
}

// ───────────────────────── log（rootd 特权链路） ─────────────────────────

/// 系统日志查看命令（rootd JournalQuery，§23.4）。
#[derive(ClapArgs, Debug)]
pub struct LogCommand {
    /// 过滤表达式 JSON（如 '{"unit":"nginx","priority":"err"}'）
    #[arg(long = "filter", default_value = "{}")]
    pub filter: String,
}

// ───────────────────────── hostname（rootd 特权链路） ─────────────────────────

/// 主机名管理命令（rootd HostnameSet，§23.4）。
#[derive(Subcommand, Debug)]
pub enum HostnameCommand {
    /// 设置系统主机名
    Set { name: String },
}

// ───────────────────────── fs（rootd 特权链路） ─────────────────────────

/// 文件系统挂载/卸载命令（rootd Mount/Unmount，§23.4）。
#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// 挂载文件系统
    Mount {
        /// 块设备路径（如 /dev/sda1）
        device: String,
        /// 挂载点路径
        target: String,
        /// 文件系统类型
        #[arg(long)]
        fstype: String,
        /// 挂载选项（逗号分隔，如 rw,noatime）
        #[arg(long, value_delimiter = ',')]
        options: Option<Vec<String>>,
    },
    /// 卸载文件系统
    Unmount {
        /// 挂载点路径
        target: String,
    },
}

// ───────────────────────── job（rootd 特权链路） ─────────────────────────

/// Job 状态查询命令（rootd JobStatus，§23.4）。
#[derive(Subcommand, Debug)]
pub enum JobCommand {
    /// 查询 job 状态
    Status { job_id: String },
}

// ───────────────────────── pkg（rootd 特权链路） ─────────────────────────

/// 系统包管理命令（rootd 特权链路，§23.4）。
#[derive(Subcommand, Debug)]
pub enum PkgCommand {
    /// 安装系统软件包
    Install {
        /// 待安装的软件包名
        packages: Vec<String>,
        /// 等待 job 完成（阻塞至退出）
        #[arg(long)]
        wait: bool,
    },
    /// 移除系统软件包
    Remove {
        /// 待移除的软件包名
        packages: Vec<String>,
        /// 等待 job 完成（阻塞至退出）
        #[arg(long)]
        wait: bool,
    },
    /// 升级系统软件包（无参数 = 全部升级）
    Update {
        /// 待升级的软件包名（省略 = 全部升级）
        #[arg(num_args = 0..)]
        packages: Vec<String>,
        /// 等待 job 完成（阻塞至退出）
        #[arg(long)]
        wait: bool,
    },
    /// 刷新包元数据缓存
    Refresh {
        /// 等待 job 完成（阻塞至退出）
        #[arg(long)]
        wait: bool,
    },
}

// ───────────────────────── sysctl（rootd 特权链路） ─────────────────────────

/// 内核参数读写命令（rootd SysctlGet/SysctlSet，§23.4）。
#[derive(Subcommand, Debug)]
pub enum SysctlCommand {
    /// 读取内核参数
    Get { key: String },
    /// 设置内核参数
    Set { key: String, value: String },
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

/// 窗口目标解析：标题子串 / `id:<native_id>` / 裸 `native_id`（daemon 返回的 WindowEntry）。
///
/// 匹配顺序：显式 id（`id:` 前缀或与 `native_id` 精确相等）→ 标题精确 →
/// 标题/app_id 子串。未命中时错误信息列出候选（QA 定位失败的常见原因是不在目标会话内）。
pub fn resolve_target_entry<'a>(
    spec: &str,
    windows: &'a [agent_shell_rpc::WindowEntry],
) -> Result<&'a agent_shell_rpc::WindowEntry, String> {
    let matches: Vec<&agent_shell_rpc::WindowEntry> = match spec.strip_prefix("id:") {
        Some(id) => windows
            .iter()
            .filter(|w| native_id_matches(id, &w.native_id))
            .collect(),
        None => {
            // 裸 native_id 回退：`windows list` 返回的 native_id（如 KWin 的
            // `{uuid}`）可直接复用，无需改写为 `id:{uuid}`。
            let by_id: Vec<_> = windows
                .iter()
                .filter(|w| native_id_matches(spec, &w.native_id))
                .collect();
            if !by_id.is_empty() {
                by_id
            } else {
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

/// 候选 id 是否命中 native_id：精确相等，或互为 `{...}` 包裹形式。
///
/// KWin `internalId.toString()` 产出 `{uuid}`，用户可能裸传 `uuid` 或原文
/// `{uuid}`；剥离单层花括号后比较可同时覆盖两种写法。
fn native_id_matches(candidate: &str, native_id: &str) -> bool {
    candidate == native_id || strip_braces(candidate) == strip_braces(native_id)
}

/// 剥离单层 `{...}` 花括号；非包裹形式原样返回。
fn strip_braces(s: &str) -> &str {
    s.strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(s)
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

/// `--at` 的统一格式提示：clap 解析误用（[`at_syntax_hint`]）与运行时
/// 坐标解析失败（[`parse_xy_json`]）共用，保证两条失败路径给同一写法示例。
const AT_SYNTAX_HINT: &str = "`--at` 需要单个 \"X,Y\" 坐标（如 `--at 100,200`）";

/// 点击坐标 "X,Y" → JSON 数组 [x, y]（经 RPC 载荷传递）。
///
/// 解析失败的错误串附带 [`AT_SYNTAX_HINT`]，与 clap 层的 [`at_syntax_hint`]
/// 同文案（运行时 `--at 100,abc` 此前缺少 X,Y 格式提示）。
pub fn parse_xy_json(s: &str) -> Result<serde_json::Value, String> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y, got {s:?}；{AT_SYNTAX_HINT}"))?;
    let x: i64 = a
        .trim()
        .parse()
        .map_err(|_| format!("bad X in {s:?}；{AT_SYNTAX_HINT}"))?;
    let y: i64 = b
        .trim()
        .parse()
        .map_err(|_| format!("bad Y in {s:?}；{AT_SYNTAX_HINT}"))?;
    Ok(serde_json::json!([x, y]))
}

/// 识别 `input click --at` 的两种常见误用并返回合并写法提示。
///
/// `--at` 只接受单个 "X,Y" 值。用户常误写为：
/// - `--at X Y`：坐标被拆成两个词，clap 报 `UnknownArgument`；
/// - `--at X --at Y`：clap 报 `ArgumentConflict`，`InvalidArg` 为 "--at <X,Y>"。
///
/// `--at X Y` 的判定需同时满足两个条件：argv 中某个 `--at` 的后继 token
/// 缺逗号（坐标被拆词，见 [`split_at_value`]），且 clap 报的意外参数是裸词
/// （`InvalidArg` 不以 `-` 开头）。后者排除 `input key --at 5` 这类命令
/// 本身无 `--at` 选项、`--at` 被当作意外 flag 的情况；前者排除
/// `--at 100,200 extra`（用户已用对写法、仅多了裸词参数）。
pub fn at_syntax_hint(args: &[std::ffi::OsString], err: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ErrorKind};
    let invalid = err.get(ContextKind::InvalidArg).map(ToString::to_string);
    let at_misuse = match err.kind() {
        ErrorKind::ArgumentConflict => invalid.as_deref().is_some_and(|s| s.starts_with("--at")),
        ErrorKind::UnknownArgument => {
            split_at_value(args) && invalid.as_deref().is_some_and(|s| !s.starts_with('-'))
        }
        _ => false,
    };
    at_misuse.then(|| format!("{AT_SYNTAX_HINT}；`--at 100 100` 或 `--at 100 --at 200` 均不可用"))
}

/// 是否存在某个 `--at`，其后继 token 不含逗号（坐标被拆成两个词）。
fn split_at_value(args: &[std::ffi::OsString]) -> bool {
    args.windows(2)
        .any(|w| w[0] == std::ffi::OsStr::new("--at") && !w[1].to_string_lossy().contains(','))
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
    fn resolve_target_entry_accepts_bare_native_id() {
        // KWin `internalId.toString()` 产出 `{uuid}`；`windows list` 直接复制的
        // 原文 `{uuid}` 与裸 `uuid` 均应命中，无需改写为 `id:{uuid}`。
        let wins = vec![entry("{e6f8-4a2c}", "Editor"), entry("w2", "Terminal")];
        assert_eq!(
            resolve_target_entry("{e6f8-4a2c}", &wins)
                .expect("braced native_id")
                .native_id,
            "{e6f8-4a2c}"
        );
        assert_eq!(
            resolve_target_entry("e6f8-4a2c", &wins)
                .expect("bare native_id")
                .native_id,
            "{e6f8-4a2c}"
        );
        assert_eq!(
            resolve_target_entry("id:e6f8-4a2c", &wins)
                .expect("id prefix with bare uuid")
                .native_id,
            "{e6f8-4a2c}"
        );
    }

    #[test]
    fn scroll_accepts_negative_numbers_without_double_dash() {
        // `input scroll -2 0` 不经 `--` 直接传负值（TSI-2915）。
        let Cli {
            command: Some(Command::Input(InputCommand::Scroll { dx, dy })),
            ..
        } = Cli::try_parse_from(["agent-shell", "input", "scroll", "-2", "0"]).expect("parse")
        else {
            panic!("expected scroll");
        };
        assert_eq!(dx, -2);
        assert_eq!(dy, 0);

        // 双负值与正负混合同样可直传。
        let Cli {
            command: Some(Command::Input(InputCommand::Scroll { dx, dy })),
            ..
        } = Cli::try_parse_from(["agent-shell", "input", "scroll", "3", "-4"]).expect("parse")
        else {
            panic!("expected scroll");
        };
        assert_eq!(dx, 3);
        assert_eq!(dy, -4);

        // `--` 分隔写法保持可用（向后兼容）。
        let Cli {
            command: Some(Command::Input(InputCommand::Scroll { dx, dy })),
            ..
        } = Cli::try_parse_from(["agent-shell", "input", "scroll", "--", "-2", "0"])
            .expect("parse")
        else {
            panic!("expected scroll");
        };
        assert_eq!(dx, -2);
        assert_eq!(dy, 0);
    }

    #[test]
    fn parse_xy_json_produces_pair() {
        let v = parse_xy_json("100, 200").expect("xy");
        assert_eq!(v, serde_json::json!([100, 200]));
        assert!(parse_xy_json("bad").is_err());
    }

    #[test]
    fn parse_xy_json_error_includes_at_hint() {
        // 运行时解析失败须给出 --at 100,200 写法示例（TSI-2875 缺口）。
        for bad in ["bad", "100,abc", "abc,200"] {
            let err = parse_xy_json(bad).unwrap_err();
            assert!(err.contains("--at 100,200"), "{bad}: {err}");
        }
    }

    #[test]
    fn at_syntax_hint_detects_misuse() {
        // `--at X Y` → clap UnknownArgument（第二个坐标被拒，裸词）。
        let err = Cli::try_parse_from(["agent-shell", "input", "click", "--at", "100", "100"])
            .unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&["agent-shell", "input", "click", "--at", "100", "100"]),
            &err
        )
        .is_some());

        // `--at X --at Y` → clap ArgumentConflict（InvalidArg = "--at <X,Y>"）。
        let err = Cli::try_parse_from([
            "agent-shell",
            "input",
            "click",
            "--at",
            "100",
            "--at",
            "200",
        ])
        .unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&[
                "agent-shell",
                "input",
                "click",
                "--at",
                "100",
                "--at",
                "200"
            ]),
            &err
        )
        .is_some());

        // 无关命令（无 --at）的 UnknownArgument 不提示。
        let err =
            Cli::try_parse_from(["agent-shell", "input", "scroll", "1", "2", "3"]).unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&["agent-shell", "input", "scroll", "1", "2", "3"]),
            &err
        )
        .is_none());

        // --at 已正确但多了未知 flag：不误报（后继 token 含逗号）。
        let err = Cli::try_parse_from([
            "agent-shell",
            "input",
            "click",
            "--at",
            "100,200",
            "--bogus",
        ])
        .unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&[
                "agent-shell",
                "input",
                "click",
                "--at",
                "100,200",
                "--bogus"
            ]),
            &err
        )
        .is_none());

        // 负例（审查要求）：--at 已正确，仅多了裸词参数，不提示。
        let err =
            Cli::try_parse_from(["agent-shell", "input", "click", "--at", "100,200", "extra"])
                .unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&["agent-shell", "input", "click", "--at", "100,200", "extra"]),
            &err
        )
        .is_none());

        // 负例（审查要求）：命令本身无 --at 选项，--at 被当作意外 flag，不提示。
        let err = Cli::try_parse_from(["agent-shell", "input", "key", "--at", "5"]).unwrap_err();
        assert!(at_syntax_hint(
            &os_args(&["agent-shell", "input", "key", "--at", "5"]),
            &err
        )
        .is_none());
    }

    #[test]
    fn match_mode_wire_values_are_snake_case() {
        assert_eq!(MatchMode::Substring.as_str(), "substring");
        assert_eq!(MatchMode::Exact.as_str(), "exact");
        assert_eq!(MatchMode::Regex.as_str(), "regex");
        assert_eq!(MatchMode::Glob.as_str(), "glob");
    }

    #[test]
    fn windows_list_parses_match_flag() {
        let cli = Cli::try_parse_from([
            "agent-shell",
            "windows",
            "list",
            "--filter",
            "Kate",
            "--match",
            "regex",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Windows(WindowsCommand::List { filter, match_mode })) => {
                assert_eq!(filter.as_deref(), Some("Kate"));
                assert_eq!(match_mode, Some(MatchMode::Regex));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // 默认无 --match：None（daemon 侧回落 substring）。
        let cli = Cli::try_parse_from(["agent-shell", "windows", "list"]).expect("parse");
        match cli.command {
            Some(Command::Windows(WindowsCommand::List { filter, match_mode })) => {
                assert!(filter.is_none());
                assert!(match_mode.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn windows_wait_parses_match_flag() {
        let cli = Cli::try_parse_from([
            "agent-shell",
            "windows",
            "wait",
            "Konsole",
            "--match",
            "glob",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Windows(WindowsCommand::Wait {
                app_id, match_mode, ..
            })) => {
                assert_eq!(app_id, "Konsole");
                assert_eq!(match_mode, Some(MatchMode::Glob));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn windows_list_rejects_invalid_match_value() {
        assert!(
            Cli::try_parse_from(["agent-shell", "windows", "list", "--match", "bogus"]).is_err()
        );
    }

    #[test]
    fn windows_list_match_requires_filter() {
        // `--match` 无 `--filter` 时 clap 报参数错误（避免静默全量返回）。
        let err = Cli::try_parse_from(["agent-shell", "windows", "list", "--match", "regex"])
            .unwrap_err();
        assert!(err.to_string().contains("--filter"), "{err}");
    }

    fn os_args(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect()
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
