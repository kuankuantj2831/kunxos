/**
 * KunxOS 云服务 —— Cloudflare Workers + D1 实现
 *
 * 响应格式与 cloud/gateway/gateway.py 的 Backend 完全一致（权威契约见 cloud/PROTOCOL.md）：
 * - 设备 API：纯文本行（OK / ERR:<代码> / C|id|type|payload）
 * - 管理 API：JSON（{"ok": ...}）
 * - 令牌认证：FNV-1a 64 位哈希 hex（与 gateway.py 的 h() 逐字节一致），不落明文令牌
 *
 * 部署步骤见 cloud/worker/README.md。
 */

// ==================== 工具函数 ====================

const encoder = new TextEncoder();
// 非致命解码：非法 UTF-8 字节以 U+FFFD 替换（等价 Python .decode("utf-8", "replace")）
const decoder = new TextDecoder("utf-8");

/**
 * 令牌哈希：FNV-1a 64 位 → 16 位小写 hex。
 * 与 gateway.py 的 h() 完全一致：hval=0xCBF29CE484222325，逐字节
 * hval ^= b; hval = (hval * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF；结果 %016x。
 */
function h(token) {
  let hval = 0xcbf29ce484222325n;
  for (const b of encoder.encode(token)) {
    hval ^= BigInt(b);
    hval = (hval * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hval.toString(16).padStart(16, "0");
}

/** UTF-8 字符串 → RFC 4648 base64（无换行）。 */
function b64encode(s) {
  let bin = "";
  for (const b of encoder.encode(s)) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** RFC 4648 base64 → UTF-8 字符串（非法字节替换为 U+FFFD）。 */
function b64decode(s) {
  const bin = atob(s);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return decoder.decode(bytes);
}

const now = () => Math.floor(Date.now() / 1000);

/** 取 "k=v" 中第一个 "=" 之后的部分；无 "=" 时返回 undefined（同 Python split("=",1)[1] 越界）。 */
function afterEq(s) {
  const i = s.indexOf("=");
  if (i === -1) return undefined;
  return s.slice(i + 1);
}

/** URLSearchParams → 普通对象（多值取第一个，同 parse_qs(...)[0] 语义）。 */
function paramsToObject(sp) {
  const o = {};
  for (const [k, v] of sp) {
    if (!(k in o)) o[k] = v;
  }
  return o;
}

/** 读取请求体：Content-Type 含 "json" 按 JSON 解析，否则按 urlencoded 解析（同 gateway.parse_body）。 */
async function readBody(request) {
  const text = await request.text();
  const ctype = request.headers.get("Content-Type") || "";
  if (ctype.includes("json")) {
    try {
      return JSON.parse(text);
    } catch {
      return {};
    }
  }
  return paramsToObject(new URLSearchParams(text));
}

/**
 * 设备认证：设备存在、account 匹配、h(token)==token_hash，否则返回 null。
 * 与 gateway.py Backend.auth 一致。
 */
async function auth(env, u, t, d) {
  const dev = await env.DB.prepare("SELECT * FROM devices WHERE device_id = ?")
    .bind(d).first();
  if (!dev || dev.account !== u) return null;
  if (h(t) !== dev.token_hash) return null;
  return dev;
}

// ==================== 设备 API ====================

/** POST /api/register（urlencoded: u/t/d/n） */
async function register(env, p) {
  const u = p.u ?? "";
  const t = p.t ?? "";
  const d = p.d ?? "";
  const n = p.n ?? "";
  if (!u || !t || !d) return "ERR:bad_request";
  const dev = await env.DB.prepare("SELECT * FROM devices WHERE device_id = ?")
    .bind(d).first();
  if (dev && dev.account !== u) return "ERR:exists";
  const ts = now();
  if (dev) {
    // 同账号重复注册：更新账号/名称/令牌哈希/last_seen（名称为空时保留旧值，同 gateway）
    await env.DB.prepare(
      "UPDATE devices SET account = ?, name = ?, token_hash = ?, last_seen = ? WHERE device_id = ?"
    ).bind(u, n || dev.name || "", h(t), ts, d).run();
  } else {
    await env.DB.prepare(
      "INSERT INTO devices (device_id, account, name, token_hash, ip, last_seen, created) VALUES (?, ?, ?, ?, '', ?, ?)"
    ).bind(d, u, n, h(t), ts, ts).run();
  }
  return "OK";
}

/** POST /api/backup（多行文本：前 4 行 u=/t=/d=/n=，随后 n 行 F|base64path|base64data；整表替换） */
async function backup(env, body) {
  const lines = body.split("\n");
  if (lines.length < 4) return "ERR:bad_request";
  // 头行值按 urlencoded 传输（与 register/poll/ack 一致），此处解码
  const u = decodeURIComponent(afterEq(lines[0]) ?? "");
  const t = decodeURIComponent(afterEq(lines[1]) ?? "");
  const d = decodeURIComponent(afterEq(lines[2]) ?? "");
  const nStr = afterEq(lines[3]);
  if (u === undefined || t === undefined || d === undefined || nStr === undefined) {
    return "ERR:bad_request";
  }
  if (!/^\d+$/.test(nStr)) return "ERR:bad_request";
  const n = parseInt(nStr, 10);
  if (!(await auth(env, u, t, d))) return "ERR:bad_auth";
  const files = [];
  for (let i = 4; i < lines.length; i++) {
    const ln = lines[i];
    if (!ln) continue; // 跳过空行（同 gateway）
    const parts = ln.split("|");
    if (parts.length !== 3) return "ERR:bad_request";
    let path, data;
    try {
      path = b64decode(parts[1]);
      data = b64decode(parts[2]);
    } catch {
      return "ERR:bad_request";
    }
    files.push({ path, data });
  }
  if (files.length !== n) return "ERR:bad_request";
  // 整表替换：先删旧再写新（用 D1 batch 保证原子性，避免中途失败留下
  // 半份备份——此前先 DELETE 再逐条 INSERT，中途出错即数据不完整）。
  const ts = now();
  const stmts = [
    env.DB.prepare("DELETE FROM backups WHERE device_id = ?").bind(d),
  ];
  const ins = env.DB.prepare(
    "INSERT INTO backups (device_id, path, data, updated) VALUES (?, ?, ?, ?)"
  );
  for (const f of files) {
    stmts.push(ins.bind(d, f.path, f.data, ts));
  }
  await env.DB.batch(stmts);
  return "OK:" + n;
}

/** GET /api/restore?u=&t=&d= */
async function restore(env, p) {
  const u = p.u ?? "";
  const t = p.t ?? "";
  const d = p.d ?? "";
  if (!(await auth(env, u, t, d))) return "ERR:bad_auth";
  const res = await env.DB.prepare(
    "SELECT path, data FROM backups WHERE device_id = ? ORDER BY rowid"
  ).bind(d).all();
  const out = ["OK:" + res.results.length];
  for (const f of res.results) {
    out.push("F|" + b64encode(f.path) + "|" + b64encode(f.data));
  }
  return out.join("\n");
}

/** POST /api/poll（urlencoded: u/t/d/ip；返回 OK + C|id|type|payload 行） */
async function poll(env, p) {
  const u = p.u ?? "";
  const t = p.t ?? "";
  const d = p.d ?? "";
  const ip = p.ip ?? "";
  if (!(await auth(env, u, t, d))) return "ERR:bad_auth";
  const ts = now();
  if (ip) {
    await env.DB.prepare("UPDATE devices SET last_seen = ?, ip = ? WHERE device_id = ?")
      .bind(ts, ip, d).run();
  } else {
    await env.DB.prepare("UPDATE devices SET last_seen = ? WHERE device_id = ?")
      .bind(ts, d).run();
  }
  const res = await env.DB.prepare(
    "SELECT id, type, payload FROM commands WHERE device_id = ? AND status = 'pending' ORDER BY id"
  ).bind(d).all();
  const out = ["OK"];
  for (const c of res.results) {
    out.push("C|" + c.id + "|" + c.type + "|" + encodeURIComponent(c.payload || ""));
  }
  return out.join("\n");
}

/** POST /api/ack（urlencoded: u/t/d/c/r） */
async function ack(env, p) {
  const cStr = p.c ?? "";
  if (!/^\d+$/.test(cStr)) return "ERR:bad_request";
  const c = parseInt(cStr, 10);
  const u = p.u ?? "";
  const t = p.t ?? "";
  const d = p.d ?? "";
  const r = p.r ?? "";
  if (!(await auth(env, u, t, d))) return "ERR:bad_auth";
  const res = await env.DB.prepare(
    "UPDATE commands SET status = 'done', result = ?, acked = ? WHERE id = ?"
  ).bind(r, now(), c).run();
  const changes = res.meta ? res.meta.changes : 0;
  if (changes === 0) return "ERR:not_found";
  return "OK";
}

// ==================== 管理 API（JSON） ====================

/** GET /api/admin/devices?key= */
async function adminDevices(env, key) {
  if (key !== env.ADMIN_KEY) return { ok: false };
  const res = await env.DB.prepare(
    "SELECT device_id, account, name, ip, last_seen, created FROM devices ORDER BY last_seen DESC"
  ).all();
  return { ok: true, devices: res.results };
}

/** POST /api/admin/command（JSON: {key, device_id, type, payload}） */
async function adminCommand(env, body) {
  const key = body.key ?? "";
  if (key !== env.ADMIN_KEY) return { ok: false, error: "denied" };
  const device_id = body.device_id ?? "";
  const type = body.type ?? "";
  const payload = body.payload ?? "";
  const dev = await env.DB.prepare("SELECT device_id FROM devices WHERE device_id = ?")
    .bind(device_id).first();
  if (!dev) return { ok: false, error: "no_such_device" };
  if (type !== "find" && type !== "lock" && type !== "wipe") {
    return { ok: false, error: "bad_type" };
  }
  await env.DB.prepare(
    "INSERT INTO commands (device_id, type, payload, status, result, created, acked) VALUES (?, ?, ?, 'pending', '', ?, 0)"
  ).bind(device_id, type, payload, now()).run();
  return { ok: true };
}

/** GET /api/admin/backup?key=&device_id= */
async function adminBackup(env, key, device_id) {
  if (key !== env.ADMIN_KEY) return { ok: false };
  const res = await env.DB.prepare(
    "SELECT path, data FROM backups WHERE device_id = ? ORDER BY rowid"
  ).bind(device_id).all();
  return { ok: true, files: res.results };
}

// ==================== 管理后台页面 ====================

// 与 gateway.py 的 ADMIN_HTML 完全一致（请求相对路径 /api/admin/*，Worker 下天然兼容）。
const ADMIN_HTML = String.raw`<!DOCTYPE html>
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
`;

// ==================== 路由入口 ====================

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;
    try {
      // 管理后台页面
      if (method === "GET" && path === "/admin") {
        return new Response(ADMIN_HTML, {
          headers: { "Content-Type": "text/html; charset=utf-8" },
        });
      }
      if (path.startsWith("/api/")) {
        return await handleApi(request, url, env);
      }
      return new Response("ERR:not_found", {
        status: 404,
        headers: { "Content-Type": "text/plain; charset=utf-8" },
      });
    } catch (e) {
      // 内部错误：HTTP 500 + 脱敏 ERR:internal（此前返回 200 并把异常文本
      // 原样回给设备）；详情只写 Worker 日志。
      console.error("[worker] internal error:", e && e.stack ? e.stack : String(e));
      return new Response("ERR:internal", {
        status: 500,
        headers: { "Content-Type": "text/plain; charset=utf-8" },
      });
    }
  },
};

/** 设备协议错误码 → HTTP 状态码（首行 ERR:<code> 保持兼容）。 */
function errStatus(code) {
  switch (code) {
    case "bad_request": return 400;
    case "bad_auth": return 401;
    case "not_found": return 404;
    case "exists": return 409;
    case "internal": return 500;
    default: return 400;
  }
}

/** 错误码 → 脱敏中文消息（不携带异常文本）。 */
function errMessage(code) {
  switch (code) {
    case "bad_request": return "请求格式错误";
    case "bad_auth": return "账号或令牌无效";
    case "not_found": return "资源不存在";
    case "exists": return "设备已被其他账号注册";
    case "internal": return "服务器内部错误";
    default: return "请求失败";
  }
}

/** /api/* 分发（路由与响应格式对齐 gateway.py 的 handle_api）。 */
async function handleApi(request, url, env) {
  const method = request.method;
  const path = url.pathname;
  const query = paramsToObject(url.searchParams);

  let out;
  let status = 200;
  let ctypeOut = "text/plain; charset=utf-8";

  if (method === "GET" && path === "/api/admin/devices") {
    out = await adminDevices(env, query.key ?? "");
    ctypeOut = "application/json";
  } else if (method === "GET" && path === "/api/admin/backup") {
    out = await adminBackup(env, query.key ?? "", query.device_id ?? "");
    ctypeOut = "application/json";
  } else if (method === "GET" && path === "/api/restore") {
    out = await restore(env, query);
  } else if (method === "POST" && path === "/api/register") {
    out = await register(env, await readBody(request));
  } else if (method === "POST" && path === "/api/backup") {
    out = await backup(env, await request.text());
  } else if (method === "POST" && path === "/api/poll") {
    out = await poll(env, await readBody(request));
  } else if (method === "POST" && path === "/api/ack") {
    out = await ack(env, await readBody(request));
  } else if (method === "POST" && path === "/api/admin/command") {
    out = await adminCommand(env, await readBody(request));
    ctypeOut = "application/json";
  } else {
    status = 404;
    out = "ERR:not_found";
  }

  // 设备协议错误：首行 ERR:<code> 保持兼容，同时映射真实 HTTP 状态码，
  // 并追加第二行 JSON detail（旧设备只读首行不受影响）。
  if (ctypeOut === "text/plain; charset=utf-8" && typeof out === "string" && out.startsWith("ERR:")) {
    const code = out.slice(4).split(":")[0];
    status = errStatus(code);
    const detail = {
      code,
      message: errMessage(code),
      retryable: code === "internal",
      http_status: status,
    };
    out = out + "\n" + JSON.stringify(detail);
  }

  return new Response(typeof out === "string" ? out : JSON.stringify(out), {
    status,
    headers: { "Content-Type": ctypeOut },
  });
}
