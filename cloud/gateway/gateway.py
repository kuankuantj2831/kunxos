#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""KunxOS 云服务 —— 明文 HTTP 网关。

KunxOS 客户机内核 TCP 栈无 TLS，只能连明文 HTTP 端点。本网关就是那个端点：

- 生产模式（默认）：`--backend <Cloudflare Worker URL>`，把客户机的明文
  HTTP 请求原样转发到 Worker（HTTPS），Worker + D1 是权威数据层。
- 本地开发模式：`--local`，用 JSON 文件存储实现与 Worker 完全一致的
  /api/* 契约（见 cloud/PROTOCOL.md），用于 QEMU 内端到端联调，
  无需部署 Cloudflare。

用法：
  # 本地开发（E2E 联调）
  python3 cloud/gateway/gateway.py --local --port 8080 --admin-key admin

  # 生产：反代到已部署的 Worker
  python3 cloud/gateway/gateway.py --backend https://cloud.kuankuantj.com \
      --port 8080 --admin-key admin

  # 管理后台：浏览器打开 http://127.0.0.1:8080/admin
  # 可用 cloudflared 隧道把 /admin 暴露到公网：
  #   cloudflared tunnel --url http://127.0.0.1:8080

仅依赖 Python 标准库。
"""

import argparse
import base64
import json
import os
import sys
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

# ==================== 本地存储 ====================

class Store:
    """JSON 文件存储（本地开发模式）。"""

    def __init__(self, data_dir):
        self.data_dir = data_dir
        self.devices_path = os.path.join(data_dir, "devices.json")
        self.commands_path = os.path.join(data_dir, "commands.json")
        self.backup_dir = os.path.join(data_dir, "backups")
        os.makedirs(self.backup_dir, exist_ok=True)
        self._load()

    def _load(self):
        self.devices = {}
        if os.path.exists(self.devices_path):
            try:
                self.devices = json.load(open(self.devices_path, encoding="utf-8"))
            except Exception:
                self.devices = {}
        self.commands = []
        if os.path.exists(self.commands_path):
            try:
                self.commands = json.load(open(self.commands_path, encoding="utf-8"))
            except Exception:
                self.commands = []
        self.cmd_seq = 0
        for c in self.commands:
            if isinstance(c.get("id"), int):
                self.cmd_seq = max(self.cmd_seq, c["id"])
        # 迁移：为既有设备补 token_hash
        for d in self.devices.values():
            d.setdefault("token_hash", d.get("token", ""))

    def save(self):
        tmp = self.devices_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(self.devices, f, ensure_ascii=False)
        os.replace(tmp, self.devices_path)
        tmp = self.commands_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(self.commands, f, ensure_ascii=False)
        os.replace(tmp, self.commands_path)

    # ---- devices ----
    def get_device(self, device_id):
        return self.devices.get(device_id)

    def upsert_device(self, device_id, account, name, token_hash):
        dev = self.devices.get(device_id)
        now = int(time.time())
        if dev is None:
            dev = {
                "device_id": device_id,
                "account": account,
                "name": name,
                "token_hash": token_hash,
                "ip": "",
                "last_seen": now,
                "created": now,
            }
            self.devices[device_id] = dev
        else:
            dev["account"] = account
            dev["name"] = name or dev.get("name", "")
            dev["token_hash"] = token_hash
        dev["last_seen"] = now
        self.save()
        return dev

    def touch(self, device_id, ip):
        dev = self.devices.get(device_id)
        if dev is None:
            return None
        dev["last_seen"] = int(time.time())
        if ip:
            dev["ip"] = ip
        self.save()
        return dev

    # ---- backups ----
    def backup_path(self, device_id):
        return os.path.join(self.backup_dir, device_id + ".json")

    def put_backup(self, device_id, files):
        path = self.backup_path(device_id)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(files, f, ensure_ascii=False)
        os.replace(tmp, path)

    def get_backup(self, device_id):
        path = self.backup_path(device_id)
        if not os.path.exists(path):
            return []
        try:
            return json.load(open(path, encoding="utf-8"))
        except Exception:
            return []

    # ---- commands ----
    def add_command(self, device_id, ctype, payload):
        self.cmd_seq += 1
        cmd = {
            "id": self.cmd_seq,
            "device_id": device_id,
            "type": ctype,
            "payload": payload or "",
            "status": "pending",
            "created": int(time.time()),
            "acked": 0,
            "result": "",
        }
        self.commands.append(cmd)
        self.save()
        return cmd

    def pending_commands(self, device_id):
        return [c for c in self.commands
                if c.get("device_id") == device_id and c.get("status") == "pending"]

    def ack_command(self, cmd_id, result):
        for c in self.commands:
            if c.get("id") == cmd_id:
                c["status"] = "done"
                c["result"] = result
                c["acked"] = int(time.time())
                self.save()
                return True
        return False


# ==================== 业务逻辑（本地模式，与 Worker 契约一致） ====================

def h(token):
    """令牌哈希（与 Worker 同：FNV-1a 64 hex）。"""
    hval = 0xCBF29CE484222325
    for b in token.encode("utf-8"):
        hval ^= b
        hval = (hval * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return "%016x" % hval


class Backend:
    """本地模式后端。"""

    def __init__(self, store, admin_key):
        self.store = store
        self.admin_key = admin_key

    def auth(self, u, t, d):
        dev = self.store.get_device(d)
        if dev is None or dev.get("account") != u:
            return None
        if h(t) != dev.get("token_hash", ""):
            return None
        return dev

    # ---- 设备 API ----
    def register(self, params):
        u = params.get("u", [""])[0]
        t = params.get("t", [""])[0]
        d = params.get("d", [""])[0]
        n = params.get("n", [""])[0]
        if not u or not t or not d:
            return "ERR:bad_request"
        dev = self.store.get_device(d)
        if dev is not None and dev.get("account") != u:
            return "ERR:exists"
        self.store.upsert_device(d, u, n, h(t))
        return "OK"

    def backup(self, body):
        lines = body.split("\n")
        if len(lines) < 4:
            return "ERR:bad_request"
        # 头行值按 urlencoded 传输（与 register/poll/ack 一致），此处解码
        from urllib.parse import unquote
        u = unquote(lines[0].split("=", 1)[1]) if "=" in lines[0] else ""
        t = unquote(lines[1].split("=", 1)[1]) if "=" in lines[1] else ""
        d = unquote(lines[2].split("=", 1)[1]) if "=" in lines[2] else ""
        try:
            n = int(lines[3].split("=", 1)[1])
        except Exception:
            return "ERR:bad_request:n"
        if self.auth(u, t, d) is None:
            return "ERR:bad_auth"
        files = []
        for ln in lines[4:]:
            if not ln:
                continue
            parts = ln.split("|")
            if len(parts) != 3:
                return "ERR:bad_request:parts=%d:line=%.60s" % (len(parts), ln)
            try:
                p = base64.b64decode(parts[1]).decode("utf-8", "replace")
                data = base64.b64decode(parts[2]).decode("utf-8", "replace")
            except Exception:
                return "ERR:bad_request:b64:line=%.60s" % ln
            files.append({"path": p, "data": data})
        if len(files) != n:
            return "ERR:bad_request:files=%d:n=%d" % (len(files), n)
        self.store.put_backup(d, files)
        return "OK:%d" % n

    def restore(self, params):
        u = params.get("u", [""])[0]
        t = params.get("t", [""])[0]
        d = params.get("d", [""])[0]
        if self.auth(u, t, d) is None:
            return "ERR:bad_auth"
        files = self.store.get_backup(d)
        out = ["OK:%d" % len(files)]
        for f in files:
            p = base64.b64encode(f["path"].encode("utf-8")).decode()
            data = base64.b64encode(f["data"].encode("utf-8")).decode()
            out.append("F|%s|%s" % (p, data))
        return "\n".join(out)

    def poll(self, params):
        u = params.get("u", [""])[0]
        t = params.get("t", [""])[0]
        d = params.get("d", [""])[0]
        ip = params.get("ip", [""])[0]
        if self.auth(u, t, d) is None:
            return "ERR:bad_auth"
        self.store.touch(d, ip)
        out = ["OK"]
        for c in self.store.pending_commands(d):
            payload = c.get("payload", "") or ""
            # payload 用 urlencoded 传递（与设备端约定）
            import urllib.parse
            payload = urllib.parse.quote(payload, safe="")
            out.append("C|%d|%s|%s" % (c["id"], c["type"], payload))
        return "\n".join(out)

    def ack(self, params):
        u = params.get("u", [""])[0]
        t = params.get("t", [""])[0]
        d = params.get("d", [""])[0]
        try:
            c = int(params.get("c", [""])[0])
        except Exception:
            return "ERR:bad_request"
        r = params.get("r", [""])[0]
        if self.auth(u, t, d) is None:
            return "ERR:bad_auth"
        if not self.store.ack_command(c, r):
            return "ERR:not_found"
        return "OK"

    # ---- 管理 API ----
    def admin_devices(self, key):
        if key != self.admin_key:
            return None
        devs = [dict(v) for v in self.store.devices.values()]
        devs.sort(key=lambda x: x.get("last_seen", 0), reverse=True)
        return {"ok": True, "devices": devs}

    def admin_command(self, key, device_id, ctype, payload):
        if key != self.admin_key:
            return {"ok": False, "error": "denied"}
        if not self.store.get_device(device_id):
            return {"ok": False, "error": "no_such_device"}
        if ctype not in ("find", "lock", "wipe"):
            return {"ok": False, "error": "bad_type"}
        self.store.add_command(device_id, ctype, payload)
        return {"ok": True}

    def admin_backup(self, key, device_id):
        if key != self.admin_key:
            return None
        files = self.store.get_backup(device_id)
        return {"ok": True, "files": files}


# ==================== HTTP 处理 ====================

def parse_body(body, ctype):
    if not body:
        return {}
    if ctype and "json" in ctype:
        try:
            return json.loads(body)
        except Exception:
            return {}
    return parse_qs(body.decode("utf-8", "replace"))


ADMIN_HTML = """<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<title>KunxOS 云服务 · 管理后台</title>
<style>
body{font-family:system-ui,sans-serif;background:#0f172a;color:#e2e8f0;margin:0;padding:24px}
h1{font-size:20px}input,select,button{padding:8px 10px;border-radius:6px;border:1px solid #334155;background:#1e293b;color:#e2e8f0;font-size:14px}
button{cursor:pointer;background:#22c55e;border:none;color:#052e16;font-weight:600}
table{width:100%;border-collapse:collapse;margin-top:12px;font-size:13px}
th,td{text-align:left;padding:8px;border-bottom:1px solid #1e293b}
tr:hover{background:#1e293b}.badge{padding:2px 8px;border-radius:10px;font-size:12px}
.on{background:#16a34a}.off{background:#64748b}
a{color:#4a9fe8}
#err{color:#f87171;margin-top:8px}
</style>
</head>
<body>
<h1>KunxOS 云服务 · 管理后台</h1>
<div>
  管理密钥 <input id="key" type="password" value="admin">
  <button onclick="loadDevices()">刷新</button>
</div>
<div id="err"></div>
<table id="tbl"><thead><tr>
<th>设备 ID</th><th>账号</th><th>名称</th><th>IP</th><th>最后在线</th><th>操作</th><th>备份</th>
</tr></thead><tbody></tbody></table>
<h2>下发命令</h2>
<div>
  设备 <input id="did" placeholder="设备 ID">
  类型 <select id="ctype"><option value="find">查找设备</option>
  <option value="lock">远程锁定</option><option value="wipe">远程擦除</option></select>
  <input id="payload" placeholder="备注（可选）">
  <button onclick="sendCmd()">下发</button>
</div>
<pre id="backup" style="background:#0b1220;padding:12px;border-radius:8px;white-space:pre-wrap"></pre>
<script>
const $=id=>document.getElementById(id);
async function j(url,opt){const r=await fetch(url,opt);if(!r.ok)throw new Error(r.status);return r.json()}
function esc(s){return String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]))}
function ago(ts){if(!ts)return '-';const s=Math.floor(Date.now()/1000-ts);if(s<60)return s+'s前';
 if(s<3600)return Math.floor(s/60)+'m前';if(s<86400)return Math.floor(s/3600)+'h前';return Math.floor(s/86400)+'d前'}
async function loadDevices(){
 $('err').textContent='';
 try{
  const d=await j('/api/admin/devices?key='+encodeURIComponent($('key').value));
  const tb=$('tbl').querySelector('tbody');tb.innerHTML='';
  for(const v of d.devices){
   const tr=document.createElement('tr');
   tr.innerHTML='<td>'+esc(v.device_id)+'</td><td>'+esc(v.account)+'</td><td>'+esc(v.name||'')+
    '</td><td>'+esc(v.ip||'-')+'</td><td>'+ago(v.last_seen)+'</td>'+
    '<td><button onclick="sendTo(\''+esc(v.device_id)+'\',\'find\')">查找</button> '+
    '<button onclick="sendTo(\''+esc(v.device_id)+'\',\'lock\')" style="background:#eab308;color:#422006">锁定</button> '+
    '<button onclick="sendTo(\''+esc(v.device_id)+'\',\'wipe\')" style="background:#ef4444;color:#fff">擦除</button></td>'+
    '<td><a href="javascript:viewBk(\''+esc(v.device_id)+'\')">查看</a></td>';
   tb.appendChild(tr);
  }
 }catch(e){$('err').textContent='加载失败: '+e}
}
async function sendTo(did,type){$('did').value=did;$('ctype').value=type;await sendCmd()}
async function sendCmd(){
 try{
  const r=await j('/api/admin/command',{method:'POST',headers:{'Content-Type':'application/json'},
   body:JSON.stringify({key:$('key').value,device_id:$('did').value,type:$('ctype').value,payload:$('payload').value})});
  if(!r.ok)throw new Error(r.error||'下发失败');
  alert('命令已下发');await loadDevices();
 }catch(e){$('err').textContent='下发失败: '+e}
}
async function viewBk(did){
 try{
  const r=await j('/api/admin/backup?key='+encodeURIComponent($('key').value)+'&device_id='+encodeURIComponent(did));
  $('backup').textContent=r.files.map(f=>f.path+'\n  → '+f.data).join('\n');
 }catch(e){$('backup').textContent='读取失败: '+e}
}
loadDevices();
</script>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # 每个响应后关闭连接（KunxOS 客户机读 EOF）

    def log_message(self, fmt, *args):
        ts = time.strftime("%H:%M:%S")
        sys.stderr.write("[%s] %s\n" % (ts, fmt % args))

    # ---- 路由 ----
    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/admin":
            body = ADMIN_HTML.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path.startswith("/api/"):
            self.handle_api("GET")
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        path = urlparse(self.path).path
        if path.startswith("/api/"):
            self.handle_api("POST")
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    # ---- API 入口 ----
    def handle_api(self, method):
        global BACKEND
        # Content-Length 解析必须受保护：畸形请求此前会抛出未捕获的
        # ValueError，产生裸 HTML 500，破坏 ERR:<code> 协议形状。
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except (TypeError, ValueError):
            self.send_response(400)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(b"ERR:bad_request:content-length")))
            self.end_headers()
            self.wfile.write(b"ERR:bad_request:content-length")
            return
        body = self.rfile.read(length) if length else b""
        ctype = self.headers.get("Content-Type") or ""

        if BACKEND_MODE == "proxy":
            # 反代到 Worker（HTTPS），原样转发
            url = BACKEND_URL.rstrip("/") + urlparse(self.path).path
            if urlparse(self.path).query:
                url += "?" + urlparse(self.path).query
            req = urllib.request.Request(url, data=body if method == "POST" else None,
                                         method=method)
            if ctype:
                req.add_header("Content-Type", ctype)
            # Cloudflare 会拦截 Python-urllib 默认 UA（error 1010）；客户机
            # 是无 UA 的裸请求，此处同样不带爬虫特征 UA 以免被 CF 拦下。
            req.add_header("User-Agent", "KunxOS-Cloud-Gateway/1.0")
            try:
                with urllib.request.urlopen(req, timeout=30) as resp:
                    out = resp.read()
                    status = resp.status
            except urllib.error.HTTPError as e:
                out = e.read()
                status = e.code
            except Exception as e:
                # 代理到 Worker 失败：错误详情只写服务端日志，不泄露给设备。
                print("[gateway] proxy error: %r" % (e,), file=sys.stderr)
                out = b"ERR:internal"
                status = 502
                detail = {
                    "code": "internal",
                    "message": "云端网关不可达",
                    "retryable": True,
                    "http_status": status,
                }
                out = out + b"\n" + json.dumps(detail, ensure_ascii=False).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return

        # 本地模式
        params = parse_qs(urlparse(self.path).query)
        status = 200
        out = b""
        ctype_out = "text/plain; charset=utf-8"
        try:
            if method == "GET" and self.path.startswith("/api/admin/devices"):
                r = BACKEND.admin_devices(params.get("key", [""])[0])
                out = json.dumps(r, ensure_ascii=False).encode("utf-8") if r else b'{"ok":false,"error":"denied"}'
                ctype_out = "application/json"
            elif method == "GET" and self.path.startswith("/api/admin/backup"):
                r = BACKEND.admin_backup(params.get("key", [""])[0], params.get("device_id", [""])[0])
                out = json.dumps(r, ensure_ascii=False).encode("utf-8") if r else b'{"ok":false,"error":"denied"}'
                ctype_out = "application/json"
            elif method == "GET" and self.path.startswith("/api/restore"):
                out = BACKEND.restore(params).encode("utf-8")
            elif method == "POST" and self.path.startswith("/api/register"):
                p = parse_body(body, ctype)
                out = BACKEND.register(p).encode("utf-8")
            elif method == "POST" and self.path.startswith("/api/backup"):
                out = BACKEND.backup(body.decode("utf-8", "replace")).encode("utf-8")
            elif method == "POST" and self.path.startswith("/api/poll"):
                p = parse_body(body, ctype)
                out = BACKEND.poll(p).encode("utf-8")
            elif method == "POST" and self.path.startswith("/api/ack"):
                p = parse_body(body, ctype)
                out = BACKEND.ack(p).encode("utf-8")
            elif method == "POST" and self.path.startswith("/api/admin/command"):
                p = parse_body(body, ctype)
                r = BACKEND.admin_command(p.get("key", ""), p.get("device_id", ""),
                                          p.get("type", ""), p.get("payload", ""))
                out = json.dumps(r, ensure_ascii=False).encode("utf-8")
                ctype_out = "application/json"
            else:
                status = 404
                out = b"ERR:not_found"
        except Exception as e:
            # 内部错误：HTTP 500 + 脱敏 ERR:internal；详情只进服务端日志。
            print("[gateway] %s %s -> internal error: %r"
                  % (method, self.path, e), file=sys.stderr)
            status = 500
            out = b"ERR:internal"
        # 设备协议：首行 OK / ERR:<code>（保持兼容，设备不读状态码）；
        # 同时把业务错误映射为真实 HTTP 状态码，便于 web/管理端区分。
        if ctype_out == "text/plain; charset=utf-8" and out.startswith(b"ERR:"):
            code_b = out[4:].split(b":", 1)[0]
            status = {
                b"bad_request": 400,
                b"bad_auth": 401,
                b"not_found": 404,
                b"exists": 409,
                b"internal": 500,
            }.get(code_b, 400)
            # 结构化 detail（第二行 JSON）：旧设备只读首行不受影响；
            # web/管理端可据 code/retryable 区分错误类型。消息脱敏。
            code = code_b.decode("ascii", "replace")
            detail = {
                "code": code,
                "message": {
                    "bad_request": "请求格式错误",
                    "bad_auth": "账号或令牌无效",
                    "not_found": "资源不存在",
                    "exists": "设备已被其他账号注册",
                    "internal": "服务器内部错误",
                }.get(code, "请求失败"),
                "retryable": code in ("internal",),
                "http_status": status,
            }
            out = out + b"\n" + json.dumps(detail, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", ctype_out)
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


BACKEND = None
BACKEND_MODE = "local"
BACKEND_URL = ""


def main():
    global BACKEND, BACKEND_MODE, BACKEND_URL
    ap = argparse.ArgumentParser(description="KunxOS cloud gateway (plain HTTP)")
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--backend", default="", help="Worker URL for production proxy mode")
    ap.add_argument("--local", action="store_true", help="local JSON-file storage mode")
    ap.add_argument("--data", default="cloud/data", help="storage dir (local mode)")
    ap.add_argument("--admin-key", default="admin")
    args = ap.parse_args()

    if not args.local and args.backend:
        BACKEND_MODE = "proxy"
        BACKEND_URL = args.backend
        print("[gateway] proxy mode -> %s" % BACKEND_URL)
    else:
        store = Store(args.data)
        BACKEND = Backend(store, args.admin_key)
        print("[gateway] local mode, data dir: %s" % args.data)
        print("[gateway] admin key: %s" % args.admin_key)

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    print("[gateway] listening on http://%s:%d (QEMU 客户机经 10.0.2.2:%d 访问)" %
          (args.host, args.port, args.port))
    print("[gateway] admin UI: http://127.0.0.1:%d/admin" % args.port)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\n[gateway] bye")


if __name__ == "__main__":
    import sys
    sys.exit(main())
