#!/usr/bin/env python3
"""KunxOS 软件源仓库工具：生成已签名索引、增量补丁，并提供 HTTP 分发。

用法：
  python3 tools/kunxrepo/kunxrepo.py init <repo-dir> [--key keys/kunx-repo.key]
  python3 tools/kunxrepo/kunxrepo.py add <repo-dir> <pkg.kunx>...
  python3 tools/kunxrepo/kunxrepo.py gen <repo-dir>
  python3 tools/kunxrepo/kunxrepo.py serve <repo-dir> [--port 8080] [--host 0.0.0.0]

仓库目录布局：
  <repo-dir>/files/<name>-<version>.kunx   包文件
  <repo-dir>/Packages                       索引（Debian Packages 风格 + 扩展字段）
  <repo-dir>/Packages.sig                   Ed25519 签名（仓库密钥）
  <repo-dir>/Packages.meta                  Revision / Count
  <repo-dir>/delta-<old>-<new>              增量补丁
  <repo-dir>/delta-<old>-<new>.sig
  <repo-dir>/.kunx-state.json               revision 状态

安全模型：索引签名是真实性的根本保证；HTTP 明文可接受（TLS 是纵深防御，
设备端 kunxpkg 对 https:// 源当前明确拒绝）。
"""
import argparse
import hashlib
import json
import os
import struct
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ed25519

DELTA_MAGIC = b"KUNXDELTA1"
# 与设备端 kunxpkg/src/repo.rs 的 INDEX_MAX=16384 对齐：超限索引会被
# 客户端 fetch_full 直接拒绝（pkg update 整体不可用），生成期即报错。
INDEX_MAX = 16384


def load_key(path):
    with open(path, "rb") as f:
        return f.read()


def put_u32(b, off, v):
    b[off:off + 4] = struct.pack("<I", v)


def parse_kunx_manifest(path):
    """读取 .kunx 的 manifest.json（解析头部 + manifest 区）。"""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"KUNXPKG1":
        sys.exit(f"{path}: 不是 .kunx 包")
    mo = struct.unpack_from("<I", data, 16)[0]
    ml = struct.unpack_from("<I", data, 20)[0]
    return json.loads(data[mo:mo + ml])


def ver_key(v):
    """'1.2.3-rc' → (1, 2, 3, 'rc') 用于版本比较（简化 semver）。"""
    core = v.split("-")[0]
    pre = v.split("-")[1] if "-" in v else ""
    parts = []
    for x in core.split("."):
        try:
            parts.append(int(x))
        except ValueError:
            parts.append(0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2], pre)


def build_index(repo_dir):
    files_dir = os.path.join(repo_dir, "files")
    latest = {}  # name → (ver_key, record_text)
    if os.path.isdir(files_dir):
        for fn in sorted(os.listdir(files_dir)):
            if not fn.endswith(".kunx"):
                continue
            p = os.path.join(files_dir, fn)
            m = parse_kunx_manifest(p)
            size = os.path.getsize(p)
            h = hashlib.sha512(open(p, "rb").read()).digest()[:16].hex()
            rec = [f"Package: {m['name']}", f"Version: {m.get('version', '0.0.0')}"]
            if m.get("api_level"):
                rec.append(f"ApiLevel: {m['api_level']}")
            perms = m.get("permissions", [])
            if perms:
                rec.append("Permissions: " + ",".join(perms))
            deps = m.get("dependencies", {})
            if deps:
                rec.append("Depends: " + ",".join(f"{k} {v}" for k, v in deps.items()))
            prov = m.get("provides", [])
            if prov:
                rec.append("Provides: " + ",".join(prov))
            conf = m.get("conflicts", [])
            if conf:
                rec.append("Conflicts: " + ",".join(conf))
            if m.get("description"):
                rec.append("Description: " + m["description"])
            # Filename 相对源根：包文件位于 files/ 子目录（客户端 join <src>/<Filename>）
            rec.append(f"Filename: files/{fn}")
            rec.append(f"Size: {size}")
            rec.append(f"KunxHash: {h}")
            rec.append("SignedBy: kunx-repo")
            text = "\n".join(rec)
            name = m["name"]
            vk = ver_key(m.get("version", "0.0.0"))
            if name not in latest or vk > latest[name][0]:
                latest[name] = (vk, text)
    records = [r for _, r in sorted(latest.values(), key=lambda x: x[0])]
    return ("\n\n".join(records) + "\n\n").encode("utf-8") if records else b""


