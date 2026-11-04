// agent-shell-bridge@tsic.top — org.gnome.Shell.AgentShell
// 由 agent-shell-compositor-mutter 内嵌交付；安装到 extensions 目录后启用。
//
// GNOME 45+ ESM 入口：默认导出 Extension 子类（enable/disable 生命周期）。
// 窗口语义接口 org.gnome.Shell.AgentShell 经 Gio.DBus 注册到 session bus，
// 方法调用在 method_call 层先校验 sender 再分派（详见 §8.1 安全说明）。
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.gnome.Shell.AgentShell';
const OBJECT_PATH = '/org/gnome/Shell/AgentShell';
// daemon 在 session bus 上申请的 well-known 名称——method_call 据此校验
// sender 确为 agent-shell-daemon（须与 Rust 侧 DAEMON_BUS_NAME 一致）。
const DAEMON_BUS_NAME = 'org.agentshell.Daemon';

const IFACE_NAME = 'org.gnome.Shell.AgentShell';

const DBusInterface = `
<node>
  <interface name="org.gnome.Shell.AgentShell">
    <method name="ListWindows"><arg type="s" direction="out"/></method>
    <method name="GetActiveWindow"><arg type="s" direction="out"/></method>
    <method name="FocusWindow"><arg type="u" name="window_id" direction="in"/></method>
    <method name="MoveWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="CloseWindow"><arg type="u" name="window_id" direction="in"/></method>
    <method name="MinimizeWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="b" name="minimize" direction="in"/>
    </method>
    <method name="MaximizeWindow">
      <arg type="u" name="window_id" direction="in"/>
      <arg type="b" name="maximize" direction="in"/>
    </method>
    <method name="GetMonitorLayout"><arg type="s" direction="out"/></method>
    <signal name="WindowOpened"><arg type="s" name="info"/></signal>
    <signal name="WindowClosed"><arg type="u" name="window_id"/></signal>
    <signal name="ActiveWindowChanged"><arg type="s" name="info"/></signal>
  </interface>
</node>`;

// 返回字符串（JSON）的方法集合；其余方法无返回值（void）。
const STRING_RETURN_METHODS = new Set(['ListWindows', 'GetActiveWindow', 'GetMonitorLayout']);

class AgentShellBridge {
    constructor() {
        this._impl = null;
        this._nodeInfo = null;
        this._ifaceInfo = null;
        this._regId = 0;        // register_object 句柄（对象路径）
        this._nameOwnerId = 0;  // bus_own_name 句柄（well-known 名称）
        this._signalIds = [];
        this._watchers = new Map();
        this._closedWatch = new Map();  // MetaWindow → unmanaged 句柄
        this._daemonWatcherId = 0;      // DAEMON_BUS_NAME 的 watch 句柄
        this._daemonOwner = null;       // 当前持有者唯一名（sender 比对基准）
    }

    enable() {
        this._nodeInfo = Gio.DBusNodeInfo.new_for_xml(DBusInterface);
        // interfaces 是数组：按 name 定位（字符串索引得 undefined，注册必失败）。
        this._ifaceInfo = this._nodeInfo.interfaces.find(i => i.name === IFACE_NAME);

        // 方法名 → 函数；返回值由 method_call 按签名打包成 D-Bus out 参数。
        this._impl = {
            ListWindows: () => this._listWindows(),
            GetActiveWindow: () => this._getActiveWindow(),
            FocusWindow: id => this._withWindow(id, mw => mw.activate(global.get_current_time())),
            MoveWindow: (id, x, y) => this._withWindow(id, mw => mw.move_frame(true, x, y)),
            CloseWindow: id => this._withWindow(id, mw => mw.delete()),
            MinimizeWindow: (id, minimize) =>
                this._withWindow(id, mw => { mw.minimized = !!minimize; }),
            MaximizeWindow: (id, maximize) => this._withWindow(id, mw =>
                maximize ? mw.maximize(Meta.MaximizeFlags.BOTH)
                         : mw.unmaximize(Meta.MaximizeFlags.BOTH)),
            GetMonitorLayout: () => this._monitorLayout(),
        };

        // method_call 闭包：先校验 sender 再按方法名分派。
        // 不用 Gio.DBusExportedObject.wrapJSObject——其 method 不暴露 sender，
        // 无法做调用方校验（安全要求）。GNOME 51（GJS 1.90）起
        // `Gio.DBusInterfaceVTable` 不能直接构造，只能用闭包形态注册。
        this._regId = Gio.DBus.session.register_object(
            OBJECT_PATH, this._ifaceInfo,
            (connection, sender, objectPath, ifaceName, methodName, parameters, invocation) => {
                this._onMethodCall(sender, methodName, parameters, invocation);
            },
            null, null);

        this._nameOwnerId = Gio.bus_own_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameOwnerFlags.NONE, null, null, null);

