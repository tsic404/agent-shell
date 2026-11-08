// Dispatch/wiring harness: loads the *shipped* extension.js (import specifiers
// rewritten by the Rust test to the mocks in this directory) and drives it
// through the parts that cannot be judged from source text:
//
//   method_call 分派（授权三分支 + 成功载荷）：
//   1. owner 未填充（daemon 未运行、watch 也未回调）→ Unauthorized，立即回包；
//   2. owner 已填充但与调用方不一致            → Unauthorized；
//   3. enable 同步取到 owner，watch 尚未回调    → 首个调用即返回 ListWindows 载荷
//      （「启动窗口已消除」的判据：删掉 enable 里的同步查询即变红）；
//   4. daemon 在扩展之后启动（watch 填充）       → 后续调用返回载荷。
//
//   窗口接线与载荷字段（GNOME 51 兼容改动的判据）：
//   5. enable() 为既有窗口接线用的是 `unmanaged`（51 无 window-destroyed）；
//   6. window-created 到达时新窗口同样接 `unmanaged`；
//   7. 触发已接线窗口的 `unmanaged` → 对外发 WindowClosed，载荷带该窗口标识；
//   8. 窗口 is_maximized() 为 true 时载荷 maximized === true；
//   9. 窗口 is_maximized() 为 false 时载荷 maximized === false
//      （读 `mw.maximized` 得 undefined，8/9 都会红）；
//  10. 几何取 frame 四边实值（Meta.Rectangle 直接序列化只得 `{}`）；
//  11. 工作区为 0 基序号，sticky 窗口为 null（不伪造序号）；
//  12. 窗口类型映射为契约枚举名（Dialog）；
//  13. 置顶标志与 is_above() 一致。
//
//   CloseWindow 应答（关闭真正生效的判据）：
//  14. CloseWindow 把 shell 时间戳传给 mw.delete（缺参即抛，关闭不会发生）；
//  15. 非 GLib.Error 抛出时只回一次 D-Bus 错误（one-shot 下兜底重试会被静默丢弃）；
//  16. GLib.Error 抛出时走 return_gerror，不混入 dbus_error；
//  17. 回包自身抛错只记日志，不逃出 method_call。
//
// 退出码非 0 即测试失败；Rust 侧断言 stdout 含 "PASS 17 checks"。

import AgentShellExtension from './extension_under_test.mjs';
import GLib from './mock-glib.mjs';
import Meta from './mock-meta.mjs';
import {emittedSignals, session, setSyncOwner, watched} from './mock-gio.mjs';

const IFACE = 'org.gnome.Shell.AgentShell';
const PATH = '/org/gnome/Shell/AgentShell';
const UNAUTHORIZED = `${IFACE}.Error.Unauthorized`;

let failures = 0;
let handleSeq = 0;

function check(name, fn) {
    try {
        fn();
        console.log(`ok - ${name}`);
    } catch (e) {
        failures += 1;
        console.error(`not ok - ${name}: ${e && e.message}`);
    }
}

function assert(condition, message) {
    if (!condition)
        throw new Error(message);
}

/// Meta.Rectangle 替身：GJS 的 boxed struct 字段不在自有可枚举属性上，
/// `JSON.stringify(rect)` 得到 `{}`——这正是几何退化为 0×0 的成因，替身必须同形。
function boxedRect(x, y, width, height) {
    const rect = {};
    for (const [key, value] of Object.entries({x, y, width, height}))
        Object.defineProperty(rect, key, {value, enumerable: false});
    return rect;
}

/// MetaWindow 替身：connect 记录信号名，便于断言接线用的是哪个信号。
function makeWindow(id, {
    maximized = false,
    workspace = 3,
    windowType = Meta.WindowType.NORMAL,
    keepAbove = false,
    frameRect = {x: 8, y: 16, width: 644, height: 492},
    deleteThrows = null,
} = {}) {
    const signals = [];
    const deleteCalls = [];
    const window = {
        signals,
        deleteCalls,
        get_id: () => id,
        get_title: () => `window-${id}`,
        get_wm_class: () => 'harness-app',
        get_pid: () => 1000 + id,
        minimized: false,
        fullscreen: false,
        is_maximized: () => maximized,
        get_frame_rect: () => boxedRect(frameRect.x, frameRect.y,
            frameRect.width, frameRect.height),
        get_workspace: () => workspace === null ? null : {index: () => workspace},
        get_window_type: () => windowType,
        is_above: () => keepAbove,
        has_focus: () => id === 42,
        // Meta-51 的 delete 需 guint32 timestamp：缺参在 GIR 类型检查处抛，
        // 与真机「关闭不发生且 invocation 不应答」的触发条件同形。
        delete(timestamp) {
            deleteCalls.push(timestamp);
            if (timestamp === undefined)
                throw new TypeError('method Meta.Window.delete: At least 1 argument required');
            if (deleteThrows)
                throw deleteThrows;
        },
        connect(signal, callback) {
            const handle = ++handleSeq;
            signals.push({handle, signal, callback});
            return handle;
        },
        disconnect(handle) {
            const index = signals.findIndex(entry => entry.handle === handle);
            if (index >= 0)
                signals.splice(index, 1);
        },
    };
    return window;
}

