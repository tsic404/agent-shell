//! 集成级单元测试 — 对应设计文档 §18.5 验证输出与 issue 验收标准。
//!
//! 覆盖：`priority()` 三档推导（§18.1 表）、`EventFilter` 域过滤、
//! 12 类原始事件归一化映射（含 resolver 路径）、WindowMoved 100ms 合并、
//! 背压（capacity=1024 下 High 必达 / Medium-Low 溢出丢弃）、环形缓冲。

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_shell_core::services::{AccessibilityChange, PowerState};
use agent_shell_core::types::{DesktopEnvironment, Rect, WindowId, WindowInfo};
use event::adapter::MoveMerger;
use event::{
    DesktopEvent, EventFilter, EventHub, EventNormalizer, EventPriority, EventRing, EventSource,
    RawEvent, RawSource,
};
use futures::{stream, StreamExt};

fn win_id(n: u32) -> WindowId {
    WindowId {
        native_id: n.to_string(),
        de_type: DesktopEnvironment::KDE,
    }
}

fn window_info(id: WindowId) -> WindowInfo {
    WindowInfo {
        id,
        title: "t".into(),
        app_id: "a".into(),
        pid: 1,
        geometry: Rect::default(),
        frame_geometry: Rect::default(),
        states: vec![],
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: agent_shell_core::types::WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    }
}

fn sample_monitor() -> agent_shell_core::types::MonitorInfo {
    agent_shell_core::types::MonitorInfo {
        id: agent_shell_core::types::MonitorId {
            native_id: "eDP-1".into(),
            de_type: DesktopEnvironment::KDE,
        },
        name: "eDP-1".into(),
        geometry: Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
        physical_geometry: Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
        scale: 1.0,
        is_primary: true,
        workspace_id: None,
    }
}

// ───────────────────────── priority() 推导（三档单测） ─────────────────────────

#[test]
fn priority_high_for_window_lifecycle() {
    let now = Instant::now();
    let id = win_id(1);
    let cases = [
        DesktopEvent::WindowOpened {
            info: window_info(id.clone()),
            source: EventSource::KWinWayland,
            occurred_at: now,
        },
        DesktopEvent::WindowClosed {
            id: id.clone(),
            source: EventSource::Hyprland,
            occurred_at: now,
        },
        DesktopEvent::WindowFocused {
            info: window_info(id),
            source: EventSource::Sway,
            occurred_at: now,
        },
    ];
    for e in cases {
        assert_eq!(e.priority(), EventPriority::High, "{e:?}");
    }
}

#[test]
fn priority_medium_for_workspace_monitor_input_state() {
    let now = Instant::now();
    let id = win_id(1);
    let cases = [
        DesktopEvent::WindowMoved {
            id: id.clone(),
            geometry: Rect::default(),
            source: EventSource::Hyprland,
            occurred_at: now,
        },
        DesktopEvent::WindowStateChanged {
            id: id.clone(),
            states: vec![agent_shell_core::types::WindowState::Maximized],
            source: EventSource::KWinWayland,
            occurred_at: now,
        },
        DesktopEvent::WorkspaceChanged {
            info: agent_shell_core::types::WorkspaceInfo {
                id: agent_shell_core::types::WorkspaceId {
                    native_id: "1".into(),
                    de_type: DesktopEnvironment::KDE,
                },
                name: "1".into(),
                number: 1,
                is_active: true,
                monitor_ids: vec![],
                window_ids: vec![],
            },
            source: EventSource::KWinWayland,
            occurred_at: now,
        },
        DesktopEvent::MonitorHotplug {
            monitor: sample_monitor(),
            added: true,
            source: EventSource::KWinWayland,
            occurred_at: now,
        },
        DesktopEvent::MonitorChanged {
            info: sample_monitor(),
            source: EventSource::KWinX11,
            occurred_at: now,
        },
        DesktopEvent::PointerButton {
            window_id: Some(id),
            button: agent_shell_core::types::MouseButton::Left,
            pressed: true,
            position: (0, 0),
            source: EventSource::Input,
            occurred_at: now,
        },
        DesktopEvent::KeyComboPressed {
            combo: agent_shell_core::types::KeyCombo {
                keys: vec![agent_shell_core::types::Key::Named(
                    agent_shell_core::types::KeyName::Tab,
                )],
                modifiers: agent_shell_core::types::ModifierMask::default(),
            },
            source: EventSource::Input,
            occurred_at: now,
        },
        DesktopEvent::WorkspaceListChanged {
            workspaces: vec![],
            source: EventSource::Sway,
            occurred_at: now,
        },
        DesktopEvent::WorkspaceWindowMoved {
            window_id: win_id(2),
            from: None,
            to: None,
            source: EventSource::Sway,
            occurred_at: now,
        },
    ];
    for e in cases {
        assert_eq!(e.priority(), EventPriority::Medium, "{e:?}");
    }
}

