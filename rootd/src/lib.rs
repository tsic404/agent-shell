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

pub mod dbus;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
/// 阻塞性系统命令的默认超时（秒）。
///
/// `journalctl` 全量查询在超大 journal（数百万条）上可运行数分钟——
/// 超时使命令失败而非无限占用线程。`JournalQuery` 有更长的专项超时
/// [`JOURNAL_QUERY_TIMEOUT`]。本常量是实际执行阻塞命令的方法
/// （systemctl/sysctl/hostnamectl/mount/umount）的上界；Package* 方法
/// 只返回 job id 不阻塞执行，30s 对其是无实际约束的兜底。
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 并发阻塞命令的准入上限（TSI-2504）。
///
/// D-Bus 服务层用 [`tokio::sync::Semaphore`] 限制同时运行、经 `spawn_blocking`
/// 执行并受 [`COMMAND_TIMEOUT`] 约束的**同步阻塞命令**数（journalctl/
/// sysctl/hostnamectl/mount/umount，及 systemd D-Bus 调用）：16 路并发足以
/// 覆盖正常负载，且远低于 tokio 默认 512 个 blocking 线程的上限——防止
/// 恶意或失控的调用潮耗尽线程池与 spawn 的短生命周期子进程。
///
/// 注意：Package* 类方法只注册 job 即返回，真实包管理器进程在
/// `spawn_package_job` 的独立线程中异步执行，不占用 blocking 线程池，
/// 也不受本闸约束。
pub const MAX_CONCURRENT_BLOCKING_COMMANDS: usize = 16;

/// `JournalQuery` 专项超时（秒）。
///
/// 有界查询（默认 `--lines` 上界）通常在 1s 内返回；给足 60s 覆盖
/// 冷缓存与慢磁盘场景，同时保证绝不无限阻塞 rootd 事件循环。
pub const JOURNAL_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// `JournalQuery` 默认行数上界（无显式 `limit` 时强制）。
pub const DEFAULT_JOURNAL_LINES: u64 = 1000;

/// `JournalQuery` 显式 `limit` 的最大值（防 `{"limit": <极大值>}` 绕过上界）。
pub const MAX_JOURNAL_LINES: u64 = 10000;

/// `JournalQuery` 默认时间上界（无 `since`/`until`/`boot` 时强制）。
pub const DEFAULT_JOURNAL_SINCE: &str = "-24h";

/// 单个阻塞分派的命令跟踪槽（TSI-2545）。
///
/// `dispatch_with_slot` 在阻塞线程内执行；`spawn_blocking` + `timeout` 超时后
/// 该线程被 detach，异步包装层无法再接触其中的局部变量。本结构把取消意图
/// 与子进程 pidfd 放进跨线程共享状态：包装层超时后置取消位并等待 kill 目标，
/// 阻塞线程在 `Command::spawn` 前后各检查一次取消位——spawn 前检查跳过延迟
/// 的 spawn，spawn 后检查在包装层来不及 kill 时自行清理并 reap。
/// [`tokio::sync::Notify`] 让等待以「pidfd 可用」或「分派结束」为准，取代
/// 固定 200ms 宽限启发式；异步等待不阻塞 tokio 工作线程（TSI-2504 事件
/// 循环隔离的延续）。
pub(crate) struct CommandSlot {
    /// 包装层超时后置位；阻塞线程据此跳过 spawn 或 spawn 后自清理。
    cancelled: AtomicBool,
    /// 阻塞线程分派结束（正常或异常返回）后置位；`cancel` 也会置位以封住
    /// 分派闭包尚未开始执行时的等待上界。等待方据此提前退出。
    finished: AtomicBool,
    /// 阻塞线程 spawn 成功后立即写入的 pidfd；包装层据此精确 kill。
    pidfd: Mutex<Option<OwnedFd>>,
    /// 在 pidfd 写入或分派结束（finished）时通知等待方。
    state_changed: tokio::sync::Notify,
}

impl CommandSlot {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            pidfd: Mutex::new(None),
            state_changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // 与 `mark_finished`/`store_pidfd` 同理：置 finished 并 notify_one，
        // 使 `wait_for_kill_target` 在分派闭包尚未开始执行（blocking 池饱和，
        // 闭包仍在队列，`dispatch_with_slot` 无从走到 `mark_finished`）时立即
        // 返回 None，包装层随即返回超时错误——封住无界等待（TSI-2545 C2）。
        self.finished.store(true, Ordering::Release);
        self.state_changed.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn store_pidfd(&self, fd: OwnedFd) {
        *self.pidfd.lock().expect("pidfd slot poisoned") = Some(fd);
        // `notify_one` 在无等待方时存储 permit，使「检查状态 → notified()」
        // 的等待循环不会因写入与通知间的竞态错过唤醒；单等待方契约见
        // `wait_for_kill_target`。
        self.state_changed.notify_one();
    }

    fn take_pidfd(&self) -> Option<OwnedFd> {
        self.pidfd.lock().expect("pidfd slot poisoned").take()
    }

    fn clear_pidfd(&self) {
        *self.pidfd.lock().expect("pidfd slot poisoned") = None;
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
        // 与 `store_pidfd` 同理用 `notify_one`：无等待方时存储 permit，
        // 使等待循环在「检查 finished → notified()」间不因竞态漏醒。
        self.state_changed.notify_one();
    }

    /// 等待 kill 目标就绪：返回可精确 kill 子进程的 pidfd；分派在 spawn 前
    /// 已结束（如取消在 spawn 前命中）则返回 `None`。不再依赖固定宽限时长。
    ///
    /// 「先检查状态、再 `notified()`、取回后复查」循环：`notify_one` 在无
    /// 等待方时存储 permit，`notified()` 会立即消费该 permit 返回，因此写入
    /// 与通知之间的任意交错都不会漏醒。单等待方契约（超时分支唯一调用方）。
    pub(crate) async fn wait_for_kill_target(&self) -> Option<OwnedFd> {
        loop {
            if let Some(fd) = self.take_pidfd() {
                return Some(fd);
            }
            if self.finished.load(Ordering::Acquire) {
                return None;
            }
            self.state_changed.notified().await;
        }
    }
}

/// 执行系统命令（rootd 以 root 运行）。失败时返回错误描述。
/// 薄代理职责：校验后的参数直接转发给系统工具，不做额外业务逻辑。
///
/// 同步阻塞实现——调用方（D-Bus 服务层）负责经 `spawn_blocking` +
/// `tokio::time::timeout` 包装，本函数自身不含超时，便于单测。
///
/// 子进程句柄经 [`Command`](std::process::Command) 的 `spawn()` 取得后立即
/// 克隆为 pidfd 写入 `slot`——超时后的 kill 据此以无 PID 复用竞态的方式
/// 精确作用于本函数产生的进程（TSI-2504/TSI-2545）。spawn 前后双重检查
/// 取消位：spawn 前取消则跳过，spawn 后取消则自行 kill 并 reap。
fn run_command(cmd: impl AsRef<Path>, args: &[&str], slot: &CommandSlot) -> Result<String, String> {
    let cmd_path = cmd.as_ref();
    let cmd_name = cmd_path.display().to_string();
    // spawn 前检查：包装层已超时置取消位 → 直接跳过，不留孤儿（TSI-2545）。
    if slot.is_cancelled() {
        return Err(format!("{cmd_name} cancelled before spawn"));
    }
    let mut child = std::process::Command::new(cmd_path)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{cmd_name} execution failed: {e}"))?;
    if let Ok(pidfd) = child_pidfd(&mut child) {
        slot.store_pidfd(pidfd);
    }
    // spawn 后复查：取消若恰发生在 spawn 与复查之间，且包装层的轮询已耗尽，
    // 本线程自行 kill 并 reap，杜绝延迟 spawn 孤儿（TSI-2545）。
    if slot.is_cancelled() {
        if let Some(pidfd) = slot.take_pidfd() {
            kill_via_pidfd(&pidfd);
        } else {
            let _ = child.kill();
        }
        let _ = child.wait();
        return Err(format!("{cmd_name} cancelled after spawn"));
    }
    let output = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => {
            slot.clear_pidfd();
            return Err(format!("{cmd_name} wait failed: {e}"));
        }
    };
    slot.clear_pidfd();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd_name} failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 为已 spawn 的 [`Child`](std::process::Child) 生成 pidfd。
///
/// pidfd 引用进程本体而非 PID 数值，`pidfd_send_signal` 因此不受 PID 复用
/// 竞态影响。项目 Linux-only，`SYS_pidfd_open` 常量随目标编译；老内核
/// （< 5.3）或容器环境禁用 pidfd 时失败，调用方退化为不跟踪。
fn child_pidfd(child: &mut std::process::Child) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: 非负返回值是内核分配的有效 fd，所有权转入 OwnedFd。
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// 通过 pidfd 向目标进程发送 SIGKILL（TSI-2504 超时后终结子进程）。
///
/// pidfd 引用进程本体，不因 PID 复用错杀；`ESRCH` 表示进程已自然退出，
/// 属预期竞态，静默忽略。其余错误以 warn 记录——rootd 以 root 运行，
/// kill 失败需要可见。
pub fn kill_via_pidfd(pidfd: &OwnedFd) {
    // SAFETY: pidfd 由 `child_pidfd` 保证有效（内核分配，OwnedFd 持有生命周期）。
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(error = %err, "pidfd_send_signal(SIGKILL) failed");
        }
    }
}

/// 当前安全模型版本（§23.4.3 版本对账字段）。
pub const SECURITY_MODEL_VERSION: u32 = 1;

/// 白名单方法执行结果。
pub type RootResult = Result<Value, String>;

// ───────────────────── polkit action 映射（§23.4.2） ─────────────────────
//
// 每个白名单方法对应独立 polkit action id。`polkit_action_for` 是唯一
// 映射源——D-Bus 服务层（main）调用此函数获取 action id，再经
// `org.freedesktop.PolicyKit1` 校验。未映射的方法（无 action）不走
// polkit 但仍在白名单内（如 Hello——纯版本对账，无副作用）。

