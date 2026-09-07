//! 内置演示页面（kunxos:// 协议），用于无网络环境展示浏览器引擎能力。
//!
//! 页面覆盖：标题/段落/链接/列表/表格/表单/图片/行内样式/类选择器/
//! 背景/边框/圆角（不支持，忽略）/文本对齐/相对定位。

/// 按路径加载内置页面；未知路径返回 None。
pub fn load(path: &str) -> Option<&'static str> {
    let p = path.trim_matches('/');
    Some(match p {
        "" | "home" => HOME,
        "news" => NEWS,
        "apps" => APPS,
        "about" => ABOUT,
        "demo/form" => DEMO_FORM,
        "demo/table" => DEMO_TABLE,
        "demo/list" => DEMO_LIST,
        "demo/img" => DEMO_IMG,
        _ => return None,
    })
}

const STYLE: &str = "<style>
body { font-family: sans-serif; background:#0f172a; color:#e2e8f0; margin:0; padding:0 16px 24px 16px; }
a { color:#4a9fe8; }
h1 { color:#ffffff; margin-top:20px; }
h2 { color:#7dd3fc; }
h3 { color:#a5b4fc; }
.card { background:#1e293b; border:1px solid #334155; border-radius:10px; padding:12px; margin:10px 0; }
.card h3 { margin-top:0; }
.badge { background:#22c55e; color:#052e16; padding:2px 8px; }
.warn { background:#f59e0b; color:#451a03; padding:2px 8px; }
.center { text-align:center; }
table { border-collapse:collapse; width:100%; }
td, th { border:1px solid #475569; padding:6px; }
th { background:#334155; color:#fff; }
</style>";

const HOME: &str = "<html><head><title>KunxOS 首页</title></head><body>
<h1>KunxOS 浏览器</h1>
<p>欢迎使用 KunxOS 内置浏览器。<b>本引擎</b>从零实现：HTML 解析、CSS 样式计算、
流式布局与 CPU 光栅化，全部运行在系统用户态。</p>
<div class='card'><h3>演示页面</h3>
<ul>
<li><a href='kunxos://news'>新闻中心</a></li>
<li><a href='kunxos://apps'>应用推荐</a></li>
<li><a href='kunxos://demo/form'>表单演示</a></li>
<li><a href='kunxos://demo/table'>表格演示</a></li>
<li><a href='kunxos://demo/list'>排版与列表</a></li>
<li><a href='kunxos://demo/img'>图片演示</a></li>
</ul></div>
<div class='card'><h3>功能特性</h3>
<p><span class='badge'>流式布局</span> <span class='badge'>表单</span>
<span class='badge'>表格</span> <span class='badge'>HTTP 客户端</span></p>
<p>在地址栏输入 <span style='background:#334155;padding:2px 6px;'>kunxos://news</span>
或 <span style='background:#334155;padding:2px 6px;'>file:///var/www/index.html</span>
体验更多页面。</p></div>
<p class='center' style='color:#64748b;'>KunxOS Browser 0.1 · 简化版引擎 · 无 JS</p>
</body></html>";

const NEWS: &str = "<html><head><title>今日要闻</title></head><body>
<h1>今日要闻</h1>
<div class='card'><h3>KunxOS 1.0 发布</h3>
<p>全新动画系统、应用商店与 OOBE 首次启动向导同步上线，开发者预览版开放下载。</p>
<p><a href='kunxos://about'>了解更多 →</a></p></div>
<div class='card'><h3>内置浏览器引擎完成</h3>
<p>从零编写的 HTML/CSS/布局/渲染管线，支持表单与表格，性能目标简单页面 100ms 内渲染完成。</p></div>
<div class='card'><h3>安全基线</h3>
<p>同源策略基础、无第三方 Cookie、无插件、无脚本执行；TLS 与 JavaScript 支持预留。</p></div>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const APPS: &str = "<html><head><title>应用推荐</title></head><body>
<h1>应用推荐</h1>
<div class='card'><h3>终端</h3><p>系统 Shell 与命令工具。</p></div>
<div class='card'><h3>计算器</h3><p>基础四则运算。</p></div>
<div class='card'><h3>天气</h3><p>七天预报。</p></div>
<div class='card'><h3>云服务</h3><p>设备注册、备份与远程管理。</p></div>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const ABOUT: &str = "<html><head><title>关于</title></head><body>
<h1>关于 KunxOS</h1>
<p>KunxOS 是一个从零开始编写的 ARM64 操作系统，使用 Rust 实现内核、驱动与应用，
覆盖 Bootloader 到桌面应用的完整系统栈。</p>
<p style='font-size:26px;color:#1e293b'>图标与 Emoji（Phosphor 字体渲染）：
☀️ ⛅ 🌧 ❄️ ⚡ ❤️ ⭐ ✨ 🔔 📅 📁 💬 👍 😀 😎</p>
<h2>技术栈</h2>
<ul>
<li>内核：MMU、CFS 调度、VFS、网络栈</li>
<li>用户态：launcher + 20+ 内置应用</li>
<li>浏览器引擎：Rust，无第三方渲染库</li>
</ul>
<h2>浏览器模块</h2>
<table>
<tr><th>模块</th><th>实现</th></tr>
<tr><td>HTML 解析</td><td>标签树 + 属性 + 实体</td></tr>
<tr><td>CSS 解析</td><td>选择器 + 级联 + 继承</td></tr>
<tr><td>布局引擎</td><td>流式 + 表格 + relative</td></tr>
<tr><td>渲染管线</td><td>绘制列表 + CPU 光栅化</td></tr>
<tr><td>网络层</td><td>HTTP/1.1 + 重定向 + 缓存</td></tr>
</table>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const DEMO_FORM: &str = "<html><head><title>表单演示</title></head><body>
<h1>表单演示</h1>
<p>GET 表单：点击「提交」后浏览器把参数拼接到 action 并导航。</p>
<div class='card'>
<form action='kunxos://demo/form' method='get'>
<p><label>用户名：</label><input type='text' name='user' value='kunxos' size='16'></p>
<p><label>邮箱：</label><input type='text' name='email' size='24'></p>
<p><label>密码：</label><input type='password' name='pass' size='16'></p>
<p>订阅：<input type='checkbox' name='sub' checked> 接收更新
<input type='radio' name='level' value='1' checked> 基础
<input type='radio' name='level' value='2'> 高级</p>
<p><input type='submit' value='提交'> <input type='reset' value='重置'></p>
</form>
</div>
<p>支持控件：文本输入、复选框、单选钮、提交/重置按钮。动态脚本（JS）暂不执行。</p>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const DEMO_TABLE: &str = "<html><head><title>表格演示</title></head><body>
<h1>表格演示</h1>
<p>简单等宽列布局，单元格支持文本、链接与强调样式。</p>
<table>
<tr><th>应用</th><th>分类</th><th>状态</th></tr>
<tr><td><a href='kunxos://apps'>终端</a></td><td>工具</td><td>✅</td></tr>
<tr><td><a href='kunxos://apps'>计算器</a></td><td>工具</td><td>✅</td></tr>
<tr><td><a href='kunxos://apps'>地图</a></td><td>出行</td><td>🚧</td></tr>
<tr><td><a href='kunxos://apps'>音乐</a></td><td>娱乐</td><td>✅</td></tr>
</table>
<h2>混合内容</h2>
<table>
<tr><th>编号</th><th>说明</th></tr>
<tr><td>001</td><td><b>加粗</b>与<span style='color:#4a9fe8;'>彩色文本</span>混合</td></tr>
<tr><td>002</td><td>单元格内嵌 <a href='kunxos://home'>链接</a></td></tr>
</table>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const DEMO_LIST: &str = "<html><head><title>排版演示</title></head><body>
<h1>排版与列表</h1>
<h2>二级标题</h2>
<h3>三级标题</h3>
<p>这是一段<b>加粗</b>、<span style='color:#f472b6;'>彩色</span>、
<a href='kunxos://home'>链接</a>混合的正文文本，用于验证行内折行与行盒对齐。
英文文本 The quick brown fox jumps over the lazy dog 用于验证按词折行。</p>
<ul>
<li>列表项一</li>
<li>列表项二（包含 <a href='kunxos://news'>链接</a>）</li>
<li>列表项三</li>
</ul>
<hr>
<p class='center'>居中文本示例：KunxOS Browser</p>
<p style='text-align:right;'>右对齐文本示例</p>
<div class='card' style='position:relative;top:6px;'>
<h3>相对定位</h3>
<p>本卡片通过 <code>position:relative; top:6px;</code> 下移 6 像素。</p>
</div>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";

const DEMO_IMG: &str = "<html><head><title>图片演示</title></head><body>
<h1>图片演示</h1>
<p>内置渐变图（kunxos://gfx，PPM 未压缩格式）与占位图：</p>
<div class='card'><h3>横幅</h3>
<img src='kunxos://gfx/banner' width='480' height='160' alt='banner'>
</div>
<div class='card'><h3>小图 + 链接</h3>
<a href='kunxos://home'><img src='kunxos://gfx/square' width='120' height='120' alt='square'></a>
<p>点击图片返回首页。PNG/JPEG 解码为预留能力。</p>
</div>
<p><a href='kunxos://home'>← 返回首页</a></p>
</body></html>";