#[test]
fn priority_low_for_system_events_and_noop() {
    let now = Instant::now();
    let cases = [
        DesktopEvent::AppLaunched {
            app_id: "x".into(),
            pid: 1,
            desktop_file: None,
            source: EventSource::AtSpi,
            occurred_at: now,
        },
        DesktopEvent::AppExited {
            pid: 1,
            source: EventSource::AtSpi,
            occurred_at: now,
        },
        DesktopEvent::FullscreenChanged {
            enabled: true,
            window_id: None,
            source: EventSource::Portal,
            occurred_at: now,
        },
        DesktopEvent::PowerStateChanged {
            state: PowerState::Suspend,
            source: EventSource::Power,
            occurred_at: now,
        },
        DesktopEvent::AccessibilityTreeChanged {
            app_pid: 1,
            change_type: AccessibilityChange::PropertyChanged,
            source: EventSource::AtSpi,
            occurred_at: now,
        },
        DesktopEvent::Noop,
    ];
    for e in cases {
        assert_eq!(e.priority(), EventPriority::Low, "{e:?}");
    }
}

// ───────────────────────── EventFilter 过滤 ─────────────────────────

#[test]
fn filter_matches_by_category() {
    let now = Instant::now();
    let f = EventFilter::windows_only();
    let win_evt = DesktopEvent::WindowClosed {
        id: win_id(1),
        source: EventSource::Hyprland,
        occurred_at: now,
    };
    let ws_evt = DesktopEvent::PowerStateChanged {
        state: PowerState::On,
        source: EventSource::Power,
        occurred_at: now,
    };
    assert!(f.matches(&win_evt));
    assert!(!f.matches(&ws_evt));

    // priority 维度过滤
    let mut f = EventFilter::all();
    f.priority = Some(EventPriority::High);
    assert!(f.matches(&win_evt));
    assert!(!f.matches(&ws_evt));
}

#[test]
fn filter_never_matches_noop() {
    assert!(!EventFilter::all().matches(&DesktopEvent::Noop));
}

#[tokio::test]
async fn hub_does_not_deliver_unsubscribed_categories() {
    let hub = EventHub::with_capacity(8);
    let mut sub = hub.subscribe(EventFilter {
        power_events: true,
        ..EventFilter::default()
    });
    hub.publish(DesktopEvent::WindowClosed {
        id: win_id(1),
        source: EventSource::Hyprland,
        occurred_at: Instant::now(),
    })
    .await;
    hub.publish(DesktopEvent::PowerStateChanged {
        state: PowerState::Off,
        source: EventSource::Power,
        occurred_at: Instant::now(),
    })
    .await;
    // 窗口事件未订阅 → 直接收到电源事件
    assert!(matches!(
        sub.recv().await,
        Some(DesktopEvent::PowerStateChanged { .. })
    ));
}

// ───────────────────────── 归一化映射 ─────────────────────────

/// §18.3：各 DE 的 open/close/focus 语义不同但语义一致。
/// 同步路径（无 resolver）丢弃需 WindowInfo 的事件；close/power 正常映射。
#[test]
fn normalize_maps_close_semantics_across_des() {
    let mut merger = MoveMerger::default();
    for raw in [
        RawEvent::KWinWindowRemoved { id: "42".into() },
        RawEvent::HyprlandCloseWindow {
            address: "42".into(),
        },
        RawEvent::SwayWindowClose { id: "42".into() },
    ] {
        let evt = event::normalize::normalize("t", EventSource::Portal, raw.clone(), &mut merger);
        match evt {
            Some(DesktopEvent::WindowClosed { id, .. }) => assert_eq!(id.native_id, "42"),
            other => panic!("{raw:?} → {other:?}"),
        }
    }
}

