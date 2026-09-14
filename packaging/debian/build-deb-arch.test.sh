#!/bin/sh
# build-deb-arch.test.sh — 验证 build-deb.sh 架构检测（TSI-3106）
#
# 覆盖两条契约：
#   1) DEB_HOST_ARCH 设置时，包 control 的 Architecture: 字段采用该架构
#   2) dpkg 不可用且 DEB_HOST_ARCH 未设置时，脚本显式失败（exit 1），
#      不再静默回退 amd64
#
# 用法：sh packaging/debian/build-deb-arch.test.sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD_SH="$SCRIPT_DIR/build-deb.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# install_bin 需要 $TARGET_DIR/release/* 存在；用空文件占位即可
mkdir -p "$WORK/target/release"
for bin in agent-shell-daemon agent-shell agent-shell-mcp agent-shell-rootd; do
    : > "$WORK/target/release/$bin"
done

# stub 目录：dpkg-deb 捕获 control 的 Architecture: 后即成功，避免真实构建；
# dpkg stub 失败，用于模拟无 dpkg 环境。
STUB="$WORK/stub"
mkdir -p "$STUB"

cat > "$STUB/dpkg-deb" <<'EOF'
#!/bin/sh
# 定位 --root-owner-group 之后的包目录，把其 control 的 Architecture: 追加到捕获文件
prev=
for a in "$@"; do
    if [ "$prev" = "--root-owner-group" ]; then pkg="$a"; fi
    prev="$a"
done
grep '^Architecture:' "$pkg/DEBIAN/control" >> "${CAPTURE_FILE:?}"
exit 0
EOF
chmod +x "$STUB/dpkg-deb"

cat > "$STUB/dpkg" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$STUB/dpkg"

# stub cargo：记录参数并验证 --no-default-features（pkg-config stub 失败 → 必须走该 flag）
cat > "$STUB/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >> "${CARGO_ARGS_FILE:?}"
for a in "$@"; do
    [ "$a" = "--no-default-features" ] && exit 0
done
exit 1
EOF
chmod +x "$STUB/cargo"

# stub pkg-config：模拟无 PipeWire dev 头，验证 build-deb.sh 走 --no-default-features
cat > "$STUB/pkg-config" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$STUB/pkg-config"

pass=0
fail=0
check() {
    if [ "$2" = "$3" ]; then
        pass=$((pass + 1))
        echo "ok   - $1"
    else
        fail=$((fail + 1))
        echo "FAIL - $1: got '$2', want '$3'" >&2
    fi
}

# ── 用例 1：DEB_HOST_ARCH 被采用；pkg-config stub 失败 → cargo 传 --no-default-features ──
CAPTURE_FILE="$WORK/capture1"
CARGO_ARGS_FILE="$WORK/cargo_args1"
export CAPTURE_FILE CARGO_ARGS_FILE
: > "$CAPTURE_FILE"
: > "$CARGO_ARGS_FILE"
PATH="$STUB:$PATH" DEB_HOST_ARCH=arm64 sh "$BUILD_SH" "$WORK/target" > "$WORK/out1" 2>&1
check "DEB_HOST_ARCH=arm64 时 control Architecture 字段" \
    "$(sort -u "$CAPTURE_FILE" | head -n1)" \
    "Architecture: arm64"
check "无 PipeWire dev 头时 cargo 传入 --no-default-features" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" \
    "1"

# ── 用例 2：无 dpkg（stub 失败）且未设 DEB_HOST_ARCH → 显式失败 ──
unset DEB_HOST_ARCH
if PATH="$STUB:$PATH" sh "$BUILD_SH" "$WORK/target" > "$WORK/out2" 2>&1; then
    rc=0
else
    rc=$?
fi
check "无 dpkg 且未设 DEB_HOST_ARCH 时 exit 1" "$rc" "1"
check "无 dpkg 时错误信息写入 stderr" \
    "$(grep -c 'unable to determine build architecture' "$WORK/out2")" "1"

echo
echo "build-deb-arch.test.sh: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
