# KunxOS 应用开发指南（如何开发 `.kunx` 应用）

本文档说明如何在 KunxOS 上开发原生应用并打包为 `.kunx` 应用包。
适用对象：需要编写 GUI 应用、命令行工具或系统服务，并希望集成到桌面 /
软件源 / 应用商店的开发者。

相关文档：
- [`docs/kunx-package-format.md`](kunx-package-format.md) — `.kunx` 包格式与签名规范
- [`docs/lifecycle.md`](lifecycle.md) — 应用生命周期状态机与后台管理
- [`docs/architecture.md`](architecture.md) — 系统整体架构
- [`docs/DEBUGGING.md`](DEBUGGING.md) — 内核调试与崩溃报告
- [`docs/app-store.md`](app-store.md) — 应用商店与 R2 软件源

---

## 1. 应用运行模型

KunxOS 应用是 **EL0 用户态独立进程**，与主流的“APK/IPA + 系统框架”不同：

- **无运行时、无系统库依赖**：`#![no_std]` + `#![no_main]`，直接裸 syscall
  （`svc #0`，AArch64 asm-generic 系统调用号），不使用 `std`/alloc。
- **全屏独占式窗口**：屏幕逻辑分辨率固定为 **540×960（RGBA8888）**。
  应用打开 `/dev/fb0`（openat）并 `mmap` 到用户空间直接写像素，退出后由
  launcher 重绘桌面。当前为“单前台全屏应用”模型，无窗口管理器。
- **启动方式**：launcher 点击图标 → `fork`（clone SIGCHLD）+ `execve /bin/<name>`。
  子进程在 execve 前 `setuid(10000 + n)`，构成 **per-app UID 沙箱**
  （非 root 不能向 launcher/系统服务或其他应用发信号）。
- **输入来源**：轮询 `/dev/input/event0`（linux input_event：触摸 EV_ABS /
  鼠标 EV_REL / 按键 EV_KEY）。
- **后台化**：应用在 Home / 最近任务后由 lifd 配合内核 SIGSTOP/SIGCONT 冻结与
  恢复（内存完整保留），状态可在宽限窗口内落盘 `/data/state/<app>.bin`。

![应用运行模型] 用文字描述即：

```
launcher (桌面/图标/转场动画)
   │ fork + execve /bin/<name>, setuid(SANDBOX_UID)
   ▼
应用进程 (apps/<name>, EL0, no_std)
   │ open+mmap /dev/fb0 → 写 BACK_BUF → flip / FBIO_PRESENT
   │ poll /dev/input/event0 → 点击/手势/按键
   ▼
exit(0)  → launcher wait4 回收 → 重绘桌面
```

---

## 2. 工程结构

每个应用是 apps/ 下一个独立 Cargo 工程，以 `apps/launcher`、
`apps/calculator`、`apps/notes` 等为参考：

```text
apps/<name>/
├── Cargo.toml      # no_std 应用，release 配置
├── build.rs        # 传递 linker.ld 与 max-page-size
├── linker.ld       # 用户态 ELF 布局（ENTRY(_start)）
└── src/
    └── main.rs     # 应用主逻辑（入口 _start → k_main）
```

### 2.1 Cargo.toml

```toml
[package]
name = "calculator"
version = "0.1.0"
edition = "2021"

[dependencies]
fontdue = "0.9.4"          # 可选的 crate 依赖（宿主编译期字体光栅化等）；
                           # 运行时不依赖，编译期代码由 #[path] 引入的共享模块使用

[profile.release]
opt-level = "z"            # 体积优先
lto = true
codegen-units = 1
panic = "abort"
strip = "symbols"
```

> 注意：应用是交叉编译到 `aarch64-unknown-none`（`TARGET`），
> release 产物为 ELF，直接由内核 `elf.rs` 加载。不要依赖 std / alloc。

### 2.2 build.rs 与 linker.ld

两者必须存在，且与模板一致（`apps/stub/build.rs`、`apps/stub/linker.ld`）：

