//! 审计日志（设计文档 §21.21.3 / §22.7 D6）。
//!
//! 每次权限判定写一行 JSON（JSONL），落 XDG 运行时状态目录。每个进程写自己
//! 独占的分段文件 `<审计路径>.p<pid>-<随机量>`（`O_EXCL` 创建，同名换名重试），
//! 一条记录一次追加；查询枚举分段文件与既有主日志、逐行解析合并，全程只读。
//!
//! 记录不在文件之间迁移、不删除、不按内容判重：并发、崩溃与退出都没有交接
//! 窗口。IO 失败只降级为 tracing 告警——审计是旁路记录，不是权限判定的前置
//! 依赖。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 分段文件创建的重试上限：pid 与随机量同时撞名时换名重试。
const SEGMENT_ATTEMPTS: usize = 16;

/// 退出等待在途追加的轮询间隔。
const FLUSH_POLL: Duration = Duration::from_millis(1);

/// 本进程内正在进行的审计追加数（只用于退出时的有界等待）。
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 单条审计记录（与设计 §21.21.3 `AgentAction` 对应，追加 `decision` 字段
/// 以便 `security.audit` 按判定结果过滤）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 记录生成时刻（UTC 秒级时间戳）。
    pub timestamp: u64,
    /// 发起操作的 agent（app_id 或 executable；未识别时为 `"*"`）。
    pub agent_id: String,
    /// 操作名（如 `windows.list`、`input.send`）。
    pub op: String,
    /// 门禁判定结果（`allow` / `confirm` / `deny`）。
    pub decision: String,
    /// 操作是否已执行（`true` = 执行成功，`false` = 未执行或执行失败）。
    /// 门禁判定记录恒为 `false`；`allow` 的执行结果由 dispatch 层在
    /// handler 返回后追加一条独立记录回写。
    pub result: bool,
}

/// 追加式 JSONL 审计日志。
#[derive(Clone, Debug)]
pub struct AuditLogger {
    path: PathBuf,
}

/// 本进程为某个审计锚点独占的分段文件；只由本进程写，因此无需跨进程锁。
#[derive(Debug)]
struct ProcessFile {
    path: PathBuf,
    file: std::fs::File,
    dev: u64,
    ino: u64,
    /// 回滚与补换行都失败：末尾残片无法隔离，下一条记录必须另起分段。
    broken: bool,
}

type SharedFile = Arc<Mutex<Option<ProcessFile>>>;

impl AuditLogger {
    /// 以默认路径构造（`state_dir()/audit.jsonl`）。
    pub fn default_path() -> PathBuf {
        state_dir().join("audit.jsonl")
    }

    /// 在指定路径构造；调用方负责保证父目录存在（见 [`AuditLogger::new`]）。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 创建日志器并确保父目录存在。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let _ = std::fs::create_dir_all(audit_dir(&path));
        Self { path }
    }

    /// 追加一条审计记录。
    ///
    /// 分段文件只由本进程写，追加不受其他进程的锁影响；同一进程内的线程由
    /// 分段互斥量串行化。写失败只告警并弃用该分段，下一条记录另起一段，绝不
    /// 拼在残片后面。
    pub fn log(&self, agent_id: &str, op: &str, decision: &str, result: bool) {
        let entry = AuditEntry {
            timestamp: now_secs(),
            agent_id: agent_id.to_string(),
            op: op.to_string(),
            decision: decision.to_string(),
            result,
        };
        let line = match serde_json::to_string(&entry) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!("audit serialize failed: {e}");
                return;
            }
        };
        // 正文与换行一次追加：分两次写会让两条并发记录拼成一行。
        let mut record = line;
        record.push('\n');
        self.append(record.as_bytes());
    }

    fn append(&self, record: &[u8]) {
        let _in_flight = InFlight::enter();
        let shared = shared_file(&self.path);
        let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|open| !open.still_at_path()) {
            // 状态目录被外部清理或替换：不复用已脱离路径的 inode，换新分段。
            *slot = None;
        }
        if slot.is_none() {
            match ProcessFile::create(&self.path) {
                Ok(open) => *slot = Some(open),
                Err(e) => {
                    tracing::warn!("audit file create failed ({}): {e}", self.path.display());
                    return;
                }
            }
        }
        let open = slot.as_mut().expect("分段文件已在上方创建");
        if let Err(e) = open.append(record) {
            tracing::warn!("audit write failed ({}): {e}", open.path.display());
            if open.broken {
                // 残片留在末尾且隔离不了：弃用该分段，下一条记录另起一段。
                *slot = None;
            }
        }
    }

    /// 读回全部记录；既有的主日志（旧版本写入）与各进程分段都参与读取。
    ///
    /// 查询是只读的（C-107：「查询本身不写审计、无副作用」）：不创建文件、
    /// 不补写、不迁移、不删除。读取按字节切行再逐行解析：单行损坏或末行
    /// 写到一半只丢该行，不会像 `read_to_string` 那样整文件失败而丢掉全部
    /// 历史记录。
    pub fn read_all(&self) -> Vec<AuditEntry> {
        let mut entries = Vec::new();
        for file in audit_files(&self.path) {
            let Ok(blob) = std::fs::read(&file) else {
                continue;
            };
            entries.extend(parse_complete_lines(&blob));
        }
        entries
    }

    /// 按 `agent_id` / `op` / `decision` / `result` 过滤（空串 / `None` =
    /// 不限制该维度）。`result` 区分执行态——同一 `allow` 操作会先落一条
    /// 门禁判定记录（`result=false`），执行后再追加一条结果记录，
    /// 只关心「已执行成功」须同时给 `decision=allow` + `result=Some(true)`。
    pub fn query(
        &self,
        agent_id: &str,
        op: &str,
        decision: &str,
        result: Option<bool>,
    ) -> Vec<AuditEntry> {
        self.read_all()
            .into_iter()
            .filter(|e| agent_id.is_empty() || e.agent_id == agent_id)
            .filter(|e| op.is_empty() || e.op == op)
            .filter(|e| decision.is_empty() || e.decision == decision)
            .filter(|e| match result {
                Some(r) => e.result == r,
                None => true,
            })
            .collect()
    }

    /// 日志锚点路径（配置真值，诊断/`security.status` 展示用）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 本日志当前可读的全部分段文件（诊断/测试用；不创建、不修改）。
    pub fn files(&self) -> Vec<PathBuf> {
        audit_files(&self.path)
    }
}

