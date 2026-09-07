# KunxOS Terminal / TTY / PTY 子系统（Phase 21）

本文档说明 KunxOS 终端应用的架构与内核侧 TTY/PTY 抽象：图形终端（PTY 主侧）与内核 Shell（从侧，规范行编辑）之间键盘→屏幕输出字节流的完整路径、可用命令与已知限制。

## 1. 关键设计问题（任务要求逐项确认）

### 当前 Shell 是独立二进制还是内建于内核？
**内建于内核**。Shell 是 `kunxcore/src/shell.rs` 的 `shell_main` 内核进程（EL1），由 `main.rs` 经 `process::spawn("shell", shell::shell_main, ...)` 拉起，直接调用 VFS / ELF 加载器 / 包管理器，没有单独的用户态 `/bin/sh`。

**对 PTY 设计的影响**：Unix 中「GUI 终端 fork 出 shell 子进程」的语义在本内核等价于「内核 Shell 进程绑定会话 0 作为 PTY 从侧」。因此 PTY 主从对以内核 TTY 会话实现：

```
  图形终端（EL0，主）                     内核 Shell（EL1，从）
  write(/dev/tty1) 键码 ──► in_q  ──►  read_line() 行编辑/信号
  read(/dev/tty1)        ◄── out_q ◄──  uart_send_char()（屏幕输出+回显）
```

`/dev/tty1` 即 PTY 主口；从侧是绑定的内核 Shell。若未来加入用户态 `/bin/sh`（fork+execve），可用 `elf::spawn_from_path`（`cmd run` 已使用）把子进程接入同一会话，与现有主从模型兼容（预留点位已在代码注释中标出）。

### 图形终端走 /dev/fb0 直绘还是合成器？
**走合成器**。与其它桌面应用一致：`open("/dev/fb0")` + `mmap` 内核合成器的持久应用画布（`compositor::fb_target`），全屏直绘后以 `FBIO_PRESENT` 提交整屏脏矩形，由合成器脏区拷贝 + 页翻转上屏，不做绕过合成器的裸显存直写。

## 2. 内核侧 TTY 会话（kunxcore/src/tty.rs）

- `MAX_TTY = 4` 个会话槽位，每槽：
  - `in_q`（4096B 环形）：主侧→从侧（图形终端写入的键码/CSI 序列）；
  - `out_q`（16KB 环形）：从侧→主侧（内核输出镜像，供终端渲染）；
  - `attached`：主侧是否已附接；
  - `overflow`：out_q 写满置位（终端显示「输出截断」后清除）；
  - `cols/rows`：窗口尺寸（TIOCGSWINSZ/TIOCSWINSZ）。
- 会话 0 绑定内核 Shell；`/dev/tty1`（minor 1）为默认图形终端。
- **输出镜像**：`main.rs::uart_send_char` 末尾调用 `tty::echo_mirror(c)`，将串口控制台输出（boot 日志 + Shell 全部输出）同步到会话 0 的 out_q；未附接时零开销布尔判断。
- **输入路由**：`shell::read_line` 改用 `tty::shell_input()`：会话附接时只消费 in_q（不读 UART）；未附接回退 UART（传统串口控制台）。
- 环形队列操作均在 DAIF 屏蔽临界区内（IRQ 安全）。

### 设备节点
| 节点 | 说明 |
|---|---|
| `/dev/tty` | minor 0：UART 直通（原串口控制台，保留） |
| `/dev/tty1..4` | 会话终端：主侧读写（PTY 主口） |
| `/dev/ptmx` | PTY 分配器（多终端预留）：open 预留下一空闲会话；TIOCGPTN 返回从侧号 |

## 3. 内核 Shell 行编辑（shell.rs::read_line）

PTY 从侧规范行编辑（读 in_q，回显经 uart 镜像到 out_q）：

