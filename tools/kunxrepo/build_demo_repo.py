#!/usr/bin/env python3
"""构建 KunxOS demo 软件源（rootfs 用）：打包示例应用 → 签名索引 → 增量。

用法：
  python3 tools/kunxrepo/build_demo_repo.py <repo-dir> <apps-elf-dir>
     [--key keys/kunx-repo.key]

- 读取 tools/kunxrepo/manifests/*.json，用对应应用 ELF（<apps-elf-dir>/<name>/target/...
  /release/<name>）构建 .kunx；
- 生成签名索引（Packages + Packages.sig + Packages.meta）到 <repo-dir>。
"""
import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="输出仓库目录")
    ap.add_argument("apps_dir", help="apps 源码根目录（按 target 路径定位 ELF）")
    ap.add_argument("--key", default=os.path.join(HERE, "keys", "kunx-repo.key"))
    ap.add_argument("--manifest-dir", default=os.path.join(HERE, "manifests"))
    args = ap.parse_args()

    os.makedirs(args.repo, exist_ok=True)
    subprocess.run([sys.executable, os.path.join(HERE, "kunxrepo.py"), "init", args.repo], check=True)

    dev_key = os.path.join(HERE, "keys", "kunx-dev.key")
    dev_cert = os.path.join(HERE, "keys", "kunx-dev.cert")
    if not os.path.exists(dev_cert):
        subprocess.run([sys.executable, os.path.join(HERE, "genkeys.py"), "--issue-dev"], check=True)

    built = []
    for mf in sorted(os.listdir(args.manifest_dir)):
        if not mf.endswith(".json"):
            continue
        mpath = os.path.join(args.manifest_dir, mf)
        m = json.load(open(mpath))
        name = m["name"]
        elf = os.path.join(args.apps_dir, name, "target", "aarch64-unknown-none", "release", name)
        if not os.path.exists(elf):
            print(f"[demo-repo] 跳过 {name}: 未找到 {elf}")
            continue
        out = os.path.join("/tmp", f"{name}-{m['version']}.kunx")
        cmd = [sys.executable, os.path.join(HERE, "mkkunx.py"),
               "--manifest", mpath, "--elf", elf,
               "--key", dev_key, "--cert", dev_cert, "--out", out]
        for r in m.get("_resources", []):
            cmd += ["--res", r]
        subprocess.run(cmd, check=True)
        subprocess.run([sys.executable, os.path.join(HERE, "kunxrepo.py"), "add", args.repo, out], check=True)
        built.append((name, m["version"]))
        # add 可能已把包移入仓库（rename）；不再存在则跳过清理
        if os.path.exists(out):
            os.unlink(out)

    subprocess.run([sys.executable, os.path.join(HERE, "kunxrepo.py"), "gen", args.repo, "--key", args.key], check=True)
    print(f"[demo-repo] 完成: {len(built)} 个包 -> {args.repo}")
    for n, v in built:
        print(f"  {n} {v}")


if __name__ == "__main__":
    main()