/// 退出前有界等待本进程的在途追加写完。
///
/// 审计按进程独占分段同步追加，没有内存队列，也没有跨文件迁移；这里只等
/// 正在进行的 `write` 落盘。预算耗尽返回 `false`，由调用方告警，退出不被
/// 拖住（C-154 的同类约束）。
pub fn flush_pending(wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FLUSH_POLL);
    }
    true
}

/// 本进程为某个审计锚点共享的分段槽位。
///
/// 同一进程里的多个 `AuditLogger`（含克隆）写同一个分段文件；文件名带 pid
/// 与随机量，别的进程不会写它。
fn shared_file(anchor: &Path) -> SharedFile {
    static SINKS: OnceLock<Mutex<HashMap<PathBuf, SharedFile>>> = OnceLock::new();
    let mut map = SINKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(anchor.to_path_buf()).or_default().clone()
}

/// 在途追加计数：进入时加一，写出结果（成功或失败）后减一。
struct InFlight;

impl InFlight {
    fn enter() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ProcessFile {
    /// 创建一个独占的新分段：`O_EXCL`（`create_new`），同名换名重试。
    fn create(anchor: &Path) -> std::io::Result<Self> {
        let dir = audit_dir(anchor);
        std::fs::create_dir_all(dir)?;
        let name = anchor
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audit.jsonl".into());
        for _ in 0..SEGMENT_ATTEMPTS {
            let candidate = dir.join(format!("{name}.p{}-{}", std::process::id(), random_token()));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .append(true)
                .open(&candidate)
            {
                Ok(file) => {
                    let meta = file.metadata()?;
                    return Ok(Self {
                        path: candidate,
                        file,
                        dev: meta.dev(),
                        ino: meta.ino(),
                        broken: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "audit segment name collision retries exhausted",
        ))
    }

    /// 路径仍指向创建时的 inode；外部清理过的分段不再复用。
    fn still_at_path(&self) -> bool {
        std::fs::metadata(&self.path)
            .is_ok_and(|meta| meta.dev() == self.dev && meta.ino() == self.ino)
    }

    /// 追加一条完整记录。
    ///
    /// 上一次写入若被中断，文件可能停在无换行的残片上：先补一个换行把它隔离
    /// 成独立坏行，避免新记录拼在残片后面一起被解析器跳过。
    fn append(&mut self, record: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        fault::write_delay();
        match ends_with_newline(&mut self.file) {
            Ok(true) => {}
            Ok(false) => {
                if write_bytes(&mut self.file, b"\n").is_err() {
                    self.broken = true;
                    return Err(std::io::Error::other("audit boundary repair failed"));
                }
            }
            Err(e) => {
                self.broken = true;
                return Err(e);
            }
        }
        let start = self.file.metadata()?.len();
        if let Err(e) = write_bytes(&mut self.file, record) {
            // 回滚本次写入；分段只由本进程写，截回起点不会碰到别人的记录。
            if self.file.set_len(start).is_err() {
                // 回滚失败：尽力补一个换行把残片隔离成独立坏行；补不上就弃用
                // 该分段，绝不能把下一条记录拼在残片后面。
                if write_bytes(&mut self.file, b"\n").is_err() {
                    self.broken = true;
                }
            }
            return Err(e);
        }
        Ok(())
    }
}

/// 审计锚点对应的全部可读文件（旧主日志 + 各进程分段），按名字排序。
///
/// 分段名是 `<锚点名>.p<pid>-<32 位十六进制随机量>`；旧机制留下的 `.park-*`
/// 与 `.tmp` 文件不匹配该形状，不参与读取（它们可能已并入主日志，读入会
/// 产生重复行）。
pub fn audit_files(anchor: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if anchor.is_file() {
        files.push(anchor.to_path_buf());
    }
    let dir = audit_dir(anchor);
    let Some(anchor_name) = anchor
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return files;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    let mut segments: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| is_process_file(&entry.file_name().to_string_lossy(), &anchor_name))
        .map(|entry| entry.path())
        .collect();
    segments.sort();
    files.extend(segments);
    files
}

/// 审计锚点的目录解析：写入与枚举必须共用这一处。
///
/// 裸文件名的 [`Path::parent`] 是 `Some("")`：`create_dir_all("")` 成功，
/// 而 `read_dir("")` 报 ENOENT。两侧若各写各的解析，记录就会写得进去却查
/// 不出来。这里把空父目录统一解析成当前目录 `.`；相对锚点保持「相对进程
/// 工作目录」的既有语义，不在配置入口改成绝对路径（避免在配置解析时固化
/// 工作目录，后续 `chdir` 会让写读两侧再次分叉）。
fn audit_dir(anchor: &Path) -> &Path {
    match anchor.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// 文件名是否为本机制的分段：`<锚点名>.p<pid>-<32 位十六进制>`。
fn is_process_file(name: &str, anchor_name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(anchor_name)
        .and_then(|rest| rest.strip_prefix(".p"))
    else {
        return false;
    };
    let Some((pid, token)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && token.len() == 32
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 只解析以换行结尾的完整行；末行残片（写入中断）按现有规则跳过。
fn parse_complete_lines(blob: &[u8]) -> Vec<AuditEntry> {
    let mut entries = Vec::new();
    let mut start = 0;
    for (index, byte) in blob.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &blob[start..index];
        start = index + 1;
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<AuditEntry>(line) {
            entries.push(entry);
        }
    }
    entries
}

/// 文件是否为空或以换行结尾（空文件视为边界完整）。
fn ends_with_newline(f: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// 追加一段字节；测试可注入一次「写入 N 字节后失败」，覆盖部分写入路径。
fn write_bytes(f: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(limit) = fault::take_fail_after() {
        let limit = limit.min(bytes.len());
        f.write_all(&bytes[..limit])?;
        return Err(std::io::Error::other("injected audit write failure"));
    }
    f.write_all(bytes)
}

/// 分段随机量：32 位十六进制，取自进程启动后的 v4 UUID。
fn random_token() -> String {
    #[cfg(test)]
    if let Some(token) = fault::take_token() {
        return token;
    }
    uuid::Uuid::new_v4().simple().to_string()
}

/// XDG 运行时状态目录（`XDG_STATE_HOME` → `~/.local/state/agent-shell`）。
///
/// 与 daemon `single_instance::state_dir()` 约定一致；core 不依赖 daemon，
/// 故在安全模块内自持一份。
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("agent-shell");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("agent-shell")
}

/// 配置目录（`XDG_CONFIG_HOME` → `~/.config/agent-shell`）。
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("agent-shell");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agent-shell")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 故障注入：部分写入、同名冲突与慢写，覆盖并发/中断路径。
#[cfg(test)]
mod fault {
    use std::cell::{Cell, RefCell};

    /// 消费一次注入；返回 `Some(n)` 表示本次写入应在 n 字节后失败。
    pub(super) fn take_fail_after() -> Option<usize> {
        FAIL_AFTER.with(Cell::take)
    }

    pub(super) fn fail_after(limit: usize) {
        FAIL_AFTER.with(|cell| cell.set(Some(limit)));
    }

    /// 强制下一次分段命名使用给定随机量：用于制造同名冲突。
    pub(super) fn take_token() -> Option<String> {
        TOKEN.with(|cell| cell.borrow_mut().take())
    }

    pub(super) fn force_token(token: &str) {
        TOKEN.with(|cell| *cell.borrow_mut() = Some(token.to_string()));
    }

    /// 追加前的可注入延迟：覆盖退出等待与查询并发。
    pub(super) fn write_delay() {
        if let Some(delay) = DELAY.with(Cell::get) {
            std::thread::sleep(delay);
        }
    }

    pub(super) fn set_write_delay(delay: Option<std::time::Duration>) {
        DELAY.with(|cell| cell.set(delay));
    }

    thread_local! {
        static FAIL_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        static TOKEN: RefCell<Option<String>> = const { RefCell::new(None) };
        static DELAY: Cell<Option<std::time::Duration>> = const { Cell::new(None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    /// 退出等待断言读写全局在途计数；相关用例须串行，避免并行用例干扰。
    fn flush_serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 一次性测试目录：同一用例内的所有审计文件都落在里面，用完整目录删除。
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-shell-audit-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        fn anchor(&self) -> PathBuf {
            self.0.join("audit.jsonl")
        }

        fn segment(&self, pid: u32, token: &str) -> PathBuf {
            self.0.join(format!("audit.jsonl.p{pid}-{token}"))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_record(agent: &str, op: &str) -> Vec<u8> {
        let entry = AuditEntry {
            timestamp: now_secs(),
            agent_id: agent.to_string(),
            op: op.to_string(),
            decision: "allow".to_string(),
            result: false,
        };
        let mut line = serde_json::to_string(&entry).expect("serialize audit entry");
        line.push('\n');
        line.into_bytes()
    }

    fn complete_lines(path: &Path) -> Vec<Vec<u8>> {
        let raw = std::fs::read(path).expect("read audit file");
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in raw.iter().enumerate() {
            if *byte == b'\n' {
                if index > start {
                    lines.push(raw[start..index].to_vec());
                }
                start = index + 1;
            }
        }
        lines
    }

    /// 裸文件名的空父目录按 `.` 解析：写入与枚举共用同一处，不分叉。
    #[test]
    fn audit_dir_resolves_bare_filename_to_current_directory() {
        assert_eq!(audit_dir(Path::new("audit.jsonl")), Path::new("."));
        assert_eq!(audit_dir(Path::new("./audit.jsonl")), Path::new("."));
        assert_eq!(
            audit_dir(Path::new("/var/lib/agent-shell/audit.jsonl")),
            Path::new("/var/lib/agent-shell")
        );
    }

    /// 同一进程的多个日志器写同一个分段：每条记录独占一行、逐行可解析。
    #[test]
    fn concurrent_appends_keep_one_record_per_line() {
        let dir = TestDir::new("concurrent");
        let anchor = dir.anchor();
        let logger = AuditLogger::new(&anchor);
        let writers = 8;
        let per_writer = 100;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for writer in 0..writers {
            let logger = logger.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..per_writer {
                    logger.log(
                        &format!("agent-{writer}"),
                        &format!("op.{writer}.{i}"),
                        "allow",
                        false,
                    );
                }
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread");
        }

        assert_eq!(logger.files().len(), 1, "同进程只应有一个分段");
        let lines = complete_lines(&logger.files()[0]);
        assert_eq!(lines.len(), writers * per_writer, "每条记录必须独占一行");
        for line in &lines {
            serde_json::from_slice::<AuditEntry>(line)
                .unwrap_or_else(|e| panic!("不可解析的审计行 {line:?}: {e}"));
        }
        let entries = logger.read_all();
        assert_eq!(entries.len(), writers * per_writer);
        let mut ops: Vec<String> = entries.iter().map(|entry| entry.op.clone()).collect();
        ops.sort_unstable();
        ops.dedup();
        assert_eq!(ops.len(), writers * per_writer, "不得有重复行");
    }

    /// 同进程重新构造的日志器复用同一分段：不因多次构造散出多份文件。
    #[test]
    fn independent_loggers_share_one_process_segment() {
        let dir = TestDir::new("shared");
        let anchor = dir.anchor();
        let first = AuditLogger::new(&anchor);
        let second = AuditLogger::new(&anchor);
        first.log("agent-a", "op.first", "allow", false);
        second.log("agent-a", "op.second", "allow", false);

        let files = first.files();
        assert_eq!(files.len(), 1, "同进程同锚点只应有一个分段: {files:?}");
        assert!(is_process_file(
            &files[0].file_name().unwrap().to_string_lossy(),
            "audit.jsonl"
        ));
        assert_eq!(second.read_all().len(), 2);
        assert_eq!(first.read_all(), second.read_all());
    }

    /// 预占分段名后必须换名重试：既有文件一个字节都不动，记录落到新名字。
    #[test]
    fn colliding_segment_name_is_retried_without_touching_existing_file() {
        let dir = TestDir::new("collision");
        let anchor = dir.anchor();
        let token = "a".repeat(32);
        let occupied = dir.segment(std::process::id(), &token);
        let occupied_bytes = sample_record("other", "op.occupied");
        std::fs::write(&occupied, &occupied_bytes).expect("seed colliding segment");

        fault::force_token(&token);
        let logger = AuditLogger::new(&anchor);
        logger.log("agent-a", "op.after-collision", "allow", false);

        assert_eq!(
            std::fs::read(&occupied).expect("read occupied"),
            occupied_bytes,
            "O_EXCL 冲突不得改写既有文件"
        );
        let files = logger.files();
        assert_eq!(files.len(), 2, "应另起一个新分段: {files:?}");
        let entries = logger.read_all();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.op == "op.after-collision")
                .count(),
            1
        );
    }

    /// 查询只读：不创建、不改写、不删除任何审计文件，重复查询结果一致。
    #[test]
    fn query_is_read_only_and_repeatable() {
        let dir = TestDir::new("readonly");
        let anchor = dir.anchor();
        let logger = AuditLogger::new(&anchor);
        logger.log("agent-a", "op.one", "allow", false);
        logger.log("agent-a", "op.two", "deny", false);
        logger.log("agent-b", "op.three", "confirm", false);

        let before: Vec<(PathBuf, Vec<u8>)> = logger
            .files()
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).expect("read before query");
                (path, bytes)
            })
            .collect();
        let first = logger.read_all();
        let second = logger.read_all();
        let filtered = logger.query("agent-a", "", "", None);
        let after: Vec<(PathBuf, Vec<u8>)> = logger
            .files()
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).expect("read after query");
                (path, bytes)
            })
            .collect();

        assert_eq!(before, after, "查询不得写盘或删文件");
        assert_eq!(first, second, "重复查询必须给出同一结果");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|entry| entry.agent_id == "agent-a"));
    }

    /// 写者持续追加时，任何一次查询都不得让之后查不到已写入的记录，也不得
    /// 在同一份结果里产生重复行。
    #[test]
    fn query_never_hides_records_observed_earlier() {
        const TOTAL: usize = 200;
        let dir = TestDir::new("monotonic");
        let anchor = dir.anchor();
        let writer = AuditLogger::new(&anchor);
        let reader = AuditLogger::new(&anchor);
        let handle = {
            std::thread::spawn(move || {
                for i in 0..TOTAL {
                    writer.log("agent-w", &format!("op.{i}"), "allow", false);
                    if i % 8 == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            })
        };

        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let entries = reader.read_all();
            let mut ops: Vec<String> = entries.iter().map(|entry| entry.op.clone()).collect();
            let observed = ops.len();
            ops.sort_unstable();
            ops.dedup();
            assert_eq!(ops.len(), observed, "单次查询不得出现重复行");
            for op in &seen {
                assert!(
                    ops.binary_search(op).is_ok(),
                    "已观察到的记录 {op} 不得在后续查询中消失"
                );
            }
            seen = ops;
            if seen.len() == TOTAL {
                break;
            }
            assert!(Instant::now() < deadline, "写者未在预算内完成");
            std::thread::sleep(Duration::from_millis(1));
        }
        handle.join().expect("writer thread");

        let final_entries = reader.read_all();
        assert_eq!(final_entries.len(), TOTAL, "一条都不能丢");
        let mut final_ops: Vec<String> =
            final_entries.iter().map(|entry| entry.op.clone()).collect();
        final_ops.sort_unstable();
        final_ops.dedup();
        assert_eq!(final_ops.len(), TOTAL, "每条只出现一次");
    }

    /// 同一秒内内容完全相同的多条记录仍须逐条保留：审计不按内容判重。
    #[test]
    fn identical_records_are_never_deduplicated() {
        let dir = TestDir::new("identical");
        let logger = AuditLogger::new(dir.anchor());
        for _ in 0..5 {
            logger.log("agent-a", "op.same", "allow", false);
        }
        let entries = logger.read_all();
        assert_eq!(entries.len(), 5, "相同内容的多条记录不得被合并");
        assert!(entries.iter().all(|entry| entry.op == "op.same"));
    }

    /// 末行残片（进程被杀/写入中断）只丢自己；后续记录先补换行隔离，绝不拼行。
    #[test]
    fn torn_last_line_is_skipped_and_next_record_keeps_its_line() {
        let dir = TestDir::new("torn");
        let logger = AuditLogger::new(dir.anchor());
        logger.log("agent-a", "op.before", "allow", false);
        let file = logger.files().remove(0);
        let mut raw = std::fs::read(&file).expect("read audit file");
        raw.extend_from_slice(b"{\"agent_id\":\"torn");
        std::fs::write(&file, &raw).expect("seed torn tail");

        let entries = logger.read_all();
        assert_eq!(entries.len(), 1, "未写完的末行按现有规则跳过");
        assert_eq!(entries[0].op, "op.before");

        logger.log("agent-a", "op.after", "allow", true);
        let entries = logger.read_all();
        assert_eq!(entries.len(), 2, "残片不得吞掉下一条记录: {entries:?}");
        assert_eq!(entries[0].op, "op.before");
        assert_eq!(entries[1].op, "op.after");
        assert!(entries[1].result);
        let lines = complete_lines(&file);
        assert_eq!(lines.len(), 3, "残片必须被隔离成独立坏行");
        assert!(serde_json::from_slice::<AuditEntry>(&lines[1]).is_err());
        serde_json::from_slice::<AuditEntry>(&lines[2]).expect("恢复记录可解析");
    }

    /// 部分写入失败回滚到写前偏移：故障前的记录仍在，恢复后的记录独占一行。
    #[test]
    fn partial_append_rolls_back_and_next_record_keeps_its_line() {
        let dir = TestDir::new("partial");
        let logger = AuditLogger::new(dir.anchor());
        logger.log("agent-a", "op.before", "allow", false);
        let file = logger.files().remove(0);
        let before = std::fs::metadata(&file).expect("metadata").len();

        fault::fail_after(5);
        logger.log("agent-a", "op.torn", "allow", false);
        assert_eq!(
            std::fs::metadata(&file).expect("metadata").len(),
            before,
            "部分写入必须回滚，不留残片"
        );

        logger.log("agent-a", "op.after", "allow", true);
        let entries = logger.read_all();
        assert_eq!(entries.len(), 2, "故障前的记录和恢复后的记录都要能查到");
        assert_eq!(entries[0].op, "op.before");
        assert_eq!(entries[1].op, "op.after");
        assert_eq!(
            logger
                .query("agent-a", "op.after", "allow", Some(true))
                .len(),
            1
        );
    }

    /// 补换行失败时不得追加新记录；该分段被弃用，下一条记录另起一段。
    #[test]
    fn failed_boundary_repair_never_concatenates_new_record() {
        let dir = TestDir::new("boundary");
        let logger = AuditLogger::new(dir.anchor());
        logger.log("agent-a", "op.before", "allow", false);
        let first = logger.files().remove(0);
        let mut raw = std::fs::read(&first).expect("read audit file");
        raw.extend_from_slice(b"{\"agent_id\":\"trunc");
        std::fs::write(&first, &raw).expect("seed torn tail");
        let torn_len = std::fs::metadata(&first).expect("metadata").len();

        fault::fail_after(0);
        logger.log("agent-a", "op.blocked", "allow", false);
        assert_eq!(
            std::fs::metadata(&first).expect("metadata").len(),
            torn_len,
            "补不上换行时不得追加新记录"
        );

        logger.log("agent-a", "op.recovered", "allow", false);
        assert_eq!(logger.files().len(), 2, "弃用旧分段后另起一段");
        let entries = logger.read_all();
        assert_eq!(entries.len(), 2);
        let ops: Vec<&str> = entries.iter().map(|entry| entry.op.as_str()).collect();
        assert!(ops.contains(&"op.before"), "历史记录不得丢失: {ops:?}");
        assert!(ops.contains(&"op.recovered"), "恢复记录必须可查: {ops:?}");
    }

    /// 单行里被截断的多字节 UTF-8 只丢该行，不连累整份历史记录。
    #[test]
    fn read_all_skips_bad_bytes_without_dropping_history() {
        let dir = TestDir::new("badutf8");
        let logger = AuditLogger::new(dir.anchor());
        logger.log("agent-a", "op.before", "allow", false);
        let file = logger.files().remove(0);
        let mut raw = std::fs::read(&file).expect("read audit file");
        raw.extend_from_slice(b"{\"agent_id\":\"\xe4\xb8");
        raw.push(b'\n');
        std::fs::write(&file, raw).expect("inject bad line");

        assert_eq!(logger.read_all().len(), 1, "坏行只丢自己，历史记录仍可读");
        logger.log("agent-a", "op.after", "allow", true);
        let entries = logger.read_all();
        assert_eq!(entries.len(), 2);
        assert_eq!(logger.query("", "op.before", "allow", Some(false)).len(), 1);
    }

    /// 既有 `audit.jsonl` 与新分段共存读取：旧记录可见、旧文件不被改写。
    #[test]
    fn legacy_audit_log_is_read_without_modification() {
        let dir = TestDir::new("legacy");
        let anchor = dir.anchor();
        let mut legacy = sample_record("legacy", "op.old.0");
        legacy.extend_from_slice(&sample_record("legacy", "op.old.1"));
        legacy.extend_from_slice(b"{\"agent_id\":\"old-torn");
        std::fs::write(&anchor, &legacy).expect("seed legacy log");

        let logger = AuditLogger::new(&anchor);
        logger.log("agent-a", "op.new.0", "allow", false);
        logger.log("agent-a", "op.new.1", "deny", false);

        let entries = logger.read_all();
        assert_eq!(
            entries.len(),
            4,
            "旧记录（含被跳过的残片）与新记录都必须可查"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.agent_id == "legacy")
                .count(),
            2
        );
        assert_eq!(
            std::fs::read(&anchor).expect("read legacy"),
            legacy,
            "新机制不得改写既有主日志"
        );
        assert_eq!(
            logger.query("legacy", "", "", None).len(),
            2,
            "过滤须跨旧主日志与新分段"
        );
    }

    /// 状态目录里的无关文件（旧旁路、临时文件、别的前缀）不参与读取。
    #[test]
    fn unrelated_files_in_state_dir_are_ignored() {
        let dir = TestDir::new("unrelated");
        let anchor = dir.anchor();
        let token = "b".repeat(32);
        for name in [
            "audit.jsonl.park-1-0".to_string(),
            "audit.jsonl.tmp".to_string(),
            "sessions.json".to_string(),
            "audit.jsonl.bak".to_string(),
            format!("audit.jsonlX.p1-{token}"),
        ] {
            std::fs::write(dir.0.join(name), sample_record("other", "op.other"))
                .expect("seed unrelated file");
        }

        let logger = AuditLogger::new(&anchor);
        logger.log("agent-a", "op.real", "allow", false);
        let entries = logger.read_all();
        assert_eq!(entries.len(), 1, "只读本机制的文件: {entries:?}");
        assert_eq!(entries[0].op, "op.real");
    }

    /// 目录内文件数量增长（多次进程启动）时仍完整合并，且查询不删不改。
    #[test]
    fn growing_file_count_stays_queryable_without_rewriting() {
        const FILES: usize = 200;
        let dir = TestDir::new("growth");
        let anchor = dir.anchor();
        let mut expected = Vec::new();
        for i in 0..FILES {
            let token = format!("{i:032x}");
            let path = dir.segment(10_000 + i as u32, &token);
            std::fs::write(&path, sample_record("bulk", &format!("op.bulk.{i}")))
                .expect("seed segment");
            expected.push(path);
        }
        std::fs::write(&anchor, sample_record("legacy", "op.legacy")).expect("seed legacy");

        let logger = AuditLogger::new(&anchor);
        let before: Vec<(PathBuf, Vec<u8>)> = logger
            .files()
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).expect("read before");
                (path, bytes)
            })
            .collect();
        let entries = logger.read_all();
        assert_eq!(entries.len(), FILES + 1);
        assert_eq!(
            logger.query("bulk", "", "", None).len(),
            FILES,
            "文件数量增长不得漏读"
        );
        let after: Vec<(PathBuf, Vec<u8>)> = logger
            .files()
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).expect("read after");
                (path, bytes)
            })
            .collect();
        assert_eq!(before, after, "查询不得删除或改写任何分段");
        assert!(expected.iter().all(|path| path.exists()));
    }

    /// 过滤维度覆盖调用方、操作、判定与结果，且结果只来自完整行。
    #[test]
    fn query_filters_every_dimension() {
        let dir = TestDir::new("filter");
        let anchor = dir.anchor();
        let logger = AuditLogger::new(&anchor);
        logger.log("agent-a", "op.x", "allow", false);
        logger.log("agent-a", "op.x", "allow", true);
        logger.log("agent-b", "op.x", "deny", false);
        logger.log("agent-a", "op.y", "confirm", false);

        assert_eq!(logger.query("agent-a", "", "", None).len(), 3);
        assert_eq!(logger.query("", "op.x", "", None).len(), 3);
        assert_eq!(logger.query("", "", "deny", None).len(), 1);
        assert_eq!(logger.query("", "op.x", "allow", Some(true)).len(), 1);
        assert_eq!(
            logger.query("agent-b", "op.x", "deny", Some(false)).len(),
            1
        );
        assert!(logger.query("agent-c", "", "", None).is_empty());
    }

    /// 退出等待有墙钟上限；空闲时立即完成，有在途写入时如实报告未完成。
    #[test]
    fn flush_pending_is_bounded_and_reports_in_flight_writes() {
        let _serial = flush_serial();
        assert!(
            flush_pending(Duration::from_secs(5)),
            "无在途追加时退出等待必须完成"
        );

        let dir = TestDir::new("flush");
        let logger = AuditLogger::new(dir.anchor());
        let handle = std::thread::spawn(move || {
            fault::set_write_delay(Some(Duration::from_millis(80)));
            logger.log("agent-a", "op.slow", "allow", false);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while IN_FLIGHT.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "慢写未进入在途状态");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !flush_pending(Duration::from_millis(5)),
            "在途写入未结束时不得谎报完成"
        );
        handle.join().expect("slow writer");
        assert!(flush_pending(Duration::from_secs(5)));
    }

    /// 子进程写者：由多进程与杀进程用例拉起；无环境变量时是空操作。
    #[test]
    fn audit_child_process_writer() {
        let Ok(path) = std::env::var("AUDIT_CHILD_PATH") else {
            return;
        };
        let agent = std::env::var("AUDIT_CHILD_AGENT").unwrap_or_else(|_| "child".into());
        let count: usize = std::env::var("AUDIT_CHILD_COUNT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(50);
        let logger = AuditLogger::new(path);
        for i in 0..count {
            logger.log(&agent, &format!("op.child.{i}"), "allow", false);
        }
        if std::env::var_os("AUDIT_CHILD_HOLD").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    fn spawn_child(anchor: &Path, agent: &str, count: usize, hold: bool) -> std::process::Child {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command
            .args([
                "--exact",
                "audit::tests::audit_child_process_writer",
                "--test-threads",
                "1",
            ])
            .env("AUDIT_CHILD_PATH", anchor)
            .env("AUDIT_CHILD_AGENT", agent)
            .env("AUDIT_CHILD_COUNT", count.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if hold {
            command.env("AUDIT_CHILD_HOLD", "1");
        }
        command.spawn().expect("spawn audit child")
    }

    /// 多进程并发写：各自独占分段，父进程同时写，最终每条记录恰好一次。
    #[test]
    fn cross_process_writers_keep_every_record() {
        const CHILDREN: usize = 3;
        const CHILD_COUNT: usize = 60;
        const PARENT_COUNT: usize = 60;
        let dir = TestDir::new("crossproc");
        let anchor = dir.anchor();
        let children: Vec<std::process::Child> = (0..CHILDREN)
            .map(|i| spawn_child(&anchor, &format!("child-{i}"), CHILD_COUNT, false))
            .collect();
        let logger = AuditLogger::new(&anchor);
        for i in 0..PARENT_COUNT {
            logger.log("parent", &format!("op.parent.{i}"), "allow", false);
        }
        for child in children {
            let status = child.wait_with_output().expect("wait child");
            assert!(status.status.success(), "child writer must succeed");
        }

        let entries = logger.read_all();
        assert_eq!(entries.len(), CHILDREN * CHILD_COUNT + PARENT_COUNT);
        for i in 0..CHILDREN {
            let agent = format!("child-{i}");
            let mut ops: Vec<String> = entries
                .iter()
                .filter(|entry| entry.agent_id == agent)
                .map(|entry| entry.op.clone())
                .collect();
            ops.sort_unstable();
            ops.dedup();
            assert_eq!(ops.len(), CHILD_COUNT, "{agent} 的记录必须完整且不重复");
        }
        assert_eq!(
            logger.files().len(),
            CHILDREN + 1,
            "每个进程一个分段，父进程不与他人共享"
        );
    }

    /// 进程被杀后，已写入的记录仍按行可读；新进程的记录不与残片混写。
    #[test]
    fn killed_process_records_stay_readable() {
        const CHILD_COUNT: usize = 30;
        let dir = TestDir::new("killed");
        let anchor = dir.anchor();
        let mut child = spawn_child(&anchor, "doomed", CHILD_COUNT, true);
        let observer = AuditLogger::new(&anchor);
        let deadline = Instant::now() + Duration::from_secs(10);
        while observer.read_all().len() < CHILD_COUNT {
            assert!(Instant::now() < deadline, "子进程未在预算内写完记录");
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().expect("kill child");
        child.wait().expect("reap child");

        let entries = observer.read_all();
        assert_eq!(
            entries.len(),
            CHILD_COUNT,
            "已写入的记录不得因进程被杀而消失"
        );
        assert!(entries.iter().all(|entry| entry.agent_id == "doomed"));

        observer.log("parent", "op.after-kill", "allow", true);
        let entries = observer.read_all();
        assert_eq!(entries.len(), CHILD_COUNT + 1);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.agent_id == "doomed")
                .count(),
            CHILD_COUNT,
            "新记录不得与旧分段混写或重复"
        );
    }
}