```rust
// build.rs
fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");
}
```

```ld
/* linker.ld：.text 置于 0x400000，入口 _start，丢弃 eh_frame 等 */
ENTRY(_start)
SECTIONS {
    . = 0x400000;
    .text : { KEEP(*(.text._start)) *(.text .text.*) *(.rodata .rodata.*) }
    . = ALIGN(0x1000);
    .data : { *(.data .data.*) }
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) }
    /DISCARD/ : { *(.eh_frame) *(.eh_frame_hdr) *(.comment) *(.note .note.*) }
}
```

### 2.3 入口：`_start` 蹦床 + `k_main`

```rust
#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // ... 应用逻辑 ...
    unsafe { exit(0) }
}

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",      // argc
    "    mov x1, x2",            // argv
    "    bl k_main",
    "    mov x8, #93",           // SYS_EXIT
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { /* 打印 + exit(1) */ }
```

---

## 3. 共享开发库（`shared/`）

共享代码位于 `shared/`，应用通过 **`#[path]` 引入**（不复制、不链接）：

```rust
#[path = "../../../shared/app_kit.rs"] mod kit;   // 主框架
#[path = "../../../shared/ttf_text.rs"] mod ttf_text;
#[path = "../../../shared/nav.rs"]     mod sys_nav;
#[path = "../../../shared/lifecycle.rs"] mod lifecycle;
#[path = "../../../shared/glass.rs"]   mod glass;
#[path = "../../../shared/cursor_overlay.rs"] mod cursor_overlay;
#[path = "../../../shared/error_ui.rs"] mod err_ui;
#[path = "../../../shared/sensor_client.rs"] mod sensor_client;
```

`shared/app_kit.rs`（完整 API 见文件内；以下为常用能力）：

| 类别 | API |
|---|---|
| 系统调用 | `syscall1/2/3/6`、`SYS_*` 常量（read/write/openat/close/ioctl/exit/nanosleep/mmap/gettimeofday/getuid/socket/connect/sendto/recvfrom/shutdown/dns_resolve/…） |
| 文件 | `open(path, flags)`、`write_fd`、`print`、`print_dec`、`sleep_ms` |
| 显示 | `fb_init()`（打开并 mmap `/dev/fb0`，查询分辨率）、`flip()`（整屏提交）、`fb_damage_present()`（脏区提交 FBIO_PRESENT） |
| 绘制 | `put_pixel`、`fill_rect`、`draw_rect`、`draw_line`、`draw_circle`、`draw_disc`、`draw_gradient_bg` |
| 字体 | `ttf_text` / `font_render`（cn.ttf 平滑字形）、`draw_char`、`draw_cjk`、`draw_text`、`text_width`、`draw_center`、`FONT8X8` 位图字体 |
| 玻璃效果 | `glass`/`draw_glass`、`draw_glass_style`、`draw_glass_button`、`draw_glass_key`、`glass_dark()` |
| UI 骨架 | `draw_title_bar`、`back_hit`、`content_bottom`、`show_error`/`draw_error_banner`/`error_banner_hit` |
| 系统导航 | `sys_nav_hit`、`nav_exit_code`、`draw_sys_nav`、`draw_cursor`、`cursor_overlay_*` 快速路径 |
| 鼠标指针 | `cursor_sprite`（随 `app_kit`/`cursor_overlay` 自动引入）：系统指针贴图（`tubiao/shubiao.png` 生成，24×24 + 热点），`draw_at`/`blit` alpha 混合，常量 `CUR_W/CUR_H/CUR_HOT_X/CUR_HOT_Y` |
| 输入 | `poll_input() -> (Option<(x,y)>, NavAction, moved)`、`poll_tty_esc()` |
| 时间 | `now_us`、`now_hms`、`now_date`、公历换算 `civil_from_days`/`days_from_civil`/`weekday_from_days` |
| 生命周期 | `lifecycle::`：`lc_init`、`lc_poll`、`lc_home`、`lc_go_recents`、`lc_handle_nav`、`lc_request_bg_task`、`lc_end_bg_task`、`lc_save_state`、`lc_restore_state`、`lc_notify_local`、`lc_needs_redraw`、`LcCallbacks`（on_pause/on_resume/on_suspend/on_terminate） |