def compute_delta(old_idx, new_idx):
    """对比新旧索引，生成 KUNXDELTA1 补丁。"""
    def records(idx):
        out = {}
        for block in idx.decode("utf-8").split("\n\n"):
            block = block.strip("\n")
            if not block:
                continue
            name = None
            for line in block.splitlines():
                if line.startswith("Package: "):
                    name = line[len("Package: "):]
                    break
            if name:
                out[name] = block
        return out

    old = records(old_idx)
    new = records(new_idx)
    lines = [DELTA_MAGIC.decode(), "Base: 0", "Target: 0", ""]
    for name in sorted(new):
        if name not in old or old[name] != new[name]:
            lines.append(f"A {name}")
            lines.append(new[name])
            lines.append("")
    for name in sorted(old):
        if name not in new:
            lines.append(f"D {name}")
    return ("\n".join(lines)).encode("utf-8")


def cmd_init(args):
    os.makedirs(os.path.join(args.repo, "files"), exist_ok=True)
    state = {"revision": 0, "prev_packages": None}
    with open(os.path.join(args.repo, ".kunx-state.json"), "w") as f:
        json.dump(state, f)
    print(f"[repo] initialized {args.repo}")


def cmd_add(args):
    for p in args.pkg:
        if not p.endswith(".kunx"):
            sys.exit(f"{p}: 需要 .kunx 文件")
        m = parse_kunx_manifest(p)
        dst = os.path.join(args.repo, "files", f"{m['name']}-{m['version']}.kunx")
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(p, "rb") as src, open(dst, "wb") as out:
            out.write(src.read())
        print(f"[repo] added {m['name']} {m['version']} -> {dst}")


def cmd_gen(args):
    key = load_key(args.key)
    idx = build_index(args.repo)
    state_path = os.path.join(args.repo, ".kunx-state.json")
    state = json.load(open(state_path)) if os.path.exists(state_path) else {"revision": 0, "prev_packages": None}
    new_rev = state.get("revision", 0) + 1

    if len(idx) > INDEX_MAX:
        raise SystemExit(
            f"[repo] 索引 {len(idx)} 字节超过设备端上限 {INDEX_MAX}（约 40-50 个包）；"
            "需扩容 kunxpkg/src/repo.rs 的 INDEX_MAX 后重新生成"
        )
    with open(os.path.join(args.repo, "Packages"), "wb") as f:
        f.write(idx)
    with open(os.path.join(args.repo, "Packages.sig"), "wb") as f:
        f.write(ed25519.sign(key, idx))
    meta = f"Revision: {new_rev}\nCount: {idx.count(b'Package: ')}\n".encode()
    with open(os.path.join(args.repo, "Packages.meta"), "wb") as f:
        f.write(meta)

    # 增量补丁（对比上一修订）
    prev = state.get("prev_packages")
    if prev:
        delta = compute_delta(prev.encode("utf-8"), idx)
        if delta:
            old_rev = state.get("revision", 0)
            dname = f"delta-{old_rev}-{new_rev}"
            with open(os.path.join(args.repo, dname), "wb") as f:
                f.write(delta)
            with open(os.path.join(args.repo, dname + ".sig"), "wb") as f:
                f.write(ed25519.sign(key, delta))

    state["revision"] = new_rev
    state["prev_packages"] = idx.decode("utf-8")
    with open(state_path, "w") as f:
        json.dump(state, f)
    print(f"[repo] revision {new_rev}: {idx.count(b'Package: ')} packages, index {len(idx)} bytes")
    if prev:
        print(f"[repo] delta {old_rev}->{new_rev} generated")


class RepoHandler(BaseHTTPRequestHandler):
    repo_dir = "."

    def do_GET(self):
        rel = self.path.lstrip("/")
        # 防路径穿越
        if ".." in rel:
            self.send_error(403)
            return
        p = os.path.join(self.repo_dir, rel)
        if not os.path.isfile(p):
            self.send_error(404)
            return
        data = open(p, "rb").read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        sys.stderr.write("[repo] %s\n" % (fmt % args))


def cmd_serve(args):
    RepoHandler.repo_dir = os.path.abspath(args.repo)
    srv = ThreadingHTTPServer((args.host, args.port), RepoHandler)
    print(f"[repo] serving {args.repo} at http://{args.host}:{args.port}/")
    print("[repo] 索引: /Packages  /Packages.sig  /Packages.meta")
    srv.serve_forever()


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("init")
    p.add_argument("repo")
    p = sub.add_parser("add")
    p.add_argument("repo")
    p.add_argument("pkg", nargs="+")
    p = sub.add_parser("gen")
    p.add_argument("repo")
    p.add_argument("--key", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "keys", "kunx-repo.key"))
    p = sub.add_parser("serve")
    p.add_argument("repo")
    p.add_argument("--port", type=int, default=8080)
    p.add_argument("--host", default="0.0.0.0")
    args = ap.parse_args()
    {"init": cmd_init, "add": cmd_add, "gen": cmd_gen, "serve": cmd_serve}[args.cmd](args)


if __name__ == "__main__":
    main()
