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
        this._vtable = null;
        this._regId = 0;        // register_object 句柄（对象路径）
        this._nameOwnerId = 0;  // bus_own_name 句柄（well-known 名称）
        this._signalIds = [];
        this._watchers = new Map();
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

        // 自定义 vtable：method_call 先校验 sender 再按方法名分派。
        // 不用 Gio.DBusExportedObject.wrapJSObject——其 method 不暴露 sender，
        // 无法做调用方校验（安全要求）。
        this._vtable = new Gio.DBusInterfaceVTable();
        this._vtable.method_call = (connection, sender, objectPath, ifaceName,
            methodName, parameters, invocation) => {
            this._onMethodCall(sender, methodName, parameters, invocation);
        };
        this._regId = Gio.DBus.session.register_object(
            OBJECT_PATH, this._ifaceInfo, this._vtable);

        this._nameOwnerId = Gio.bus_own_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameOwnerFlags.NONE, null, null, null);

        // 窗口事件 → D-Bus 信号（§8.1 三信号）。
        const display = global.display ?? global.compositor;
        if (display) {
            this._signalIds.push(display.connect('window-created', (_d, mw) =>
                this._onWindowCreated(mw)));
            this._signalIds.push(display.connect('window-destroyed', (_d, mw) =>
                this._emit('WindowClosed', new GLib.Variant('(u)', [mw.get_id() >>> 0]))));
            // notify::focus-window（属性通知信号）——notify-focus-window 不存在。
            this._signalIds.push(display.connect('notify::focus-window', () =>
                this._emit('ActiveWindowChanged',
                    new GLib.Variant('(s)', [this._getActiveWindow()]))));
        }
    }

    disable() {
        for (const [, toId] of this._watchers) GLib.source_remove(toId);
        this._watchers.clear();
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

        this._impl = null;
        this._nodeInfo = null;
        this._ifaceInfo = null;
        this._vtable = null;
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
            invocation.return_gerror(e);
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
            invocation.return_gerror(e);
        }
    }

    // 调用方校验：仅 agent-shell-daemon（持有 DAEMON_BUS_NAME well-known 名）
    // 可调用。防止任意同 uid 进程经 org.gnome.Shell.AgentShell 绕过 daemon 的
    // caller 门禁（§8.1 安全——GNOME 47 禁用 Eval 的同类暴露面）。
    _isAuthorized(sender) {
        try {
            const owner = Gio.DBus.session.get_name_owner_sync(DAEMON_BUS_NAME);
            return owner !== null && owner === sender;
        } catch (e) {
            return false;
        }
    }

    _withWindow(id, fn) {
        const actor = global.get_window_actors()
            .find(a => a.meta_window.get_id() === id);
        if (!actor) throw new GLib.Error(GLib.quark_from_string('agent-shell'),
            1, `window ${id} not found`);
        fn(actor.meta_window);
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
            minimized: mw.minimized, maximized: mw.maximized,
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