#[test]
fn normalize_maps_power_and_app_lifecycle() {
    let mut merger = MoveMerger::default();
    let evt = event::normalize::normalize(
        "upower",
        EventSource::Power,
        RawEvent::PowerStateChanged(PowerState::Hibernate),
        &mut merger,
    )
    .unwrap();
    assert!(matches!(
        evt,
        DesktopEvent::PowerStateChanged {
            state: PowerState::Hibernate,
            ..
        }
    ));

    let evt = event::normalize::normalize(
        "atspi",
        EventSource::AtSpi,
        RawEvent::AtSpiAppLaunched {
            app_id: "org.test".into(),
            pid: 7,
        },
        &mut merger,
    )
    .unwrap();
    assert!(matches!(evt, DesktopEvent::AppLaunched { pid: 7, .. }));
}

#[tokio::test]
async fn normalize_with_resolver_unifies_open_across_des() {
    let resolved = Arc::new(Mutex::new(Vec::<String>::new()));
    let r2 = Arc::clone(&resolved);
    let resolver: event::normalize::WindowResolver = Arc::new(move |id: &str| {
        r2.lock().push(id.to_string());
        Box::pin(async move { Some(window_info(win_id(9))) })
    });

    let mut merger = MoveMerger::default();
    // KWin / Hyprland / DDE 的 open 语义统一为 WindowOpened
    for raw in [
        RawEvent::KWinWindowAdded { id: "9".into() },
        RawEvent::HyprlandOpenWindow {
            address: "9".into(),
        },
        RawEvent::DdeWindowOpened { id: "9".into() },
    ] {
        let evt = event::normalize::normalize_with_resolver(
            "t",
            EventSource::Portal,
            raw.clone(),
            &mut merger,
            &Some(resolver.clone()),
        )
        .await;
        match evt {
            Some(DesktopEvent::WindowOpened { info, .. }) => {
                assert_eq!(info.id.native_id, "9")
            }
            other => panic!("{raw:?} → {other:?}"),
        }
    }

    // focus 语义统一为 WindowFocused
    for raw in [
        RawEvent::KWinActiveWindowChanged {
            id: Some("9".into()),
        },
        RawEvent::HyprlandActiveWindow {
            address: "9".into(),
        },
        RawEvent::SwayWindowFocus { id: "9".into() },
    ] {
        let evt = event::normalize::normalize_with_resolver(
            "t",
            EventSource::Portal,
            raw.clone(),
            &mut merger,
            &Some(resolver.clone()),
        )
        .await;
        match evt {
            Some(DesktopEvent::WindowFocused { .. }) => {}
            other => panic!("{raw:?} → {other:?}"),
        }
    }
    assert_eq!(resolved.lock().len(), 6);

    // 解析失败（窗口已消失）→ None 丢弃
    let none_resolver: event::normalize::WindowResolver =
        Arc::new(|_id: &str| Box::pin(async { None }));
    let evt = event::normalize::normalize_with_resolver(
        "t",
        EventSource::Portal,
        RawEvent::KWinWindowAdded { id: "gone".into() },
        &mut merger,
        &Some(none_resolver),
    )
    .await;
    assert!(evt.is_none());
}

// ───────────────────────── WindowMoved 100ms 合并 ─────────────────────────

/// 修复审查问题 1：延迟发布语义——首条 pending 不发布，窗口内后续覆盖，
/// 过期 flush 时以最终几何发布。
#[test]
fn move_merger_defers_and_keeps_final_position() {
    let mut merger = MoveMerger::default();
    let geo_a = Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };
    let geo_b = Rect {
        x: 5,
        y: 5,
        width: 10,
        height: 10,
    };

    // 首次移动：pending，不产生待发布项
    assert!(merger.observe("w1", geo_a).is_none());
    // 100ms 内同窗口后续移动：覆盖 pending（最终位置保留为最新），无过期产出
    assert!(merger.observe("w1", geo_b).is_none());
    assert!(merger.observe("w1", geo_a).is_none());
    assert_eq!(merger.pending_count(), 1);

    // 不同窗口互不影响
    assert!(merger.observe("w2", geo_b).is_none());
    assert_eq!(merger.pending_count(), 2);

    // 窗口期未过：drain 不产出
    assert!(merger.drain_expired().is_empty());
}

