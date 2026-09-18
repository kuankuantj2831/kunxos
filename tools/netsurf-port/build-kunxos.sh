#!/bin/bash
# NetSurf 全家桶 → musl 静态库交叉编译（KunxOS 用）。
# 用法：bash build-kunxos.sh [组件名...]   （缺省按依赖序编全部）
set -u
TOP=/home/kuankuantj/桌面/kunxos/vendor/netsurf
export CC="/home/kuankuantj/桌面/kunxos/tools/musl-cross/bin/musl-gcc -fno-link-libatomic -no-pie"
export NSSHARED="$TOP/buildsystem"
export PREFIX="$TOP/install"
BUILD="$TOP/build-kunxos.log"

build_one() {
    local name=$1; shift
    echo "=== building $name ===" | tee -a "$BUILD"
    (cd "$TOP/$name" && /usr/bin/make COMPONENT_TYPE=lib-static -j8 "$@" >> "$BUILD" 2>&1 \
        && /usr/bin/make install COMPONENT_TYPE=lib-static "$@" >> "$BUILD" 2>&1) \
        || { echo "FAILED: $name (see $BUILD tail)"; tail -20 "$BUILD"; return 1; }
    echo "OK: $name"
}

ORDER=(libparserutils libwapcaplet libhubbub libcss libdom:WITH_EXPAT_BINDING=no libnslog libnsgif libnsbmp)
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
