#!/bin/sh
# build-deb.sh — 构建 agent-shell + agent-shell-rootd deb 包（§20.2 打包）
#
# 用法：./packaging/debian/build-deb.sh [target-dir]
#
# 步骤：release 二进制缺失时 cargo build --release（自动检测 PipeWire dev 头），
#      已存在（含交叉编译预构建）则跳过；随后 dpkg-deb 打包。
# 产物：agent-shell_<version>_<arch>.deb + agent-shell-rootd_<version>_<arch>.deb
# dash 兼容：不使用 pipefail（Debian /bin/sh = dash 不支持 -o pipefail）
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${1:-$ROOT_DIR/target}"
# 规范化为绝对路径：cargo 在 $ROOT_DIR 子 shell 内执行，install_bin/cp 在调用方
# cwd 执行，相对 --target-dir 会在两处按不同基准解析（审查反馈）。
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$(pwd)/$TARGET_DIR" ;;
esac
BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT
VERSION="$(grep -m1 '^version' "$ROOT_DIR/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
ARCH="${DEB_HOST_ARCH:-$(dpkg --print-architecture 2>/dev/null || true)}"
if [ -z "$ARCH" ]; then
    echo "error: unable to determine build architecture (dpkg missing, DEB_HOST_ARCH unset)" >&2
    exit 1
fi

# ── 构建 release 二进制（已存在则跳过，保留交叉编译预构建路径）──
# portal-screencast（capture 默认 feature）需能编译 libspa-sys 0.10.1。其
# type-info.c 无条件引用 spa_type_param_bitorder（0.3.37 引入）、
# spa_type_audio_iec958_codec（0.3.34 引入）等符号；旧 pipewire（UOS 20 /
# bullseye 0.3.19）有 .pc 但头缺这些符号，`--exists` 探不到，须按版本门槛判旧。
# 不满足时回退 --no-default-features，capture 走 Screenshot/X11 降级链。
NEED_BUILD=0
for bin in agent-shell-daemon agent-shell agent-shell-mcp agent-shell-rootd; do
    if [ ! -x "$TARGET_DIR/release/$bin" ]; then
        NEED_BUILD=1
        break
    fi
done
if [ "$NEED_BUILD" = "1" ]; then
    FEATURES_FLAGS=""
    case "$ARCH" in
        arm64|aarch64)
            # arm64（company-04）系统 libspa-0.2-dev 头（0.3.15.x）与
            # libspa-sys 0.10.1 不兼容，portal-screencast 默认特性直编失败；
            # 发行构建显式回退 --no-default-features，capture 走
            # Screenshot/X11 降级链。其他平台保持默认特性构建（版本门槛见下）。
            FEATURES_FLAGS="--no-default-features"
            echo "warning: arm64 release build uses --no-default-features (libspa-sys 0.10.1 vs old libspa-0.2 headers)" >&2
            ;;
        *)
            if ! pkg-config --atleast-version=0.3.37 libpipewire-0.3 2>/dev/null; then
                FEATURES_FLAGS="--no-default-features"
                echo "warning: libpipewire-0.3 < 0.3.37 (or missing); building with --no-default-features (portal-screencast disabled)" >&2
            fi
            ;;
    esac
    (cd "$ROOT_DIR" && cargo build --release --target-dir "$TARGET_DIR" $FEATURES_FLAGS)
else
    echo "release binaries already present; skipping cargo build"
fi

echo "Building deb: agent-shell $VERSION ($ARCH)"

# ── 共享：二进制与 polkit policy ──
install_bin() {
    local name="$1" dest="$2"
    # dest 已含完整路径前缀（$PKG1/$PKG2 = $BUILD_DIR/...）——不重复拼接
    install -Dm755 "$TARGET_DIR/release/$name" "$dest"
}

# ── agent-shell 包（daemon + cli + mcp） ──
PKG1="$BUILD_DIR/agent-shell"
mkdir -p "$PKG1/DEBIAN" "$PKG1/usr/bin" "$PKG1/usr/lib/systemd/user" \
         "$PKG1/usr/lib/udev/rules.d"