/// 返回方法对应的 polkit action id；`None` = 无需 polkit（如 Hello）。
///
/// 安全模型：每个特权方法独立 action，精确到最小权限（§23.4.2）。
pub fn polkit_action_for(method: &str) -> Option<&'static str> {
    match method {
        "PackageInstall" => Some("com.agentshell.pkexec.install-package"),
        "PackageRemove" => Some("com.agentshell.package.remove"),
        "PackageUpdate" => Some("com.agentshell.package.update"),
        "PackageRefresh" => Some("com.agentshell.package.refresh"),
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceReload" => {
            Some("com.agentshell.service.control")
        }
        "ServiceEnable" | "ServiceDisable" | "DaemonReload" => {
            Some("com.agentshell.systemd.manage")
        }
        "JournalQuery" => Some("com.agentshell.system-log.view"),
        "SysctlGet" => Some("com.agentshell.sysctl.get"),
        "SysctlSet" => Some("com.agentshell.sysctl.set"),
        "HostnameSet" => Some("com.agentshell.hostname.set"),
        "ProcessKill" => Some("com.agentshell.process.kill"),
        "Mount" | "Unmount" => Some(agent_shell_rpc::MOUNT_POLKIT_ACTION),
        "JobStatus" => Some("com.agentshell.job.status"),
        "SetToken" => None, // 内部 token 管理，无系统副作用
        "Hello" => None,    // 版本对账，无副作用
        _ => None,
    }
}

// ───────────────────── Job 跟踪（JobProgress/JobDone 信号源） ─────────────────────
//
// 软件包操作返回 job id，进度经 JobProgress 信号推送，完成发 JobDone
// （§23.4.3）。job 状态存于进程内——rootd 是系统级单例，所有 session
// 共享。D-Bus 服务层订阅 job 事件并转发为 D-Bus 信号。

static JOB_SEQ: AtomicU64 = AtomicU64::new(0);

/// Job 状态。
#[derive(Clone, Debug)]
pub struct JobState {
    pub id: String,
    pub method: String,
    pub progress: f64,
    pub done: bool,
    pub success: bool,
    /// 子进程退出码（未完成/未执行时为 None）。
    pub exit_code: Option<i32>,
    /// stderr 摘要（前 4 KiB），完成时写入。
    pub stderr: String,
}

/// 测试串行闸：`JOBS` 是进程级全局静态，lib 内 job 测试与 dbus 内
/// `drive_signals` 循环测试（后者每 200ms `job_drain_done`，会抽走前者
/// 尚未断言完成的 job）必须互斥。parking_lot 守护可跨 `.await` 持有。
///
/// 同一把闸同时覆盖可注入 seam [`JOB_RUNNER`]：写入/清除 runner 的测试
/// 辅助函数（`set_job_runner`/`clear_job_runner`）要求传入本锁的守护，
/// 从编译期强制任何注入 runner 的测试先取得闸，防止未来新增的并行 job
/// runner 测试在进程级全局 seam 上互相竞争。
#[cfg(test)]
pub(crate) static JOB_TEST_MUTEX: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// 串行化所有会真实 spawn `sleep` 子进程、并依赖 `/proc` 残留扫描的测试
/// （dbus 层 B1/C0/C1 与 lib 层 CommandSlot pidfd 测试）——并行时各自的
/// `sleep` 直接子进程会互相污染 `/proc` 扫描结果，必须独占。
#[cfg(test)]
pub(crate) static SPAWN_SLEEP_TEST_MUTEX: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// 进程内 job 注册表（D-Bus 服务层轮询/订阅以发射信号）。
static JOBS: Mutex<Option<HashMap<String, JobState>>> = Mutex::new(None);

/// 已完成 job 的最终快照缓存（含 `success`/`exit_code`/`stderr`）。
///
/// `job_drain_done` 淘汰已完成 job 后，`job_status` 查询活动注册表
/// 未命中时回落到此缓存——查询方（CLI `wait_for_job` 轮询、`job status`
/// 人工查询）在 drain 后仍能读到确定正确的最终状态，而非 `{"found":false}`
/// （否则完成快的 job 会被误报为「未找到」，见 TSI-2561 审查 #1）。
/// 缓存仅追加最近一批完成项，完成即定稿、不可变；大小受
/// [`COMPLETED_JOBS_MAX`] 上界约束。
static COMPLETED_JOBS: Mutex<Vec<JobState>> = Mutex::new(Vec::new());

/// 已完成 job 缓存的上界：超出后按完成顺序淘汰最旧项。
/// 只保留最近完成的 64 个 job——查询方在 drain 后数分钟内追查
/// 足够，且 rootd 常驻进程不因此无限增长。
const COMPLETED_JOBS_MAX: usize = 64;

fn jobs_lock<'a>() -> std::sync::MutexGuard<'a, Option<HashMap<String, JobState>>> {
    JOBS.lock().expect("JOBS mutex poisoned")
}

fn completed_jobs_lock<'a>() -> std::sync::MutexGuard<'a, Vec<JobState>> {
    COMPLETED_JOBS
        .lock()
        .expect("COMPLETED_JOBS mutex poisoned")
}
fn next_job_id() -> String {
    let n = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("job-{n}")
}

/// 注册一个新 job，返回 job id。D-Bus 服务层据此发射 JobProgress/JobDone。
pub fn job_create(method: &str) -> String {
    let id = next_job_id();
    let state = JobState {
        id: id.clone(),
        method: method.to_string(),
        progress: 0.0,
        done: false,
        success: false,
        exit_code: None,
        stderr: String::new(),
    };
    let mut jobs = jobs_lock();
    jobs.get_or_insert_with(HashMap::new)
        .insert(id.clone(), state);
    id
}

/// 更新 job 进度（0.0–1.0）。D-Bus 服务层据此发射 JobProgress 信号。
pub fn job_progress(id: &str, progress: f64) {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        if let Some(job) = map.get_mut(id) {
            job.progress = progress.clamp(0.0, 1.0);
        }
    }
}

/// 标记 job 完成。D-Bus 服务层据此发射 JobDone 信号。
///
/// 完成的 job 保留在注册表中，直到被 `job_drain_done` 取走——服务层
/// 调用 drain 读取已完成 job 后从注册表淘汰，防止无限增长
/// （rootd 是系统级单例常驻进程）。
pub fn job_done(id: &str, success: bool) {
    job_done_with(id, success, None, String::new());
}

/// 标记 job 完成并记录退出码与 stderr 摘要。
pub fn job_done_with(id: &str, success: bool, exit_code: Option<i32>, stderr: String) {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        if let Some(job) = map.get_mut(id) {
            job.done = true;
            job.success = success;
            job.progress = 1.0;
            job.exit_code = exit_code;
            job.stderr = stderr;
        }
    }
}

/// 查询单个 job 状态（Issue「job：status / progress 查询」）。
///
/// 先查活动注册表；未命中再查已完成缓存——`job_drain_done` 淘汰后
/// 的完成 job 仍可查询到确定正确的最终状态。
pub fn job_status(id: &str) -> Option<JobState> {
    let jobs = jobs_lock();
    if let Some(state) = jobs.as_ref().and_then(|m| m.get(id)).cloned() {
        return Some(state);
    }
    drop(jobs);
    completed_jobs_lock().iter().find(|j| j.id == id).cloned()
}

/// `JobStatus(job_id)` 方法分派：把 `JobState` 序列化为 JSON 出参。
///
/// 未知 id 返回 `{"found": false}` 而非错误——查询语义下「不存在」是
/// 合法结果，错误会误导调用方为调用失败。drain 淘汰后的完成 job
/// 由 [`COMPLETED_JOBS`] 缓存兜底，仍返回 `found=true`。
fn job_status_dispatch(args: &[Value]) -> RootResult {
    let job_id = str_arg(args, 0)?;
    let Some(state) = job_status(job_id) else {
        return Ok(json!({ "found": false }));
    };
    Ok(json!({
        "found": true,
        "method": state.method,
        "progress": state.progress,
        "done": state.done,
        "success": state.success,
        "exit_code": state.exit_code,
        "stderr": state.stderr,
    }))
}

/// 取走并淘汰已完成 job（D-Bus 服务层发射 JobDone 后调用），
/// 同时把最终快照移入 [`COMPLETED_JOBS`] 缓存供后续查询。
/// 防止注册表无限增长——rootd 是常驻进程，不淘汰会内存泄漏。
pub fn job_drain_done() -> Vec<JobState> {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        let drained: Vec<JobState> = map.values().filter(|j| j.done).cloned().collect();
        map.retain(|_, job| !job.done);
        drop(jobs);
        let mut completed = completed_jobs_lock();
        for state in &drained {
            completed.push(state.clone());
        }
        if completed.len() > COMPLETED_JOBS_MAX {
            let overflow = completed.len() - COMPLETED_JOBS_MAX;
            completed.drain(0..overflow);
        }
        drained
    } else {
        Vec::new()
    }
}

/// 获取所有 job 快照（D-Bus 服务层轮询 JobProgress 用）。
pub fn job_snapshot() -> Vec<JobState> {
    let jobs = jobs_lock();
    jobs.as_ref()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}

// ───────────────────── 分派核心 ─────────────────────

/// 分派一个 rootd 方法调用（方法名 = §23.4.3 D-Bus 接口方法名）。
///
/// 非白名单方法一律拒绝——rootd 的安全模型是「默认拒绝 + 显式白名单」。
///
/// **polkit 前置**：调用方（D-Bus 服务层）必须在调用本函数**之前**经
/// `polkit_action_for` 获取 action id 并完成 polkit 授权校验。本函数
/// 不重复 polkit 校验——它是薄代理的业务逻辑层，授权是传输层职责。
pub fn dispatch(method: &str, args: &[Value]) -> RootResult {
    dispatch_with_slot(method, args, &CommandSlot::new())
}