### 3.1 一个最小的 GUI 主循环（模式）

```rust
fn k_main(_argc: usize, _argv: *const usize) -> usize {
    kit::fb_init();                       // mmap /dev/fb0
    kit::font_init_checked();             // 加载 cn.ttf（失败降级位图字体）
    kit::lifecycle::lc_init();            // 生命周期信箱（可选，推荐）
    redraw();
    loop {
        let (click, nav, moved) = kit::poll_input();
        if moved { /* 光标 overlay 快速路径 */ }
        if nav != kit::NavAction::None {
            if !kit::lifecycle::lc_handle_nav(nav) { break; }  // Back=返回
            continue;
        }
        if let Some((x, y)) = click {
            if kit::back_hit(x, y) { break; }
            /* 命中测试 + 更新状态 */
        }
        if kit::poll_tty_esc() { break; }  // ESC 也可退出
        kit::sleep_ms(16);                 // ~60fps 帧率
    }
    unsafe { kit::exit(0) }
}
```

---

## 4. 编写一个应用的完整步骤

### 步骤 1：建立工程

```sh
# 以 stub 为最小模板（或克隆某个已实现应用更省事）
mkdir -p apps/mykunx/src
cp apps/stub/Cargo.toml   apps/mykunx/
cp apps/stub/build.rs     apps/mykunx/
cp apps/stub/linker.ld    apps/mykunx/
# 修改 Cargo.toml 的 name/version；编写 apps/mykunx/src/main.rs
```

### 步骤 2：实现 UI 与逻辑

参照 `apps/calculator` / `apps/notes` / `apps/launcher`：
双缓冲 `BACK_BUF: [u32; 540*960]` → 绘制 → `flip()`；
输入事件流解析（EV_ABS/EV_REL/EV_KEY + BTN_TOUCH/BTN_LEFT）；
标题栏返回键 / 系统导航栏 / 错误横幅 / 光标 overlay。

### 步骤 3：加入构建系统（Makefile）

只需在 `Makefile` 的三处登记你的应用：

```make
# (1) 变量区
MYAPP  := $(APPS)/mykunx
MYAPP_ELF := $(MYAPP)/target/$(TARGET)/release/mykunx

# (2) app_rule 列表（与现有应用一致）
$(foreach a,mykunx phone sms contacts …,$(eval $(call app_rule,$a)))

# (3) RAMDISK 依赖 + 复制进 rootfs
#     $(RAMDISK) 依赖列表追加 $(MYAPP_ELF)，并在配方中：
#     cp -f $(MYAPP_ELF) $(ROOTFS)/bin/mykunx

# (4) 若有纯命令行工具（无 GUI），参照 audiotest/audiomix/audiolat 的单条规则
```

### 步骤 4：让应用显示在桌面上

launcher 的桌面表是**编译期静态表**（`apps/launcher/src/main.rs:186` 的 `APPS`）。
新增应用需要：在 `APPS` 数组追加 `App { key, name, name_zh, path: "/bin/mykunx", color, default_on }`，
并按需加入 `PAGE0_APPS` / `PAGE1_APPS` / `DOCK_KEYS` / `FOLDER_KEYS`。

### 步骤 5：构建并运行

```sh
make mykunx      # 只编译应用
make launcher    # 同时重编桌面（桌面表有改动时必须）
make             # 全量：内核 + rootfs
make run-gui     # QEMU 图形环境验证
```

---

## 5. 打包为 `.kunx`

应用 ELF 编译完成后，用主机侧 `tools/kunxrepo/mkkunx.py` 打包并签名
（格式规范见 `docs/kunx-package-format.md`）。

