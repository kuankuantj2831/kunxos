#!/usr/bin/env python3
"""KunxOS 密钥与开发者证书签发工具。

用法：
  python3 tools/kunxrepo/genkeys.py [--dir keys] [--issue-dev]

- 生成（若缺失）根密钥 kunx-root、仓库密钥 kunx-repo、开发者密钥 kunx-dev；
  公钥写入 *.pub（32 字节原始），私钥写入 *.key。
- 根公钥与仓库公钥必须与设备端 kunxpkg/src/trust.rs 中内嵌的信任锚一致：
  首次生成后如需更新信任锚，请同步修改 trust.rs 并重新构建 kunxpkg。
- --issue-dev：用根私钥签发开发者证书 kunx-dev.cert（KU_PACKAGE，有效期
  valid_from..valid_to 秒，默认 2025-2035）。

注意：仓库中的 keys/ 为 DEMO 密钥（仅开发/测试用）。生产环境请离线生成，
根私钥严格保管。
"""
import argparse
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ed25519

CERT_MAGIC = b"KUNXCERT"
CERT_VERSION = 1
KU_PACKAGE = 1
KU_INDEX = 2
KU_CERT = 4

DEFAULT_FROM = 1735689600  # 2025-01-01 UTC
DEFAULT_TO = 2077056000  # 2035-01-01 UTC


def gen_key(path):
    import os
    sk = os.urandom(32)
    with open(path + ".key", "wb") as f:
        f.write(sk)
    with open(path + ".pub", "wb") as f:
        f.write(ed25519.publickey(sk))
    print(f"  generated {path}.key / {path}.pub")


def load_key(path):
    with open(path + ".key", "rb") as f:
        return f.read()


def build_cert(pubkey, parent_sk, key_usage, subject, valid_from, valid_to):
    """构造证书字节：头部 + pubkey + subject + parent_sig（父密钥签名）。"""
    subj = subject.encode("utf-8")
    head = CERT_MAGIC
    head += struct.pack("<I", CERT_VERSION)
    head += bytes([key_usage])
    head += struct.pack("<H", len(subj))
    head += struct.pack("<QQ", valid_from, valid_to)
    head += bytes(pubkey)
    assert len(head) == 63
    body = head + subj
    sig = ed25519.sign(parent_sk, body)
    return body + sig


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "keys"))
    ap.add_argument("--issue-dev", action="store_true", help="签发开发者证书 kunx-dev.cert")
    ap.add_argument("--subject", default="KunxOS Demo Developer")
    ap.add_argument("--valid-from", type=int, default=DEFAULT_FROM)
    ap.add_argument("--valid-to", type=int, default=DEFAULT_TO)
    args = ap.parse_args()

    os.makedirs(args.dir, exist_ok=True)
    meta = {}
    for name, comment in [("kunx-root", "根密钥（信任锚）"), ("kunx-repo", "仓库索引密钥（信任锚）"), ("kunx-dev", "示例开发者密钥")]:
        if not os.path.exists(os.path.join(args.dir, name + ".key")):
            print(f"[genkeys] {comment}缺失，生成中...")
            gen_key(os.path.join(args.dir, name))
        with open(os.path.join(args.dir, name + ".pub"), "rb") as f:
            meta[name] = f.read().hex()
        print(f"[genkeys] {comment}: {meta[name]}")

    if args.issue_dev:
        root_sk = load_key(os.path.join(args.dir, "kunx-root"))
        dev_sk = load_key(os.path.join(args.dir, "kunx-dev"))
        dev_pk = ed25519.publickey(dev_sk)
        cert = build_cert(dev_pk, root_sk, KU_PACKAGE, args.subject, args.valid_from, args.valid_to)
        with open(os.path.join(args.dir, "kunx-dev.cert"), "wb") as f:
            f.write(cert)
        # 校验
        assert ed25519.verify(ed25519.publickey(root_sk), cert[:63 + len(args.subject.encode())], cert[63 + len(args.subject.encode()):])
        print(f"[genkeys] 开发者证书 kunx-dev.cert 已签发（subject={args.subject}，有效期至 {args.valid_to}）")

    with open(os.path.join(args.dir, "keys.json"), "w") as f:
        json.dump({**meta, "comment": "DEMO-ONLY keys; production must regenerate offline"}, f, indent=2)
    print("[genkeys] 完成。")


if __name__ == "__main__":
    main()