/// 分派一个 rootd 方法调用，并允许跟踪产生的子进程（TSI-2504/TSI-2545）。
///
/// 与 [`dispatch`] 唯一区别：产生子进程的五个方法（journalctl/sysctl/
/// hostnamectl/mount/umount）将子进程 pidfd 写入 `slot`，供调用方在
/// 超时后 kill。其余方法忽略 slot。分派结束（正常或异常）后置位
/// `CommandSlot::finished`，使超时分支的 kill 目标等待可据此结束，不再
/// 依赖固定宽限时长。非白名单方法一律拒绝——rootd 的安全模型是
/// 「默认拒绝 + 显式白名单」。
///
/// **polkit 前置**：调用方（D-Bus 服务层）必须在调用本函数**之前**经
/// `polkit_action_for` 获取 action id 并完成 polkit 授权校验。本函数
/// 不重复 polkit 校验——它是薄代理的业务逻辑层，授权是传输层职责。
pub(crate) fn dispatch_with_slot(method: &str, args: &[Value], slot: &CommandSlot) -> RootResult {
    let result = match method {
        // 版本对账
        "Hello" => hello(),
        // 软件包（返回 job id）
        "PackageInstall" => package_op("PackageInstall", args),
        "PackageRemove" => package_op("PackageRemove", args),
        "PackageUpdate" => package_op("PackageUpdate", args),
        "PackageRefresh" => package_refresh(),
        // systemd system 单元
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceEnable" | "ServiceDisable"
        | "ServiceReload" => service_control(method, args),
        "DaemonReload" => daemon_reload(),
        // 系统日志
        "JournalQuery" => journal_query(args, slot),
        // 系统配置
        "SysctlGet" => sysctl_get(args),
        "SysctlSet" => sysctl_set(args, slot),
        "HostnameSet" => hostname_set(args, slot),
        // 进程（跨用户）
        "ProcessKill" => process_kill(args),
        // 挂载
        "Mount" => mount(args, slot),
        "Unmount" => unmount(args, slot),
        // job 状态查询（issue「job：status / progress 查询」）
        "JobStatus" => job_status_dispatch(args),
        // 会话 Token 管理（可选，无系统副作用）
        "SetToken" => set_token(args),
        // 测试专用方法（仅 cfg(test)）：为 dbus 层超时回归测试提供真实
        // 阻塞源（TestSlowMethod）与可被 kill 的子进程（TestSpawnSleep），
        // 验证 spawn_blocking+timeout 包装隔离事件循环并终结子进程。
        #[cfg(test)]
        "TestSlowMethod" => {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(json!({"slow": true}))
        }
        #[cfg(test)]
        "TestSpawnSleep" => {
            run_command("sleep", &["30"], slot)?;
            Ok(json!({"spawned": true}))
        }
        _ => Err(format!("method not in whitelist: {method}")),
    };
    slot.mark_finished();
    result
}

// ───────────────────── 版本对账 ─────────────────────

fn hello() -> RootResult {
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "security_model": SECURITY_MODEL_VERSION,
    }))
}

// ───────────────────── 辅助：参数提取 ─────────────────────

fn str_arg(args: &[Value], idx: usize) -> Result<&str, String> {
    args.get(idx)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("arg[{idx}] must be a string"))
}

fn int_arg(args: &[Value], idx: usize) -> Result<i64, String> {
    args.get(idx)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("arg[{idx}] must be an integer"))
}

fn array_str_arg(args: &[Value], idx: usize) -> Result<Vec<String>, String> {
    let arr = args
        .get(idx)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("arg[{idx}] must be an array"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| format!("arg[{idx}] array element must be a string"))
        })
        .collect()
}

// ───────────────────── 软件包管理 ─────────────────────
//
// §23.4.3：软件包操作返回 job id，进度经 JobProgress 信号推送，完成发
// JobDone。包管理器抽象(apt/dnf/pacman/flatpak system)——探测运行时
// 哪个前端可用，调用对应命令。无业务逻辑：只转译为包管理器调用。

