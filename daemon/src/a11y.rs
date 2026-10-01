//! daemon 侧 AT-SPI 存活性探测（§23.3：AT-SPI 连接归属 daemon 域）。
//!
//! 分级定位：session bus → a11y bus 地址 → a11y bus → Registry。口径与
//! `components/a11y` 的桥接一致：先试已存在的 socket（不激活 a11y bus），
//! 再向 session bus 上的 `org.a11y.Bus` 索取地址；Registry 未运行但可激活时
//! 按需启动（会话内重启恢复）。失败原因精确到阶段——「总线未启动」「socket
//! 失效」「Registry 未注册」需要完全不同的处置，合并成一句「不可达」会让
//! 存活性漂移无从定位。

use agent_shell_a11y::atspi_bridge::{
    A11yBusOutcome, AtspiBridge, LauncherActivation, AT_SPI_BUS_ADDRESS_ENV, BUS_SERVICE,
    REGISTRY_SERVICE, ROOT_PATH, SESSION_CALL_TIMEOUT, STATUS_IFACE, STATUS_PATH,
};
use std::future::Future;
use std::time::Duration;
use zbus::names::WellKnownName;

/// Registry 激活等待上限（兜底；设计文档 §19 的 10s）：registryd 启动是秒级，
/// 远低于 D-Bus 激活超时 ~120s。实测该激活的失败现场不是超时——broker 环境缺
/// `AT_SPI_BUS_ADDRESS` 时 registryd 取不到总线地址、未注册名字即退出：dbus-broker
/// 立即回 `ServiceUnknown`（归因见 [`activation_failure_reason`]），dbus-daemon 则
/// 把激活挂到自身 `service_start_timeout`（25s）才回错。本上限先于总线返回，兜住
/// 这种挂起（挂死的 broker 不该拖住探测）。
const REGISTRY_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);

/// AT-SPI Registry 可达性报告行（✓/⚠ 前缀；doctor 与 a11y.status 共用）。
///
/// 可达行的基准前缀 `✓ AT-SPI         : enabled (a11y bus up, Registry
/// reachable` 是 doctor/QA 的判定锚点，附加细节只允许出现在其后。
///
/// **写副作用**：Registry 未运行但可激活时本探测会按需拉起 at-spi2-registryd
/// （会话内重启恢复），并在该行注明 `Registry started on demand`；a11y bus
/// 本身不会被激活（`LauncherActivation::Forbidden`）。
pub async fn atspi_line() -> String {
    match probe().await {
        Ok(detail) => enabled_line(&detail),
        Err(reason) => unavailable_line(&reason),
    }
}

/// ✓ 行构造（纯函数，供单测锚定前缀契约）。
fn enabled_line(detail: &str) -> String {
    format!("✓ AT-SPI         : enabled (a11y bus up, Registry reachable{detail})")
}

/// ⚠ 行构造（纯函数；`reason` 必须带阶段信息）。
fn unavailable_line(reason: &str) -> String {
    format!("⚠ AT-SPI         : unavailable ({reason})")
}

/// 单次探测：成功返回附加细节（以 `; ` 起始，可为空），失败返回分级原因。
async fn probe() -> Result<String, String> {
    let session = bounded(
        zbus::Connection::session(),
        SESSION_CALL_TIMEOUT,
        "session bus connect",
    )
    .await
    .map_err(|e| format!("session bus unreachable ({e})"))?;
    // 被动判据先行：`org.a11y.Status` 由 launcher 提供，未注册时读取它会把它
    // 激活（探测不得有副作用），故只有已注册才去读启用位。
    let launcher_owned = AtspiBridge::launcher_state_on(&session)
        .await
        .map(|launcher| launcher.has_owner)
        .unwrap_or(false);
    let bus = connect_bus(&session).await?;
    let registry_note = ensure_registry(&bus).await?;
    Ok(detail(&session, &bus, registry_note, launcher_owned).await)
}

/// 连接 a11y bus：与桥接共用同一条被动解析链
/// （[`AtspiBridge::dial_a11y_bus`]，不激活 launcher），只在文案上差异化——
/// 两条链各自演进会让 doctor 报告的总线与 a11y 查询用的总线漂移。
async fn connect_bus(session: &zbus::Connection) -> Result<zbus::Connection, String> {
    let outcome = AtspiBridge::dial_a11y_bus(session, LauncherActivation::Forbidden).await;
    match outcome.connected() {
        Some(bus) => Ok(bus),
        None => Err(connect_failure_line(&outcome)),
    }
}

