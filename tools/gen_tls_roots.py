#!/usr/bin/env python3
"""生成 apps/browser/src/tls/roots.rs —— 浏览器 TLS 客户端内嵌根证书（RSA）。

来源：本机 ca-certificates（/etc/ssl/certs，Mozilla 信任库子集）。
只挑 RSA 根：TLS 模块只实现 RSA PKCS#1 v1.5 验签（ECDSA 链无法验证）。
用法：python3 tools/gen_tls_roots.py  （需要 openssl 在 PATH）
"""
import os
import subprocess
import sys

# 覆盖主流 CA 体系：GlobalSign（baidu/百度系）、Let's Encrypt（ISRG）、
# DigiCert、Google Trust Services、Amazon/Starfield（AWS）、Microsoft、USERTrust。
ROOTS = [
    "GlobalSign_Root_CA_-_R3",
    "ISRG_Root_X1",
    "DigiCert_Global_Root_G2",
    "GTS_Root_R1",
    "Amazon_Root_CA_1",
    "Microsoft_RSA_Root_Certificate_Authority_2017",
    "Starfield_Root_Certificate_Authority_-_G2",
    "USERTrust_RSA_Certification_Authority",
]

# 项目自有根：tools/kunxrepo/keys/tls-roots/*.pem（不含 .key）。
# 「KunxOS Repo CA」给 demo 软件源的 HTTPS 服务签发叶子证书（SAN 含
# 10.0.2.2 = QEMU slirp 宿主机），与仓库 Ed25519 密钥同目录同级管理。
EXTRA_ROOTS_DIR = "tools/kunxrepo/keys/tls-roots"

OUT = "apps/browser/src/tls/roots.rs"


def to_der(pem_path: str) -> bytes:
    r = subprocess.run(
        ["openssl", "x509", "-in", pem_path, "-outform", "DER"],
        capture_output=True, check=True,
    )
    return r.stdout


def fmt_rows(der: bytes, indent: str) -> str:
    rows = []
    for i in range(0, len(der), 16):
        chunk = der[i:i + 16]
        rows.append(indent + "".join("0x%02X," % b for b in chunk))
    return "\n".join(rows)


def main() -> int:
    entries = []
    for name in ROOTS:
        pem = "/etc/ssl/certs/%s.pem" % name
        try:
            der = to_der(pem)
        except Exception as e:  # 缺文件：跳过并告警，不让单证书失败阻塞生成
            print("跳过 %s: %s" % (name, e), file=sys.stderr)
            continue
        ident = name.replace("-", "_").replace(".", "_")
        entries.append((name, ident, der))
    # 项目自有根（存在即并入；只收 *-ca.pem 命名的 CA 证书，叶子/密钥绝不嵌入）
    if os.path.isdir(EXTRA_ROOTS_DIR):
        for fn in sorted(os.listdir(EXTRA_ROOTS_DIR)):
            if not fn.endswith("-ca.pem"):
                continue
            path = os.path.join(EXTRA_ROOTS_DIR, fn)
            name = fn[:-4].replace("-", "_").replace(".", "_")
            try:
                der = to_der(path)
            except Exception as e:
                print("跳过 %s: %s" % (fn, e), file=sys.stderr)
                continue
            entries.append((name, name, der))

    with open(OUT, "w") as f:
        f.write("//! 浏览器 TLS 客户端内嵌根证书（DER，RSA 信任锚）。\n")
        f.write("//!\n")
        f.write("//! 由 `tools/gen_tls_roots.py` 从本机 ca-certificates（Mozilla 信任库\n")
        f.write("//! 子集）提取生成，不要手工编辑。只包含 RSA 根：TLS 模块的证书链\n")
        f.write("//! 验签只实现 RSA PKCS#1 v1.5（ECDSA 根/链直接判不可信）。\n")
        f.write("\n")
        f.write("pub struct RootCert {\n")
        f.write("    pub name: &'static str,\n")
        f.write("    pub der: &'static [u8],\n")
        f.write("}\n")
        f.write("\n")
        f.write("/// 内嵌信任锚（覆盖 GlobalSign / Let's Encrypt / DigiCert / Google /\n")
        f.write("/// Amazon / Microsoft / USERTrust 主流体系）。\n")
        f.write("pub static ROOTS: &[RootCert] = &[\n")
        for name, ident, der in entries:
            f.write("    RootCert {\n")
            f.write("        name: \"%s\",\n" % name)
            f.write("        der: &[\n%s\n        ],\n" % fmt_rows(der, "            "))
            f.write("    },\n")
        f.write("];\n")
    total = sum(len(d) for _, _, d in entries)
    print("生成 %s：%d 个根证书，共 %d 字节" % (OUT, len(entries), total))
    return 0


if __name__ == "__main__":
    sys.exit(main())
