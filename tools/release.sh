#!/usr/bin/env bash
# KunxOS 版本发布脚本。
#
# 用法：
#   tools/release.sh [版本号]      # 版本号缺省为 v0.1.0
#
# 步骤：
#   1. 检查 git 仓库状态（是否 git 仓库、工作区是否干净、当前分支）
#   2. 从 git log 生成 CHANGELOG.md
#   3. 打 annotated tag <版本号>
#   4. 输出发布摘要

set -euo pipefail

# ---- 颜色输出 ----
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info() { printf "${CYAN}[INFO]${NC} %s\n" "$*"; }
ok()   { printf "${GREEN}[ OK ]${NC} %s\n" "$*"; }
warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$*"; }
die()  { printf "${RED}[FAIL]${NC} %s\n" "$*" >&2; exit 1; }

# ---- 定位项目根目录 ----
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="${1:-v0.1.0}"

# ====================================================================
# 1. 检查 git 仓库状态
# ====================================================================
info "1/4 检查 git 仓库状态"

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    die "当前目录不是 git 仓库：$ROOT_DIR"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
info "   仓库根目录：$(git rev-parse --show-toplevel)"
info "   当前分支：  $BRANCH"

if [ -n "$(git status --porcelain)" ]; then
    warn "   工作区存在未提交的变更，请先提交后再发布："
    git status --short
    die "发布中止：工作区不干净"
fi
ok "   工作区干净"

# ====================================================================
# 2. 生成 CHANGELOG.md（从 git log）
# ====================================================================
info "2/4 生成 CHANGELOG.md"

RELEASE_DATE="$(date +%Y-%m-%d)"

# 首个版本用全部历史，否则用「上一个 tag .. HEAD」。
PREV_TAG="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -n "$PREV_TAG" ]; then
    RANGE="${PREV_TAG}..HEAD"
    info "   提交范围：${RANGE}"
else
    RANGE="HEAD"
    info "   提交范围：全部历史（首个版本）"
fi

generate_changelog() {
    local version="$1" date="$2" range="$3"
    local added=() changed=() fixed=() other=()

    # 按提交信息前缀归类：feat/add -> Added；fix -> Fixed；其余 -> Changed/Other。
    while IFS='|' read -r subject hash; do
        [ -z "$subject" ] && continue
        local entry="- ${subject} (${hash})"
        case "$subject" in
            feat:*|feat\(*|add:*|add\(*|新增:*|添加:*)          added+=("$entry") ;;
            fix:*|fix\(*|修复:*|fixup!*)                          fixed+=("$entry") ;;
            refactor:*|chore:*|docs:*|perf:*|style:*|test:*|变更:*|更新:*) changed+=("$entry") ;;
            *)                                                     other+=("$entry") ;;
        esac
    done < <(git log --format='%s|%h' "$range")

    local tmp="CHANGELOG.md.tmp"
    {
        echo "# Changelog"
        echo ""
        echo "## [${version}] - ${date}"
        echo ""
        if [ "${#added[@]}" -gt 0 ]; then
            echo "### Added"
            printf '%s\n' "${added[@]}"
            echo ""
        fi
        if [ "${#changed[@]}" -gt 0 ]; then
            echo "### Changed"
            printf '%s\n' "${changed[@]}"
            echo ""
        fi
        if [ "${#fixed[@]}" -gt 0 ]; then
            echo "### Fixed"
            printf '%s\n' "${fixed[@]}"
            echo ""
        fi
        if [ "${#other[@]}" -gt 0 ]; then
            echo "### Other"
            printf '%s\n' "${other[@]}"
            echo ""
        fi
    } > "$tmp"

    # 保留既有内容（去掉旧的 `# Changelog` 标题行），把新版本块前置。
    if [ -f CHANGELOG.md ]; then
        tail -n +2 CHANGELOG.md >> "$tmp"
    fi

    mv "$tmp" CHANGELOG.md
}

generate_changelog "$VERSION" "$RELEASE_DATE" "$RANGE"
ok "   已写入 CHANGELOG.md"

# ====================================================================
# 3. 打 tag
# ====================================================================
info "3/4 打 tag ${VERSION}"

if git rev-parse "${VERSION}" >/dev/null 2>&1; then
    die "tag ${VERSION} 已存在，请勿重复发布"
fi

git tag -a "${VERSION}" -m "KunxOS ${VERSION} 发布"
ok "   已创建 annotated tag ${VERSION}"

# ====================================================================
# 4. 输出发布摘要
# ====================================================================
info "4/4 发布摘要"
echo ""
echo "  版本号:   ${VERSION}"
echo "  发布日期: ${RELEASE_DATE}"
echo "  分支:     ${BRANCH}"
echo "  提交:     $(git rev-parse --short HEAD)"
echo "  提交数:   $(git rev-list --count "${RANGE}")"
echo "  变更文件: CHANGELOG.md"
echo "  tag:      ${VERSION} -> $(git rev-parse --short "${VERSION}")"
echo ""
ok "发布完成"