/// 连接失败文案：归因与桥接同源（[`A11yBusOutcome::reason`]），仅在此追加处置
/// 指引——只有「地址已解析、dial 失败」阶段才提示重启会话服务（socket 被替换
/// 或遗留：另一会话的 launcher 抢占过同一 runtime dir 路径，或 a11y bus 崩溃
/// 后未重启）；地址解析失败（预检/`GetAddress` 出错或超时）与 socket 状态无关，
/// 追加重启指引会误导操作者（纯函数，单测锚定分级）。
fn connect_failure_line(outcome: &A11yBusOutcome) -> String {
    let reason = outcome
        .reason()
        .unwrap_or_else(|| "a11y bus unavailable".to_string());
    match outcome {
        A11yBusOutcome::Unreachable { .. } => {
            format!("{reason}; stale a11y bus socket — restart at-spi-dbus-bus.service")
        }
        _ => reason,
    }
}

/// Registry 存活性：已注册即通过；未注册但可激活时按需启动并复核。
///
/// 返回 `Some(说明)` 表示本次探测触发了会话内重启恢复。
async fn ensure_registry(bus: &zbus::Connection) -> Result<Option<String>, String> {
    let dbus = zbus::fdo::DBusProxy::new(bus)
        .await
        .map_err(|e| format!("a11y bus DBusProxy: {e}"))?;
    if name_owned(&dbus, REGISTRY_SERVICE).await? {
        return Ok(None);
    }
    if !is_activatable(&dbus, REGISTRY_SERVICE).await? {
        return Err(format!(
            "a11y bus up, {REGISTRY_SERVICE} not running and not activatable"
        ));
    }
    // registryd 退出后由 D-Bus 激活拉起——「Registry 存活性漂移」的恢复路径。
    let started = start_registry(&dbus).await?;
    if !name_owned(&dbus, REGISTRY_SERVICE).await? {
        return Err(format!(
            "{REGISTRY_SERVICE} activation returned {started} but name is still unowned — {}",
            unregistered_registry_hint()
        ));
    }
    Ok(Some("Registry started on demand".to_string()))
}

/// 拉起 Registry：激活失败按错误名归因（[`activation_failure_reason`]），
/// 超时归一为带上下限的调用文案（兜底，见 [`REGISTRY_ACTIVATION_TIMEOUT`]）。
async fn start_registry(dbus: &zbus::fdo::DBusProxy<'_>) -> Result<u32, String> {
    let activation = dbus.start_service_by_name(well_known(REGISTRY_SERVICE)?, 0);
    match tokio::time::timeout(REGISTRY_ACTIVATION_TIMEOUT, activation).await {
        Ok(Ok(started)) => Ok(started),
        Ok(Err(err)) => Err(activation_failure_reason(&err)),
        Err(_) => Err(format!(
            "StartServiceByName(Registry) timed out after {}s",
            REGISTRY_ACTIVATION_TIMEOUT.as_secs()
        )),
    }
}

/// 「Registry 拉起后仍未注册」的归因提示（激活失败与激活后名字仍无主共用）。
///
/// broker 环境缺 `AT_SPI_BUS_ADDRESS` 时 registryd 取不到自身总线地址、连不上
/// 即退出，总线只回一个错误名（`ServiceUnknown` / `Spawn.ChildExited`）——现场
/// 无从归因，故在此补出可疑根因与处置。
fn unregistered_registry_hint() -> String {
    format!(
        "a11y broker environment likely lacks {AT_SPI_BUS_ADDRESS_ENV} — registryd cannot reach the a11y bus without it and exits instead of registering; restart at-spi-dbus-bus.service"
    )
}

/// 激活失败归因（供 [`start_registry`] 归一）：未注册名字即退出类
/// （[`activation_left_unregistered`]）追加 broker 环境提示，其余保留 D-Bus
/// 原始错误名与描述。
fn activation_failure_reason(err: &zbus::fdo::Error) -> String {
    if activation_left_unregistered(err) {
        format!("{err}; {}", unregistered_registry_hint())
    } else {
        err.to_string()
    }
}

/// 激活失败是否为「进程未注册名字即退出」类（两种总线实现下的同一现场）：
/// dbus-broker（经 systemd 瞬态单元激活）报 `ServiceUnknown`，dbus-daemon 报
/// `Spawn.ChildExited`。
fn activation_left_unregistered(err: &zbus::fdo::Error) -> bool {
    matches!(
        err,
        zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::SpawnChildExited(_)
    )
}

/// 名称是否已在总线上注册。
async fn name_owned(dbus: &zbus::fdo::DBusProxy<'_>, name: &'static str) -> Result<bool, String> {
    bounded(
        dbus.name_has_owner(well_known(name)?.into()),
        SESSION_CALL_TIMEOUT,
        "NameHasOwner",
    )
    .await
}

