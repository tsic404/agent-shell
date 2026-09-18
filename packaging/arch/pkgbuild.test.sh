#!/bin/bash
# pkgbuild.test.sh — 验证 PKGBUILD build() 恒走 BE（--no-default-features）
#
# PKGBUILD 由 makepkg 以 bash source（顶层含数组语法），故本测试须 bash 运行，
# 不能像 build-deb-arch.test.sh 那样用 dash。覆盖契约：
#   1) 任意 CARCH（x86_64 / aarch64）build() 均带 --no-default-features（恒 BE）
#   2) build() 不探测 pkg-config（无 libpipewire 版本门槛）——portal-screencast
#      依赖 libspa-sys 0.10.1 的 `_libspa_rs` shim（wrap_static_fns 生成）与
#      release LTO 交互，在部分 Arch 类机器链接期报 undefined `spa_*_libspa_rs`，
#     为 pre-existing 上游链接问题；Arch 一律不启用 portal-screencast。
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

# stub pkg-config：仅记录调用。build() 恒走 BE，不得探测 libpipewire 版本门槛；
# 用例 3 断言其从未被调用，回归「重新引入门槛」时失败。
PKGCONFIG_CALLED_FILE="$WORK/pkgconfig_called"
export PKGCONFIG_CALLED_FILE
: > "$PKGCONFIG_CALLED_FILE"
cat > "$STUB/pkg-config" <<'EOF'
#!/bin/sh
printf 'called\n' >> "${PKGCONFIG_CALLED_FILE:?}"
exit 0
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

# ── 用例 1：x86_64 → 恒走 BE ──
CARGO_ARGS_FILE="$WORK/cargo_args1"
export CARGO_ARGS_FILE
: > "$CARGO_ARGS_FILE"
export CARCH=x86_64
build > "$WORK/out1" 2>&1
check "x86_64 build() 带 --no-default-features（恒 BE）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

# ── 用例 2：aarch64 → 恒走 BE ──
CARGO_ARGS_FILE="$WORK/cargo_args2"
export CARGO_ARGS_FILE
: > "$CARGO_ARGS_FILE"
export CARCH=aarch64
build > "$WORK/out2" 2>&1
check "aarch64 build() 带 --no-default-features（恒 BE）" \
    "$(grep -c -- '--no-default-features' "$CARGO_ARGS_FILE")" "1"

# ── 用例 3：build() 不探测 pkg-config（无 libpipewire 版本门槛）──
check "build() 不调用 pkg-config（门槛已移除）" \
    "$(grep -c 'called' "$PKGCONFIG_CALLED_FILE")" "0"

echo
echo "pkgbuild.test.sh: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
