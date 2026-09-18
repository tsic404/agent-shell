#!/bin/sh
# build-deb-arch.test.sh — 验证 build-deb.sh 架构检测
#
# 覆盖四条契约：
#   1) DEB_HOST_ARCH 设置时，包 control 的 Architecture: 字段采用该架构
#   2) dpkg 不可用且 DEB_HOST_ARCH 未设置时，脚本显式失败（exit 1），
#      不再静默回退 amd64
#   3) 构建时默认走 BE（--no-default-features）；非 arm64 平台仅在
#      libpipewire-0.3 >= 0.3.37（现代头）时升级为默认特性（portal-screencast）；
#      arm64（company-04）头版本上报不可靠，无视门槛强制 BE
#   4) TMPDIR 指向不存在目录时，内层 build-deb.sh 回退到有效临时目录，仍成功打包
#
# 用法：sh packaging/debian/build-deb-arch.test.sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD_SH="$SCRIPT_DIR/build-deb.sh"

# /tmp 挂 noexec 时（如 company-04 的 tmpfs），mktemp -d 仍会成功，但下面的
# stub 二进制无法执行（EACCES）。按优先级探测候选临时目录（默认位置、$HOME、
# $PWD、$SCRIPT_DIR），取首个「可写且可执行」者作 WORK 基座；全部不可用则明确
# 失败。正常主机首选默认位置，行为不回退。
exec_probe() {
    _p="$1/.exec-probe.$$"
    if (printf '#!/bin/sh\n' > "$_p" && chmod +x "$_p" && "$_p") 2>/dev/null; then
        rm -f "$_p"
        return 0
    fi
    rm -f "$_p"
    return 1
}

_workbase=""
for _cand in "${TMPDIR:-/tmp}" "${HOME:-}" "${PWD:-}" "$SCRIPT_DIR"; do
    if [ -d "$_cand" ] && exec_probe "$_cand"; then
        _workbase="$_cand"
        break
    fi
done
if [ -z "$_workbase" ]; then
    echo "build-deb-arch.test.sh: 无可用临时目录（/tmp、\$HOME、\$PWD、\$SCRIPT_DIR 均不可写或不可执行）" >&2
    exit 1
fi
WORK="$(TMPDIR="$_workbase" mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
unset _cand _workbase _p

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
# 定位 --root-owner-group 之后的包目录，grep 其 control Architecture: 到捕获文件；
# 并 touch 输出 .deb（最后一个参数），让 build-deb.sh 末尾的 cp 有产物可拷。
prev=
out=
for a in "$@"; do
    if [ "$prev" = "--root-owner-group" ]; then pkg="$a"; fi
    prev="$a"
    out="$a"
done
grep '^Architecture:' "$pkg/DEBIAN/control" >> "${CAPTURE_FILE:?}"
: > "$out"
exit 0
EOF
chmod +x "$STUB/dpkg-deb"

cat > "$STUB/dpkg" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$STUB/dpkg"

# stub cargo：跳过真实构建，捕获参数供断言（test 已预置 $WORK/target/release 二进制）
cat > "$STUB/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >> "${CARGO_ARGS_FILE:?}"
exit 0
EOF
chmod +x "$STUB/cargo"

# stub pkg-config：断言 build-deb.sh 恰以版本门槛探测（--atleast-version=0.3.37
# libpipewire-0.3）。门槛/包名回归（--exists、0.3.19、错包名）都在此失败并被
# 用例捕获；PKGCONFIG_PIPEWIRE=1 表示门槛满足（现代头），否则缺失/过旧（exit 1）。
cat > "$STUB/pkg-config" <<'EOF'
#!/bin/sh
if [ "$#" -ne 2 ] || [ "$1" != "--atleast-version=0.3.37" ] || [ "$2" != "libpipewire-0.3" ]; then
    echo "pkg-config stub: expected '--atleast-version=0.3.37 libpipewire-0.3', got: $*" >&2
    exit 2
fi
[ "${PKGCONFIG_PIPEWIRE:-0}" = "1" ]
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

# ── 用例 1：DEB_HOST_ARCH=arm64 + 预构建二进制 → 跳过构建并采用架构 ──
CAPTURE_FILE="$WORK/capture1"
CARGO_ARGS_FILE="$WORK/cargo_args1"
export CAPTURE_FILE CARGO_ARGS_FILE
: > "$CAPTURE_FILE"
: > "$CARGO_ARGS_FILE"
chmod +x "$WORK/target/release/"*
PATH="$STUB:$PATH" DEB_HOST_ARCH=arm64 sh "$BUILD_SH" "$WORK/target" > "$WORK/out1" 2>&1
check "DEB_HOST_ARCH=arm64 时 control Architecture 字段" \
    "$(sort -u "$CAPTURE_FILE" | head -n1)" \
    "Architecture: arm64"
if [ -s "$CARGO_ARGS_FILE" ]; then
    cargo_called="called"
else
    cargo_called="not-called"
fi
check "预构建二进制时跳过 cargo 构建（交叉打包）" "$cargo_called" "not-called"

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