- 字符输入：在光标处插入（行中插入重绘尾部并移回光标）；
- 光标移动：←→（`ESC[D`/`ESC[C`）、Ctrl+A（行首）、Ctrl+E（行尾）；
- 删除：Backspace（行中删除）、Ctrl+U（删至行尾）、Ctrl+W（删前一词）；
- 历史：↑↓ 浏览 16 条；Tab 补全（命令词：内置列表 + /bin；其余按 cwd 路径）；
- 信号：
  - `Ctrl+C`：丢当前行、输出 `^C`；有前台子进程（`run`/`busybox`）时发 SIGINT；
  - `Ctrl+D`：空行为 EOF，有前台子进程时发 SIGTERM（否则忽略，避免误杀 Shell）；
  - `Ctrl+Z`：丢行 + 输出 `^Z`，有前台子进程时发 SIGTSTP（挂起预留）。
- 提示符：`$PS1` 环境变量（`export PS1=...` 可定制），默认 `kunxos$ `。

### 新增内置命令
- `alias` / `alias name=value` / `alias name` / `alias name=`（定义/查询/删除）；
- `unalias name` / `unalias *`；
- `source <file>`（别名 `sh`）：读脚本逐行执行（跳过空行与 `#` 注释，命令词先做别名展开）——简单脚本支持；
- 兼容已有：`cd` `pwd` `exit` `export` `env` `echo` `ls` `cat` `run` `busybox` `pkg` `history` 等（管道 `|`、重定向 `< > >>`、环境变量为 Phase 8 已有）。

## 4. 图形终端应用（apps/terminal/src/main.rs）

- 界面：玻璃标题栏（返回键）+ 终端文本区 + 滚动条 + 系统导航栏；
- VT 仿真器（文本网格 96×60）：
  - 光标移动 `ESC[A/B/C/D`、定位 `ESC[H/f/G/d`、存/取光标 `ESC[s/u`；
  - 清屏 `ESC[2J`（当前屏推入回看 = Linux `clear` 语义）、`ESC[J`/`ESC[K`；
  - SGR：`0` 重置、`1/22` 粗体、`30-37/90-97` 前景、`40-47/100-107` 背景；
  - 控制字符：`\r`、`\n`（滚动）、`\t`、`\b`、`\x0c`（清屏）；
  - `ESC[?...l/h` DEC 私有序列与 `>`/`=` 键区修饰忽略（不干扰 Shell 输出）；
- 滚动回看：24 行环形历史；终端区竖直拖动滚动，滚动条滑块显示位置，新输出（换行）自动回到底部（`SCROLL=0`）；
- 字体渲染复用 `ttf_text/font_render`（提示词 #11 国际化文本渲染）：ASCII + CJK 混排，CJK 按字形实际推进（宽字符），光标位置按字形累计；
- 光标块闪烁（500ms 周期），仅跟随底部时可见；
- 输入转发：ASCII/Shift 大写、Enter(`\r`)、Backspace(`\x7f`)、Tab(`\t`)、方向键(`ESC[A..D`)、Ctrl+字母→控制字节（`^A..^Z`）、ESC 原样转发；
- 本应用不做本地回显（哑终端，回显来自内核）。

## 5. 交互流程示例

```
launcher 启动 terminal：
  1. open /dev/fb0 → mmap 合成器画布 → 提交首帧；
  2. open /dev/tty1（PTY 主口），TIOCSWINSZ 上报列×行；
  3. 内核 Shell 检测到会话附接，提示符经镜像出现在屏幕；
  4. 键入 → 转发 → in_q → Shell 行编辑/回显 → out_q → 终端渲染；
  5. export PS1="kunxos# "、alias ll="ls -l"、source /etc/script.sh；
  6. Ctrl+C/D/Z 发信号；方向键/历史/Tab 补全在 Shell 内完成；
  7. 返回键 / 导航栏 Back / 全面屏手势退出，内核 Shell 回到 UART 控制台。
```

## 6. 已知限制与后续

- 初始范围单活动会话：`/dev/tty1` 绑定 Shell；`/dev/tty2..4` 预留多终端（ptmx 分配器已就绪，Shell 绑定切换留后续）；
- 回看行丢色彩（历史行按默认前景渲染）；
- 回看上限 24 行（内存预算约束）；
- Ctrl+Z 为 SIGTSTP 预留接口，无前台子进程时仅丢行；
- 无本地剪贴板/软键盘（可用内核 `cd`/`ls`/`help` 等验证交互）。