/// 安装假 shell（窗口集合 + 可触发的 display 信号），返回 display 处理器表。
function installFakeShell(windows) {
    const displayHandlers = new Map();
    globalThis.global = {
        get_window_actors: () => windows.map(window => ({meta_window: window})),
        display: {
            connect(signal, callback) {
                displayHandlers.set(signal, callback);
                return ++handleSeq;
            },
            disconnect() {},
        },
        backend: {get_monitor_manager: () => ({get_monitors: () => []})},
        get_current_time: () => 0,
    };
    globalThis.logError = (error, message) => console.error(`logError: ${message}: ${error}`);
    return displayHandlers;
}

function boot(syncOwner) {
    setSyncOwner(syncOwner);
    session.reset();
    const extension = new AgentShellExtension({uuid: 'agent-shell-bridge@tsic.top'});
    extension.enable();
    return extension;
}

/// GJS 1.90 给 invocation 的 return_* 装了 one-shot 包装：首次调用先占位再执行，
/// 其后任何调用都是 no-op（只 logError，不抛）。mock 必须同形——否则「先试 A
/// 失败再试 B」的兜底会被误判成可用，而真机上第二次必定无声丢弃。
function invocation(overrides = {}) {
    const record = {};
    const settled = () => {
        if (record.settled) {
            record.duplicates = (record.duplicates ?? 0) + 1;
            return false;
        }
        record.settled = true;
        return true;
    };
    const target = {
        return_value: (variant) => {
            if (settled())
                record.value = variant;
        },
        return_gerror: (error) => {
            if (!settled())
                return;
            // 先占位后做 GI 类型检查：非 GLib.Error 在此抛出，调用方已无可回包。
            if (!(error instanceof GLib.Error))
                throw new TypeError('return_gerror expects a GLib.Error');
            record.error = error;
        },
        return_dbus_error: (name, message) => {
            if (settled())
                record.dbusError = {name, message};
        },
        ...overrides,
    };
    return {record, target};
}

function callMethod(sender, methodName, parameters, target) {
    const methodCall = session.methodCall(PATH);
    assert(typeof methodCall === 'function', 'method_call closure must be registered');
    methodCall(null, sender, PATH, IFACE, methodName, parameters, target);
}

function invokeMethod(sender, methodName, parameters = null, overrides = {}) {
    const {record, target} = invocation(overrides);
    callMethod(sender, methodName, parameters, target);
    return record;
}

function invoke(sender) {
    return invokeMethod(sender, 'ListWindows');
}

function payloadOf(record) {
    assert(record.value, `expected a reply, got ${JSON.stringify(record)}`);
    const [json] = record.value.deepUnpack();
    return JSON.parse(json);
}

function assertUnauthorized(record, why) {
    assert(record.dbusError && record.dbusError.name === UNAUTHORIZED,
        `${why}: expected ${UNAUTHORIZED}, got ${JSON.stringify(record)}`);
    assert(!record.value, `${why}: unauthorized call must not return data`);
}

function assertWindowList(record, why) {
    assert(!record.dbusError, `${why}: call must not be rejected: ${JSON.stringify(record)}`);
    const windows = payloadOf(record);
    assert(Array.isArray(windows), `${why}: ListWindows must return a JSON array`);
    assert(windows.length === 1, `${why}: expected one window, got ${windows.length}`);
    assert(windows[0].id === 42, `${why}: window id must round-trip`);
    assert(windows[0].title === 'window-42', `${why}: title must round-trip`);
    return windows[0];
}

