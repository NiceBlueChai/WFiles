#!/usr/bin/env bash
# WFiles TUI 冒烟：用管道喂命令跑真的 examples/wfiles-tui.rs，逐项验
# 「这个最小前端只靠 contract::Core 就能拿到东西」。
#
# 判据落在**内容**上，不是「命令没报错」：
#   · preview 那句来自磁盘上真写进去的文件（core::preview::load）
#   · 撤销标签来自 core 的 History::last_describe
#   · 重名策略回显的是内核里的权威值
#   · 搜索命中是从事件流里 drain 出来的，不在回执里
#   · volumes 验「core 至少报出一块盘」：把 `core.volumes()` 换空列表会红
#     （负验证过）。这一条有环境前提——没挂任何外置盘的机器上会红。
# 另外全文不许出现「不认得这条命令」，否则说明有命令没接上契约。
#
# 这台机器的两个坑：exFAT 卷要关增量缓存，git/cargo 要 DEVELOPER_DIR。
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export CARGO_INCREMENTAL=0

ROOT="$(mktemp -d)"
OUT="$(mktemp)"
trap 'rm -rf "$ROOT" "$OUT"' EXIT

mkdir -p "$ROOT/src"
printf 'fn main() { println!("tui-contract-ok"); }\n' > "$ROOT/main.rs"
printf 'alpha needle\nbeta\n' > "$ROOT/notes.txt"

{
  printf 'preview %s/main.rs\n' "$ROOT"
  printf 'mkdir sub\n'
  printf 'policy replace\n'
  printf 'search content:needle\n'
  printf 'volumes\n'
  printf 'quit\n'
} | cargo run --quiet --example wfiles-tui -- "$ROOT" > "$OUT"

fail=0
check() {
  if grep -qF -- "$1" "$OUT"; then
    printf '  ok   %s\n' "$2"
  else
    printf '  FAIL %s\n' "$2"
    fail=1
  fi
}

check 'tui-contract-ok' 'preview 读到了磁盘上那份文件（Core::preview）'
check '可撤销：新建“sub”' '状态行说清下一步撤销什么（Core::undo_label）'
check '重名：replace' '策略回显的是内核里的权威值（Core::conflict_policy）'
check '[命中] notes.txt' '搜索命中是从事件流 drain 出来的名字，不是初始清单'
check '搜索结束' '收到 SearchDone 才收工，不把零命中说成搜完了'
check '磁盘：' 'volumes 命令被接受并走了 Core::volumes'

if grep -qF '不认得这条命令' "$OUT"; then
  printf '  FAIL 有命令没接上契约\n'
  fail=1
fi

if (( fail )); then
  printf '\n--- TUI 输出 ---\n'
  cat "$OUT"
  exit 1
fi
printf 'TUI 冒烟：6/6 通过\n'