/// 探测系统包管理器前端。
fn detect_package_manager() -> Option<PackageManager> {
    // 按优先级探测：apt → dnf → pacman → flatpak(system)
    if std::path::Path::new("/usr/bin/apt-get").exists() {
        return Some(PackageManager::Apt);
    }
    if std::path::Path::new("/usr/bin/dnf").exists() {
        return Some(PackageManager::Dnf);
    }
    if std::path::Path::new("/usr/bin/pacman").exists() {
        return Some(PackageManager::Pacman);
    }
    if std::path::Path::new("/usr/bin/flatpak").exists() {
        return Some(PackageManager::Flatpak);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Flatpak,
}

impl PackageManager {
    fn install_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                let mut cmd = vec!["apt-get".into(), "install".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Dnf => {
                let mut cmd = vec!["dnf".into(), "install".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Pacman => {
                let mut cmd = vec!["pacman".into(), "-S".into(), "--noconfirm".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Flatpak => {
                let mut cmd = vec![
                    "flatpak".into(),
                    "install".into(),
                    "-y".into(),
                    "--system".into(),
                ];
                cmd.extend(packages.iter().cloned());
                cmd
            }
        }
    }

    fn remove_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                let mut cmd = vec!["apt-get".into(), "remove".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Dnf => {
                let mut cmd = vec!["dnf".into(), "remove".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Pacman => {
                let mut cmd = vec!["pacman".into(), "-R".into(), "--noconfirm".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Flatpak => {
                let mut cmd = vec![
                    "flatpak".into(),
                    "uninstall".into(),
                    "-y".into(),
                    "--system".into(),
                ];
                cmd.extend(packages.iter().cloned());
                cmd
            }
        }
    }

    fn update_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                if packages.is_empty() {
                    vec!["apt-get".into(), "upgrade".into(), "-y".into()]
                } else {
                    let mut cmd = vec![
                        "apt-get".into(),
                        "install".into(),
                        "-y".into(),
                        "--only-upgrade".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Dnf => {
                if packages.is_empty() {
                    vec!["dnf".into(), "upgrade".into(), "-y".into()]
                } else {
                    let mut cmd = vec!["dnf".into(), "upgrade".into(), "-y".into()];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Pacman => {
                if packages.is_empty() {
                    vec!["pacman".into(), "-Syu".into(), "--noconfirm".into()]
                } else {
                    let mut cmd = vec![
                        "pacman".into(),
                        "-S".into(),
                        "--noconfirm".into(),
                        "--needed".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Flatpak => {
                if packages.is_empty() {
                    vec![
                        "flatpak".into(),
                        "update".into(),
                        "-y".into(),
                        "--system".into(),
                    ]
                } else {
                    let mut cmd = vec![
                        "flatpak".into(),
                        "update".into(),
                        "-y".into(),
                        "--system".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
        }
    }

    fn refresh_cmd(&self) -> Vec<String> {
        match self {
            PackageManager::Apt => vec!["apt-get".into(), "update".into()],
            PackageManager::Dnf => vec!["dnf".into(), "makecache".into()],
            PackageManager::Pacman => vec!["pacman".into(), "-Sy".into()],
            PackageManager::Flatpak => {
                vec!["flatpak".into(), "update".into(), "--appstream".into()]
            }
        }
    }
}

/// 校验包名合法性（防注入：只允许字母数字 + `.` `-` `_` `+` `:` `/`）。
fn validate_package_name(pkg: &str) -> Result<(), String> {
    if pkg.is_empty() || pkg.len() > 256 {
        return Err("invalid package name length".into());
    }
    if !pkg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '/'))
    {
        return Err(format!("illegal character in package name: {pkg:?}"));
    }
    Ok(())
}
fn package_op(method: &str, args: &[Value]) -> RootResult {
    let packages = array_str_arg(args, 0)?;
    for pkg in &packages {
        validate_package_name(pkg)?;
    }
    let pm = detect_package_manager().ok_or("no package manager available")?;
    let cmd = match method {
        "PackageInstall" => pm.install_cmd(&packages),
        "PackageRemove" => pm.remove_cmd(&packages),
        "PackageUpdate" => pm.update_cmd(&packages),
        _ => return Err(format!("unknown package method: {method}")),
    };
    let job_id = job_create(method);
    tracing::info!(
        method,
        job = %job_id,
        pm = ?pm,
        cmd = ?cmd,
        "package operation queued (polkit action: {})",
        polkit_action_for(method).unwrap_or("none"),
    );
    spawn_package_job(job_id.clone(), method.to_string(), cmd);
    Ok(json!({ "job_id": job_id, "pm": format!("{pm:?}") }))
}

/// 包管理器命令执行器（可注入 seam，供单测替换真实进程 spawn）。
type JobRunner = fn(&str, &[String]) -> (bool, Option<i32>, String);

/// 测试注入的包管理器执行器；`None` = 使用默认真实执行器。
///
/// 测试中的写入必须持有 [`JOB_TEST_MUTEX`]（经 `set_job_runner`/
/// `clear_job_runner` 的守护参数强制），与 `JOBS` 注册表共享同一把测试
/// 串行闸，杜绝并行测试在进程级全局 seam 上竞争。
static JOB_RUNNER: Mutex<Option<JobRunner>> = Mutex::new(None);

/// 按 UTF-8 字节边界截断 stderr 到 `max_bytes`（与 JobState「前 4 KiB」一致）。
///
/// 不按 `.chars().take(n)` 计数——多字节字符会突破字节上界。也不在
/// 任意字节处硬切——那会产出非法 UTF-8。做法：找到不超过 `max_bytes`
/// 的最大合法字符边界（`char_indices` 累加 `len_utf8`），截断于该处。
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = 0;
    for (idx, ch) in s.char_indices() {
        if idx + ch.len_utf8() > max_bytes {
            break;
        }
        boundary = idx + ch.len_utf8();
    }
    &s[..boundary]
}

/// 默认执行器：真实 spawn 包管理器命令并汇总结果。
fn default_job_runner(program: &str, args: &[String]) -> (bool, Option<i32>, String) {
    match std::process::Command::new(program).args(args).output() {
        Ok(out) => {
            let code = out.status.code();
            let err = truncate_utf8(&String::from_utf8_lossy(&out.stderr), 4096).to_string();
            (out.status.success(), code, err)
        }
        Err(e) => (false, None, format!("spawn failed: {e}")),
    }
}

/// 经注入 seam 执行包管理器命令。
fn run_job(program: &str, args: &[String]) -> (bool, Option<i32>, String) {
    let runner = JOB_RUNNER
        .lock()
        .expect("JOB_RUNNER mutex poisoned")
        .unwrap_or(default_job_runner);
    runner(program, args)
}

/// spawn 后台包管理器进程并驱动 job 进度（JobProgress → JobDone）。
///
/// 使用独立线程 + std `Command` 同步执行——不阻塞调用方；该线程不经
/// `spawn_blocking`、不受 [`MAX_CONCURRENT_BLOCKING_COMMANDS`] 闸约束，
/// 进度经 job 注册表（`job_progress`/`job_done`）对外可见。
fn spawn_package_job(job_id: String, method: String, cmd: Vec<String>) {
    std::thread::spawn(move || {
        job_progress(&job_id, 0.1); // queued
        let (program, args) = match cmd.split_first() {
            Some((p, a)) => (p.clone(), a.to_vec()),
            None => {
                job_done(&job_id, false);
                return;
            }
        };
        job_progress(&job_id, 0.3); // running
        let (success, exit_code, stderr) = run_job(&program, &args);
        job_progress(&job_id, 0.75);
        job_done_with(&job_id, success, exit_code, stderr);
        tracing::info!(method, job = %job_id, success, "package job finished");
    });
}

fn package_refresh() -> RootResult {
    let pm = detect_package_manager().ok_or("no package manager available")?;
    let cmd = pm.refresh_cmd();
    let job_id = job_create("PackageRefresh");
    tracing::info!(
        job = %job_id,
        pm = ?pm,
        cmd = ?cmd,
        "package refresh queued (polkit action: com.agentshell.package.refresh)",
    );
    spawn_package_job(job_id.clone(), "PackageRefresh".to_string(), cmd);
    Ok(json!({ "job_id": job_id, "pm": format!("{pm:?}") }))
}

// ───────────────────── systemd system 单元控制 ─────────────────────
//
// 单元名做基本合法性校验（防 shell 注入面：实际执行走 systemd D-Bus API
// 而非 shell，但入口仍拒绝可疑字符）。

fn service_control(method: &str, args: &[Value]) -> RootResult {
    let unit = str_arg(args, 0)?;
    validate_unit_name(unit)?;
    // systemd D-Bus API 映射（§23.4.3）：
    //   ServiceStart   → org.freedesktop.systemd1.Manager.StartUnit(unit, "replace")
    //   ServiceStop    → StopUnit(unit, "replace")
    //   ServiceRestart → RestartUnit(unit, "replace")
    //   ServiceReload  → ReloadUnit(unit, "replace")
    //   ServiceEnable  → EnableUnitFiles([unit], false, true)
    //   ServiceDisable → DisableUnitFiles([unit], false)
    // StartUnit mode 用 "replace"（agent 场景默认，§开放问题 #6）
    let systemd_method = match method {
        "ServiceStart" => "StartUnit",
        "ServiceStop" => "StopUnit",
        "ServiceRestart" => "RestartUnit",
        "ServiceReload" => "ReloadUnit",
        "ServiceEnable" => "EnableUnitFiles",
        "ServiceDisable" => "DisableUnitFiles",
        _ => return Err(format!("unknown service method: {method}")),
    };
    tracing::info!(
        method,
        unit,
        systemd_method,
        "service control requested (polkit action: com.agentshell.service.control)",
    );
    // Enable/Disable 走 EnableUnitFiles/DisableUnitFiles：
    //   EnableUnitFiles(files, runtime, force) → (carries_install_info, changes)
    //   DisableUnitFiles(files, runtime)       → changes
    // 其余走 Start/Stop/Restart/ReloadUnit（参数 (unit, "replace")）。
    match method {
        "ServiceEnable" => {
            let _: (bool, Vec<(String, String, String)>) =
                systemd_call(systemd_method, (vec![unit.to_string()], false, true))?;
        }
        "ServiceDisable" => {
            let _: Vec<(String, String, String)> =
                systemd_call(systemd_method, (vec![unit.to_string()], false))?;
        }
        _ => {
            let _: SystemdJobPath = systemd_call(systemd_method, unit_call_body(unit))?;
        }
    }
    // mode 只对 Start/Stop/Restart/Reload 有效（Enable/Disable 走
    // EnableUnitFiles/DisableUnitFiles，无 mode 参数，只有 runtime/persistent 语义）
    let mut result = json!({
        "accepted": true,
        "unit": unit,
        "action": method,
        "systemd_method": systemd_method,
    });
    if matches!(
        method,
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceReload"
    ) {
        result["mode"] = json!("replace");
    }
    Ok(result)
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
    // 实际执行：org.freedesktop.systemd1.Manager.Reload()（无参）
    systemd_call::<(), ()>("Reload", ())?;
    Ok(json!({ "accepted": true, "systemd_method": "Reload" }))
}

// ───────────────────── systemd1 D-Bus 调用（§23.4.3） ─────────────────────

/// 经 org.freedesktop.systemd1.Manager D-Bus 接口执行一个方法。
///
/// 在独立线程内用 `zbus::blocking` 调用——`dispatch` 是同步函数，
/// 可能运行在 tokio runtime 内（D-Bus 服务层 `call_method`），
/// 直接 `block_on` 会触发 "Cannot start a runtime from within a runtime"。
/// 新线程无 tokio 上下文，blocking zbus 可自建 runtime。
fn systemd_call<B, R>(method: &'static str, body: B) -> Result<R, String>
where
    B: serde::ser::Serialize + zvariant::DynamicType + Send + 'static,
    R: for<'d> zvariant::DynamicDeserialize<'d> + Send + 'static,
{
    std::thread::spawn(move || -> Result<R, String> {
        let conn = zbus::blocking::Connection::system()
            .map_err(|e| format!("systemd D-Bus connect failed: {e}"))?;
        let proxy = zbus::blocking::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .map_err(|e| format!("systemd Manager proxy failed: {e}"))?;
        proxy
            .call(method, &body)
            .map_err(|e| format!("systemd {method} failed: {e}"))
    })
    .join()
    .map_err(|_| "systemd D-Bus thread panicked".to_string())?
}

/// StartUnit/StopUnit/RestartUnit/ReloadUnit 请求体。
fn unit_call_body(unit: &str) -> (String, &'static str) {
    (unit.to_string(), "replace")
}

/// systemd1 方法返回值：Start/Stop/Restart/Reload 返回 `o`（job object path）。
type SystemdJobPath = zvariant::OwnedObjectPath;

// ───────────────────── 系统日志 ─────────────────────
//
// journal 解析注意多行消息和二进制字段（_MESSAGE 可能跨多行，§开放问题 #7）。
// 过滤表达式为结构化 JSON 对象，非自由文本拼接——防注入。

/// 构造 `journalctl` 参数（不含 `--output=json` 前缀），供 `journal_query`
/// 与测试复用。参数校验（JSON 对象 / 白名单键）由 `journal_query` 前置完成。
fn build_journal_args(filter_obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut cmd_args: Vec<String> = Vec::new();
    if let Some(unit) = filter_obj.get("unit").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--unit={unit}"));
    }
    if let Some(priority) = filter_obj.get("priority").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--priority={priority}"));
    }
    if let Some(since) = filter_obj.get("since").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--since={since}"));
    }
    if let Some(until) = filter_obj.get("until").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--until={until}"));
    }
    if filter_obj
        .get("boot")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd_args.push("--boot".into());
    }
    if filter_obj
        .get("dmesg")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd_args.push("--dmesg".into());
    }
    if let Some(grep) = filter_obj.get("grep").and_then(|v| v.as_str()) {
        cmd_args.push("--grep".into());
        cmd_args.push(grep.to_string());
    }
    let mut has_limit = false;
    if let Some(limit) = filter_obj.get("limit").and_then(|v| v.as_u64()) {
        cmd_args.push(format!("--lines={}", limit.min(MAX_JOURNAL_LINES)));
        has_limit = true;
    }
    if !has_limit {
        cmd_args.push(format!("--lines={}", DEFAULT_JOURNAL_LINES));
    }
    let has_time_bound = filter_obj.contains_key("since")
        || filter_obj.contains_key("until")
        || filter_obj.contains_key("boot");
    if !has_time_bound {
        cmd_args.push(format!("--since={}", DEFAULT_JOURNAL_SINCE));
    }
    cmd_args
}

fn journal_query(args: &[Value], slot: &CommandSlot) -> RootResult {
    let filter = str_arg(args, 0)?;
    // 过滤表达式必须是合法 JSON 对象（结构化查询，非自由文本拼接）。
    let parsed: Value = serde_json::from_str(filter)
        .map_err(|e| format!("journal filter must be JSON object: {e}"))?;
    if !parsed.is_object() {
        return Err("journal filter must be a JSON object".into());
    }
    // 支持的过滤键（白名单，防注入）：
    //   unit, priority, since, until, boot, dmesg, grep, limit
    let filter_obj = parsed.as_object().unwrap();
    let allowed_keys = [
        "unit", "priority", "since", "until", "boot", "dmesg", "grep", "limit",
    ];
    for key in filter_obj.keys() {
        if !allowed_keys.contains(&key.as_str()) {
            return Err(format!("unknown journal filter key: {key}"));
        }
    }
    tracing::info!(
        filter_keys = ?filter_obj.keys().collect::<Vec<_>>(),
        "journal query requested (polkit action: com.agentshell.system-log.view)",
    );
    // 实际执行：journalctl --output=json + 过滤参数（含默认上界与 clamp）。
    let mut cmd_args: Vec<String> = vec!["--output=json".into()];
    cmd_args.extend(build_journal_args(filter_obj));
    let refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let output = run_command("journalctl", &refs, slot)?;
    Ok(json!({
        "output": output,
        "output_format": "json-stream",
    }))
}

// ───────────────────── 系统配置 ─────────────────────

fn sysctl_get(args: &[Value]) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    let value = std::fs::read_to_string(&path).map_err(|e| format!("sysctl read {key}: {e}"))?;
    Ok(json!({ "key": key, "value": value.trim() }))
}

fn sysctl_set(args: &[Value], slot: &CommandSlot) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let value = args.get(1).ok_or("arg[1] (value) required")?;
    let value_str = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    };
    // value 字符校验——拒绝空格/分号/空字节（防 sysctl 参数异常）
    if value_str.contains(' ') || value_str.contains(';') || value_str.contains('\0') {
        return Err(format!("illegal character in sysctl value: {value_str:?}"));
    }
    tracing::info!(
        key,
        %value_str,
        "sysctl set requested (polkit action: com.agentshell.sysctl.set)",
    );
    // 实际执行：sysctl -w key=value
    let arg = format!("{key}={value_str}");
    run_command("sysctl", &["-w", &arg], slot)?;
    Ok(json!({ "accepted": true, "key": key }))
}

/// sysctl key 只允许 `a-z0-9.-_/`。`/` 合法（如 `kernel/random/uuid`
/// 映射 `/proc/sys/kernel/random/uuid`），`..` 检查防路径穿越。
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

fn hostname_set(args: &[Value], slot: &CommandSlot) -> RootResult {
    let hostname = str_arg(args, 0)?;
    validate_hostname(hostname)?;
    tracing::info!(
        hostname,
        "hostname set requested (polkit action: com.agentshell.hostname.set)",
    );
    // 实际执行：hostnamectl set-hostname
    run_command("hostnamectl", &["set-hostname", hostname], slot)?;
    Ok(json!({ "accepted": true, "hostname": hostname }))
}

/// 主机名校验：RFC 1123 简化——字母数字 + `-` `.`，不以 `-` 开头/结尾。
fn validate_hostname(h: &str) -> Result<(), String> {
    if h.is_empty() || h.len() > 253 {
        return Err("invalid hostname length".into());
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
    {
        return Err(format!("illegal character in hostname: {h:?}"));
    }
    if h.starts_with('-') || h.starts_with('.') {
        return Err("hostname must not start with '-' or '.'".into());
    }
    if h.ends_with('-') || h.ends_with('.') {
        return Err("hostname must not end with '-' or '.'".into());
    }
    Ok(())
}

// ───────────────────── 进程管理（跨用户） ─────────────────────
//
// §开放问题 #10：进程管理涉及权限，kill 仅能操作同用户/同组进程（root
// 可跨用户）。rootd 以 root 运行，可 kill 任意进程。PID 校验防注入。

fn process_kill(args: &[Value]) -> RootResult {
    let pid = int_arg(args, 0)?;
    let signal = int_arg(args, 1)?;
    // PID 校验：正整数，非 0/1（init 不可 kill），≤ INT_MAX
    if pid <= 0 {
        return Err("pid must be a positive integer".into());
    }
    if pid == 1 {
        return Err("refusing to kill PID 1 (init)".into());
    }
    // 信号校验：常见信号 1–31（标准 POSIX 实时信号子集）
    if !(1..=31).contains(&signal) {
        return Err(format!("signal must be in range 1-31, got {signal}"));
    }
    // PID 上界：kill(2) 的 pid_t 是 i32；i64 窄化截断会把
    // pid > i32::MAX 回绕为负 → kill(-pid) 语义变为「杀进程组」。
    // 显式 try_from 拒绝而非截断。
    let pid_c = match i32::try_from(pid) {
        Ok(p) => p,
        Err(_) => return Err(format!("pid out of range for i32: {pid}")),
    };
    let signal_c = signal as i32;
    tracing::info!(
        pid,
        signal,
        "process kill requested (polkit action: com.agentshell.process.kill)",
    );
    // 实际执行：kill 系统调用（rootd 以 root 运行）
    let rc = unsafe { libc::kill(pid_c, signal_c) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("kill({pid}, {signal}) failed: {err}"));
    }
    Ok(json!({ "accepted": true, "pid": pid, "signal": signal }))
}

// ───────────────────── 挂载 ─────────────────────
//
// §23.4.3 Mount/Unmount。device/target/fstype/options 校验防注入。
// 实际执行走 mount(2) 系统调用或 `mount` 命令。
/// 解析 `mount`/`umount` 可执行文件绝对路径，成功后进程内缓存。
///
/// rootd 以 root 运行，`PATH` 与用户 shell 可能不同（systemd system unit
/// 默认不继承用户环境），故不硬编码绝对路径：经 `which` 按当前 `PATH`
/// 探测，命中即缓存（`OnceLock`）。缓存的是绝对路径而非内容；rootd 是
/// 系统级单例，进程生命周期内 `PATH` 与挂载布局稳定，二进制被替换后
/// 同一路径仍有效。失败不缓存——若启动时二进制暂缺（如 /usr 后挂载），
/// 后续调用可自愈；命中后每次调用不再触发 PATH 扫描，消除原有每次调用
/// 两条 ENOENT execve 尝试（TSI-2616）。
fn mount_bin(name: &str) -> Result<&'static Path, String> {
    static MOUNT_BIN: OnceLock<PathBuf> = OnceLock::new();
    static UMOUNT_BIN: OnceLock<PathBuf> = OnceLock::new();
    let slot = match name {
        "mount" => &MOUNT_BIN,
        "umount" => &UMOUNT_BIN,
        other => return Err(format!("unsupported mount binary: {other}")),
    };
    if let Some(path) = slot.get() {
        return Ok(path.as_path());
    }
    let path = which::which(name).map_err(|e| format!("{name} not found in PATH: {e}"))?;
    Ok(slot.get_or_init(|| path).as_path())
}

fn mount(args: &[Value], slot: &CommandSlot) -> RootResult {
    let device = str_arg(args, 0)?;
    let target = str_arg(args, 1)?;
    let fstype = str_arg(args, 2)?;
    let options = array_str_arg(args, 3)?;
    validate_mount_path(device, "device")?;
    validate_mount_path(target, "target")?;
    validate_fstype(fstype)?;
    for opt in &options {
        validate_mount_option(opt)?;
    }
    tracing::info!(
        device,
        target,
        fstype,
        ?options,
        "mount requested (polkit action: com.agentshell.mount)",
    );
    // 实际执行：mount -t fstype [-o options] device target
    let opts_str = options.join(",");
    let mut mount_args: Vec<&str> = vec!["-t", fstype];
    if !options.is_empty() {
        mount_args.push("-o");
        mount_args.push(&opts_str);
    }
    mount_args.push(device);
    mount_args.push(target);
    run_command(mount_bin("mount")?, &mount_args, slot)?;
    Ok(json!({
        "accepted": true,
        "device": device,
        "target": target,
        "fstype": fstype,
        "options": options,
    }))
}

fn unmount(args: &[Value], slot: &CommandSlot) -> RootResult {
    let target = str_arg(args, 0)?;
    validate_mount_path(target, "target")?;
    tracing::info!(
        target,
        "unmount requested (polkit action: com.agentshell.mount)",
    );
    // 实际执行：umount target
    run_command(mount_bin("umount")?, &[target], slot)?;
    Ok(json!({ "accepted": true, "target": target }))
}

fn validate_mount_path(path: &str, field: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{field} path must not be empty"));
    }
    if path.len() > 4096 {
        return Err(format!("{field} path too long"));
    }
    // 拒绝路径穿越与空字节（防注入）
    if path.contains('\0') {
        return Err(format!("{field} path contains null byte"));
    }
    // 必须是绝对路径或 /dev/ 块设备别名
    if !path.starts_with('/') {
        return Err(format!("{field} path must be absolute: {path:?}"));
    }
    // 拒绝 `..` 穿越
    if path.contains("/..") || path.contains("../") {
        return Err(format!("{field} path contains '..' traversal: {path:?}"));
    }
    Ok(())
}

fn validate_fstype(fstype: &str) -> Result<(), String> {
    if fstype.is_empty() {
        return Err("fstype must not be empty".into());
    }
    if fstype.len() > 64 {
        return Err("fstype too long".into());
    }
    // 只允许字母数字 + `-` `_`（防注入 mount 命令参数）
    if !fstype
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!("illegal character in fstype: {fstype:?}"));
    }
    Ok(())
}

