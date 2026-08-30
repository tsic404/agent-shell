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

cargo test -p agent-shell-rootd --lib hostname_set_snapshot_and_restore_live --no-run

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