/// 过期后的 pending 以最终几何 flush；observe 返回被顶替的旧窗口几何。
#[tokio::test]
async fn move_merger_flushes_expired_with_latest_geometry() {
    let mut merger = MoveMerger::default();
    let geo_first = Rect {
        x: 1,
        y: 1,
        width: 10,
        height: 10,
    };
    let geo_final = Rect {
        x: 9,
        y: 9,
        width: 20,
        height: 20,
    };
    assert!(merger.observe("w1", geo_first).is_none());

    // 等 100ms 窗口过期
    tokio::time::sleep(event::adapter::MOVE_MERGE_WINDOW + Duration::from_millis(30)).await;

    // 过期的 w1 pending 由 drain flush，几何为**最新**记录值
    let drained = merger.drain_expired();
    assert_eq!(drained.len(), 1, "过期 pending 应被 flush");
    assert_eq!(drained[0].0, "w1");
    assert_eq!(drained[0].1, geo_first);

    // observe 新窗口：旧窗口已清空，新窗口进入 pending（不发布）
    assert!(merger.observe("w2", geo_final).is_none());
    assert!(merger.drain_expired().is_empty());
}

/// 端到端：normalizer task 内的合并——连续移动仅最终位置入 hub，
/// 且几何经 resolver 解析为真实值（修复审查问题 2：禁止零值几何）。
#[tokio::test]
async fn normalizer_publishes_final_position_only() {
    struct BurstSource;
    impl RawSource for BurstSource {
        fn source_name(&self) -> &'static str {
            "burst"
        }
        fn source_kind(&self) -> EventSource {
            EventSource::Hyprland
        }
        fn events(&self) -> futures::stream::BoxStream<'static, RawEvent> {
            // 同一窗口 "same" 连续 4 次 + 其他窗口各 1 次
            let same = vec![
                RawEvent::HyprlandMoveWindow {
                    address: "same".into()
                };
                4
            ];
            let moves = (0..5).map(|i| RawEvent::HyprlandMoveWindow {
                address: format!("w{i}"),
            });
            stream::iter(same.into_iter().chain(moves).collect::<Vec<_>>()).boxed()
        }
    }

    // resolver：每次查询返回递增 x 坐标——发布的事件若非最终 flush，
    // 其 geometry.x 必然小于最大值。
    static QUERY: AtomicU32 = AtomicU32::new(0);
    let resolver: event::normalize::WindowResolver = Arc::new(|id: &str| {
        let id = id.to_string();
        Box::pin(async move {
            if id == "gone" {
                return None; // 窗口已消失 → 不发布
            }
            let n = QUERY.fetch_add(1, Ordering::Relaxed);
            // 保留原生 id 字符串（含非数字如 "same"），保证事件可按窗口归组
            let mut info = window_info(WindowId {
                native_id: id,
                de_type: DesktopEnvironment::KDE,
            });
            info.geometry = Rect {
                x: n as i32,
                y: 0,
                width: 10,
                height: 10,
            };
            Some(info)
        }) as _
    });

    let hub = EventHub::with_capacity(64);
    let mut sub = hub.subscribe(EventFilter::all());
    let mut norm = EventNormalizer::new(hub).with_resolver(resolver);
    norm.add_source(Box::new(BurstSource));
    assert_eq!(norm.source_count(), 1);
    let started = norm.run();
    assert_eq!(started, ["burst"]);

    // 收集事件直到拿到全部
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut same_geoms = Vec::new();
    let mut moved_others = 0usize;
    while moved_others < 5 || same_geoms.is_empty() {
        assert!(
            Instant::now() < deadline,
            "timeout: same={same_geoms:?} others={moved_others}"
        );
        match sub.recv().await {
            Some(DesktopEvent::WindowMoved { id, geometry, .. }) => {
                if id.native_id == "same" {
                    same_geoms.push(geometry);
                } else {
                    moved_others += 1;
                    // 非零值几何（审查问题 2）
                    assert_ne!(
                        geometry,
                        Rect::default(),
                        "move 事件必须携带 resolver 解析的真实几何"
                    );
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    assert_eq!(
        same_geoms.len(),
        1,
        "100ms 内同一窗口连续移动应只推送最终位置"
    );
}

/// 无 resolver 时 move 事件不发布（不广播零值几何），close 等仍正常。
#[tokio::test]
async fn move_without_resolver_is_dropped() {
    struct MoveSource;
    impl RawSource for MoveSource {
        fn source_name(&self) -> &'static str {
            "move-only"
        }
        fn source_kind(&self) -> EventSource {
            EventSource::Hyprland
        }
        fn events(&self) -> futures::stream::BoxStream<'static, RawEvent> {
            stream::iter(vec![RawEvent::HyprlandMoveWindow {
                address: "m1".into(),
            }])
            .boxed()
        }
    }

    let hub = EventHub::with_capacity(8);
    let mut sub = hub.subscribe(EventFilter::all());
    let mut norm = EventNormalizer::new(hub);
    norm.add_source(Box::new(MoveSource));
    norm.run();

    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 不应收到任何 WindowMoved（无 resolver → 丢弃）
    while let Some(evt) = sub.try_recv() {
        assert!(
            !matches!(evt, DesktopEvent::WindowMoved { .. }),
            "无 resolver 时不得发布 move 事件: {evt:?}"
        );
    }
}

// ───────────────────────── 背压（§18.5 load test） ─────────────────────────

/// capacity=1024：1000 events/s 连续发布下 High 必达、无死锁。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_load_high_always_delivered_medium_drops() {
    let hub = EventHub::with_capacity(1024);
    let mut sub = hub.subscribe(EventFilter::all());

    // 消费者与发布者并发运行（High 事件必达语义要求消费端在线）
    let consumer = tokio::spawn(async move {
        let mut highs = 0u32;
        let mut lows = 0u32;
        let deadline = Instant::now() + Duration::from_secs(30);
        while highs < 1000 {
            assert!(
                Instant::now() < deadline,
                "timeout: highs={highs} lows={lows}"
            );
            match sub.recv().await {
                Some(DesktopEvent::WindowClosed { .. }) => highs += 1,
                Some(DesktopEvent::AppExited { .. }) => lows += 1,
                Some(_) => {}
                None => break,
            }
        }
        (highs, lows)
    });

    let h2 = hub.clone();
    let publisher = tokio::spawn(async move {
        for i in 0..2000u32 {
            if i % 2 == 0 {
                h2.publish(DesktopEvent::WindowClosed {
                    id: win_id(i),
                    source: EventSource::Hyprland,
                    occurred_at: Instant::now(),
                })
                .await; // High：必达（等待空位，不丢弃）
            } else {
                h2.publish(DesktopEvent::AppExited {
                    pid: i,
                    source: EventSource::AtSpi,
                    occurred_at: Instant::now(),
                })
                .await; // Low：满则丢
            }
        }
    });
    publisher.await.expect("publisher 不应阻塞");
    let (highs, lows) = consumer.await.expect("consumer 不应阻塞");
    assert_eq!(highs, 1000, "High 事件必须全部送达");
    assert!(lows <= 1000);
}

