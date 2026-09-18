#!/bin/bash
# pkgbuild.test.sh — 验证 PKGBUILD build() 的 features-gate（CARCH × pkg-config 版本门槛）
#
# PKGBUILD 由 makepkg 以 bash source（顶层含数组语法），故本测试须 bash 运行，
# 不能像 build-deb-arch.test.sh 那样用 dash。覆盖契约（与 debian 侧对称）：
#   1) 非 aarch64 + libpipewire-0.3 >= 0.3.37（现代头）→ 升级默认特性
#      （cargo 不带 --no-default-features）
#   2) 非 aarch64 + 头缺失/过旧 → 保持 BE（cargo 带 --no-default-features）
#   3) aarch64 无视门槛强制 BE（现代头亦然）
#
# 用法：bash packaging/arch/pkgbuild.test.sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PKGBUILD_FILE="$SCRIPT_DIR/PKGBUILD"

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
    echo "pkgbuild.test.sh: 无可用临时目录（/tmp、\$HOME、\$PWD、\$SCRIPT_DIR 均不可写或不可执行）" >&2
    exit 1
fi
WORK="$(TMPDIR="$_workbase" mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
unset _cand _workbase _p

# 假 srcdir：build() 会 cd "$srcdir/agent-shell"
mkdir -p "$WORK/srcdir/agent-shell"

STUB="$WORK/stub"
mkdir -p "$STUB"

# stub cargo：跳过真实构建，捕获参数供断言
cat > "$STUB/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >> "${CARGO_ARGS_FILE:?}"
exit 0
EOF
chmod +x "$STUB/cargo"

# stub pkg-config：断言 PKGBUILD 恰以版本门槛探测（--atleast-version=0.3.37
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

# source 真实 PKGBUILD（bash），暴露 build()
. "$PKGBUILD_FILE"

PATH="$STUB:$PATH"
srcdir="$WORK/srcdir"
export PATH srcdir

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

# ── 用例 1：x86_64 + 现代头（>= 0.3.37）→ 升级默认特性 ──
CARGO_ARGS_FILE="$WORK/cargo_args1"
export CARGO_ARGS_FILE
: > "$CARGO_ARGS_FILE"
export CARCH=x86_64 PKGCONFIG_PIPEWIRE=1
build > "$WORK/out1" 2>&1
check "x86_64 + 现代头时 cargo 不带 --no-default-features（升级默认特性）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "0"

# ── 用例 2：x86_64 + 头缺失/过旧 → 保持 BE ──
CARGO_ARGS_FILE="$WORK/cargo_args2"
export CARGO_ARGS_FILE
: > "$CARGO_ARGS_FILE"
export CARCH=x86_64 PKGCONFIG_PIPEWIRE=0
build > "$WORK/out2" 2>&1
check "x86_64 + 头缺失/过旧时 cargo 带 --no-default-features（保持 BE）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

# ── 用例 3：aarch64 + 现代头 → 无视门槛强制 BE ──
CARGO_ARGS_FILE="$WORK/cargo_args3"
export CARGO_ARGS_FILE
: > "$CARGO_ARGS_FILE"
export CARCH=aarch64 PKGCONFIG_PIPEWIRE=1
build > "$WORK/out3" 2>&1
check "aarch64 + 现代头时 cargo 仍带 --no-default-features（强制 BE）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

echo
echo "pkgbuild.test.sh: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