# ── 用例 3：PipeWire 头缺失/过旧 → 保持 BE（cargo 带 --no-default-features）──
CARGO_ARGS_FILE="$WORK/cargo_args3"
CAPTURE_FILE="$WORK/capture3"
export CARGO_ARGS_FILE CAPTURE_FILE
: > "$CARGO_ARGS_FILE"
: > "$CAPTURE_FILE"
chmod 644 "$WORK/target/release/"*
PATH="$STUB:$PATH" DEB_HOST_ARCH=amd64 PKGCONFIG_PIPEWIRE=0 sh "$BUILD_SH" "$WORK/target" > "$WORK/out3" 2>&1
check "PipeWire 头缺失/过旧时 cargo 带 --no-default-features（保持 BE 默认）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

# ── 用例 4：现代 PipeWire 头（>= 0.3.37）→ 从 BE 升级为默认特性 ──
CARGO_ARGS_FILE="$WORK/cargo_args4"
CAPTURE_FILE="$WORK/capture4"
export CARGO_ARGS_FILE CAPTURE_FILE
: > "$CARGO_ARGS_FILE"
: > "$CAPTURE_FILE"
chmod 644 "$WORK/target/release/"*
PATH="$STUB:$PATH" DEB_HOST_ARCH=amd64 PKGCONFIG_PIPEWIRE=1 sh "$BUILD_SH" "$WORK/target" > "$WORK/out4" 2>&1
# 期望缺失型断言不抗回归：先证 cargo 确被调用，再证 argv 不含 --no-default-features，
# 否则构建路径未来不可达时 grep -c 得 0 == 0 仍会静默通过。
if [ -s "$CARGO_ARGS_FILE" ]; then
    cargo_called="called"
else
    cargo_called="not-called"
fi
check "现代 PipeWire 头时 cargo 被调用（构建路径可达）" "$cargo_called" "called"
check "现代 PipeWire 头时 cargo 不带 --no-default-features（升级默认特性）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "0"

# ── 用例 5：arm64 + 需构建 → 显式 --no-default-features（无视 PipeWire 头版本）──
CARGO_ARGS_FILE="$WORK/cargo_args5"
CAPTURE_FILE="$WORK/capture5"
export CARGO_ARGS_FILE CAPTURE_FILE
: > "$CARGO_ARGS_FILE"
: > "$CAPTURE_FILE"
chmod 644 "$WORK/target/release/"*
PATH="$STUB:$PATH" DEB_HOST_ARCH=arm64 PKGCONFIG_PIPEWIRE=1 sh "$BUILD_SH" "$WORK/target" > "$WORK/out5" 2>&1
check "arm64 需构建时 cargo 显式 --no-default-features（现代头亦然）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

# ── 用例 6：TMPDIR 指向不存在目录 → 内层 build-deb.sh 回退到有效临时目录 ──
CARGO_ARGS_FILE="$WORK/cargo_args6"
CAPTURE_FILE="$WORK/capture6"
export CARGO_ARGS_FILE CAPTURE_FILE
: > "$CARGO_ARGS_FILE"
: > "$CAPTURE_FILE"
chmod +x "$WORK/target/release/"*
if TMPDIR="$WORK/nonexistent" PATH="$STUB:$PATH" DEB_HOST_ARCH=amd64 PKGCONFIG_PIPEWIRE=1 sh "$BUILD_SH" "$WORK/target" > "$WORK/out6" 2>&1; then
    rc6=0
else
    rc6=$?
fi
check "TMPDIR 不存在时内层回退（build-deb.sh exit 0）" "$rc6" "0"
check "内层回退后仍端到端打包（control Architecture 字段）" \
    "$(sort -u "$CAPTURE_FILE" | head -n1)" \
    "Architecture: amd64"

# 真实 dpkg-deb（非 stub）：stub 不建 staging 临时文件，会遮蔽「子进程继承失效
# TMPDIR」回归。此处以真实 dpkg-deb 跑一次，验证回退后 dpkg-deb 也能建临时文件。
mkdir -p "$WORK/target-real/release"
for bin in agent-shell-daemon agent-shell agent-shell-mcp agent-shell-rootd; do
    : > "$WORK/target-real/release/$bin"
done
chmod +x "$WORK/target-real/release/"*
if TMPDIR="$WORK/nonexistent" DEB_HOST_ARCH=amd64 sh "$BUILD_SH" "$WORK/target-real" > "$WORK/out6real" 2>&1; then
    rc6real=0
else
    rc6real=$?
fi
check "真实 dpkg-deb：TMPDIR 不存在时内层回退仍 exit 0" "$rc6real" "0"
set -- "$WORK/target-real"/agent-shell*.deb
check "真实 dpkg-deb：产出 2 个 .deb" "$#" "2"
check "真实 dpkg-deb：deb Architecture 为 amd64" \
    "$(dpkg-deb -f "$WORK/target-real"/agent-shell_*.deb Architecture)" \
    "amd64"

echo
echo "build-deb-arch.test.sh: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
