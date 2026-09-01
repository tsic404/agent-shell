#!/bin/sh
# hostname-fixture.sh — 主机名快照/恢复实况夹具（TSI-2630）
#
# 背景：共享容器 /etc/hostname 曾被 sibling 测试改动且未恢复，主机名类
# 断言不可复现。rootd 的 HostnameGuard 夹具（snapshot/restore + Drop 兜底）
# 需要真实 root + systemd-hostnamed 环境才能实跑，托管 CI 均不满足。
# 本脚本把该实况夹具固化为共享容器里的可复现入口。
#
# 用法（必须在共享容器内以 root 运行）：
#   sudo scripts/hostname-fixture.sh [N]
#
#   N：连续跑夹具的遍数，默认 2。每遍前后都抓取静态/瞬时主机名，
#   夹具结束后必须仍是原始值，否则脚本以非零退出。
#
#   可选环境变量：CARGO_HOME / RUSTUP_HOME 显式指定已预热缓存；
#   CARGO_NO_RUN_TIMEOUT 覆盖 --no-run 超时秒数（默认 120）。
#
# 退出码：0 = 各遍夹具全过且主机名无漂移；非 0 = 夹具失败或发生漂移。

set -eu

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNS="${1:-2}"

if [ "$(id -u)" -ne 0 ]; then
    echo "FATAL: 需要 root（写回主机名需要权限）。用 sudo 运行。" >&2
    exit 1
fi
if ! command -v hostnamectl >/dev/null 2>&1; then
    echo "FATAL: 找不到 hostnamectl。" >&2
    exit 1
fi

snapshot() {
    echo "/etc/hostname='$(cat /etc/hostname 2>/dev/null)' /proc/sys/kernel/hostname='$(cat /proc/sys/kernel/hostname 2>/dev/null)'"
}

cd "$ROOT_DIR"
ORIGINAL="$(snapshot)"

# —— cargo/rustup 缓存探测与超时保护 ——
# root（尤其 sudo）下 HOME 指向 /root，通常没有预热的 cargo 缓存，
# cargo 会退化为联网拉取整条 zbus/tokio 依赖树，静默挂死数分钟。
# 这里优先复用真实用户（SUDO_USER）已预热的缓存，并用 timeout 兜底。

non_empty_dir() {
    [ -d "$1" ] && [ -n "$(ls -A "$1" 2>/dev/null)" ]
}

resolve_cargo_home() {
    if [ -n "${CARGO_HOME:-}" ]; then
        if non_empty_dir "$CARGO_HOME/registry/cache"; then
            return 0
        fi
        echo "warn: 显式传入的 CARGO_HOME=$CARGO_HOME 无预热 registry/cache，回退探测 HOME/SUDO_USER。" >&2
    fi
    if [ -n "${HOME:-}" ] && non_empty_dir "$HOME/.cargo/registry/cache"; then
        CARGO_HOME="$HOME/.cargo"
        return 0
    fi
    if [ -n "${SUDO_USER:-}" ]; then
        # gotcha：root 复用普通用户缓存时，cargo 更新 registry index 可能向该
        # 用户目录写入 root 属主文件。共享容器内缓存通常已预热、只读命中即可
        # 自愈；如需绝对隔离，请显式 CARGO_HOME 指向 root 私有缓存目录。
        _suhome="$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6)"
        if [ -z "${_suhome:-}" ]; then
            _suhome="/home/$SUDO_USER"
        fi
        if non_empty_dir "$_suhome/.cargo/registry/cache"; then
            CARGO_HOME="$_suhome/.cargo"
            return 0
        fi
    fi
    return 1
}

resolve_rustup_home() {
    if [ -n "${RUSTUP_HOME:-}" ] && non_empty_dir "$RUSTUP_HOME/toolchains"; then
        return 0
    fi
    if [ -n "${HOME:-}" ] && non_empty_dir "$HOME/.rustup/toolchains"; then
        RUSTUP_HOME="$HOME/.rustup"
        return 0
    fi
    if [ -n "${SUDO_USER:-}" ]; then
        _suhome="$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6)"
        if [ -z "${_suhome:-}" ]; then
            _suhome="/home/$SUDO_USER"
        fi
        if non_empty_dir "$_suhome/.rustup/toolchains"; then
            RUSTUP_HOME="$_suhome/.rustup"
            return 0
        fi
    fi
    return 1
}

NO_RUN_TIMEOUT="${CARGO_NO_RUN_TIMEOUT:-120}"
case "$NO_RUN_TIMEOUT" in
    ''|*[!0-9]*|0|0*)
        echo "FATAL: CARGO_NO_RUN_TIMEOUT 必须是正整数，当前值：'$NO_RUN_TIMEOUT'。" >&2
        exit 1
        ;;
esac

if resolve_cargo_home; then
    export CARGO_HOME
    echo "info: 复用 cargo 缓存 CARGO_HOME=$CARGO_HOME"
else
    echo "warn: 未探测到已预热的 cargo 缓存，首次运行可能因下载依赖耗时较长。" >&2
fi
if resolve_rustup_home; then
    export RUSTUP_HOME
    echo "info: 复用 rustup 工具链 RUSTUP_HOME=$RUSTUP_HOME"
else
    echo "warn: 未探测到已预热的 rustup 工具链，构建可能触发工具链下载（已由 timeout 兜底）。" >&2
fi

echo "info: cargo test --no-run（超时 ${NO_RUN_TIMEOUT}s）..."
set +e
timeout "$NO_RUN_TIMEOUT" cargo test -p agent-shell-rootd --lib hostname_set_snapshot_and_restore_live --no-run
_NO_RUN_RC=$?
set -e
if [ "$_NO_RUN_RC" -ne 0 ]; then
    echo "FATAL: cargo test --no-run 失败（退出码 $_NO_RUN_RC）。" >&2
    if [ "$_NO_RUN_RC" -eq 124 ] || [ "$_NO_RUN_RC" -eq 137 ]; then
        echo "  疑似 cargo 缓存未预热，依赖下载 ${NO_RUN_TIMEOUT}s 未收敛。" >&2
        echo "  缓解：以普通用户预热缓存后重试，或显式注入已预热缓存：" >&2
        echo "    sudo CARGO_HOME=/home/<user>/.cargo RUSTUP_HOME=/home/<user>/.rustup scripts/hostname-fixture.sh" >&2
    fi
    exit 1
fi

# 实况测试是 #[ignore] 的，需显式 --ignored。在 lib / bin 两个同名测试
# 二进制里，用 --list 挑出真正包含该实况测试的那个再执行。
BIN=""
for b in $(find target/debug/deps -maxdepth 1 -name 'agent_shell_rootd-*' -type f -perm -111); do
    if "$b" --list 2>/dev/null | grep -q '^tests::hostname_set_snapshot_and_restore_live'; then
        BIN="$b"
        break
    fi
done
if [ -z "$BIN" ]; then
    echo "FATAL: 找不到含实况夹具的 rootd 测试二进制，先跑 cargo test --no-run。" >&2
    exit 1
fi

i=1
while [ "$i" -le "$RUNS" ]; do
    echo "== RUN $i =="
    echo "pre: $ORIGINAL"
    "$BIN" hostname_set_snapshot_and_restore_live --ignored --nocapture
    POST="$(snapshot)"
    echo "post: $POST"
    if [ "$POST" != "$ORIGINAL" ]; then
        echo "FATAL: 第 $i 遍夹具结束后主机名漂移: $ORIGINAL -> $POST" >&2
        exit 1
    fi
    i=$((i + 1))
done

echo "OK: $RUNS 遍夹具通过，主机名保持 $ORIGINAL"
