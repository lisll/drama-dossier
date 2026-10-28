#!/usr/bin/env bash
#
# 提交前质量门槛：与 .github/workflows/desktop.yml 的 Format / Clippy / Test 三步一致。
# 本地跑通即等同于 CI 会跑通，避免「推上去才发现红的」。
#
#   scripts/check.sh              # 全量：Rust（fmt / clippy / test）+ Web + JSON
#   scripts/check.sh --fast       # 跳过 cargo test，用于 pre-commit
#   scripts/check.sh --rust       # 只跑 Rust 三项
#   scripts/check.sh --web        # 只跑 node --check
#   scripts/check.sh --json       # 只校验 JSON
#
# 未指定范围时默认全选；缺少某个工具时跳过该项并给出提示，不算失败。
# 退出码：0 通过；1 有检查失败；2 用法错误。

set -uo pipefail

cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)" || exit 2

usage() {
  sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
}

fast=0
run_rust=0
run_web=0
run_json=0
any_scope=0

for arg in "$@"; do
  case "$arg" in
    --fast) fast=1 ;;
    --rust) run_rust=1; any_scope=1 ;;
    --web) run_web=1; any_scope=1 ;;
    --json) run_json=1; any_scope=1 ;;
    -h|--help) usage; exit 0 ;;
    *)
      printf 'check.sh: 未知参数：%s\n\n' "$arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ "$any_scope" -eq 0 ]; then
  run_rust=1
  run_web=1
  run_json=1
fi

fail=0
skipped=""

require() {
  command -v "$1" >/dev/null 2>&1 && return 0
  printf '==> 跳过需要 %s 的检查（当前环境找不到该命令）\n' "$1"
  skipped="${skipped}${skipped:+, }$1"
  return 1
}

# clippy / test 需要先能编译 src-tauri。Linux 上缺系统依赖时，
# 报出来的是「装包失败」而不是「代码有问题」，这种假失败会让人直接关掉 hook，
# 所以这里判一次依赖是否齐全，缺了就如实跳过。
rust_env_ready() {
  [ "$(uname -s)" = "Linux" ] || return 0
  command -v pkg-config >/dev/null 2>&1 || return 0
  pkg-config --exists webkit2gtk-4.1 2>/dev/null || return 1
  pkg-config --exists libsoup-3.0 2>/dev/null || return 1
  return 0
}

step() {
  local title="$1"
  shift
  printf '==> %s\n' "$title"
  if "$@"; then
    printf '    OK\n'
    return 0
  fi
  printf '    FAILED\n' >&2
  fail=1
  return 1
}

# 成功时安静，失败时重跑一次把错误原样打出来。
step_quiet() {
  local title="$1"
  shift
  printf '==> %s\n' "$title"
  if "$@" >/dev/null 2>&1; then
    printf '    OK\n'
    return 0
  fi
  printf '    FAILED\n' >&2
  "$@" || true
  fail=1
  return 1
}

if [ "$run_rust" -eq 1 ] && require cargo; then
  step "Format：cargo fmt --all -- --check" \
    cargo fmt --all -- --check
  if rust_env_ready; then
    # 与 CI 一致：授权形态 feature 互斥（official-release 与 license-test-mode 不能同开，
    # official-release 在填好正式公钥前必然编译失败），不能用 --all-features；统一按内测版授权检查。
    license_feature="--features drama-dossier/license-test-mode"
    step "Clippy：cargo clippy --workspace --all-targets --no-deps $license_feature -- -D warnings" \
      cargo clippy --workspace --all-targets --no-deps $license_feature -- -D warnings
    # windows 目标交叉 lint：抓只在 Windows 编译路径上的问题（死代码、cfg 拼写错等）。
    # tauri-winres 要调 llvm-rc（mac 没有），用桩糊弄过构建脚本——clippy 不链接资源，
    # 空产物无害。桩解析 /FO 参数生成空文件后退出 0。
    if command -v cargo >/dev/null 2>&1; then
      stub_dir=$(mktemp -d)
      {
        printf '#!/bin/bash\nprev=""\nfor a in "$@"; do\n  case "$prev" in\n    /FO|-FO|/fo|-fo) mkdir -p "$(dirname "$a")"; : > "$a" ;;\n  esac\n  prev="$a"\ndone\nexit 0\n'
      } > "$stub_dir/llvm-rc"
      chmod +x "$stub_dir/llvm-rc"
      step "Clippy（windows 目标）：--target x86_64-pc-windows-msvc -- -D warnings" \
        env PATH="$stub_dir:$PATH" cargo clippy --workspace --all-targets \
          --target x86_64-pc-windows-msvc --no-deps $license_feature -- -D warnings
    fi
    if [ "$fast" -eq 1 ]; then
      printf '==> 跳过 Test（--fast，pre-push 或 CI 会跑）\n'
    else
      step "Test：cargo test --workspace $license_feature" \
        cargo test --workspace $license_feature
      step "Check：cargo check -p drama-dossier（默认不开授权 feature）" \
        cargo check -p drama-dossier
    fi
  else
    printf '==> 跳过 Clippy / Test：本机缺 webkit2gtk-4.1 或 libsoup-3.0，编译不了 src-tauri\n'
    printf '    安装见 docs/BUILD.md「本地开发」；这些检查由 CI 兜底。\n'
    skipped="${skipped}${skipped:+, }webkit2gtk-4.1/libsoup-3.0"
  fi
fi

if [ "$run_web" -eq 1 ] && require node; then
  # src/main.js 用了可选链（?.），Node 14 以下解析不了，会变成假失败。
  node_major=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)
  if [ "${node_major:-0}" -ge 14 ] 2>/dev/null; then
    step "语法检查：node --check src/main.js" \
      node --check src/main.js
  else
    printf '==> 跳过 node --check：当前 node 是 %s，低于 14 无法解析 src/main.js\n' \
      "$(node --version 2>/dev/null)"
    skipped="${skipped}${skipped:+, }node(>=14)"
  fi
fi

if [ "$run_json" -eq 1 ] && require python3; then
  step_quiet "JSON 校验：src-tauri/tauri.conf.json" \
    python3 -m json.tool src-tauri/tauri.conf.json
  step_quiet "JSON 校验：src-tauri/capabilities/default.json" \
    python3 -m json.tool src-tauri/capabilities/default.json
fi

printf '\n'
if [ -n "$skipped" ]; then
  printf '注意：因缺少 %s，有检查被跳过。\n' "$skipped" >&2
fi
if [ "$fail" -eq 0 ]; then
  printf '质量门槛通过。\n'
  exit 0
fi
printf '质量门槛未通过，请修好后重试。\n' >&2
exit 1
