#!/bin/bash
# NetSurf 全家桶 → musl 静态库交叉编译（KunxOS riscv64 用）。
# 与 build-kunxos.sh（aarch64）同流程；PREFIX 独立，避免两套架构产物混链。
# 前置：tools/riscv64-musl-cross（musl.cc 预编译 riscv64-linux-musl-cross）。
# 用法：bash build-riscv64.sh [组件名...]   （缺省按依赖序编全部）
set -u
TOP=/home/kuankuantj/桌面/kunxos/vendor/netsurf
CROSS=/home/kuankuantj/桌面/kunxos/tools/riscv64-musl-cross/bin
export CC="$CROSS/riscv64-linux-musl-gcc -no-pie"
export NSSHARED="$TOP/buildsystem"
export PREFIX="$TOP/install-riscv64"
BUILD="$TOP/build-riscv64.log"

build_one() {
    local name=$1; shift
    echo "=== building $name ===" | tee -a "$BUILD"
    (cd "$TOP/$name" && /usr/bin/make COMPONENT_TYPE=lib-static -j8 "$@" >> "$BUILD" 2>&1 \
        && /usr/bin/make install COMPONENT_TYPE=lib-static "$@" >> "$BUILD" 2>&1) \
        || { echo "FAILED: $name (see $BUILD tail)"; tail -20 "$BUILD"; return 1; }
    echo "OK: $name"
}

ORDER=(libparserutils libwapcaplet libhubbub libcss libdom:WITH_EXPAT_BINDING=no libnslog libnsgif libnsbmp libnsfb libnsutils)
if [ $# -gt 0 ]; then
    ORDER=("$@")
fi
for c in "${ORDER[@]}"; do
    lib=${c%%:*}
    IFS=':' read -ra FLAGS <<< "${c#*:}"
    [ "$lib" = "$c" ] && FLAGS=()
    build_one "$lib" "${FLAGS[@]}" || exit 1
done
echo "ALL DONE"