### 5.1 manifest.json（放在 `tools/kunxrepo/manifests/<name>.json`）

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✅ | 包名 `<28` 字节，`[A-Za-z0-9._-]`（将作为 `/bin/<name>`、/data/app/<name>、桌面 key） |
| `version` | ✅ | 语义版本 `X.Y.Z[-prerelease][+build]` |
| `api_level` | ✅ | `≤` 系统 `API_LEVEL`(=1, `kunxpkg/src/trust.rs`) 才可安装 |
| `main` | ✅ | 入口 ELF 在包内的路径，如 `"bin/mykunx"` |
| `description` / `author` | | 描述与作者 |
| `permissions` | | 权限声明（见 §6） |
| `dependencies` | | `{"libc": ">=1.0.0"}` 版本约束 |
| `provides` | | 提供的虚拟名（冲突检测用） |
| `conflicts` | | 冲突包/虚拟名（已安装则拒绝） |

示例：

```json
{
  "name": "mykunx",
  "version": "1.0.0",
  "api_level": 1,
  "description": "我的 KunxOS 应用",
  "author": "me",
  "main": "bin/mykunx",
  "permissions": ["INTERNET"],
  "dependencies": {"libc": ">=1.0.0"},
  "provides": [],
  "conflicts": []
}
```

### 5.2 载荷条目与目录映射

`mkkunx.py` 支持三类条目（前缀约定 → 安装器映射）：

| 条目前缀 | 映射到 | 用途 |
|---|---|---|
| `bin/<name>` | `/bin/<name>` | 入口 ELF（`--elf` 自动生成） |
| `res/<resname>` | `/res/<name>/<resname>` | 应用资源（`--res 安装名=源文件`，可重复） |
| `data/<rest>` | `/data/app/<name>/<rest>` | 沙箱预置数据（`--data 安装名=源文件`，可重复） |

约束：条目 ≤64、路径 ≤128 字节、白名单字符
`[A-Za-z0-9._/-]`，拒绝 `..`/绝对路径/双斜杠/NUL。

### 5.3 密钥与证书

```sh
# 一次性初始化（生成 kunx-root / kunx-repo / kunx-dev + 签发开发者证书）
python3 tools/kunxrepo/genkeys.py --issue-dev

# 每次发布：用开发者私钥+证书签名打包
python3 tools/kunxrepo/mkkunx.py \
    --manifest tools/kunxrepo/manifests/mykunx.json \
    --elf apps/mykunx/target/aarch64-unknown-none/release/mykunx \
    --res icon.bin=res/myicon.bin \
    --key tools/kunxrepo/keys/kunx-dev.key \
    --cert tools/kunxrepo/keys/kunx-dev.cert \
    --out /tmp/mykunx-1.0.0.kunx
```

> 信任链：系统根锚 `ROOT_ANCHOR`（内嵌 `kunxpkg/src/trust.rs`）→ 开发者证书
> (leaf, KU_PACKAGE) → 包 Ed25519 签名。**设备只信任由根锚签发的证书**；仓库的
> `keys/` 是 demo 密钥，生产环境必须离线生成并保管根私钥。
> 更换根密钥时须同步更新 `kunxpkg/src/trust.rs` 里的 `ROOT_ANCHOR` 并重建系统。

主机侧验签（复用设备端同一份代码，`tools/kunxverify`）：

```sh
kunxverify check /tmp/mykunx-1.0.0.kunx
```

---

## 6. 权限与沙箱

- **per-app UID**：launcher 以 root 启动应用前统一 `setuid`（基址 10000），
  内核的 kill 权限检查阻止跨应用发信号 → 最小进程沙箱。
- **manifest `permissions`** 声明即审批单（`kunxpkg/src/perms.rs`）：
  `INTERNET`(Normal)、`STORAGE`/`CAMERA`/`MICROPHONE`/`LOCATION`/`SMS`/`PHONE`(Dangerous)。
  安装时 `pkg install` 会列出权限；`pkg perms <name> [grant|revoke <perm>]` 可运行时调整。
