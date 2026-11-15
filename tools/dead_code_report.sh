#!/usr/bin/env bash
# 汇总内核 dead_code 告警清单（不修改任何源码）。
# 用法：tools/dead_code_report.sh [输出文件]   默认 build/dead_code_report.txt
# 需要已安装 riscv64gc-unknown-none-elf target。
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-build/dead_code_report.txt}"
mkdir -p "$(dirname "$OUT")"
cargo check --release --manifest-path kunxcore/Cargo.toml \
  --target riscv64gc-unknown-none-elf --features strict-dead-code \
  --message-format short 2>&1 | grep -E 'warning: .*(never used|never read|never constructed|unused)' | sort > "$OUT" || true
echo "dead_code 告警 $(wc -l < "$OUT") 条 -> $OUT"
echo "按文件统计："
sed -E 's/^([^:]+):.*/\1/' "$OUT" | sort | uniq -c | sort -rn | head -30