        // daemon 的 well-known 名持有者：
        //  · enable 阶段同步取一次——此时不在方法分派内，不会与正在分派的消息互等；
        //    少了这一步，watcher 回调落地前到达的首个调用会被判 Unauthorized，
        //    daemon 首次 windows.list 就是 1002（AC-110「就绪能力不返回后端不可用」）。
        //  · 之后由 NameOwnerChanged 维护（覆盖 daemon 在扩展之后启动）。
        //  ⚠ method_call 内不得同步查询该连接：会与正在分派的这条消息互相等待。
        this._daemonOwner = this._lookupDaemonOwner();
        this._daemonWatcherId = Gio.bus_watch_name(Gio.BusType.SESSION,
            DAEMON_BUS_NAME, Gio.BusNameWatcherFlags.NONE,
            (_conn, _name, owner) => { this._daemonOwner = owner; },
            () => { this._daemonOwner = null; });

        // 窗口事件 → D-Bus 信号（§8.1 三信号）。
        const display = global.display ?? global.compositor;
        if (display) {
            this._signalIds.push(display.connect('window-created', (_d, mw) => {
                this._watchWindowClosed(mw);
                this._onWindowCreated(mw);
            }));
            // notify::focus-window（属性通知信号）——notify-focus-window 不存在。
            this._signalIds.push(display.connect('notify::focus-window', () =>
                this._emit('ActiveWindowChanged',
                    new GLib.Variant('(s)', [this._getActiveWindow()]))));
        }
        // GNOME 51 起 MetaDisplay 不再提供 window-destroyed：逐窗订阅
        // MetaWindow::unmanaged（关闭/销毁都经此），已存在窗口同样纳入。
        for (const actor of global.get_window_actors())
            this._watchWindowClosed(actor.meta_window);
    }

    disable() {
        for (const [, toId] of this._watchers) GLib.source_remove(toId);
        this._watchers.clear();
        for (const [mw, id] of this._closedWatch) {
            try {
                mw.disconnect(id);
            } catch (e) {
                // 窗口已销毁，句柄随之失效。
            }
        }
        this._closedWatch.clear();
        for (const id of this._signalIds)
            (global.display ?? global.compositor)?.disconnect(id);
        this._signalIds = [];

        // 分离清理：名称句柄走 bus_unown_name，对象路径句柄走 unregister_object。
        if (this._nameOwnerId > 0) {
            Gio.bus_unown_name(this._nameOwnerId);
            this._nameOwnerId = 0;
        }
        if (this._regId > 0) {
            Gio.DBus.session.unregister_object(this._regId);
            this._regId = 0;
        }
        if (this._daemonWatcherId > 0) {
            Gio.bus_unwatch_name(this._daemonWatcherId);
            this._daemonWatcherId = 0;
            this._daemonOwner = null;
        }

        this._impl = null;
        this._nodeInfo = null;
        this._ifaceInfo = null;
    }

    // 把处理异常翻成 D-Bus 错误：JS 异常不是 GLib.Error，直接
    // return_gerror 会二次抛出，invocation 永不回包（调用方只能等超时）。
    _returnError(invocation, e) {
        try {
            invocation.return_gerror(e);
        } catch (_notGlibError) {
            invocation.return_dbus_error('org.freedesktop.DBus.Error.Failed',
                String(e));
        }
    }

    // 校验 sender 后按方法名分派（method_call 入口）。
    _onMethodCall(sender, methodName, parameters, invocation) {
        if (!this._isAuthorized(sender)) {
            invocation.return_dbus_error(`${IFACE_NAME}.Error.Unauthorized`,
                'sender is not authorized to control windows');
            return;
        }
        const fn = this._impl?.[methodName];
        if (typeof fn !== 'function') {
            invocation.return_dbus_error('org.freedesktop.DBus.Error.UnknownMethod',
                `method ${methodName} not found`);
            return;
        }
        let args;
        try {
            args = parameters ? parameters.deepUnpack() : [];
        } catch (e) {
            this._returnError(invocation, e);
            return;
        }
        try {
            const result = fn(...args);
            if (STRING_RETURN_METHODS.has(methodName)) {
                invocation.return_value(new GLib.Variant('(s)', [result]));
            } else {
                invocation.return_value(new GLib.Variant('()', []));
            }
        } catch (e) {
            this._returnError(invocation, e);
        }
    }

    // 取 daemon 当前持有者（仅 enable 阶段调用；method_call 内调用会互等，见 enable）。
    _lookupDaemonOwner() {
        try {
            const [owner] = Gio.DBus.session.call_sync(
                'org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetNameOwner',
                new GLib.Variant('(s)', [DAEMON_BUS_NAME]), null,
                Gio.DBusCallFlags.NONE, -1, null).deepUnpack();
            return owner ?? null;
        } catch (e) {
            return null;  // daemon 尚未运行：由 watch 的 appeared 回调填充。
        }
    }

    // 调用方校验：仅 agent-shell-daemon（持有 DAEMON_BUS_NAME well-known 名）
    // 可调用。防止任意同 uid 进程经 org.gnome.Shell.AgentShell 绕过 daemon 的
    // caller 门禁（§8.1 安全——GNOME 47 禁用 Eval 的同类暴露面）。
    _isAuthorized(sender) {
        return this._daemonOwner !== null && this._daemonOwner === sender;
    }

    _withWindow(id, fn) {
        const actor = global.get_window_actors()
            .find(a => a.meta_window.get_id() === id);
        if (!actor) throw new GLib.Error(GLib.quark_from_string('agent-shell'),
            1, `window ${id} not found`);
        fn(actor.meta_window);
    }

    // 逐窗订阅 `unmanaged` → WindowClosed（GNOME 51 起 MetaDisplay 无
    // window-destroyed；`unmanaged` 在 45+ 均可用）。
    _watchWindowClosed(mw) {
        if (this._closedWatch.has(mw)) return;
        const id = mw.connect('unmanaged', () => {
            this._closedWatch.delete(mw);
            try {
                this._emit('WindowClosed',
                    new GLib.Variant('(u)', [mw.get_id() >>> 0]));
            } catch (e) {
                logError(e, 'agent-shell-bridge: WindowClosed emit failed');
            }
        });
        this._closedWatch.set(mw, id);
    }

    _onWindowCreated(mw) {
        // window-created 时 actor 属性未就绪，延迟一拍再取详情并广播。
        const toId = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._watchers.delete(mw);
            try { this._emit('WindowOpened',
                new GLib.Variant('(s)', [this._windowJson(mw)])); } catch (e) {
                logError(e, 'agent-shell-bridge: WindowOpened emit failed');
            }
            return GLib.SOURCE_REMOVE;
        });
        this._watchers.set(mw, toId);
    }

    _emit(name, variant) {
        Gio.DBus.session.emit_signal(null, OBJECT_PATH, IFACE_NAME, name, variant);
    }

    _windowJson(a_or_mw) {
        let mw = a_or_mw.meta_window ?? a_or_mw;
        return JSON.stringify({
            id: mw.get_id(), title: mw.get_title() || '',
            appId: mw.get_wm_class() || '', pid: mw.get_pid(),
            minimized: mw.minimized, maximized: mw.is_maximized(),
            fullscreen: mw.fullscreen,
            geometry: mw.get_frame_rect(),
            hasFocus: mw.has_focus()
        });
    }

    _listWindows() {
        return JSON.stringify(global.get_window_actors().map(a =>
            JSON.parse(this._windowJson(a))));
    }

    _getActiveWindow() {
        const a = global.get_window_actors().find(a => a.meta_window.has_focus());
        return a === undefined ? '' : this._windowJson(a);
    }

    _monitorLayout() {
        const mm = global.backend.get_monitor_manager();
        return JSON.stringify(mm.get_monitors().map(m => m.get_properties?.() ?? {}));
    }
}

export default class AgentShellExtension extends Extension {
    enable() {
        this._bridge = new AgentShellBridge();
        this._bridge.enable();
    }

    disable() {
        this._bridge?.disable();
        this._bridge = null;
    }
}