/// 编译期字面量 → 良构总线名；解析失败只能是拼接错误，归为探测失败原因。
fn well_known(name: &'static str) -> Result<WellKnownName<'static>, String> {
    name.try_into()
        .map_err(|e| format!("bad bus name {name}: {e}"))
}

/// 名称在总线的可激活列表内（`org.a11y.Bus` / `org.a11y.atspi.Registry` 都是
/// 可激活服务；不可激活说明对应进程缺失）。
async fn is_activatable(dbus: &zbus::fdo::DBusProxy<'_>, name: &str) -> Result<bool, String> {
    let activatable = bounded(
        dbus.list_activatable_names(),
        SESSION_CALL_TIMEOUT,
        "ListActivatableNames",
    )
    .await?;
    Ok(activatable.iter().any(|n| n.as_str() == name))
}

/// 状态行附加细节（尽力而为：读取失败即省略对应片段）：恢复说明 +
/// 会话 AT-SPI 启用态 + Registry 已注册节点数。
///
/// 「Registry 可达但应用数为 0」正是无障碍未启用的形态——工具包（Qt/GTK）在
/// `org.a11y.Status` 为假时不向 a11y bus 注册，`a11y query` 因此恒空。把启用位
/// 与节点数一并报出，操作者无需二次排查即可定位。
///
/// `launcher_owned` 决定是否读启用位：launcher 未注册时该读取会激活它
/// （`org.a11y.Status` 挂在 `org.a11y.Bus` 名下），探测不做这种事。
async fn detail(
    session: &zbus::Connection,
    bus: &zbus::Connection,
    note: Option<String>,
    launcher_owned: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(note) = note {
        parts.push(note);
    }
    if launcher_owned {
        match session_is_enabled(session).await.as_deref() {
            Some("true") => parts.push("AT-SPI on".to_string()),
            Some(_) => parts.push("AT-SPI off (a11y query enables it on demand)".to_string()),
            None => {}
        }
    }
    if let Some(count) = root_child_count(bus).await {
        parts.push(format!("{count} nodes registered"));
    }
    join_detail(&parts)
}

/// 细节拼接：空集合产出空串，非空产出 `; a, b`（前导分号接在 ✓ 前缀后）。
fn join_detail(parts: &[String]) -> String {
    if parts.is_empty() {
        String::new()
    } else {
        format!("; {}", parts.join(", "))
    }
}

/// 读 session bus 上的 `org.a11y.Status.IsEnabled`（失败/属性缺失返回 `None`）。
async fn session_is_enabled(session: &zbus::Connection) -> Option<String> {
    let proxy = zbus::Proxy::new(session, BUS_SERVICE, STATUS_PATH, STATUS_IFACE)
        .await
        .ok()?;
    let enabled: bool = tokio::time::timeout(SESSION_CALL_TIMEOUT, proxy.get_property("IsEnabled"))
        .await
        .ok()?
        .ok()?;
    Some(enabled.to_string())
}

/// 读 Registry 桌面根节点的 `ChildCount`（= 已注册应用数；失败返回 `None`）。
///
/// AT-SPI 不实现标准 `GetAll` 缓存语义（部分实现返回空 `a{sv}`），必须禁用
/// zbus 属性缓存、逐个 Get（同桥接 `proxy_for`）。
async fn root_child_count(bus: &zbus::Connection) -> Option<i32> {
    let path: zbus::zvariant::OwnedObjectPath = ROOT_PATH.try_into().ok()?;
    let builder = zbus::proxy::Builder::<zbus::Proxy<'static>>::new(bus)
        .destination(REGISTRY_SERVICE)
        .and_then(|b| b.path(path))
        .and_then(|b| b.interface("org.a11y.atspi.Accessible"))
        .ok()?;
    let proxy = builder
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    tokio::time::timeout(
        SESSION_CALL_TIMEOUT,
        proxy.get_property::<i32>("ChildCount"),
    )
    .await
    .ok()?
    .ok()
}

/// 有界 D-Bus 调用：超时归一为「{what} timed out」，错误保留调用上下文。
async fn bounded<F, T, E>(fut: F, timeout: Duration, what: &'static str) -> Result<T, String>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| format!("{what} timed out"))?
        .map_err(|e| format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态行文案是 QA/doctor 的判定锚点：✓ 行必须保留基准前缀且细节只允许
    /// 追加在其后，⚠ 行必须携带阶段原因（漂移定位依赖它）。
    #[test]
    fn status_line_keeps_anchor_prefix_and_appends_detail() {
        let line = enabled_line("; AT-SPI on, 5 nodes registered");
        assert_eq!(
            line,
            "✓ AT-SPI         : enabled (a11y bus up, Registry reachable; AT-SPI on, 5 nodes registered)"
        );
        assert!(enabled_line("")
            .starts_with("✓ AT-SPI         : enabled (a11y bus up, Registry reachable)"));

        assert_eq!(
            unavailable_line("a11y bus unreachable at unix:path=/x (Connection refused)"),
            "⚠ AT-SPI         : unavailable (a11y bus unreachable at unix:path=/x (Connection refused))"
        );
    }

    /// 细节拼接：空集合不产出裸分号；恢复说明与启用位/节点数同序拼接。
    #[test]
    fn join_detail_is_compact() {
        assert_eq!(join_detail(&[]), "");
        assert_eq!(join_detail(&["AT-SPI on".to_string()]), "; AT-SPI on");
        assert_eq!(
            join_detail(&[
                "Registry started on demand".to_string(),
                "AT-SPI on".to_string(),
                "3 nodes registered".to_string(),
            ]),
            "; Registry started on demand, AT-SPI on, 3 nodes registered"
        );
    }

    /// 处置指引精确到阶段：只有 dial 失败（socket 被替换/遗留）追加「重启会话
    /// 服务」；地址解析失败（预检/GetAddress 出错或超时）与仅「可激活未启动」
    /// 都不追加——避免在调用级错误上给出与 socket 无关的误导处置。
    #[test]
    fn connect_failure_hint_is_stage_precise() {
        let dial = A11yBusOutcome::Unreachable {
            address: "unix:path=/run/user/1000/at-spi/bus_0".to_string(),
            reason: "a11y bus dial: Connection refused".to_string(),
        };
        let line = connect_failure_line(&dial);
        assert!(line.contains("restart at-spi-dbus-bus.service"), "{line}");
        assert!(line.contains("unreachable at unix:path="), "{line}");

        let resolve = A11yBusOutcome::AddressUnavailable {
            reason: "NameHasOwner(org.a11y.Bus) timed out".to_string(),
        };
        let line = connect_failure_line(&resolve);
        assert!(
            !line.contains("restart at-spi-dbus-bus.service"),
            "地址解析失败不得给 socket 处置指引: {line}"
        );
        assert!(line.contains("address resolution failed"), "{line}");

        let line = connect_failure_line(&A11yBusOutcome::NotStarted);
        assert!(!line.contains("restart at-spi-dbus-bus.service"), "{line}");
        assert!(line.contains("starts on demand"), "{line}");
    }

    /// 激活失败归因：两种总线实现的「拉起后未注册」错误名都要带 broker 环境
    /// 提示（否则现场只剩一个 D-Bus 错误名）；调用级失败不得被误归因。
    #[test]
    fn activation_failure_names_broker_env_hint() {
        for name in [
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.Spawn.ChildExited",
        ] {
            let reason = activation_failure_reason(&method_error(name));
            assert!(reason.contains(name), "{reason}");
            assert!(reason.contains(AT_SPI_BUS_ADDRESS_ENV), "{reason}");
            assert!(
                reason.contains("restart at-spi-dbus-bus.service"),
                "{reason}"
            );
        }

        let denied =
            activation_failure_reason(&method_error("org.freedesktop.DBus.Error.AccessDenied"));
        assert!(
            !denied.contains(AT_SPI_BUS_ADDRESS_ENV),
            "调用级失败不得归因到 broker 环境: {denied}"
        );
        assert!(denied.contains("AccessDenied"), "{denied}");
    }

    /// 现场等价的错误（fdo proxy 把错误回复映射为 [`zbus::fdo::Error`]；`Message`
    /// 仅作载体），错误名 → 变体的映射与生产中同源。
    fn method_error(name: &str) -> zbus::fdo::Error {
        let reply = zbus::Message::method_call(ROOT_PATH, "StartServiceByName")
            .and_then(|builder| builder.build(&()))
            .expect("valid method call message");
        zbus::fdo::Error::from(zbus::Error::MethodError(
            name.try_into().expect("valid D-Bus error name"),
            Some("detail".to_string()),
            reply,
        ))
    }

    /// 无 a11y bus 的环境（CI/容器）下探测返回 ⚠ 行而非 panic；可达时保留
    /// doctor/a11y.status 基准前缀。
    #[tokio::test]
    async fn atspi_line_never_panics_without_bus() {
        let line = atspi_line().await;
        assert!(line.starts_with('✓') || line.starts_with('⚠'), "{line}");
        if line.starts_with('✓') {
            assert!(
                line.starts_with("✓ AT-SPI         : enabled (a11y bus up, Registry reachable"),
                "{line}"
            );
        }
    }
}