- **文件沙箱**：应用预置数据在 `/data/app/<name>/`；系统数据根 `/data/<app>`
  为惯例位置（如 `/data/notes/`）。当前无强制文件路径沙箱，遵守惯例即可。

---

## 7. 分发与安装

### 7.1 本地软件源（推荐开发流程）

```sh
# 生成签名索引 Packages / Packages.sig / Packages.meta
python3 tools/kunxrepo/build_demo_repo.py /tmp/kunx-demo-repo <apps-elf-dir>
# 设备端（QEMU shell 或终端应用里）：
pkg update                 # 拉取并验签索引（file:// 源）
pkg install mykunx --yes   # 依赖解析 + 权限确认 + 安装
pkg list / pkg search mykunx / pkg info mykunx
pkg verify mykunx          # 重新验签（证书链 + 包签名）
```

Rootfs 已内置 demo 仓库（`rootfs/var/lib/pkg/repo/main`，由 Makefile 的
`build_demo_repo.py` 构建，含 calculator/terminal/notes/…）。新增包只需在
`tools/kunxrepo/manifests/` 放好 manifest，Makefile 的 `.kunx-done` 目标会自动
打包进 `/tmp/kunx-demo-repo` 再复制进 rootfs。

### 7.2 安装本地文件

```sh
pkg install-file /data/cache/mykunx-1.0.0.kunx --yes
pkg review mykunx-1.0.0.kunx   # 安装前审核（证书+签名+权限）
```

### 7.3 卸载/更新/回滚

```sh
pkg remove mykunx [--keep-data]
pkg install mykunx    # 版本 >= 已装版本 → 更新
pkg rollback mykunx   # 回滚到备份版本
```

### 7.4 应用商店（R2 软件源）

商店从 R2（S3 兼容）拉取 `Packages.json` 索引 → 下载 `.kunx` →
`review::review` 验签 → `/bin/pkg install-file` 安装（详见 `docs/app-store.md`）。

---

## 8. 生命周期与后台（可选但推荐）

接入 `shared/lifecycle.rs` 后，应用可获得 iOS/Android 式后台化、恢复、
状态持久化与本地通知（完整状态机见 `docs/lifecycle.md`）：

```rust
kit::lc_set_name(b"mykunx");                 // 显式应用名（默认从 argv[0] 推导）
kit::lc_init();
kit::lc_set_callbacks(LcCallbacks {
    on_pause:     || { /* 暂停动画/音频 */ },
    on_resume:    || { /* 重绘/恢复 */ },
    on_suspend:   || {
        // 在 SIGSTOP 前的宽限窗口内保存状态（同步落盘）
        let _ = kit::lc_save_state(b"...UI 状态...");
    },
    on_terminate: || { /* 终止前最终保存（尽力而为） */ },
});

// 主循环中把 exit(nav_exit_code(ev.nav)) 换成：
if nav != None && !kit::lc_handle_nav(nav) { break; }

// 后台任务、本地通知
kit::lc_request_bg_task(kit::BGT_AUDIO, 3600_000); // 后台播放音频 1 小时
kit::lc_notify_local(b"标题", b"正文");             // 经 pushd 投递
```

`poll_input()` 内部已自动调用 `lc_poll()`，大多数应用零额外改动即可后台化；
恢复后如有重绘需求，检查 `lc_needs_redraw()`。

---

## 9. 系统服务与命令行工具（非 GUI 应用）

无需 GUI 的工具同样适用上述工程结构，只是不打开 framebuffer：

- 桌面应用：`apps/`（phone/sms/…）/ 工具类：`apps/audiotest`、`apps/modemtest`、
  `apps/sfsrace`、`apps/crashme`
- 后台守护进程：`kunxserv/`（lifd/pushd/networkd/wifid/rild/audiod/camerad/
  sensord/positiond/logd/alarmd），由 `kunxinit`（PID 1）按 `/etc/inittab` 拉起
