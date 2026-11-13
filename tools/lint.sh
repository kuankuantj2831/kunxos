#!/usr/bin/env bash
# KunxOS 代码审查脚本
#
# 功能：
#   1. 检查所有 Rust 代码的 warnings（cargo check）
#   2. 统计代码行数
#   3. 检查 TODO/FIXME 注释
#   4. 输出审查报告（同时打印到终端并写入报告文件）
#
# 用法：
#   tools/lint.sh [报告文件路径]
#
# 环境变量：
#   TARGET  编译目标架构（默认 riscv64gc-unknown-none-elf）

set -uo pipefail

TOPDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${TARGET:-riscv64gc-unknown-none-elf}"
REPORT_FILE="${1:-${TOPDIR}/lint-report.txt}"

cd "$TOPDIR" || exit 1

# 仅检查仓库源码与未忽略的新源码，排除本地工具链和构建缓存。
mapfile -t CRATES < <(
  git ls-files --cached --others --exclude-standard -- '**/Cargo.toml' |
    while IFS= read -r path; do [ ! -f "$path" ] || printf '%s\n' "$path"; done |
    sort -u
)
mapfile -t RS_FILES < <(
  git ls-files --cached --others --exclude-standard -- '*.rs' |
    while IFS= read -r path; do [ ! -f "$path" ] || printf '%s\n' "$path"; done |
    sort -u
)

# 统一输出：既打印到终端，又写入报告文件。
: > "$REPORT_FILE"
log() {
  printf '%s\n' "$*" | tee -a "$REPORT_FILE"
}

WARNINGS=0
ERRORS=0
declare -a FAILED_CRATES=()

log "================================================================"
log " KunxOS 代码审查报告"
log " 生成时间: $(date '+%Y-%m-%d %H:%M:%S')"
log " 目标架构: ${TARGET}"
log "================================================================"
log ""

# ---------------------------------------------------------------------------
# 1. 检查 Rust 代码 warnings
# ---------------------------------------------------------------------------
log "[1/3] Rust 编译器 Warnings 检查"
log "----------------------------------------------------------------"

for manifest in "${CRATES[@]}"; do
  crate_dir="$(dirname "$manifest")"
  crate_name="$(basename "$crate_dir")"
  log_file="$(mktemp)"

  # 宿主工具与测试使用宿主目标，裸机系统组件使用 TARGET。
  target_args=(--target "$TARGET")
  case "$manifest" in
    kos/*|tests/*|tools/*|*/hosttest/*) target_args=() ;;
  esac
  if cargo check "${target_args[@]}" --manifest-path "$manifest" >"$log_file" 2>&1; then
    # 统计真实的 warning 数量：去掉 cargo 末尾的 "generated N warning(s)" 汇总行。
    total_warn_lines="$(grep -cE '^warning:' "$log_file" || true)"
    summary_lines="$(grep -cE '^warning: .* generated [0-9]+ warning' "$log_file" || true)"
    crate_warnings=$((total_warn_lines - summary_lines))

    if [ "$crate_warnings" -gt 0 ]; then
      WARNINGS=$((WARNINGS + crate_warnings))
      log "  [WARN] ${crate_name}: ${crate_warnings} warning(s)"
      grep -E '^warning:' "$log_file" |
        grep -vE ' generated [0-9]+ warning$' |
        sed 's/^/         /' |
        tee -a "$REPORT_FILE"
    else
      log "  [ OK ] ${crate_name}: 0 warnings"
    fi
  else
    ERRORS=$((ERRORS + 1))
    FAILED_CRATES+=("$crate_name")
    log "  [FAIL] ${crate_name}: cargo check 失败"
    sed 's/^/         /' "$log_file" | tee -a "$REPORT_FILE"
  fi

  rm -f "$log_file"
done

log "----------------------------------------------------------------"
log "  Warnings 合计: ${WARNINGS}"
log "  编译失败 crate: ${ERRORS}"
log ""

# ---------------------------------------------------------------------------
# 2. 统计代码行数
# ---------------------------------------------------------------------------
log "[2/3] 代码行数统计"
log "----------------------------------------------------------------"

TOTAL_LINES=0

for f in "${RS_FILES[@]}"; do
  lines="$(wc -l < "$f")"
  TOTAL_LINES=$((TOTAL_LINES + lines))
done

log "  Rust 源文件数量: ${#RS_FILES[@]}"
log "  Rust 代码总行数: ${TOTAL_LINES}"
log ""

# 按顶级 crate 目录汇总行数。
for manifest in "${CRATES[@]}"; do
  crate_dir="$(dirname "$manifest")"
  crate_name="$(basename "$crate_dir")"
  crate_lines=0
  for f in "${RS_FILES[@]}"; do
    case "$f" in
      "${crate_dir}/"*) crate_lines=$((crate_lines + $(wc -l < "$f"))) ;;
    esac
  done
  log "  ${crate_lines}  ${crate_dir#./}"
done

log ""

# ---------------------------------------------------------------------------
# 3. 检查 TODO/FIXME 注释
# ---------------------------------------------------------------------------
log "[3/3] TODO/FIXME 注释检查"
log "----------------------------------------------------------------"

todo_out="$(grep -nE 'TODO|FIXME' "${RS_FILES[@]}" 2>/dev/null || true)"

if [ -n "$todo_out" ]; then
  todo_count="$(printf '%s\n' "$todo_out" | wc -l)"
  log "  发现 ${todo_count} 处 TODO/FIXME："
  printf '%s\n' "$todo_out" | sed 's/^/    /' | tee -a "$REPORT_FILE"
else
  log "  未发现 TODO/FIXME 注释。"
fi

log ""

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
log "================================================================"
log " 审查结论"
log "----------------------------------------------------------------"
log "  Warnings: ${WARNINGS}"
log "  编译失败: ${ERRORS}"
log "  Rust 代码总行数: ${TOTAL_LINES}"
log "  TODO/FIXME: ${todo_count:-0}"
log "================================================================"
log " 报告已保存至: ${REPORT_FILE}"

# 有 warnings 或编译失败时返回非零退出码，便于接入 CI。
if [ "$WARNINGS" -gt 0 ] || [ "$ERRORS" -gt 0 ]; then
  exit 1
fi
exit 0