// ── 1–4：method_call 分派（授权 + 载荷） ───────────────────────────────────

check('owner 未填充时首个调用被拒（不误放行）', () => {
    installFakeShell([makeWindow(42)]);
    boot(null); // GetNameOwner 抛 NameHasNoOwner，watch 也未回调
    assertUnauthorized(invoke(':1.7'), 'owner unknown');
});

check('owner 与调用方不一致时被拒', () => {
    installFakeShell([makeWindow(42)]);
    boot(':1.7');
    assertUnauthorized(invoke(':1.9'), 'foreign sender');
});

check('enable 同步取得 owner 时首个调用即返回载荷（无启动窗口）', () => {
    installFakeShell([makeWindow(42)]);
    boot(':1.7'); // watch 的 appeared 回调尚未触发
    assertWindowList(invoke(':1.7'), 'first call after enable');
});

check('daemon 后启动（watch 填充 owner）时后续调用返回载荷', () => {
    installFakeShell([makeWindow(42)]);
    boot(null);
    assertUnauthorized(invoke(':1.7'), 'before owner appears');
    watched.fireAppeared(':1.7');
    assertWindowList(invoke(':1.7'), 'after owner appears');
});

// ── 5–7：窗口关闭接线（GNOME 51 无 window-destroyed） ──────────────────────

check('enable() 为既有窗口接线用 unmanaged', () => {
    const existing = makeWindow(42);
    installFakeShell([existing]);
    boot(':1.7');
    const wired = existing.signals.map(entry => entry.signal);
    assert(wired.includes('unmanaged'),
        `existing window must be wired to unmanaged, got [${wired}]`);
    assert(!wired.includes('window-destroyed'),
        `existing window must not be wired to window-destroyed, got [${wired}]`);
});

check('window-created 到达时新窗口接线用 unmanaged', () => {
    const existing = makeWindow(42);
    const displayHandlers = installFakeShell([existing]);
    boot(':1.7');
    const created = makeWindow(43);
    const onWindowCreated = displayHandlers.get('window-created');
    assert(typeof onWindowCreated === 'function', 'display must be watched for window-created');
    onWindowCreated(null, created);
    const wired = created.signals.map(entry => entry.signal);
    assert(wired.includes('unmanaged'),
        `new window must be wired to unmanaged, got [${wired}]`);
});

check('已接线的 unmanaged 触发时对外发 WindowClosed 且带窗口标识', () => {
    const existing = makeWindow(42);
    installFakeShell([existing]);
    boot(':1.7');
    const wired = existing.signals.find(entry => entry.signal === 'unmanaged');
    assert(wired, 'existing window must be wired to unmanaged');
    wired.callback();
    const closed = emittedSignals().filter(signal => signal.name === 'WindowClosed');
    assert(closed.length === 1, `expected one WindowClosed, got ${closed.length}`);
    assert(closed[0].signature === '(u)', `expected (u) signature, got ${closed[0].signature}`);
    assert(closed[0].value[0] === 42, `expected window id 42, got ${closed[0].value[0]}`);
});

// ── 8–9：maximized 真值（GNOME 51 无 maximized 属性） ──────────────────────

for (const maximized of [true, false]) {
    check(`is_maximized()=${maximized} 时载荷 maximized 与之一致`, () => {
        installFakeShell([makeWindow(42, {maximized})]);
        boot(':1.7');
        const window = assertWindowList(invoke(':1.7'), `maximized=${maximized}`);
        assert(window.maximized === maximized,
            `expected maximized=${maximized}, got ${JSON.stringify(window.maximized)}`);
    });
}

// ── 10–13：投影字段（几何/工作区/类型/置顶） ───────────────────────────────
//
// 10 的替身用 non-enumerable 字段模拟 GJS boxed struct：直接序列化
// `Meta.Rectangle` 得 `{}`（几何 0×0 的真实成因），显式取四边才通过。

check('载荷几何取实值（frame 四边，不是空对象）', () => {
    installFakeShell([makeWindow(42)]);
    boot(':1.7');
    const window = assertWindowList(invoke(':1.7'), 'geometry');
    assert(JSON.stringify(window.geometry) ===
        JSON.stringify({x: 8, y: 16, width: 644, height: 492}),
    `expected frame rect, got ${JSON.stringify(window.geometry)}`);
});

