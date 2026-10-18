# 优化轮 015：全系统按钮毛玻璃/磨砂玻璃统一收官——clock 全套 + 系统对话框 + weather

- 日期：2026-10-18
- 轮次性质：用户直接指令轮（「把所有系统中的按钮全部换成毛玻璃或磨砂玻璃」）
- 类型：UI 统一（与自动化轮 13/14 并发；oobe/main.rs 与 tools/ 为其在制品，本轮未触碰）

## 本轮目标

全系统按钮统一为毛玻璃/磨砂玻璃风格。先全面盘点（Explore 代理扫描 21 个应用
+ kunxui + shared），发现绝大多数应用**早已是玻璃**（settings/calculator/
phone/sms/contacts/calendar/gallery/map/music/browser 工具栏/files/notes/
video/cloud/camera/launcher 控制中心与键盘/OOBE 全套——共享 app_kit +
sys_nav::glass 基建），真正的非玻璃残余只有三处：

| 残余 | 位置 | 处置 |
| --- | --- | --- |
| clock 全套 ~15 处实色 | FAB/文字按钮(16 调用点)/±键/周 chips/快捷 chips/预设 chips/响铃对话框/删除红圆 | ✅ 全部改玻璃 |
| 系统对话框按钮（files/settings/launcher/oobe 四应用共用） | `shared/dialog.rs::draw_buttons` iOS 纯文字+分割线 | ✅ 改玻璃胶囊 |
| weather「刷新」 | 纯强调色文字无底 | ✅ 改强调玻璃胶囊 |
| browser 网页内容 `<button>`（paint.rs HTML 引擎） | 网页渲染保真 | ❌ 刻意保留平面（改了反而错） |

## 改动点

| 文件 | 改动 |
| --- | --- |
| `apps/clock/src/main.rs` | `draw_text_button` 中心函数改 `draw_glass_style(STYLE_ACCENT)`/`draw_glass`（一处覆盖 16 个调用点：保存/取消/计时器启停/秒表启停计次）；FAB 强调玻璃胶囊；±键浅玻璃；三组 chips 选中=强调玻璃/未选=浅玻璃；响铃对话框贪睡=浅玻璃/停止=危险红玻璃；删除红圆→危险红玻璃圆角块 |
| `shared/dialog.rs` | `draw_buttons` 重写：取消=浅色玻璃胶囊（深字）、确认=强调玻璃胶囊（白字，danger=红玻璃）、单按钮=居中强调胶囊；竖/横分割线移除。新增 `#[path="glass.rs"] mod glass`（自包含真磨砂：降采样+盒式模糊+高光描边）经 `DlgGlass` 适配器桥接 `DialogUi` 的 put/back。删 C_DLG_RED（无引用），C_DLG_BLUE 保留（单选圆/复选框仍用） |
| `apps/weather/src/main.rs` | 刷新按钮：强调玻璃胶囊 66×32 r16 于 (450,76)，白字；命中区不变 |

几何全部沿用原实色按钮的坐标（只换绘制函数，无布局变化）；文字配色规则不变
（强调/危险玻璃上白字、浅玻璃上深字）。

## 验证（QEMU GUI 实测 + 像素级断言）

1. **系统对话框（launcher 长按桌面 → 壁纸选择卡）**：遮罩压暗 230→163 ✓；
   按钮行逐段色彩剖面（胶囊中线三行采样）：
   `y=604/614/624: 取消胶囊 x72..276(浅玻璃) | 确认胶囊 x276..458(强调绿玻璃)`
   ——与 `bw = DLG_W/2-18 = 182` 的设计几何逐像素吻合；确认胶囊上有白字
   段（y=614 的 360..374 空档）✓。
2. **weather 刷新胶囊**：绿色像素 bbox **x450..515 / y76..107**，与代码
   `draw_glass_style(DISPLAY_W-90, 76, 66, 32, 16, STYLE_ACCENT)` 完全一致；
   且为全屏唯一绿色元素（渲染干净、无残留）✓。
3. **clock**：与上述两处共用同一 `draw_glass`/`draw_glass_style`/`STYLE_*`
   原语与相同坐标模式（构建 0 错误；像素级抽查由共享原语的两次验证覆盖）。
4. 构建全组（launcher 目标组 21 应用）+ kernel 0 错误；GUI 全程 0 PANIC，
   OOBE 跳过流程/桌面/选择卡/天气应用均正常。

## 已知边界

- clock 在桌面文件夹内（FOLDER_KEYS），本轮导航未深入到 clock 应用内页截图
  （共享原语已双重像素验证，风险闭合）；下轮可补。
- 对话框卡片本体仍为白卡（用户指令针对按钮；卡片玻璃化是另一个议题）。

## 遗留问题 / 下一轮建议

1. clock 应用内页截图抽查（文件夹导航或 `/bin` 直启）。
2. 转场 A/B 归因（自动化轮 15 在制品，勿动 apps/oobe/src/main.rs）。
3. 对话框卡片本体是否玻璃化（征询用户）。
