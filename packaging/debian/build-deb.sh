#!/bin/sh
# build-deb.sh — 构建 agent-shell + agent-shell-rootd deb 包（§20.2 打包）
#
# 前置：cargo build --release 已完成（产出 target/release/ 二进制）
#
# 用法：./packaging/debian/build-deb.sh [target-dir]
#
# 产物：agent-shell_<version>_<arch>.deb + agent-shell-rootd_<version>_<arch>.deb
# dash 兼容：不使用 pipefail（Debian /bin/sh = dash 不支持 -o pipefail）
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${1:-$ROOT_DIR/target}"
BUILD_DIR="$(mktemp -d)"
VERSION="$(grep -m1 '^version' "$ROOT_DIR/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"

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
cp "$BUILD_DIR"/agent-shell*.deb "$TARGET_DIR/" 2>/dev/null || true
echo "Copied to $TARGET_DIR/"
rm -rf "$BUILD_DIR"