fn validate_mount_option(opt: &str) -> Result<(), String> {
    if opt.len() > 256 {
        return Err("mount option too long".into());
    }
    // 挂载选项白名单字符（防注入）
    if !opt
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | '-' | '_' | '.'))
    {
        return Err(format!("illegal character in mount option: {opt:?}"));
    }
    Ok(())
}

// ───────────────────── 会话 Token 管理 ─────────────────────
//
// §23.4.3 SetToken：持久化 token（可选）。无系统副作用——内部状态管理，
// 不触发 polkit。token 用于 portal 授权持久化（§20.3），daemon 调用此方
// 法将 token 下发给 rootd 存储。
//
// 安全注记：token 明文常驻进程内存。若 rootd 进程内存被 dump
// （core dump / ptrace），token 泄露。rootd 以 root 运行，普通用户
// 无法 ptrace——但这是纵深防御的已知边界。后续可引入 `zeroize` crate
// 在 drop 时清零敏感内存。
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn set_token(args: &[Value]) -> RootResult {
    let token = str_arg(args, 0)?;
    if token.is_empty() {
        return Err("token must not be empty".into());
    }
    if token.len() > 4096 {
        return Err("token too long".into());
    }
    let mut guard = TOKEN.lock().expect("TOKEN mutex poisoned");
    *guard = Some(token.to_string());
    tracing::info!("token set (len={})", token.len());
    Ok(json!({ "accepted": true }))
}

/// 读取当前持久化 token（D-Bus 服务层 / 测试用）。
pub fn get_token() -> Option<String> {
    TOKEN.lock().expect("TOKEN mutex poisoned").clone()
}

// ───────────────────── 白名单自省 ─────────────────────

/// 返回白名单方法列表（D-Bus Introspect 校验用）。
pub fn whitelist_methods() -> &'static [&'static str] {
    &[
        "Hello",
        "PackageInstall",
        "PackageRemove",
        "PackageUpdate",
        "PackageRefresh",
        "ServiceStart",
        "ServiceStop",
        "ServiceRestart",
        "ServiceEnable",
        "ServiceDisable",
        "ServiceReload",
        "DaemonReload",
        "JournalQuery",
        "SysctlGet",
        "SysctlSet",
        "HostnameSet",
        "ProcessKill",
        "Mount",
        "Unmount",
        "JobStatus",
        "SetToken",
    ]
}

/// 白名单内？（Introspect 输出与白名单 XML 一致校验——无额外方法）。
pub fn is_whitelisted(method: &str) -> bool {
    whitelist_methods().contains(&method)
}