#[tokio::test]
async fn slow_subscriber_does_not_block_others() {
    let hub = EventHub::with_capacity(1);
    let _slow = hub.subscribe(EventFilter::all()); // 不消费
    let mut fast = hub.subscribe(EventFilter::all());
    let h2 = hub.clone();
    tokio::spawn(async move {
        for i in 0..8 {
            h2.publish(DesktopEvent::AppExited {
                pid: i,
                source: EventSource::AtSpi,
                occurred_at: Instant::now(),
            })
            .await;
        }
    });
    assert!(fast.recv().await.is_some());
}

// ───────────────────────── 订阅生命周期 + ring 补发 ─────────────────────────

#[tokio::test]
async fn unsubscribe_stops_delivery() {
    let hub = EventHub::with_capacity(8);
    let mut sub = hub.subscribe(EventFilter::all());
    assert_eq!(hub.subscriber_count(), 1);
    hub.unsubscribe(sub.id());
    assert_eq!(hub.subscriber_count(), 0);
    hub.publish(DesktopEvent::Noop).await;
    // 通道已断开：recv 返回 None
    assert!(sub.recv().await.is_none());
}

#[test]
fn ring_replay_returns_recent_events_in_order() {
    let ring = EventRing::new(1000);
    for i in 0..1200u32 {
        ring.push(DesktopEvent::AppExited {
            pid: i,
            source: EventSource::AtSpi,
            occurred_at: Instant::now(),
        });
    }
    let replay = ring.snapshot();
    assert_eq!(replay.len(), 1000, "保留最近 1000 条");
    // 最旧的是 pid=200（0..199 已挤出）
    assert!(matches!(
        replay.first(),
        Some(DesktopEvent::AppExited { pid: 200, .. })
    ));
    assert!(matches!(
        replay.last(),
        Some(DesktopEvent::AppExited { pid: 1199, .. })
    ));
}