install_bin agent-shell-daemon "$PKG1/usr/bin/agent-shell-daemon"
install_bin agent-shell "$PKG1/usr/bin/agent-shell"
install_bin agent-shell-mcp "$PKG1/usr/bin/agent-shell-mcp"
install -Dm644 "$ROOT_DIR/daemon/agent-shell-daemon.service" \
    "$PKG1/usr/lib/systemd/user/agent-shell-daemon.service"
install -Dm644 "$SCRIPT_DIR/60-agent-shell-uinput.rules" \
    "$PKG1/usr/lib/udev/rules.d/60-agent-shell-uinput.rules"
# postinst / prerm
install -Dm755 "$SCRIPT_DIR/postinst" "$PKG1/DEBIAN/postinst"
install -Dm755 "$SCRIPT_DIR/prerm" "$PKG1/DEBIAN/prerm"
# control（裁剪到仅 agent-shell 包）
cat > "$PKG1/DEBIAN/control" <<EOF
Package: agent-shell
Architecture: $ARCH
Version: $VERSION
Maintainer: Agent Shell <agent-shell@example.com>
Depends: systemd, xdg-desktop-portal, pipewire
Description: Agent Shell — Linux desktop agent shell
 Agent Shell is a desktop automation framework.
 .
 systemd user service: agent-shell-daemon.service.
 udev rules: /dev/uinput for ydotool input injection.
EOF

# ── agent-shell-rootd 包（rootd + polkit + pkexec） ──
PKG2="$BUILD_DIR/agent-shell-rootd"
mkdir -p "$PKG2/DEBIAN" "$PKG2/usr/bin" \
         "$PKG2/usr/lib/agent-shell" \
         "$PKG2/usr/lib/systemd/system" \
         "$PKG2/usr/share/polkit-1/actions" \
         "$PKG2/usr/share/dbus-1/system.d"
install_bin agent-shell-rootd "$PKG2/usr/bin/agent-shell-rootd"
install -Dm644 "$SCRIPT_DIR/../org.agentshell.Rootd.conf" \
    "$PKG2/usr/share/dbus-1/system.d/org.agentshell.Rootd.conf"
install -Dm755 "$SCRIPT_DIR/../rootd-pkexec" \
    "$PKG2/usr/lib/agent-shell/rootd-pkexec"
install -Dm644 "$ROOT_DIR/rootd/agent-shell-rootd.service" \
    "$PKG2/usr/lib/systemd/system/agent-shell-rootd.service"
install -Dm644 "$SCRIPT_DIR/../com.agentshell.policy" \
    "$PKG2/usr/share/polkit-1/actions/com.agentshell.policy"
install -Dm755 "$SCRIPT_DIR/postinst" "$PKG2/DEBIAN/postinst"
install -Dm755 "$SCRIPT_DIR/prerm" "$PKG2/DEBIAN/prerm"
cat > "$PKG2/DEBIAN/control" <<EOF
Package: agent-shell-rootd
Architecture: $ARCH
Version: $VERSION
Maintainer: Agent Shell <agent-shell@example.com>
Depends: policykit-1, systemd
Description: Agent Shell rootd — privileged proxy (systemd system unit)
 Agent Shell rootd: thin privileged proxy, whitelisted D-Bus + polkit.
 Independently installable — absent rootd degrades to user-only ops.
EOF

# ── 构建 deb ──
dpkg-deb --build --root-owner-group "$PKG1" \
    "$BUILD_DIR/agent-shell_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG2" \
    "$BUILD_DIR/agent-shell-rootd_${VERSION}_${ARCH}.deb"

echo "Built:"
echo "  $BUILD_DIR/agent-shell_${VERSION}_${ARCH}.deb"
echo "  $BUILD_DIR/agent-shell-rootd_${VERSION}_${ARCH}.deb"
cp "$BUILD_DIR"/agent-shell*.deb "$TARGET_DIR/"
echo "Copied to $TARGET_DIR/"