// ───────────────────── 测试 ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    // ── 版本对账 ──

    #[test]
    fn hello_reports_version_and_security_model() {
        let r = dispatch("Hello", &[]).expect("hello");
        assert_eq!(r["security_model"], SECURITY_MODEL_VERSION);
        assert!(r["version"].as_str().is_some());
    }

    #[test]
    fn non_whitelisted_method_is_rejected() {
        let e = dispatch("PackageInstallx", &[json!(["curl"])]).unwrap_err();
        assert!(e.contains("not in whitelist"), "{e}");
        // 五组核心场景方法不属于 rootd（§23.3 daemon 域）。
        assert!(dispatch("windows.list", &[]).is_err());
        assert!(dispatch("input.send", &[]).is_err());
        assert!(dispatch("screenshot.capture", &[]).is_err());
        assert!(dispatch("a11y.status", &[]).is_err());
        assert!(dispatch("doctor.run", &[]).is_err());
    }

    // ── polkit action 映射 ──

    #[test]
    fn polkit_action_for_service_control_methods() {
        assert_eq!(
            polkit_action_for("ServiceStart"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceStop"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceRestart"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceReload"),
            Some("com.agentshell.service.control")
        );
    }

    #[test]
    fn polkit_action_for_systemd_manage_methods() {
        assert_eq!(
            polkit_action_for("ServiceEnable"),
            Some("com.agentshell.systemd.manage")
        );
        assert_eq!(
            polkit_action_for("ServiceDisable"),
            Some("com.agentshell.systemd.manage")
        );
        assert_eq!(
            polkit_action_for("DaemonReload"),
            Some("com.agentshell.systemd.manage")
        );
    }

    #[test]
    fn polkit_action_for_package_methods() {
        assert_eq!(
            polkit_action_for("PackageInstall"),
            Some("com.agentshell.pkexec.install-package")
        );
        assert_eq!(
            polkit_action_for("PackageRemove"),
            Some("com.agentshell.package.remove")
        );
        assert_eq!(
            polkit_action_for("PackageUpdate"),
            Some("com.agentshell.package.update")
        );
        assert_eq!(
            polkit_action_for("PackageRefresh"),
            Some("com.agentshell.package.refresh")
        );
    }

    #[test]
    fn polkit_action_for_misc_methods() {
        assert_eq!(
            polkit_action_for("JournalQuery"),
            Some("com.agentshell.system-log.view")
        );
        assert_eq!(
            polkit_action_for("SysctlGet"),
            Some("com.agentshell.sysctl.get")
        );
        assert_eq!(
            polkit_action_for("SysctlSet"),
            Some("com.agentshell.sysctl.set")
        );
        assert_eq!(
            polkit_action_for("HostnameSet"),
            Some("com.agentshell.hostname.set")
        );
        assert_eq!(
            polkit_action_for("ProcessKill"),
            Some("com.agentshell.process.kill")
        );
        assert_eq!(
            polkit_action_for("Mount"),
            Some(agent_shell_rpc::MOUNT_POLKIT_ACTION)
        );
        assert_eq!(
            polkit_action_for("Unmount"),
            Some(agent_shell_rpc::MOUNT_POLKIT_ACTION)
        );
        assert_eq!(
            polkit_action_for("JobStatus"),
            Some("com.agentshell.job.status")
        );
    }

    #[test]
    fn polkit_action_none_for_hello_and_token() {
        assert_eq!(polkit_action_for("Hello"), None);
        assert_eq!(polkit_action_for("SetToken"), None);
    }

    // ── 软件包管理 ──

    #[test]
    fn package_job_tracks_success() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        set_job_runner(&_guard, fake_success_runner);
        let id = job_create("PackageInstall");
        spawn_package_job(
            id.clone(),
            "PackageInstall".to_string(),
            vec![
                "apt-get".to_string(),
                "install".to_string(),
                "-y".to_string(),
            ],
        );
        let job = wait_job_done(&id);
        assert!(job.success, "{job:?}");
        assert_eq!(job.exit_code, Some(0));
        clear_job_runner(&_guard);
        let _ = job_drain_done();
    }

    #[test]
    fn package_job_tracks_failure() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        set_job_runner(&_guard, fake_failure_runner);
        let id = job_create("PackageRemove");
        spawn_package_job(
            id.clone(),
            "PackageRemove".to_string(),
            vec![
                "apt-get".to_string(),
                "remove".to_string(),
                "-y".to_string(),
            ],
        );
        let job = wait_job_done(&id);
        assert!(!job.success, "{job:?}");
        assert_eq!(job.exit_code, Some(1));
        assert_eq!(job.stderr, "permission denied");
        clear_job_runner(&_guard);
        let _ = job_drain_done();
    }

    #[test]
    fn job_runner_seam_defaults_to_real_executor() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 未注入 runner 时走默认真实 spawn：`true` 成功、`false` 非零退出。
        let (ok, code, stderr) = run_job("true", &[]);
        assert!(ok);
        assert_eq!(code, Some(0));
        assert_eq!(stderr, "");

        let (ok, code, _) = run_job("false", &[]);
        assert!(!ok);
        assert_eq!(code, Some(1));
    }

    #[test]
    fn job_runner_seam_injects_and_clears() {
        let _guard = JOB_TEST_MUTEX.lock();
        set_job_runner(&_guard, fake_failure_runner);
        let (ok, code, stderr) = run_job("apt-get", &["install".to_string()]);
        assert!(!ok);
        assert_eq!(code, Some(1));
        assert_eq!(stderr, "permission denied");

        clear_job_runner(&_guard);
        // 清除后恢复默认真实执行器。
        let (ok, code, _) = run_job("true", &[]);
        assert!(ok);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn package_install_rejects_invalid_name() {
        assert!(dispatch("PackageInstall", &[json!(["evil; rm -rf /"])]).is_err());
        assert!(dispatch("PackageInstall", &[json!([""])]).is_err());
        assert!(dispatch("PackageInstall", &[json!(["$(whoami)"])]).is_err());
    }

    #[test]
    fn package_install_rejects_non_array() {
        assert!(dispatch("PackageInstall", &[json!("curl")]).is_err());
    }

    /// 轮询等待 job 完成（后台线程异步推进），返回最终状态。超时视为失败。
    fn wait_job_done(job_id: &str) -> JobState {
        for _ in 0..100 {
            if let Some(job) = job_snapshot()
                .into_iter()
                .find(|j| j.id == job_id && j.done)
            {
                return job;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("job {job_id} did not finish within 5s");
    }

    fn fake_success_runner(_p: &str, _a: &[String]) -> (bool, Option<i32>, String) {
        (true, Some(0), String::new())
    }

    fn fake_failure_runner(_p: &str, _a: &[String]) -> (bool, Option<i32>, String) {
        (false, Some(1), "permission denied".to_string())
    }

    /// 注入测试 runner。要求传入 [`JOB_TEST_MUTEX`] 的守护——从类型上保证
    /// 任何触碰进程级全局 seam 的测试先持有测试串行闸（与 `JOBS` 注册表
    /// 同闸），防止并行 job runner 测试互相竞争。
    fn set_job_runner(_guard: &parking_lot::MutexGuard<'_, ()>, r: JobRunner) {
        *JOB_RUNNER.lock().expect("JOB_RUNNER mutex poisoned") = Some(r);
    }

    /// 清除注入的 runner 并恢复默认真实执行器。要求持有 [`JOB_TEST_MUTEX`]。
    fn clear_job_runner(_guard: &parking_lot::MutexGuard<'_, ()>) {
        *JOB_RUNNER.lock().expect("JOB_RUNNER mutex poisoned") = None;
    }

    #[test]
    fn job_create_and_progress() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id = job_create("PackageInstall");
        assert!(id.starts_with("job-"));
        job_progress(&id, 0.5);
        let snap = job_snapshot();
        let job = snap.iter().find(|j| j.id == id).expect("job found");
        assert_eq!(job.method, "PackageInstall");
        assert!((job.progress - 0.5).abs() < f64::EPSILON);
        assert!(!job.done);
    }

    #[test]
    fn job_done_records_exit_code_and_stderr() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        let id = job_create("PackageInstall");
        job_done_with(&id, false, Some(1), "permission denied".to_string());
        let job = job_status(&id).expect("job found");
        assert!(!job.success);
        assert_eq!(job.exit_code, Some(1));
        assert_eq!(job.stderr, "permission denied");
        assert!((job.progress - 1.0).abs() < f64::EPSILON);
        let _ = job_drain_done();
    }
    #[test]
    fn job_status_dispatch_known_job() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        let id = job_create("PackageInstall");
        job_done_with(&id, false, Some(2), "boom".to_string());
        let v = dispatch("JobStatus", &[json!(id.clone())]).expect("job status");
        assert_eq!(v["found"], json!(true));
        assert_eq!(v["method"], json!("PackageInstall"));
        assert_eq!(v["progress"], json!(1.0));
        assert_eq!(v["done"], json!(true));
        assert_eq!(v["success"], json!(false));
        assert_eq!(v["exit_code"], json!(2));
        assert_eq!(v["stderr"], json!("boom"));
        let _ = job_drain_done();
    }

    #[test]
    fn job_status_dispatch_unknown_and_missing_arg() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        let v = dispatch("JobStatus", &[json!("job-nonexistent")]).expect("job status");
        assert_eq!(v["found"], json!(false));
        assert!(dispatch("JobStatus", &[]).is_err());
        let _ = job_drain_done();
    }

    #[test]
    fn truncate_utf8_respects_byte_boundary() {
        // "你" 是 3 字节；4096 不是 3 的倍数 → 最后截断在 4095 字节边界
        let s = "你".repeat(2000);
        assert!(s.len() > 4096);
        let t = truncate_utf8(&s, 4096);
        assert!(t.len() <= 4096);
        assert!(t.is_char_boundary(t.len()));
        assert_eq!(t, &s[..t.len()]);
        // 输入 ≤ 上限：原样返回（不重分配）
        let short = "abc";
        assert_eq!(truncate_utf8(short, 4096), short);
        // 精确 4096 字节边界：整体保留
        let exact = "a".repeat(4096);
        assert_eq!(truncate_utf8(&exact, 4096).len(), 4096);
    }

    #[test]
    fn job_status_after_drain_reads_completed_cache() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 清空活动注册表与完成缓存，隔离历史状态。
        let _ = job_drain_done();
        completed_jobs_lock().clear();
        let id = job_create("PackageInstall");
        job_done_with(&id, true, Some(0), String::new());
        // drain 后活动注册表不再含该 job，但完成缓存保留最终快照。
        let _ = job_drain_done();
        let state = job_status(&id).expect("job readable from completed cache");
        assert!(state.done);
        assert!(state.success);
        assert_eq!(state.exit_code, Some(0));
        // 分派层同样命中缓存：found=true 而非 found=false。
        let v = dispatch("JobStatus", &[json!(id.clone())]).expect("job status");
        assert_eq!(v["found"], json!(true));
        assert_eq!(v["success"], json!(true));
    }

    #[test]
    fn job_drain_done_cache_evicts_oldest_beyond_bound() {
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = job_drain_done();
        completed_jobs_lock().clear();
        // 创建并完成 COMPLETED_JOBS_MAX + 1 个 job，逐个 drain 注入缓存。
        let ids: Vec<String> = (0..=COMPLETED_JOBS_MAX)
            .map(|_| {
                let id = job_create("PackageInstall");
                job_done(&id, true);
                let _ = job_drain_done();
                id
            })
            .collect();
        // 最旧的已淘汰，最新仍可查。
        assert!(job_status(&ids[0]).is_none(), "oldest must be evicted");
        let newest = job_status(ids.last().expect("has newest")).expect("newest cached");
        assert!(newest.success);
        // 缓存大小不超过上界。
        assert!(completed_jobs_lock().len() <= COMPLETED_JOBS_MAX);
    }

    #[test]
    fn job_done_marks_complete() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id = job_create("PackageUpdate");
        job_done(&id, true);
        let snap = job_snapshot();
        let job = snap.iter().find(|j| j.id == id).expect("job found");
        assert!(job.done);
        assert!(job.success);
        assert!((job.progress - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn job_drain_done_removes_completed_jobs() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 先清空全局注册表（测试间共享 static 状态）
        let _ = job_drain_done();
        let id1 = job_create("PackageInstall");
        let id2 = job_create("PackageUpdate");
        job_done(&id1, true);
        job_done(&id2, false);
        // drain 取走已完成 job
        let drained = job_drain_done();
        assert_eq!(drained.len(), 2);
        // drain 后注册表不再含已完成 job
        let snap = job_snapshot();
        assert!(snap.iter().all(|j| !j.done));
        assert!(snap.iter().find(|j| j.id == id1).is_none());
        assert!(snap.iter().find(|j| j.id == id2).is_none());
    }

    #[test]
    fn job_drain_done_preserves_incomplete_jobs() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 先清空全局注册表（测试间共享 static 状态）
        let _ = job_drain_done();
        let id_active = job_create("PackageInstall");
        let id_done = job_create("PackageRemove");
        job_done(&id_done, true);
        let drained = job_drain_done();
        // 只 drain 已完成的 id_done
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, id_done);
        // 未完成 job 保留
        let snap = job_snapshot();
        assert!(snap.iter().find(|j| j.id == id_active).is_some());
    }

    #[test]
    fn job_ids_are_unique() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id1 = job_create("PackageInstall");
        let id2 = job_create("PackageInstall");
        assert_ne!(id1, id2);
    }

    // ── systemd 单元控制 ──

    #[test]
    fn service_control_validates_unit_name() {
        // 校验失败仍返回 Err（不触达 systemctl）
        assert!(dispatch("ServiceStop", &[json!("evil; rm -rf /")]).is_err());
        assert!(dispatch("ServiceRestart", &[json!("../escape")]).is_err());
        assert!(dispatch("ServiceEnable", &[json!("")]).is_err());
        // 有效单元名通过校验——CI 无 systemd system bus，D-Bus 连接失败，预期 Err
        assert!(dispatch("ServiceStart", &[json!("nginx.service")]).is_err());
    }

    #[test]
    fn service_start_maps_to_systemd_method() {
        // CI 无 systemd system bus → D-Bus 连接失败；但 Err 消息含 systemd
        let e = dispatch("ServiceStart", &[json!("nginx.service")]).unwrap_err();
        assert!(e.contains("systemd"), "{e}");
    }

    #[test]
    fn service_enable_maps_to_enable_unit_files() {
        let e = dispatch("ServiceEnable", &[json!("nginx.service")]).unwrap_err();
        assert!(e.contains("systemd"), "{e}");
    }

    #[test]
    fn daemon_reload_maps_to_reload() {
        // CI 无 systemd system bus → D-Bus 连接失败，预期 Err
        let e = dispatch("DaemonReload", &[]).unwrap_err();
        assert!(e.contains("systemd"), "{e}");
    }

    // ── 系统日志 ──

    #[test]
    fn journal_query_requires_json_object() {
        // 非法 filter 仍 Err（不触达 journalctl）
        assert!(dispatch("JournalQuery", &[json!("free text")]).is_err());
        // 有效 filter 通过校验——journalctl 在 CI 可能失败
        let r = dispatch("JournalQuery", &[json!(r#"{"unit":"nginx"}"#)]);
        match &r {
            Ok(v) => assert!(v.get("output").is_some(), "Ok should have output field"),
            Err(e) => assert!(e.contains("journalctl"), "{e}"),
        }
    }

    #[test]
    fn journal_query_rejects_unknown_keys() {
        let bad = json!(r#"{"evil_key":"x"}"#);
        assert!(dispatch("JournalQuery", &[bad]).is_err());
    }

    #[test]
    fn journal_query_accepts_allowed_keys() {
        let filter = json!(r#"{"unit":"nginx","priority":"err","limit":100}"#);
        // 有效 filter 通过校验——journalctl 在 CI 可能失败
        let r = dispatch("JournalQuery", &[filter]);
        match &r {
            Ok(v) => assert!(v.get("output").is_some(), "Ok should have output field"),
            Err(e) => assert!(e.contains("journalctl"), "{e}"),
        }
    }

    fn parse_filter(s: &str) -> serde_json::Map<String, Value> {
        serde_json::from_str::<Value>(s)
            .expect("valid filter JSON")
            .as_object()
            .expect("filter is an object")
            .clone()
    }

    #[test]
    fn journal_args_defaults_line_and_time_bounds() {
        let args = build_journal_args(&parse_filter(r#"{}"#));
        assert!(args
            .iter()
            .any(|a| a == &format!("--lines={}", DEFAULT_JOURNAL_LINES)));
        assert!(args
            .iter()
            .any(|a| a == &format!("--since={}", DEFAULT_JOURNAL_SINCE)));
    }

    #[test]
    fn journal_args_explicit_limit_omits_default_and_clamps() {
        // 显式 limit：不带默认 --lines，且钳到 MAX_JOURNAL_LINES
        let args = build_journal_args(&parse_filter(r#"{"limit":100}"#));
        assert!(args.iter().any(|a| a == "--lines=100"));
        assert!(!args
            .iter()
            .any(|a| a == &format!("--lines={}", DEFAULT_JOURNAL_LINES)));
        assert!(args
            .iter()
            .any(|a| a == &format!("--since={}", DEFAULT_JOURNAL_SINCE)));

        let args = build_journal_args(&parse_filter(r#"{"limit":999999999}"#));
        assert!(args
            .iter()
            .any(|a| a == &format!("--lines={}", MAX_JOURNAL_LINES)));
    }

    #[test]
    fn journal_args_explicit_time_bound_omits_default_since() {
        let args = build_journal_args(&parse_filter(r#"{"since":"-48h"}"#));
        assert!(args.iter().any(|a| a == "--since=-48h"));
        assert!(!args
            .iter()
            .any(|a| a == &format!("--since={}", DEFAULT_JOURNAL_SINCE)));
        // 无显式 limit 时仍带默认行数上界
        assert!(args
            .iter()
            .any(|a| a == &format!("--lines={}", DEFAULT_JOURNAL_LINES)));

        let args = build_journal_args(&parse_filter(r#"{"boot":true}"#));
        assert!(!args
            .iter()
            .any(|a| a == &format!("--since={}", DEFAULT_JOURNAL_SINCE)));
    }

    #[test]
    fn sysctl_key_validation_blocks_traversal() {
        assert!(dispatch("SysctlGet", &[json!("net.ipv4.ip_forward")]).is_ok());
        assert!(dispatch("SysctlSet", &[json!("../../etc/passwd"), json!(1)]).is_err());
        assert!(dispatch("SysctlSet", &[json!("/abs/path"), json!(1)]).is_err());
    }

    #[test]
    fn hostname_validation() {
        // 校验失败仍返回 Err（不触达 hostnamectl）
        assert!(dispatch("HostnameSet", &[json!("")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("-bad")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("bad-")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("host; rm -rf /")]).is_err());
        // 有效 hostname 通过校验——hostnamectl 在 CI 无权限/无 polkit → Err
        assert!(dispatch("HostnameSet", &[json!("myhost")]).is_err());
    }

    // ── 主机名快照/恢复（TSI-2630：共享容器 /etc/hostname 漂移） ──

    /// 读取 hostnamed 报告的静态/瞬时主机名；hostnamed 不可用时回落到
    /// `/etc/hostname` 与内核 hostname。共享容器内二者都是跨 run 可变的
    /// 全局状态——夹具必须原值进出、失败也回滚。
    fn current_hostnames() -> (String, String) {
        fn hostnamectl_out(args: &[&str]) -> Option<String> {
            std::process::Command::new("hostnamectl")
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        }
        let static_ = hostnamectl_out(&["--static"]).unwrap_or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        });
        let transient = hostnamectl_out(&["--transient"]).unwrap_or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        });
        (static_, transient)
    }

    /// 快照/恢复守卫：构造时读取当前主机名，`Drop` 写回。断言失败也会
    /// 触发 `Drop`——漂移绝不泄漏到共享容器的后续 run。
    struct HostnameGuard {
        original: (String, String),
    }

    impl HostnameGuard {
        fn snapshot() -> Self {
            Self {
                original: current_hostnames(),
            }
        }

        /// 独立于被测代码直接调用 hostnamectl 写回：即便 `HostnameSet`
        /// 路径本身有缺陷，恢复也不依赖它。任一写回失败都返回错误并记录，
        /// 绝不让 `Drop` 兜底契约静默假绿。
        fn restore(&self) -> Result<(), String> {
            let (static_, transient) = &self.original;
            let mut failures = Vec::new();
            for (flag, value) in [("--static", static_), ("--transient", transient)] {
                if let Err(e) = run_command(
                    "hostnamectl",
                    &["set-hostname", flag, value.as_str()],
                    &Mutex::new(None),
                ) {
                    eprintln!("HostnameGuard restore {flag} 失败: {e}");
                    failures.push(e);
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(failures.join("; "))
            }
        }
    }

    impl Drop for HostnameGuard {
        fn drop(&mut self) {
            if let Err(e) = self.restore() {
                eprintln!("HostnameGuard Drop 兜底恢复失败: {e}");
            }
        }
    }

    /// 实况夹具入口见 `scripts/hostname-fixture.sh`：root 下跑 ignored 测试，
    /// 前后抓取主机名断言无漂移。托管 CI 无 systemd-hostnamed，不能跑。
    #[test]
    #[ignore = "requires root/hostnamed; live snapshot/restore fixture (TSI-2630)"]
    fn hostname_set_snapshot_and_restore_live() {
        // 非 root 无法写回主机名：跳过而非假失败。真实验证需以 root 运行
        // （rootd 部署即 root），CI 默认忽略本测试。
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("SKIP: 非 root 无法写回主机名（环境性跳过）");
            return;
        }
        let guard = HostnameGuard::snapshot();
        let (orig_static, orig_transient) = guard.original.clone();
        let target = "agent-shell-tsi2630";
        assert_ne!(orig_static, target, "原主机名与夹具目标值冲突");

        // 走真实分派路径（hostname_set → hostnamectl set-hostname）。
        let r = dispatch("HostnameSet", &[json!(target)]);
        assert!(r.is_ok(), "HostnameSet 失败: {r:?}");
        assert_eq!(
            current_hostnames().0,
            target,
            "set-hostname 后静态主机名未变更"
        );

        // 显式恢复并断言；即使此处断言失败，Drop 兜底也已写回原值。
        guard.restore().expect("HostnameGuard restore 返回错误");
        assert_eq!(
            current_hostnames(),
            (orig_static, orig_transient),
            "主机名未恢复原始值"
        );
    }

    // ── 进程管理 ──

    #[test]
    fn process_kill_rejects_pid_zero_and_one() {
        assert!(dispatch("ProcessKill", &[json!(0), json!(9)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(1), json!(9)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(-5), json!(9)]).is_err());
    }

    #[test]
    fn process_kill_validates_signal_range() {
        // 信号校验失败仍返回 Err（不触达 kill）
        assert!(dispatch("ProcessKill", &[json!(1234), json!(0)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(1234), json!(32)]).is_err());
        // 有效输入通过校验——kill(1234,9) 在 CI 无 PID 1234 → Err
        assert!(dispatch("ProcessKill", &[json!(1234), json!(9)]).is_err());
    }
    #[test]
    fn process_kill_rejects_pid_out_of_i32_range() {
        // pid > i32::MAX：try_from 拒绝，不窄化截断为负（kill 进程组语义）
        let pid = i32::MAX as i64 + 1;
        assert!(dispatch("ProcessKill", &[json!(pid), json!(9)]).is_err());
    }

    // ── 挂载 ──

    #[test]
    fn mount_validates_paths() {
        let opts = json!(["ro", "noexec"]);
        // 校验失败仍返回 Err（不触达 mount）
        assert!(dispatch(
            "Mount",
            &[json!("relpath"), json!("/mnt"), json!("ext4"), opts.clone()]
        )
        .is_err());
        assert!(dispatch(
            "Mount",
            &[
                json!("/dev/../etc"),
                json!("/mnt"),
                json!("ext4"),
                opts.clone()
            ]
        )
        .is_err());
        // 有效输入通过校验——mount 在 CI 无设备 → Err
        assert!(dispatch(
            "Mount",
            &[
                json!("/dev/sda1"),
                json!("/mnt/data"),
                json!("ext4"),
                opts.clone()
            ]
        )
        .is_err());
    }

    #[test]
    fn mount_validates_fstype() {
        let opts = json!(["ro"]);
        // fstype 校验失败仍返回 Err
        assert!(dispatch(
            "Mount",
            &[json!("/dev/sda1"), json!("/mnt"), json!("ext4; rm /"), opts]
        )
        .is_err());
        // 有效输入通过校验——mount 在 CI 无设备 → Err
        let opts2 = json!(["ro"]);
        assert!(dispatch(
            "Mount",
            &[json!("/dev/sda1"), json!("/mnt"), json!("ext4"), opts2]
        )
        .is_err());
    }

    #[test]
    fn unmount_validates_target() {
        // 校验失败仍返回 Err
        assert!(dispatch("Unmount", &[json!("relpath")]).is_err());
        // 有效输入通过校验——umount 在 CI 无挂载 → Err
        assert!(dispatch("Unmount", &[json!("/mnt/data")]).is_err());
    }

    #[test]
    fn mount_bin_resolves_and_caches_absolute_paths() {
        // 标准 Linux 环境必有 mount/umount；缺失属环境问题，直接失败（而非
        // 静默放行），避免无 binary 环境假绿。
        for name in ["mount", "umount"] {
            let first = mount_bin(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                first.is_absolute(),
                "{name} path must be absolute: {first:?}"
            );
            let second = mount_bin(name).expect("cached binary path");
            // OnceLock 缓存：两次调用必须返回同一静态路径引用，证明只解析一次。
            assert!(
                std::ptr::eq(first, second),
                "mount_bin must cache its resolution result"
            );
        }
        assert!(mount_bin("unsupported").is_err());
    }

    // ── Token 管理 ──

    #[test]
    fn set_token_stores_and_retrieves() {
        dispatch("SetToken", &[json!("secret-token-123")]).expect("set token");
        assert_eq!(get_token().as_deref(), Some("secret-token-123"));
    }

    #[test]
    fn set_token_rejects_empty() {
        assert!(dispatch("SetToken", &[json!("")]).is_err());
    }

    // ── CommandSlot 取消/等待语义（TSI-2545） ──

    #[test]
    fn run_command_skips_spawn_when_cancelled() {
        // spawn 前取消位已置位 → run_command 必须直接返回错误，不产生子进程。
        // 这是关闭「极端调度延迟下延迟 spawn 孤儿」窗口的关键路径。
        let slot = CommandSlot::new();
        slot.cancel();
        let err = run_command("sleep", &["5"], &slot).unwrap_err();
        assert!(err.contains("cancelled before spawn"), "{err}");
    }

    #[tokio::test]
    async fn command_slot_wait_returns_none_after_finished() {
        // 分派在 spawn 前已结束（无 pidfd）→ 等待方必须立即返回 None 而非
        // 无限阻塞。超时分支对不产生子进程或 spawn 前取消的方法依赖此路径。
        let slot = CommandSlot::new();
        slot.mark_finished();
        assert!(slot.wait_for_kill_target().await.is_none());
    }

    #[tokio::test]
    async fn command_slot_wait_returns_none_after_cancel() {
        // C2 回归：分派闭包尚未开始执行（blocking 池饱和，闭包仍在队列）时，
        // 超时分支置 cancel 后必须立即拿到 None 而非无限等待——`cancel` 置
        // finished 并 notify_one，与 `mark_finished` 同路径封住等待上界。
        let slot = CommandSlot::new();
        slot.cancel();
        assert!(slot.wait_for_kill_target().await.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn command_slot_wait_returns_stored_pidfd() {
        let _guard = SPAWN_SLEEP_TEST_MUTEX.lock();
        // pidfd 写入后等待方应立即拿到并取走（take），供超时分支精确 kill。
        let slot = CommandSlot::new();
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .expect("spawn sleep for pidfd");
        let pidfd = child_pidfd(&mut child).expect("pidfd");
        slot.store_pidfd(pidfd);
        assert!(
            slot.wait_for_kill_target().await.is_some(),
            "pidfd must be retrievable after store"
        );
        let _ = child.wait();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn command_slot_wait_wakes_on_late_store() {
        let _guard = SPAWN_SLEEP_TEST_MUTEX.lock();
        // 漏通知死等。这是 `wait_for_kill_target` 的核心契约（TSI-2545）。
        let slot = Arc::new(CommandSlot::new());
        let writer = Arc::clone(&slot);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut child = std::process::Command::new("sleep")
                .arg("1")
                .spawn()
                .expect("spawn sleep for pidfd");
            let pidfd = child_pidfd(&mut child).expect("pidfd");
            writer.store_pidfd(pidfd);
            let _ = child.wait();
        });
        let fd = slot
            .wait_for_kill_target()
            .await
            .expect("late pidfd store must wake the waiter");
        drop(fd);
        worker.join().expect("writer thread");
    }

    #[test]
    fn whitelist_matches_design_doc() {
        // §23.4.3 全部方法（不含信号 JobProgress/JobDone——信号非方法）
        let expected = [
            "Hello",
            "PackageInstall",
            "PackageRemove",
            "PackageUpdate",
            "PackageRefresh",
            "ServiceStart",
            "ServiceStop",
            "ServiceRestart",
            "ServiceEnable",
            "ServiceDisable",
            "ServiceReload",
            "DaemonReload",
            "JournalQuery",
            "SysctlGet",
            "SysctlSet",
            "HostnameSet",
            "ProcessKill",
            "Mount",
            "Unmount",
            "JobStatus",
            "SetToken",
        ];
        for method in expected {
            assert!(is_whitelisted(method), "{method} should be whitelisted");
        }
        // 信号名不在白名单方法列表中
        assert!(!is_whitelisted("JobProgress"));
        assert!(!is_whitelisted("JobDone"));
    }

    #[test]
    fn whitelist_has_no_extra_methods() {
        // 额外方法不在白名单中（Introspect 输出与白名单 XML 一致）
        assert!(!is_whitelisted("EvilMethod"));
        assert!(!is_whitelisted("Reboot"));
        assert!(!is_whitelisted("Shutdown"));
    }
}
