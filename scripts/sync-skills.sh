#!/bin/sh
# sync-skills.sh — 从 Multica 平台技能库镜像 agent-shell 项目技能到 .skills/
#
# 用法：./scripts/sync-skills.sh
#
# 平台技能库是权威源；本脚本把 agent-shell 项目绑定的两个技能（SKILL.md +
# 全部子文件）逐字节拉取到 .skills/<name>/，作为 PR/diff/commit 审计渠道。
# 镜像不参与编译（.skills/ 不含 .rs，也不在任何 Cargo workspace member 中）。
#
# dash 兼容：不使用 pipefail、[[ ]]、数组。
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SKILLS_DIR="$ROOT_DIR/.skills"

command -v multica >/dev/null 2>&1 || { echo "error: multica CLI not found in PATH" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "error: jq not found in PATH" >&2; exit 1; }

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# sync_skill <name> <skill-id> — 重建 .skills/<name>/：清空后按平台内容逐字节写入。
sync_skill() {
    name="$1"
    id="$2"
    dest="$SKILLS_DIR/$name"
    json="$TMP_DIR/skill.json"
    paths="$TMP_DIR/paths"

    multica skill get "$id" --with-content --output json > "$json"

    # 响应形状校验：缺 .content 时在清空旧镜像前中止，避免写空 SKILL.md 后仍报 synced。
    jq -e '.content' "$json" >/dev/null 2>&1 \
        || { echo "error: $name: response missing .content" >&2; exit 1; }

    # 路径预校验：拒绝 ..（逃逸）、绝对路径、空路径、缺失 .path 产生的 "null"。
    # 用 <file 重定向而非管道读，使循环内 exit 1 真正中止脚本（管道丢进子 shell）。
    jq -r '.files[]? | .path // "null"' "$json" > "$paths"
    while IFS= read -r path; do
        case "$path" in
            *..*|/*|""|null) echo "error: $name: unsafe file path '$path'" >&2; exit 1 ;;
        esac
    done < "$paths"

    rm -rf "$dest"
    mkdir -p "$dest"
    # jq -j 原样输出、不追加换行，保证逐字节一致（部分文件无末尾换行）。
    jq -j '.content' "$json" > "$dest/SKILL.md"

    # 逐文件按已校验路径精确提取，不依赖逐行协议解析整个文件对象。
    while IFS= read -r path; do
        mkdir -p "$dest/$(dirname "$path")"
        jq -j --arg p "$path" '.files[] | select(.path == $p) | .content' "$json" > "$dest/$path"
    done < "$paths"

    echo "synced $name -> .skills/$name/"
}

sync_skill agent-shell-qa-testing ac704044-c4ef-46b9-8523-02c851bca37c
sync_skill agent-shell-design 3708650e-20cd-4d7d-9eb8-7227f5c3c7d5
