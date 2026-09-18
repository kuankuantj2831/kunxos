#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""KunxOS 本地 HTTPS 演示源服务器（宿主机侧）。

用途：
  1. 给 kunxpkg 提供 https:// 软件源（Packages + Packages.sig + *.kunx），
     证书由 tools/kunxrepo/keys/tls-roots/kunx-repo-ca 签发（根已内嵌
     apps/browser/src/tls/roots.rs，gen_tls_roots.py 自动并入）；
  2. 给浏览器提供 https://10.0.2.2:<port>/ 演示页（root_dir/index.html）。

KunxOS 客户机经 QEMU slirp 以 10.0.2.2 访问宿主机。强制 TLS 1.2 +
ECDHE_RSA_AES128_GCM_SHA256（KunxOS TLS 客户端唯一套件），仅标准库。

用法：
  python3 tools/tls_repo_serve.py [port] [root_dir] [cert] [key]
默认：
  port=8443  root_dir=rootfs/var/lib/pkg/repo/main
  cert/key=tools/kunxrepo/keys/tls-roots/leaf.pem|.key
"""

import os
import ssl
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
TOP = os.path.dirname(HERE)

PAGE = """<!DOCTYPE html>
<html><head><title>KunxOS HTTPS 演示</title>
<style>
body { font-family:sans-serif; background:#0b3d2e; color:#e8f5ee; margin:0; padding:16px; }
h1 { color:#fff; }
.card { background:#14503c; border:1px solid #2a6b52; padding:12px; margin:10px 0; }
b { color:#7ee2b8; }
</style></head><body>
<h1>KunxOS HTTPS 演示源</h1>
<div class="card">本页面经 <b>TLS 1.2</b>（ECDHE_RSA_AES128_GCM_SHA256）传输，
证书链由内嵌 <b>KunxOS Repo CA</b> 验证。</div>
<div class="card">同一服务也提供 pkg 软件源：sources.list 写入
<b>https://10.0.2.2:PORT/</b> 后执行 <b>pkg update</b>。</div>
</body></html>
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        root = self.server.root_dir
        path = self.path.split("?", 1)[0].split("#", 1)[0]
        if path == "/" or path == "/index.html":
            body = PAGE.replace("PORT", str(self.server.server_port)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        # 归一化路径，禁止出 root_dir
        rel = os.path.normpath(path.lstrip("/"))
        if rel.startswith("..") or os.path.isabs(rel):
            self.send_error(403)
            return
        fpath = os.path.join(root, rel)
        if not os.path.isfile(fpath):
            self.send_error(404)
            return
        with open(fpath, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        sys.stderr.write("[tls-repo] %s %s\n" % (self.address_string(), fmt % args))


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8443
    root = sys.argv[2] if len(sys.argv) > 2 else os.path.join(TOP, "rootfs/var/lib/pkg/repo/main")
    cert = sys.argv[3] if len(sys.argv) > 3 else os.path.join(TOP, "tools/kunxrepo/keys/tls-roots/leaf.pem")
    key = sys.argv[4] if len(sys.argv) > 4 else os.path.join(TOP, "tools/kunxrepo/keys/tls-roots/leaf.key")

    srv = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    srv.root_dir = root
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    # KunxOS 客户端只实现 TLS 1.2：上限钉在 1.2，避免协商 1.3 握手失败
    try:
        ctx.maximum_version = ssl.TLSVersion.TLSv1_2
    except AttributeError:
        pass
    ctx.load_cert_chain(cert, key)
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    print("tls_repo_serve: https://0.0.0.0:%d/ root=%s cert=%s" % (port, root, cert))
    srv.serve_forever()


if __name__ == "__main__":
    main()
