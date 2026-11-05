// Mock of `gi://Meta`: extension.js reads maximize flags and window types.
export default {
    MaximizeFlags: {NONE: 0, HORIZONTAL: 1, VERTICAL: 2, BOTH: 3},
    // GI enum 成员名与真实 Meta.WindowType 一致（数值取 Meta-51.gir）。
    WindowType: {
        NORMAL: 0,
        DESKTOP: 1,
        DOCK: 2,
        DIALOG: 3,
        MODAL_DIALOG: 4,
        TOOLBAR: 5,
        MENU: 6,
        UTILITY: 7,
        SPLASHSCREEN: 8,
        DROPDOWN_MENU: 9,
        POPUP_MENU: 10,
        TOOLTIP: 11,
        NOTIFICATION: 12,
        COMBO: 13,
        DND: 14,
        OVERRIDE_OTHER: 15,
    },
};