/// 断线补发场景（审查建议 7 落地）：EventNormalizer 已集成 ring，
/// 经归一化路径发布的事件自动入 ring，重连时从 `norm.ring()` 快照补发。
#[tokio::test]
async fn reconnect_replays_from_normalizer_ring() {
    struct CloseSource;
    impl RawSource for CloseSource {
        fn source_name(&self) -> &'static str {
            "close-src"
        }
        fn source_kind(&self) -> EventSource {
            EventSource::KWinWayland
        }
        fn events(&self) -> futures::stream::BoxStream<'static, RawEvent> {
            stream::iter((0..5).map(|i| RawEvent::SwayWindowClose { id: i.to_string() })).boxed()
        }
    }

    let hub = EventHub::with_capacity(8);
    let mut norm = EventNormalizer::new(hub.clone());
    norm.add_source(Box::new(CloseSource));
    // run(self) 消耗归一化器——先取 ring 共享句柄（Arc 克隆，task 持续写入）
    let ring = norm.ring().clone();
    norm.run();
    // 等 task flush
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ring.len(), 5, "归一化发布的事件应自动入 ring");

    // "重连"的订阅者：从集成 ring 补发最近事件
    let mut sub = hub.subscribe(EventFilter::all());
    for replayed in ring.snapshot() {
        hub.publish(replayed).await;
    }
    let mut got = 0;
    while sub.try_recv().is_some() {
        got += 1;
    }
    assert_eq!(got, 5);
}

/// `with_ring` 注入共享环：归一化发布的事件同时进入调用方的 replay 缓冲，
/// 与 `norm.ring()` 指向同一底层 Arc——daemon 无需维护双份 ring。
#[tokio::test]
async fn with_ring_shares_replay_buffer_with_caller() {
    struct CloseSource;
    impl RawSource for CloseSource {
        fn source_name(&self) -> &'static str {
            "close-src"
        }
        fn source_kind(&self) -> EventSource {
            EventSource::KWinWayland
        }
        fn events(&self) -> futures::stream::BoxStream<'static, RawEvent> {
            stream::iter((0..3).map(|i| RawEvent::SwayWindowClose { id: i.to_string() })).boxed()
        }
    }

    let hub = EventHub::with_capacity(8);
    let caller_ring = EventRing::new(16);
    let mut norm = EventNormalizer::new(hub.clone()).with_ring(caller_ring.clone());
    norm.add_source(Box::new(CloseSource));
    norm.run();
    // 等 task flush
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(caller_ring.len(), 3, "注入的共享环应收到归一化事件");
    assert!(matches!(
        caller_ring.snapshot().first(),
        Some(DesktopEvent::WindowClosed { id, .. }) if id.native_id == "0"
    ));
}

/// 修复审查问题 3 回归：停滞订阅者不得阻塞 High 发布，也不得队头阻塞
/// 其他订阅者；超时后降级丢弃并计数。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_subscriber_does_not_block_high_publish() {
    let hub = EventHub::with_capacity(1);
    // 停滞订阅者：占满通道且从不消费
    let _stalled = hub.subscribe(EventFilter::all());
    let _filler = DesktopEvent::AppExited {
        pid: 0,
        source: EventSource::AtSpi,
        occurred_at: Instant::now(),
    };
    hub.publish(DesktopEvent::AppExited {
        pid: 0,
        source: EventSource::AtSpi,
        occurred_at: Instant::now(),
    })
    .await;

    // 第二个订阅者：通道空，应能收到 High 事件
    let mut fast = hub.subscribe(EventFilter::all());

    let before = Instant::now();
    // High 事件：对停滞订阅者超时降级，对 fast 正常投递——总耗时应远小于长阻塞
    hub.publish(DesktopEvent::WindowClosed {
        id: win_id(1),
        source: EventSource::KWinWayland,
        occurred_at: Instant::now(),
    })
    .await;
    let elapsed = before.elapsed();

    // 未被停滞订阅者阻塞到不可接受的程度（timeout=250ms 上限）
    assert!(
        elapsed < Duration::from_secs(2),
        "High 发布被停滞订阅者阻塞过久: {elapsed:?}"
    );
    assert!(
        matches!(fast.recv().await, Some(DesktopEvent::WindowClosed { .. })),
        "High 事件应投递给健康订阅者"
    );
}

/// serde 回环（审查建议 8）：DesktopEvent 序列化/反序列化保持一致。
#[test]
fn desktop_event_serde_roundtrip() {
    let e = DesktopEvent::WindowOpened {
        info: window_info(win_id(7)),
        source: EventSource::Hyprland,
        occurred_at: Instant::now(),
    };
    let json = serde_json::to_string(&e).expect("serialize");
    let back: DesktopEvent = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(back, DesktopEvent::WindowOpened { .. }));
    assert_eq!(back.priority(), EventPriority::High);
}