- 网络：TCP/UDP 走 `syscall(socket/connect/sendto/recvfrom)`，DNS 走
  `SYS_DNS_RESOLVE`(403)；HTTP 客户端参考 `apps/weather`、`apps/cloud`、
  `apps/store/src/r2.rs`
- 音频：`shared/audio.rs` + `/bin/audiod` 混音；传感器：`shared/sensor_client.rs` +
  `/bin/sensord`；定位：`shared/sensor_client.rs` + `/bin/positiond`

---

## 10. 调试与测试

| 工具 | 用途 |
|---|---|
| `make run-debug` | 串口走 TCP，宿主 gdb `target remote localhost:1234`（内核 GDB 桩） |
| `make run` / `make run-gui` | 无图形 / 图形环境运行 |
| `/dev/kmsg` + `/bin/logd` | 结构化日志 |
| tombstone | 崩溃落盘 `/data/crash/`（`crash` 工具 + `docs/DEBUGGING.md`） |
| `tests/scripts/apps_regression.py` | 各应用启动/退出回归 |
| `tests/scripts/kunx_pkg_test.py` | 包管理器 & `.kunx` 验签回归 |
| `tests/scripts/lifecycle_test.py` | 生命周期（后台化/恢复/LMK）回归 |
| `tests/scripts/devtool_verify.py` / `tests/scripts/verify_files_app.py` | 专项验证 |

应用自身打印用 `print()`（走 fd 1/2，可在串口看到）；调试模式下
`print("[mykunx] …")` 均可从 QEMU 串口观察。

---

## 11. 约定与常见问题

1. **分辨率写死**：540×960。绘制前用 `fb_init()` 查询的 `FB_W/FB_H` 做边界钳制，
   不要假设 stride=width（用 `FB_STRIDE_PX`）。
2. **无堆/无 std**：字符串与数字格式化需手写（`fmt_i64`、`print_dec` 模式）；
   共享模块已提供大部分工具，避免引入 std 依赖。
3. **UID 沙箱**：应用是非 root（10000+），不能向 launcher/服务发信号；需要
   root 能力请通过系统服务（daemon）而非应用本身。
4. **签名与信任锚**：demo 密钥仅限开发；生产密钥离线生成。证书过期或根锚
   更换都会导致安装被拒绝。
5. **`api_level`**：manifest 的 JSON 字段名为 `api_level`（内核解析为内部
   字段 `min_api`），必须 `≤ API_LEVEL`(1)，否则 `pkg install` / store 拒绝。
6. **退出协议**：应用退出由 launcher 的 `wait4` 回收并重绘桌面；返回键/
   ESC 都应当 `exit(0)`（或按导航动作 `nav_exit_code`）。
7. **主循环节奏**：`sleep_ms(16)` ≈ 60fps；追求流畅可在输入事件就绪时立即
   处理、空闲才 sleep，参考各应用的 `poll_input` 非阻塞轮询。
8. **新应用集成三处**：Makefile（构建+拷入 rootfs）、launcher `APPS` 表、
   （可选）`tools/kunxrepo/manifests/`（打包进软件源）。

## 12. 完整示例索引

- 最小 GUI：`apps/stub`（占位模板）、`apps/calculator`（全屏网格 UI + 玻璃按键）
- 复杂界面：`apps/settings`、`apps/files`、`apps/browser`（多页面/网络）
- 生命周期示范：`apps/lcapp`（docs/lifecycle-test.md 的 GUI 验证载体）
- 状态持久化：`apps/notes`（`/data/notes/` 存储）
- 网络：`apps/weather`、`apps/cloud`（DNS+HTTP）、`apps/store`（S3 签名）
- 工具类：`apps/audiotest` / `apps/modemtest` / `apps/sfsrace` / `apps/crashme`
- 守护进程：`kunxserv/{lifd,pushd,networkd,wifid,rild,audiod,camerad,sensord,positiond,logd,alarmd}`