check('载荷工作区为 0 基序号；sticky 窗口为 null', () => {
    installFakeShell([makeWindow(42, {workspace: 3})]);
    boot(':1.7');
    assert(assertWindowList(invoke(':1.7'), 'workspace').workspace === 3,
        'workspace index must round-trip');

    installFakeShell([makeWindow(42, {workspace: null})]);
    boot(':1.7');
    assert(assertWindowList(invoke(':1.7'), 'sticky').workspace === null,
        'window without a workspace must report null, not a sentinel index');
});

check('载荷窗口类型为首字母大写枚举名', () => {
    installFakeShell([makeWindow(42, {windowType: Meta.WindowType.DIALOG})]);
    boot(':1.7');
    assert(assertWindowList(invoke(':1.7'), 'windowType').windowType === 'Dialog',
        'window type must map to the contract enum name');
});

check('载荷置顶标志与 is_above() 一致', () => {
    for (const keepAbove of [true, false]) {
        installFakeShell([makeWindow(42, {keepAbove})]);
        boot(':1.7');
        assert(assertWindowList(invoke(':1.7'), `keepAbove=${keepAbove}`).keepAbove === keepAbove,
            `expected keepAbove=${keepAbove}`);
    }
});

// ── 14–17：CloseWindow 应答（关闭真正生效的判据） ─────────────────────────

check('CloseWindow 把 shell 时间戳传给 mw.delete', () => {
    const window = makeWindow(42);
    installFakeShell([window]);
    boot(':1.7');
    const record = invokeMethod(':1.7', 'CloseWindow', new GLib.Variant('(u)', [42]));
    assert(!record.error && !record.dbusError,
        `close must not be rejected: ${JSON.stringify(record)}`);
    assert(window.deleteCalls.length === 1,
        `delete must be called exactly once, got ${window.deleteCalls.length}`);
    assert(window.deleteCalls[0] === 0,
        `delete must receive global.get_current_time() (0 in the harness), ` +
        `got ${window.deleteCalls[0]}`);
});

check('非 GLib.Error 抛出时只回一次 D-Bus 错误（无兜底重试）', () => {
    const window = makeWindow(42, {deleteThrows: new TypeError('delete refused')});
    installFakeShell([window]);
    boot(':1.7');
    const record = invokeMethod(':1.7', 'CloseWindow', new GLib.Variant('(u)', [42]));
    assert(record.dbusError &&
        record.dbusError.name === 'org.freedesktop.DBus.Error.Failed',
    `expected a D-Bus error reply, got ${JSON.stringify(record)}`);
    assert(!record.value, 'a failed handler must not reply with a value');
    // invocation 是 one-shot：先试 return_gerror 再兜底在真机上只会被静默丢弃，
    // 判据必须钉住「只尝试过一次回包」。
    assert(record.duplicates === undefined,
        `exactly one reply may be attempted, got ${record.duplicates} duplicate(s)`);
});

check('GLib.Error 抛出时走 return_gerror（不混入 dbus_error）', () => {
    // 目标窗口不在列表里 → `_withWindow` 抛 GLib.Error（真机同形路径）。
    installFakeShell([makeWindow(43)]);
    boot(':1.7');
    const record = invokeMethod(':1.7', 'CloseWindow', new GLib.Variant('(u)', [42]));
    assert(record.error instanceof GLib.Error,
        `expected the return_gerror path, got ${JSON.stringify(record)}`);
    assert(!record.dbusError,
        'a GLib.Error must not be re-routed to return_dbus_error');
    assert(record.duplicates === undefined,
        `exactly one reply may be attempted, got ${record.duplicates} duplicate(s)`);
});

check('错误回包自身抛错时只记日志、不逃出 method_call', () => {
    const window = makeWindow(42, {deleteThrows: new TypeError('delete refused')});
    installFakeShell([window]);
    boot(':1.7');
    const replyThrows = () => {
        throw new TypeError('invalid error name');
    };
    // 回包抛错若逃出 method_call，GJS 只会在主循环记一条异常，调用方无应答。
    const record = invokeMethod(':1.7', 'CloseWindow', new GLib.Variant('(u)', [42]), {
        return_dbus_error: replyThrows,
    });
    assert(!record.value && !record.dbusError && !record.error,
        `a throwing reply must not be retried: ${JSON.stringify(record)}`);
});

if (failures > 0) {
    console.error(`${failures} check(s) failed`);
    process.exit(1);
}
console.log('PASS 17 checks');
process.exit(0);
