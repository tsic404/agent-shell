// Mock of `gi://GLib` for the extension dispatch harness.
// Only the surface extension.js touches is provided.

class GLib {}

GLib.Variant = class Variant {
    constructor(signature, value) {
        this.signature = signature;
        this.value = value;
    }

    deepUnpack() {
        return this.value;
    }
};

GLib.Error = class GError extends Error {
    constructor(quark, code, message) {
        super(message);
        this.code = code;
    }
};

GLib.PRIORITY_DEFAULT = 0;
GLib.PRIORITY_DEFAULT_IDLE = 200;
GLib.SOURCE_REMOVE = false;
GLib.quark_from_string = () => 0;
GLib.idle_add = () => 1;
GLib.source_remove = () => {};

export default GLib;
