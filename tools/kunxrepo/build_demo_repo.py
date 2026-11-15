#!/usr/bin/env python3
"""构建 KunxOS demo 软件源（rootfs 用）：打包示例应用 → 签名索引 → 增量。

用法：
  python3 tools/kunxrepo/build_demo_repo.py <repo-dir> <apps-elf-dir>
     [--key keys/kunx-repo.key]

- 读取 tools/kunxrepo/manifests/*.json，用对应应用 ELF（<apps-elf-dir>/<name>/target/...
  /release/<name>）构建 .kunx；
- 生成签名索引（Packages + Packages.sig + Packages.meta）到 <repo-dir>。

manifest 可选扩展字段（以 "_" 开头，仅本脚本使用）：
  _elf       指定入口 ELF 路径（相对仓库顶层目录，即 <apps-elf-dir> 的上级），
             用于复用既有二进制打包依赖链示例包（libbase/libgfx/depdemo 复用
             kunxpkg 自身 ELF，安装后 `run /bin/depdemo keys` 即自检）；
  _elf_text  用给定文本生成最小载荷（不执行，仅供环/缺失依赖等负例包使用，
             避免负例包占用 ramdisk 空间）；
  _badsig    构建后破坏末尾 64 字节签名区（签名校验失败负例：证书链有效、
             包签名无效，安装必须拒绝）。
"""
import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

TINY_ELF_PATH = "/tmp/kunx-demo-tiny-payload.bin"


def python_bin():
    """子进程解释器：sys.executable 异常（嵌入解释器/被包装）时回退 PATH 的 python3。"""
    exe = sys.executable or ""
    if os.path.basename(exe).startswith("python"):
        return exe
    return "python3"


PY = python_bin()


def find_elf(m, apps_dir):
    """按 manifest 扩展字段或默认约定定位入口 ELF。"""
    if "_elf" in m:
        top = os.path.dirname(os.path.abspath(os.path.normpath(apps_dir)))
        return os.path.join(top, m["_elf"])
    if "_elf_text" in m:
        with open(TINY_ELF_PATH, "wb") as f:
            f.write(m["_elf_text"].encode() + b"\n")
        return TINY_ELF_PATH
    return os.path.join(apps_dir, m["name"], "target", "riscv64gc-unknown-none-elf", "release", m["name"])


def corrupt_signature(pkg_path):
    """翻转末 8 字节（位于 64 字节 Ed25519 签名区内）→ 签名校验必失败。"""
    with open(pkg_path, "r+b") as f:
        f.seek(0, os.SEEK_END)
        size = f.tell()
        f.seek(size - 8)
        tail = f.read(8)
        f.seek(size - 8)
        f.write(bytes(b ^ 0xFF for b in tail))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="输出仓库目录")
    ap.add_argument("apps_dir", help="apps 源码根目录（按 target 路径定位 ELF）")
    ap.add_argument("--key", default=os.path.join(HERE, "keys", "kunx-repo.key"))
    ap.add_argument("--manifest-dir", default=os.path.join(HERE, "manifests"))
    args = ap.parse_args()

    os.makedirs(args.repo, exist_ok=True)
    subprocess.run([PY, os.path.join(HERE, "kunxrepo.py"), "init", args.repo], check=True)

    dev_key = os.path.join(HERE, "keys", "kunx-dev.key")
    dev_cert = os.path.join(HERE, "keys", "kunx-dev.cert")
    if not os.path.exists(dev_cert):
        subprocess.run([PY, os.path.join(HERE, "genkeys.py"), "--issue-dev"], check=True)

    built = []
    for mf in sorted(os.listdir(args.manifest_dir)):
        if not mf.endswith(".json"):
            continue
        mpath = os.path.join(args.manifest_dir, mf)
        m = json.load(open(mpath))
        name = m["name"]
        elf = find_elf(m, args.apps_dir)
        if not os.path.exists(elf):
            print(f"[demo-repo] 跳过 {name}: 未找到 {elf}")
            continue
        out = os.path.join("/tmp", f"{name}-{m['version']}.kunx")
        cmd = [PY, os.path.join(HERE, "mkkunx.py"),
               "--manifest", mpath, "--elf", elf,
               "--key", dev_key, "--cert", dev_cert, "--out", out]
        for r in m.get("_resources", []):
            cmd += ["--res", r]
        subprocess.run(cmd, check=True)
        if m.get("_badsig"):
            corrupt_signature(out)
            print(f"[demo-repo] {name}: 已破坏签名区（签名校验负例）")
        subprocess.run([PY, os.path.join(HERE, "kunxrepo.py"), "add", args.repo, out], check=True)
        built.append((name, m["version"]))
        # add 可能已把包移入仓库（rename）；不再存在则跳过清理
        if os.path.exists(out):
            os.unlink(out)

    subprocess.run([PY, os.path.join(HERE, "kunxrepo.py"), "gen", args.repo, "--key", args.key], check=True)
    print(f"[demo-repo] 完成: {len(built)} 个包 -> {args.repo}")
    for n, v in built:
        print(f"  {n} {v}")


if __name__ == "__main__":
    main()
