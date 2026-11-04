// Mock of `gi://Gio` for the extension dispatch harness.
// Records the register_object closure, the bus_watch_name callbacks and every
// emitted signal so the harness can drive the method_call path and observe the
// extension's outbound notifications without a session bus.

const state = { syncOwner: null };
const registry = new Map();
const watchers = { appeared: null, vanished: null };
const emitted = [];

export const session = {
    reset() {
        registry.clear();
        watchers.appeared = null;
        watchers.vanished = null;
        emitted.length = 0;
    },
    methodCall(path) {
        return registry.get(path);
    },
    register_object(path, _iface, methodCall) {
        registry.set(path, methodCall);
        return 1;
    },
    unregister_object() {},
    emit_signal(_sender, path, iface, name, variant) {
        emitted.push({
            path,
            iface,
            name,
            signature: variant.signature,
            value: variant.deepUnpack(),
        });
    },
    // Only the enable-time ownership lookup is expected here.
    call_sync(_dest, _path, _iface, method) {
        if (method !== 'GetNameOwner')
            throw new Error(`unexpected sync call: ${method}`);
        if (state.syncOwner === null)
            throw new Error('org.freedesktop.DBus.Error.NameHasNoOwner');
        return {deepUnpack: () => [state.syncOwner]};
    },
};

export function setSyncOwner(owner) {
    state.syncOwner = owner;
}

export function emittedSignals() {
    return emitted.slice();
}

export const watched = {
    fireAppeared(owner) {
        watchers.appeared?.(session, 'org.agentshell.Daemon', owner);
    },
    fireVanished() {
        watchers.vanished?.(session, 'org.agentshell.Daemon');
    },
};

const Gio = {
    DBus: {session},
    BusType: {SESSION: 1},
    BusNameOwnerFlags: {NONE: 0},
    BusNameWatcherFlags: {NONE: 0},
    DBusCallFlags: {NONE: 0},
    DBusNodeInfo: {
        new_for_xml(xml) {
            const name = /interface name="([^"]+)"/.exec(xml)?.[1];
            return {interfaces: name ? [{name}] : []};
        },
    },
    bus_own_name() {
        return 1;
    },
    bus_unown_name() {},
    bus_watch_name(_type, _name, _flags, appeared, vanished) {
        watchers.appeared = appeared;
        watchers.vanished = vanished;
        return 2;
    },
    bus_unwatch_name() {},
};

export default Gio;
