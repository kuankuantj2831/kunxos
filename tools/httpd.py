#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""KunxOS 浏览器演示 HTTP 服务器（宿主机侧）。

KunxOS 客户机经 QEMU slirp 以 10.0.2.2 访问宿主机。启动本服务器后，
在浏览器地址栏输入 http://10.0.2.2:8000/ 即可加载演示站点。

用法：
  python3 tools/httpd.py [port] [root_dir]

仅依赖 Python 标准库；HTTP/1.0 语义（每次请求关闭连接），适配内核 TCP 栈。
"""

import argparse
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAGE = """<!DOCTYPE html>
<html><head><title>KunxOS 网络演示</title>
<style>
body { font-family:sans-serif; background:#0f172a; color:#e2e8f0; margin:0; padding:16px; }
a { color:#4a9fe8; }
h1 { color:#fff; }
.card { background:#1e293b; border:1px solid #334155; padding:12px; margin:10px 0; }
table { border-collapse:collapse; width:100%; }
td, th { border:1px solid #475569; padding:6px; }
</style></head><body>
<h1>KunxOS 网络演示</h1>
<p>本页面由宿主机 <b>tools/httpd.py</b> 提供，经 QEMU slirp 网络栈传输，
验证浏览器的 HTTP/1.1 客户端与 HTML/CSS/布局/渲染管线。</p>
<div class="card"><h3>页面</h3>
<ul>
<li><a href="/news.html">新闻页</a></li>
<li><a href="/form.html">表单页</a></li>
<li><a href="/table.html">表格页</a></li>
</ul></div>
<div class="card"><h3>响应头</h3>
<table><tr><th>字段</th><th>值</th></tr>
<tr><td>Server</td><td>kunxos-httpd</td></tr>
<tr><td>协议</td><td>HTTP/1.1</td></tr>
</table></div>
</body></html>
"""

NEWS = """<!DOCTYPE html>
<html><head><title>网络新闻</title></head><body>
<h1>网络新闻（HTTP 演示）</h1>
<p>该页面内容来自宿主机文件系统，浏览器通过内核 TCP 栈获取。</p>
<div class="card"><h3>重定向演示</h3>
<p><a href="/redirect">访问 /redirect</a>（302 → /news.html）</p></div>
<p><a href="/">← 返回</a></p>
</body></html>
"""

FORM = """<!DOCTYPE html>
<html><head><title>网络表单</title></head><body>
<h1>网络表单</h1>
<form action="/echo" method="get">
<p>关键词：<input type="text" name="q" size="20"></p>
<p><input type="submit" value="搜索"></p>
</form>
<p><a href="/">← 返回</a></p>
</body></html>
"""

TABLE = """<!DOCTYPE html>
<html><head><title>网络表格</title></head><body>
<h1>网络表格</h1>
<table>
<tr><th>名称</th><th>端口</th><th>状态</th></tr>
<tr><td>HTTP</td><td>80</td><td>OK</td></tr>
<tr><td>HTTPS</td><td>443</td><td>TLS 预留</td></tr>
</table>
<p><a href="/">← 返回</a></p>
</body></html>
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, fmt, *args):  # 静默访问日志
        sys.stderr.write("[httpd] %s\n" % (fmt % args))

    def _send(self, body, ctype="text/html; charset=utf-8"):
        data = body.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/":
            self._send(PAGE)
        elif path == "/news.html":
            self._send(NEWS)
        elif path == "/form.html":
            self._send(FORM)
        elif path == "/table.html":
            self._send(TABLE)
        elif path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/news.html")
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif path == "/echo":
            q = self.path.split("?", 1)[1] if "?" in self.path else ""
            self._send("<html><head><title>回显</title></head><body>"
                       "<h1>查询参数</h1><p>%s</p><p><a href='/'>返回</a></p></body></html>" % q)
        else:
            # 静态文件（root_dir 下）
            if ROOT and not path.startswith(".."):
                fp = os.path.join(ROOT, path.lstrip("/"))
                if os.path.isfile(fp):
                    data = open(fp, "rb").read()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/html; charset=utf-8"
                                     if fp.endswith((".html", ".htm")) else "application/octet-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(data)
                    return
            self.send_error(404)


ROOT = None


def main():
    global ROOT
    ap = argparse.ArgumentParser()
    ap.add_argument("port", nargs="?", type=int, default=8000)
    ap.add_argument("root", nargs="?", default=None)
    args = ap.parse_args()
    ROOT = args.root
    print(f"[httpd] listening on 0.0.0.0:{args.port} (guest: http://10.0.2.2:{args.port}/)")
    ThreadingHTTPServer(("0.0.0.0", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
