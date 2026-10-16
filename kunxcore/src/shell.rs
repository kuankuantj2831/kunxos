//! 交互式 Shell（Phase 8）：命令解析、管道、重定向与内置命令。
//!
//! 作为内核进程（EL1）运行，直接调用 VFS / ELF 加载器 / 包管理器。
//! 支持内置命令：`cd`、`echo`、`export`、`exit`、`pwd`、`ls`、`cat`、
//! `run`、`pkg`、`help`；以及 `|` 管道、`<` / `>` / `>>` 重定向。

use crate::elf;
use crate::pkg;
use crate::process;
use crate::thread;
use crate::vfs;

// ==================== 常量 ====================

const LINE_MAX: usize = 256;
const MAX_TOK: usize = 32;
const MAX_ARGC: usize = 16;
// 64：与内核 execve 的 EXEC_STR_LEN 一致；32 时常见 repo 路径
// （/var/lib/pkg/repo/main/files/xxx.kunx ≈ 50 字符）作参数必然截断，
// review/install-file/verify 等长路径命令无法使用。
const ARG_LEN: usize = 64;
const MAX_PIPE: usize = 4;
/// 管道/重定向输出缓冲（逐级拷贝的中间产物上限）。
///
/// 缓冲位于 .bss（static，shell 为单线程内核进程，无并发），因此可以
/// 远大于栈帧限制而不会占用任务栈。
const OUT_MAX: usize = 32768;
const MAX_ENV: usize = 16;
/// 命令历史条数。
const HIST_MAX: usize = 16;

// ==================== 词法单元 ====================

#[derive(Clone, Copy)]
struct Word {
    buf: [u8; ARG_LEN],
    len: usize,
}

const EMPTY_WORD: Word = Word { buf: [0; ARG_LEN], len: 0 };

fn word_str(w: &Word) -> &str {
    core::str::from_utf8(&w.buf[..w.len]).unwrap_or("")
}

const OP_WORD: u8 = 0;
const OP_PIPE: u8 = 1;
const OP_IN: u8 = 2;
const OP_OUT: u8 = 3;
const OP_APPEND: u8 = 4;

#[derive(Clone, Copy)]
struct Token {
    word: Word,
    op: u8,
}

// ==================== 命令结构 ====================

#[derive(Clone, Copy)]
struct Command {
    argv: [Word; MAX_ARGC],
    argc: usize,
    in_redir: Word,
    out_redir: Word,
    append: bool,
}

impl Command {
    const fn new() -> Self {
        Command {
            argv: [EMPTY_WORD; MAX_ARGC],
            argc: 0,
            in_redir: EMPTY_WORD,
            out_redir: EMPTY_WORD,
            append: false,
        }
    }
}

#[derive(Clone, Copy)]
struct Pipeline {
    cmds: [Command; MAX_PIPE],
    n: usize,
}

// ==================== 全局状态 ====================

/// 当前工作目录。
static mut CWD: [u8; 128] = [0; 128];
/// 路径解析临时缓冲。
static mut PATH_BUF: [u8; 128] = [0; 128];
/// 路径规范化临时缓冲。
static mut NORM_BUF: [u8; 128] = [0; 128];
/// 环境变量。
static mut ENV_NAMES: [[u8; 32]; MAX_ENV] = [[0; 32]; MAX_ENV];
static mut ENV_VALUES: [[u8; 64]; MAX_ENV] = [[0; 64]; MAX_ENV];
static mut ENV_COUNT: usize = 0;
/// 别名表上限。
const MAX_ALIAS: usize = 12;
/// 命令别名（alias name=value；列表模式逐一列出）。
static mut ALIAS_NAMES: [[u8; 24]; MAX_ALIAS] = [[0; 24]; MAX_ALIAS];
static mut ALIAS_VALUES: [[u8; 96]; MAX_ALIAS] = [[0; 96]; MAX_ALIAS];
static mut ALIAS_COUNT: usize = 0;
/// 前台子进程 pid（Ctrl+C/D/Z 的信号目标；0 = 当前无外部程序等待）。
static mut FG_CHILD: u32 = 0;
/// `source` 脚本内容缓冲（32768 字节；放 .bss 避免任务栈溢出）。
static mut SRC_BUF: [u8; OUT_MAX] = [0; OUT_MAX];

/// 当前前台子进程（外部程序 `run`/`busybox` 等待期间；Shell 行编辑用）。
pub fn fg_child() -> usize {
    unsafe { FG_CHILD as usize }
}

/// 记录前台子进程（启动外部程序后、进入 wait 前调用）。
pub fn fg_child_set(pid: usize) {
    unsafe { FG_CHILD = pid as u32; }
}

/// 清除前台子进程（外部程序退出、wait 返回后调用）。
pub fn fg_child_clear() {
    unsafe { FG_CHILD = 0; }
}

/// 提示符字符串：优先环境变量 `PS1`（终端可 `export PS1=...` 定制），
/// 未设置时使用默认 `kunxos$ `。
pub fn shell_prompt_str() -> &'static str {
    env_get("PS1").unwrap_or("kunxos$ ")
}

fn static_str(buf: &[u8]) -> &'static str {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    // buf 实际指向 static 数组：先取 'static 切片（sound，数据确为静态），
    // 再做 UTF-8 校验——非法字节（echo/export 等可能输入任意字节）降级为空串，
    // 避免 from_utf8_unchecked 的未定义行为。
    let slice: &'static [u8] = unsafe { core::slice::from_raw_parts(buf.as_ptr(), len) };
    core::str::from_utf8(slice).unwrap_or("")
}

fn cwd_str() -> &'static str {
    unsafe { static_str(&CWD) }
}

fn set_cwd(s: &str) {
    unsafe {
        CWD.fill(0);
        let n = s.len().min(CWD.len() - 1);
        CWD[..n].copy_from_slice(&s.as_bytes()[..n]);
    }
}

/// 规范化绝对路径：折叠 `.` / `..` 分量，结果写入 `NORM_BUF`。
///
/// 分量 >32 或超长分量放不下缓冲时**返回空串**（调用方报错），
/// 而不是静默截断成另一条路径去操作错误文件。
fn normalize(path: &str) -> &'static str {
    let b = path.as_bytes();
    let mut comps: [(usize, usize); 32] = [(0, 0); 32];
    let mut n = 0usize;
    let mut overflow = false;
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && b[i] == b'/' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && b[i] != b'/' {
            i += 1;
        }
        let len = i - start;
        if len == 1 && b[start] == b'.' {
            continue;
        }
        if len == 2 && b[start] == b'.' && b[start + 1] == b'.' {
            if n > 0 {
                n -= 1;
            }
            continue;
        }
        if n < 32 {
            comps[n] = (start, len);
            n += 1;
        } else {
            overflow = true; // 分量超过 32：路径过长
        }
    }
    unsafe {
        let buf = &mut NORM_BUF;
        buf.fill(0);
        let mut off = 0usize;
        for j in 0..n {
            if off < buf.len() - 1 {
                buf[off] = b'/';
                off += 1;
            }
            let (s, l) = comps[j];
            let c = l.min(buf.len().saturating_sub(off + 1));
            if c < l {
                overflow = true; // 分量被截断：路径过长
            }
            if off + c < buf.len() {
                buf[off..off + c].copy_from_slice(&b[s..s + c]);
                off += c;
            }
        }
        if off == 0 {
            buf[0] = b'/';
        }
    }
    if overflow {
        return "";
    }
    unsafe { static_str(&NORM_BUF) }
}

/// 把相对路径基于 cwd 解析为规范化的绝对路径。
fn resolve_path(p: &str) -> &'static str {
    let cwd = cwd_str();
    unsafe {
        let buf = &mut PATH_BUF;
        buf.fill(0);
        if p.starts_with('/') {
            let n = p.len().min(buf.len() - 1);
            buf[..n].copy_from_slice(&p.as_bytes()[..n]);
        } else {
            let cl = cwd.len();
            let mut off = 0;
            if cl > 0 && cl < buf.len() {
                buf[..cl].copy_from_slice(cwd.as_bytes());
                off = cl;
            }
            if off < buf.len() - 1 && (off == 0 || buf[off - 1] != b'/') {
                buf[off] = b'/';
                off += 1;
            }
            let space = buf.len() - off - 1;
            let n = p.len().min(space);
            buf[off..off + n].copy_from_slice(&p.as_bytes()[..n]);
        }
    }
    normalize(unsafe { static_str(&PATH_BUF) })
}

// ==================== 环境变量 ====================

fn env_get(name: &str) -> Option<&'static str> {
    unsafe {
        for i in 0..ENV_COUNT {
            let nm = static_str(&ENV_NAMES[i]);
            if nm == name {
                return Some(static_str(&ENV_VALUES[i]));
            }
        }
    }
    None
}

fn env_set(name: &str, value: &str) {
    unsafe {
        for i in 0..ENV_COUNT {
            let nm = static_str(&ENV_NAMES[i]);
            if nm == name {
                ENV_VALUES[i].fill(0);
                let n = value.len().min(ENV_VALUES[i].len() - 1);
                ENV_VALUES[i][..n].copy_from_slice(&value.as_bytes()[..n]);
                return;
            }
        }
        if ENV_COUNT < MAX_ENV {
            let i = ENV_COUNT;
            ENV_NAMES[i].fill(0);
            ENV_VALUES[i].fill(0);
            let n = name.len().min(ENV_NAMES[i].len() - 1);
            ENV_NAMES[i][..n].copy_from_slice(&name.as_bytes()[..n]);
            let n = value.len().min(ENV_VALUES[i].len() - 1);
            ENV_VALUES[i][..n].copy_from_slice(&value.as_bytes()[..n]);
            ENV_COUNT += 1;
        }
    }
}

// ==================== 输出汇 ====================

/// 输出汇：写入文件、或写入缓冲（供管道/控制台）。
struct Sink<'a> {
    output: &'a mut [u8],
    out_len: usize,
    out_file: i32,
    /// 缓冲写入是否发生截断（输出超过 `OUT_MAX`）。
    truncated: bool,
}

impl Sink<'_> {
    /// 重定向模式下也先缓冲再写入：echo 的各个参数原本逐个 emit，逐次
    /// write 会把 `echo force 1 > /proc/pressure` 拆成多条命令（/proc 写路径
    /// 期望一次拿到完整命令行）。缓冲满时整块刷出，保证大输出不被截断。
    fn emit(&mut self, data: &[u8]) {
        if self.out_file >= 0 {
            for &c in data {
                if self.out_len == self.output.len() {
                    vfs::write(self.out_file, &self.output[..self.out_len]);
                    self.out_len = 0;
                }
                self.output[self.out_len] = c;
                self.out_len += 1;
            }
        } else {
            let space = self.output.len() - self.out_len;
            let n = data.len().min(space);
            self.output[self.out_len..self.out_len + n].copy_from_slice(&data[..n]);
            self.out_len += n;
            if n < data.len() {
                self.truncated = true;
            }
        }
    }
    fn emit_str(&mut self, s: &str) {
        self.emit(s.as_bytes());
    }

    /// 立即把缓冲内容输出到 UART（供 tcp_echo/http_serve 等阻塞型命令在
    /// 进入事件循环前刷新输出——否则缓冲要到命令返回才 flush，永不显示）。
    fn flush_uart(&mut self) {
        if self.out_file < 0 && self.out_len > 0 {
            for &c in &self.output[..self.out_len] {
                if c == b'\n' {
                    crate::uart_send_char(b'\r');
                }
                crate::uart_send_char(c);
            }
            self.out_len = 0;
        }
    }

    fn emit_u32(&mut self, mut v: u32) {
        if v == 0 {
            self.emit_str("0");
            return;
        }
        let mut buf = [0u8; 12];
        let mut i = buf.len();
        while v > 0 {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.emit(&buf[i..]);
    }

    fn emit_i32(&mut self, mut v: i32) {
        if v < 0 {
            self.emit_str("-");
            v = v.wrapping_neg();
        }
        self.emit_u32(v as u32);
    }
}

// ==================== 词法分析 ====================

fn is_sep(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'|' || c == b'<' || c == b'>'
}

fn tokenize(line: &[u8], len: usize, tokens: &mut [Token; MAX_TOK], truncated: &mut bool) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < len && n < MAX_TOK {
        let c = line[i];
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        match c {
            b'|' => {
                tokens[n] = Token { word: EMPTY_WORD, op: OP_PIPE };
                n += 1;
                i += 1;
            }
            b'<' => {
                tokens[n] = Token { word: EMPTY_WORD, op: OP_IN };
                n += 1;
                i += 1;
            }
            b'>' => {
                if i + 1 < len && line[i + 1] == b'>' {
                    tokens[n] = Token { word: EMPTY_WORD, op: OP_APPEND };
                    i += 2;
                } else {
                    tokens[n] = Token { word: EMPTY_WORD, op: OP_OUT };
                    i += 1;
                }
                n += 1;
            }
            _ => {
                let mut w = Word { buf: [0; ARG_LEN], len: 0 };
                while i < len && !is_sep(line[i]) {
                    if w.len < ARG_LEN - 1 {
                        w.buf[w.len] = line[i];
                        w.len += 1;
                    } else {
                        // 单词超长：截断标记，调用方拒绝执行
                        *truncated = true;
                    }
                    i += 1;
                }
                tokens[n] = Token { word: w, op: OP_WORD };
                n += 1;
            }
        }
    }
    // 记号数超限：剩余输入被丢弃，命令已不完整
    if i < len && n >= MAX_TOK {
        *truncated = true;
    }
    n
}

// ==================== 语法分析 ====================

fn parse(tokens: &[Token], tok_count: usize, truncated: &mut bool) -> Pipeline {
    let mut pipe = Pipeline { cmds: [Command::new(); MAX_PIPE], n: 0 };
    let mut expect_in = false;
    let mut expect_out = false;
    let mut expect_append = false;

    for i in 0..tok_count {
        let tok = &tokens[i];
        let cur = pipe.n.min(MAX_PIPE - 1);
        match tok.op {
            OP_WORD => {
                if expect_in {
                    pipe.cmds[cur].in_redir = tok.word;
                    expect_in = false;
                } else if expect_out {
                    pipe.cmds[cur].out_redir = tok.word;
                    pipe.cmds[cur].append = false;
                    expect_out = false;
                } else if expect_append {
                    pipe.cmds[cur].out_redir = tok.word;
                    pipe.cmds[cur].append = true;
                    expect_append = false;
                } else if pipe.cmds[cur].argc < MAX_ARGC {
                    pipe.cmds[cur].argv[pipe.cmds[cur].argc] = tok.word;
                    pipe.cmds[cur].argc += 1;
                } else {
                    // 参数数超限：命令将残缺，标记后由调用方拒绝执行
                    *truncated = true;
                }
            }
            OP_PIPE => {
                if pipe.n < MAX_PIPE - 1 {
                    pipe.n += 1;
                } else {
                    // 管道段数超限：后续段被丢弃
                    *truncated = true;
                }
            }
            OP_IN => expect_in = true,
            OP_OUT => expect_out = true,
            OP_APPEND => expect_append = true,
            _ => {}
        }
    }
    if pipe.n == 0 && pipe.cmds[0].argc == 0 {
        pipe.n = 0;
    } else {
        pipe.n += 1;
    }
    pipe
}

// ==================== 执行 ====================

fn read_all(path: &str, buf: &mut [u8]) -> usize {
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 256];
        let r = vfs::read(fd, &mut tmp);
        if r <= 0 {
            break;
        }
        if total + r as usize > buf.len() {
            break;
        }
        buf[total..total + r as usize].copy_from_slice(&tmp[..r as usize]);
        total += r as usize;
    }
    vfs::close(fd);
    total
}

fn exec_one(cmd: &Command, input: &[u8], output: &mut [u8]) -> (usize, bool, bool) {
    // stdin：重定向文件 或 上游管道
    // 缓冲放 .bss（shell 单线程，无并发），避免 32KB 栈帧
    static mut IN_BUF: [u8; OUT_MAX] = [0; OUT_MAX];
    let stdin_data: &[u8];
    if cmd.in_redir.len > 0 {
        let path = resolve_path(word_str(&cmd.in_redir));
        let n = unsafe { read_all(path, &mut IN_BUF) };
        stdin_data = unsafe { &IN_BUF[..n] };
    } else {
        stdin_data = input;
    }

    // stdout：重定向文件 或 输出缓冲
    let mut out_file = -1i32;
    if cmd.out_redir.len > 0 {
        let path = resolve_path(word_str(&cmd.out_redir));
        let mut flags = vfs::O_WRONLY | vfs::O_CREAT;
        if !cmd.append {
            flags |= vfs::O_TRUNC;
        }
        out_file = vfs::open(path, flags);
        if out_file < 0 {
            // 打开失败必须报错：静默回退 stdout 会把内容直接上屏，与
            // 「写入成功」无法区分（排查 /data inode 耗尽时只能靠猜）。
            crate::uart_send_str("sh: 无法打开 ");
            crate::uart_send_str(path);
            crate::uart_send_str("\n");
        }
        if out_file >= 0 && cmd.append {
            vfs::seek(out_file, 0, vfs::SEEK_END);
        }
    }

    let redirected = out_file >= 0;
    let mut sink = Sink { output, out_len: 0, out_file, truncated: false };
    run_builtin(cmd, stdin_data, &mut sink);

    // 重定向模式：把缓冲中的剩余输出一次性写入目标文件（整行命令语义）
    if out_file >= 0 && sink.out_len > 0 {
        vfs::write(out_file, &sink.output[..sink.out_len]);
        sink.out_len = 0;
    }
    if out_file >= 0 {
        vfs::close(out_file);
    }
    (sink.out_len, redirected, sink.truncated)
}

fn run_pipeline(p: &Pipeline) {
    // 管道级间缓冲放 .bss（shell 单线程，无并发），避免 32KB 栈帧
    static mut INPUT_BUF: [u8; OUT_MAX] = [0; OUT_MAX];
    static mut OUTPUT_BUF: [u8; OUT_MAX] = [0; OUT_MAX];
    let mut input_len = 0usize;
    for i in 0..p.n {
        let cmd = &p.cmds[i];
        let is_last = i == p.n - 1;
        // source 作为管道段拒绝：cmd_source 会在本 run_pipeline 的栈帧内
        // 逐行再入 run_pipeline，复用同一对静态 INPUT_BUF/OUTPUT_BUF ——
        // 外层 exec_one(source) 持有的输出缓冲被嵌套管线原地改写
        // （SOURCE_DEPTH 只防 source 套 source，防不住这条路径）。
        if p.n > 1 && matches!(word_str(&cmd.argv[0]), "source" | "sh") {
            crate::uart_send_str("source: 不支持作为管道段执行\n");
            input_len = 0;
            continue;
        }
        let mut output: &mut [u8] = unsafe { &mut OUTPUT_BUF };
        let (out_len, redirected, truncated) =
            exec_one(cmd, unsafe { &INPUT_BUF[..input_len] }, &mut output);
        if redirected {
            input_len = 0;
        } else if is_last {
            for &c in &output[..out_len] {
                if c == b'\n' {
                    crate::uart_send_char(b'\r');
                }
                crate::uart_send_char(c);
            }
            if truncated {
                crate::uart_send_str("\n[shell] 警告：输出被截断（上限 ");
                crate::print_dec(OUT_MAX as u64);
                crate::uart_send_str(" 字节）\n");
            }
            input_len = 0;
        } else {
            unsafe {
                INPUT_BUF[..out_len].copy_from_slice(&output[..out_len]);
            }
            input_len = out_len;
            if truncated {
                crate::uart_send_str("[shell] 警告：管道级输出被截断\n");
            }
        }
    }
}

// ==================== 内置命令 ====================

fn emit_expanded(sink: &mut Sink, w: &Word) {
    let s = word_str(w);
    if s.len() > 1 && s.as_bytes()[0] == b'$' {
        if let Some(v) = env_get(&s[1..]) {
            sink.emit_str(v);
        }
    } else {
        sink.emit_str(s);
    }
}

fn cmd_echo(cmd: &Command, sink: &mut Sink) {
    let mut newline = true;
    let mut start = 1usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        newline = false;
        start = 2;
    }
    for i in start..cmd.argc {
        if i > start {
            sink.emit_str(" ");
        }
        emit_expanded(sink, &cmd.argv[i]);
    }
    if newline {
        sink.emit_str("\n");
    }
}

fn cmd_export(cmd: &Command, sink: &mut Sink) {
    if cmd.argc == 1 {
        unsafe {
            for i in 0..ENV_COUNT {
                sink.emit_str(static_str(&ENV_NAMES[i]));
                sink.emit_str("=");
                sink.emit_str(static_str(&ENV_VALUES[i]));
                sink.emit_str("\n");
            }
        }
        return;
    }
    for i in 1..cmd.argc {
        let s = word_str(&cmd.argv[i]);
        if let Some(eq) = s.find('=') {
            env_set(&s[..eq], &s[eq + 1..]);
        } else {
            env_set(s, "");
        }
    }
}

/// 上一工作目录（供 `cd -`）。
static mut OLDPWD: [u8; 128] = [0; 128];

fn cmd_cd(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        // `cd` 无参数：记录旧目录并回根目录
        unsafe {
            let old = cwd_str();
            let n = old.len().min(OLDPWD.len() - 1);
            OLDPWD.fill(0);
            OLDPWD[..n].copy_from_slice(&old.as_bytes()[..n]);
        }
        set_cwd("/");
        return;
    }
    // `cd -`：回到上一目录
    if word_str(&cmd.argv[1]) == "-" {
        unsafe {
            let old = static_str(&OLDPWD);
            if old.len() > 0 {
                set_cwd(old);
                sink.emit_str(old);
                sink.emit_str("\n");
            } else {
                sink.emit_str("cd: 没有上一目录\n");
            }
        }
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    if path.is_empty() {
        sink.emit_str("cd: 路径过长\n");
        return;
    }
    let mut dev = 0u32;
    let mut ino = 0u32;
    if vfs::resolve(path, &mut dev, &mut ino) == 0 {
        // 目标必须是目录（此前只查存在性，`cd 普通文件` 会"成功"）
        if vfs::stat_path(path).map_or(true, |(t, _)| t != crate::fs_simple::T_DIR) {
            sink.emit_str("cd: 不是目录：");
            sink.emit_str(path);
            sink.emit_str("\n");
            return;
        }
        // 记录旧目录
        unsafe {
            let old = cwd_str();
            let n = old.len().min(OLDPWD.len() - 1);
            OLDPWD.fill(0);
            OLDPWD[..n].copy_from_slice(&old.as_bytes()[..n]);
        }
        set_cwd(path);
    } else {
        sink.emit_str("cd: 无此目录：");
        sink.emit_str(path);
        sink.emit_str("\n");
    }
}

fn cmd_pwd(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str(cwd_str());
    sink.emit_str("\n");
}

fn cmd_ls(cmd: &Command, sink: &mut Sink) {
    // `ls [-l] [-a] [path]`：-l 显示类型与大小，-a 显示 . 与 ..
    let mut long = false;
    let mut all = false;
    let mut path_arg = 1usize;
    while cmd.argc > path_arg {
        let a = word_str(&cmd.argv[path_arg]);
        // 支持组合标志（-la / -al）与单独标志（-l、-a）
        if a.starts_with('-') && a.len() > 1 && !a.starts_with("--") {
            let bytes = a.as_bytes();
            let mut matched = false;
            for &b in &bytes[1..] {
                if b == b'l' {
                    long = true;
                    matched = true;
                } else if b == b'a' {
                    all = true;
                    matched = true;
                }
            }
            if matched {
                path_arg += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    let path = if cmd.argc > path_arg {
        resolve_path(word_str(&cmd.argv[path_arg]))
    } else {
        cwd_str()
    };
    // 参数是普通文件/设备：直接列出该条目（标准 ls 语义），目录则列目录内容
    if let Some((t, size)) = vfs::lstat_path(path) {
        if t != crate::fs_simple::T_DIR {
            let mut bn = [0u8; 32];
            let bl = path_basename(path, &mut bn);
            if long {
                sink.emit_str(match t {
                    crate::fs_simple::T_CHR => "C",
                    crate::fs_simple::T_BLK => "B",
                    crate::fs_simple::T_FIFO => "F",
                    crate::fs_simple::T_SYM => "L",
                    _ => "R",
                });
                sink.emit_str(" ");
                sink.emit_u32(size as u32);
                sink.emit_str(" ");
                sink.emit(&bn[..bl]);
                if t == crate::fs_simple::T_SYM {
                    sink.emit_str(" -> ");
                    let mut tb = [0u8; 240];
                    let tn = vfs::readlink(path, &mut tb);
                    if tn > 0 {
                        let ts = static_str(&tb[..tn as usize]);
                        sink.emit_str(ts);
                    }
                }
                sink.emit_str("\n");
            } else {
                sink.emit(&bn[..bl]);
                sink.emit_str("\n");
            }
            return;
        }
    } else {
        sink.emit_str("ls: 无法访问：");
        sink.emit_str(path);
        sink.emit_str("\n");
        return;
    }
    // 收集目录项并按名称字典序排序（与标准 ls 一致；目录项本身按创建顺序存储）。
    // 缓冲放 .bss（static，shell 单线程无并发）：最多 64 项 × 32 字节。
    static mut LS_NAMES: [[u8; 32]; 64] = [[0; 32]; 64];
    static mut LS_LEN: [usize; 64] = [0; 64];
    let mut count = 0usize;
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    loop {
        let ino = vfs::readdir(path, idx, &mut name);
        if ino < 0 {
            break;
        }
        let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if count < 64 {
            unsafe {
                LS_NAMES[count][..elen].copy_from_slice(&name[..elen]);
                LS_LEN[count] = elen;
            }
            count += 1;
        }
        idx += 1;
    }
    // 插入排序（按字节字典序；项数少，简单排序足够）
    for i in 1..count {
        let mut j = i;
        while j > 0 {
            let (a, b) = unsafe {
                (
                    &LS_NAMES[j - 1][..LS_LEN[j - 1]],
                    &LS_NAMES[j][..LS_LEN[j]],
                )
            };
            if a <= b {
                break;
            }
            unsafe {
                let mut tmp = [0u8; 32];
                let alen = LS_LEN[j - 1];
                tmp[..alen].copy_from_slice(&LS_NAMES[j - 1][..alen]);
                let blen = LS_LEN[j];
                LS_NAMES[j - 1][..blen].copy_from_slice(&LS_NAMES[j][..blen]);
                LS_LEN[j - 1] = blen;
                LS_NAMES[j][..alen].copy_from_slice(&tmp[..alen]);
                LS_LEN[j] = alen;
            }
            j -= 1;
        }
    }
    for i in 0..count {
        let elen = unsafe { LS_LEN[i] };
        let entry = unsafe { core::str::from_utf8(&LS_NAMES[i][..elen]).unwrap_or("") };
        if all || (entry != "." && entry != "..") {
            if long {
                // 拼接完整路径查询类型与大小
                let mut full = [0u8; 128];
                let mut off = 0usize;
                for &c in path.as_bytes() {
                    if off + 1 >= full.len() {
                        break;
                    }
                    full[off] = c;
                    off += 1;
                }
                if off > 0 && full[off - 1] != b'/' && off + 1 < full.len() {
                    full[off] = b'/';
                    off += 1;
                }
                for &c in unsafe { &LS_NAMES[i][..elen] } {
                    if off + 1 >= full.len() {
                        break;
                    }
                    full[off] = c;
                    off += 1;
                }
                full[off] = 0;
                let full_str = static_str(&full);
                match vfs::lstat_path(full_str) {
                    Some((t, size)) => {
                        sink.emit_str(match t {
                            crate::fs_simple::T_DIR => "D",
                            crate::fs_simple::T_CHR => "C",
                            crate::fs_simple::T_BLK => "B",
                            crate::fs_simple::T_FIFO => "F",
                            crate::fs_simple::T_SYM => "L",
                            _ => "R",
                        });
                        sink.emit_str(" ");
                        sink.emit_u32(size as u32);
                        sink.emit_str(" ");
                        sink.emit(unsafe { &LS_NAMES[i][..elen] });
                        if t == crate::fs_simple::T_SYM {
                            sink.emit_str(" -> ");
                            let mut tb = [0u8; 240];
                            let tn = vfs::readlink(full_str, &mut tb);
                            if tn > 0 {
                                let ts = static_str(&tb[..tn as usize]);
                                sink.emit_str(ts);
                            }
                        }
                        sink.emit_str("\n");
                    }
                    None => {
                        sink.emit_str("? ? ");
                        sink.emit(unsafe { &LS_NAMES[i][..elen] });
                        sink.emit_str("\n");
                    }
                }
            } else {
                sink.emit(unsafe { &LS_NAMES[i][..elen] });
                sink.emit_str("\n");
            }
        }
    }
}

fn cmd_cat(cmd: &Command, input: &[u8], sink: &mut Sink) {
    // `cat [-n] [file...]`：-n 为每行加行号（读 stdin 时同样生效）
    let mut numbered = false;
    let mut argi = 1usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        numbered = true;
        argi = 2;
    }
    if cmd.argc <= argi {
        // 无文件：输出 stdin（来自管道），-n 时逐行编号
        if numbered {
            let mut n = 1u32;
            let mut line_start = 0usize;
            for (i, &b) in input.iter().enumerate() {
                if b == b'\n' {
                    sink.emit_u32(n);
                    sink.emit_str("\t");
                    sink.emit(&input[line_start..i + 1]);
                    n += 1;
                    line_start = i + 1;
                }
            }
            if line_start < input.len() {
                sink.emit_u32(n);
                sink.emit_str("\t");
                sink.emit(&input[line_start..]);
                sink.emit_str("\n");
            }
        } else {
            sink.emit(input);
        }
        return;
    }
    let mut line_no = 1u32;
    for i in argi..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let fd = vfs::open(path, vfs::O_RDONLY);
        if fd < 0 {
            sink.emit_str("cat: 无此文件：");
            sink.emit_str(path);
            sink.emit_str("\n");
            continue;
        }
        let mut buf = [0u8; 256];
        loop {
            let r = vfs::read(fd, &mut buf);
            if r <= 0 {
                break;
            }
            if numbered {
                let data = &buf[..r as usize];
                let mut line_start = 0usize;
                for (j, &b) in data.iter().enumerate() {
                    if b == b'\n' {
                        sink.emit_u32(line_no);
                        sink.emit_str("\t");
                        sink.emit(&data[line_start..j + 1]);
                        line_no += 1;
                        line_start = j + 1;
                    }
                }
                if line_start < data.len() {
                    sink.emit_u32(line_no);
                    sink.emit_str("\t");
                    sink.emit(&data[line_start..]);
                    sink.emit_str("\n");
                    line_no += 1;
                }
            } else {
                sink.emit(&buf[..r as usize]);
            }
        }
        vfs::close(fd);
    }
}

/// 把当前环境表打包成 "NAME=VALUE" 静态串（供子进程初始栈 envp）。
/// 返回静态切片；无环境时为空。
fn envp_pack() -> &'static [&'static str] {
    static mut ENV_STRS: [[u8; 96]; MAX_ENV] = [[0u8; 96]; MAX_ENV];
    static mut ENV_PTRS: [&'static str; MAX_ENV] = [""; MAX_ENV];
    unsafe {
        let mut n = 0usize;
        for i in 0..ENV_COUNT {
            if n >= MAX_ENV {
                break;
            }
            let nm = static_str(&ENV_NAMES[i]);
            let vl = static_str(&ENV_VALUES[i]);
            // "NAME=VALUE"（含 NUL ≤ 96：NAME≤31 + '=' + VALUE≤63）
            let mut len = 0usize;
            for &b in nm.as_bytes() {
                if len >= 94 {
                    break;
                }
                ENV_STRS[n][len] = b;
                len += 1;
            }
            if len >= 94 {
                continue;
            }
            ENV_STRS[n][len] = b'=';
            len += 1;
            for &b in vl.as_bytes() {
                if len >= 94 {
                    break;
                }
                ENV_STRS[n][len] = b;
                len += 1;
            }
            ENV_PTRS[n] = core::str::from_utf8(&ENV_STRS[n][..len]).unwrap_or("");
            n += 1;
        }
        &ENV_PTRS[..n]
    }
}

fn cmd_run(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：run <elf> [args...]\n");
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    let mut argv: [&str; MAX_ARGC] = [""; MAX_ARGC];
    let mut n = 0usize;
    for i in 1..cmd.argc {
        argv[n] = word_str(&cmd.argv[i]);
        n += 1;
    }
    let pid = elf::spawn_from_path_env(path, &argv[..n], envp_pack());
    if pid < 0 {
        sink.emit_str("run: 加载失败：");
        sink.emit_str(path);
        sink.emit_str("\n");
        return;
    }
    crate::shell::fg_child_set(pid as usize);
    let (rpid, code) = process::wait(pid);
    crate::shell::fg_child_clear();
    elf::release(pid as u32);
    if rpid > 0 {
        sink.emit_str("[run] 进程号 ");
        sink.emit_u32(rpid as u32);
        sink.emit_str(" 退出码=");
        sink.emit_i32(code);
        sink.emit_str("\n");
    }
}

fn cmd_busybox(cmd: &Command, sink: &mut Sink) {
    let mut argv: [&str; MAX_ARGC] = [""; MAX_ARGC];
    let mut n = 0usize;
    for i in 0..cmd.argc {
        argv[n] = word_str(&cmd.argv[i]);
        n += 1;
    }
    let pid = elf::spawn_from_path_env("/bin/busybox", &argv[..n], envp_pack());
    if pid < 0 {
        sink.emit_str("busybox: 未找到\n");
        return;
    }
    crate::shell::fg_child_set(pid as usize);
    let _ = process::wait(pid);
    crate::shell::fg_child_clear();
    elf::release(pid as u32);
}

fn cmd_pkg(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：pkg <install|uninstall|list>\n");
        return;
    }
    let sub = word_str(&cmd.argv[1]);
    match sub {
        "install" => {
            if cmd.argc < 3 {
                sink.emit_str("用法：pkg install <file>\n");
                return;
            }
            let path = resolve_path(word_str(&cmd.argv[2]));
            let mut buf = [0u8; 16384];
            let total = read_all(path, &mut buf);
            if total == 0 {
                sink.emit_str("pkg: 无法读取软件包\n");
                return;
            }
            match pkg::parse(&buf[..total]) {
                Ok(p) => {
                    if pkg::install(&p, &buf[..total]) == 0 {
                        sink.emit_str("pkg: 已安装\n");
                    } else {
                        sink.emit_str("pkg: 安装失败（签名不匹配？）\n");
                    }
                }
                Err(_) => sink.emit_str("pkg: 软件包无效\n"),
            }
        }
        "uninstall" => {
            if cmd.argc < 3 {
                sink.emit_str("用法：pkg uninstall <name>\n");
                return;
            }
            pkg::uninstall(word_str(&cmd.argv[2]));
            sink.emit_str("pkg: 已卸载\n");
        }
        "list" => pkg::list_to(&mut |s| sink.emit_str(s)),
        _ => sink.emit_str("pkg: 未知子命令\n"),
    }
}

/// `ota`：安全启动链 / OTA 状态与控制（BCB、OTP、安装、恢复模式）。
fn cmd_ota(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：ota <status|check FILE|install FILE|recovery|activate A|B>\n");
        return;
    }
    match word_str(&cmd.argv[1]) {
        "status" => {
            crate::ota::print_status();
            sink.emit_str("ota: 状态已输出\n");
        }
        "check" => {
            if cmd.argc < 3 {
                sink.emit_str("用法：ota check <file>\n");
                return;
            }
            if crate::ota::ota_check(resolve_path(word_str(&cmd.argv[2]))) == 0 {
                sink.emit_str("ota check: 通过\n");
            } else {
                sink.emit_str("ota check: 失败\n");
            }
        }
        "install" => {
            if cmd.argc < 3 {
                sink.emit_str("用法：ota install <file>\n");
                return;
            }
            if crate::ota::ota_install(resolve_path(word_str(&cmd.argv[2]))) == 0 {
                sink.emit_str("ota install: 成功，重启后切换槽位\n");
            } else {
                sink.emit_str("ota install: 失败\n");
            }
        }
        "recovery" => {
            if crate::ota::ota_set_recovery() {
                sink.emit_str("ota: 已设恢复模式，重启后进入\n");
            } else {
                sink.emit_str("ota: 设置恢复模式失败\n");
            }
        }
        "activate" => {
            if cmd.argc < 3 {
                sink.emit_str("用法：ota activate <A|B>\n");
                return;
            }
            let c = word_str(&cmd.argv[2]).as_bytes().first().copied().unwrap_or(0);
            if crate::ota::ota_force_slot(c) {
                sink.emit_str("ota: 槽位已切换，请重启\n");
            } else {
                sink.emit_str("ota: 激活失败\n");
            }
        }
        _ => sink.emit_str("ota: 未知子命令\n"),
    }
}

/// `history`：列出命令历史（带序号）。
fn cmd_history(_cmd: &Command, sink: &mut Sink) {
    unsafe {
        for i in 0..HIST_COUNT {
            sink.emit_u32(i as u32);
            sink.emit_str("  ");
            let len = HIST_LEN[i].min(LINE_MAX - 1);
            sink.emit(&HIST[i][..len]);
            sink.emit_str("\n");
        }
    }
}

/// 查找别名（按名字）；返回下标或 `usize::MAX`。
fn alias_lookup(name: &[u8]) -> usize {
    unsafe {
        for i in 0..ALIAS_COUNT {
            if ALIAS_NAMES[i].iter().zip(name).all(|(&a, &b)| a == b)
                && ALIAS_NAMES[i].iter().position(|&c| c == 0).unwrap_or(ALIAS_NAMES[i].len())
                    == name.len()
            {
                return i;
            }
        }
    }
    usize::MAX
}

/// `alias [name[=value]]`：定义命令别名。
///
/// - `alias`：列出全部别名（`name=value`）。
/// - `alias name`：显示单条。
/// - `alias name=value`：定义（`name` 为空值时删除）。
fn cmd_alias(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        unsafe {
            for i in 0..ALIAS_COUNT {
                let nl = ALIAS_NAMES[i].iter().position(|&c| c == 0).unwrap_or(ALIAS_NAMES[i].len());
                let vl = ALIAS_VALUES[i].iter().position(|&c| c == 0).unwrap_or(ALIAS_VALUES[i].len());
                sink.emit(&ALIAS_NAMES[i][..nl]);
                sink.emit_str("=");
                sink.emit(&ALIAS_VALUES[i][..vl]);
                sink.emit_str("\n");
            }
        }
        return;
    }
    let arg = &cmd.argv[1].buf[..cmd.argv[1].len];
    match arg.iter().position(|&c| c == b'=') {
        Some(eq) => {
            let name = &arg[..eq];
            let value = &arg[eq + 1..];
            if name.is_empty() {
                sink.emit_str("alias: 名称为空\n");
                return;
            }
            if value.is_empty() {
                // name= （空值）：删除别名。
                unsafe {
                    let i = alias_lookup(name);
                    if i < ALIAS_COUNT {
                        for j in i..ALIAS_COUNT - 1 {
                            ALIAS_NAMES[j] = ALIAS_NAMES[j + 1];
                            ALIAS_VALUES[j] = ALIAS_VALUES[j + 1];
                        }
                        ALIAS_COUNT -= 1;
                    }
                }
                return;
            }
            unsafe {
                let mut i = alias_lookup(name);
                if i >= ALIAS_COUNT {
                    if ALIAS_COUNT >= MAX_ALIAS {
                        sink.emit_str("alias: 别名表已满\n");
                        return;
                    }
                    i = ALIAS_COUNT;
                    ALIAS_NAMES[i].fill(0);
                    ALIAS_VALUES[i].fill(0);
                    ALIAS_COUNT += 1;
                }
                let n = name.len().min(ALIAS_NAMES[i].len());
                ALIAS_NAMES[i][..n].copy_from_slice(&name[..n]);
                let v = value.len().min(ALIAS_VALUES[i].len());
                ALIAS_VALUES[i][..v].copy_from_slice(&value[..v]);
            }
        }
        None => {
            // `alias name`：显示单条（未定义报错）。
            let i = alias_lookup(arg);
            if i >= unsafe { ALIAS_COUNT } {
                sink.emit_str("alias: 未找到：");
                sink.emit(arg);
                sink.emit_str("\n");
            } else {
                unsafe {
                    let vl = ALIAS_VALUES[i].iter().position(|&c| c == 0).unwrap_or(ALIAS_VALUES[i].len());
                    sink.emit(&ALIAS_VALUES[i][..vl]);
                    sink.emit_str("\n");
                }
            }
        }
    }
}

/// `unalias name` / `unalias *`：删除一个或全部别名。
fn cmd_unalias(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：unalias name|*\n");
        return;
    }
    let arg = &cmd.argv[1].buf[..cmd.argv[1].len];
    if arg == b"*" {
        unsafe { ALIAS_COUNT = 0; }
        return;
    }
    unsafe {
        let i = alias_lookup(arg);
        if i >= ALIAS_COUNT {
            sink.emit_str("unalias: 未找到：");
            sink.emit(arg);
            sink.emit_str("\n");
            return;
        }
        for j in i..ALIAS_COUNT - 1 {
            ALIAS_NAMES[j] = ALIAS_NAMES[j + 1];
            ALIAS_VALUES[j] = ALIAS_VALUES[j + 1];
        }
        ALIAS_COUNT -= 1;
    }
}

/// 展开一行命令中的首词别名（`cmd argv[0]` 命中别名表时用其值替换整行首词）。
fn alias_expand(line: &mut [u8], len: usize) -> usize {
    if line.is_empty() || len == 0 {
        return len;
    }
    // 段长度与传入 len 取较小者，防止调用方传入超界值
    let l = len.min(line.len());
    if line[..l].iter().any(|&c| c == b' ') {
        return len;
    }
    let i = alias_lookup(&line[..l]);
    if i >= unsafe { ALIAS_COUNT } {
        return len;
    }
    unsafe {
        let v = &ALIAS_VALUES[i];
        let vl = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        if vl > 0 && vl >= LINE_MAX {
            return len;
        }
        // 别名值可能长于当前命令段：只按段长截断写入，避免越界写
        let n = vl.min(l);
        if vl > l {
            crate::uart_send_str("[shell] 警告：别名值过长，已截断\n");
        }
        for k in 0..n {
            line[k] = v[k];
        }
        n
    }
}

/// `source <file>` / `sh <file>`：读取脚本文件并逐行执行（简化脚本支持）。
fn cmd_source(cmd: &Command, sink: &mut Sink) {
    // 嵌套防护：脚本行内再 source（或任何走 run_pipeline 的命令）会改写
    // 同一静态 SRC_BUF，外层仍按旧 total 继续切行 → 执行错乱/陈旧内容。
    static mut SOURCE_DEPTH: u32 = 0;
    if unsafe { SOURCE_DEPTH } >= 1 {
        sink.emit_str("source: 不支持嵌套执行\n");
        return;
    }
    struct SourceDepthGuard;
    impl Drop for SourceDepthGuard {
        fn drop(&mut self) {
            unsafe { SOURCE_DEPTH -= 1 };
        }
    }
    if cmd.argc < 2 {
        sink.emit_str("用法：source <file>\n");
        return;
    }
    let path_str = word_str(&cmd.argv[1]);
    if path_str.is_empty() {
        return;
    }
    unsafe { SOURCE_DEPTH += 1 };
    let _guard = SourceDepthGuard;
    // 读取整个文件到 .bss 缓冲，逐行（'\n'）切分后交给解析/管道执行。
    // 使用静态 SRC_BUF 而非栈上 32KB 数组，避免大栈帧
    let buf: &mut [u8] = unsafe { &mut SRC_BUF };
    let fd = vfs::open(path_str, vfs::O_RDONLY);
    if fd < 0 {
        sink.emit_str("source: 无法打开：");
        sink.emit_str(path_str);
        sink.emit_str("\n");
        return;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 256];
        let n = vfs::read(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        if total + n as usize > buf.len() {
            break;
        }
        buf[total..total + n as usize].copy_from_slice(&tmp[..n as usize]);
        total += n as usize;
    }
    vfs::close(fd);
    let mut start = 0usize;
    let mut seg = 0usize;
    while start <= total {
        let end = if seg >= total { total } else { buf[start..total].iter().position(|&c| c == b'\n').map(|i| start + i).unwrap_or(total) };
        let seg_len = end - start;
        if seg_len > 0 && start < end {
            let m = alias_expand(&mut buf[start..end], seg_len);
            let mut tokens = [Token { word: EMPTY_WORD, op: OP_WORD }; MAX_TOK];
            let mut truncated = false;
            let tc = tokenize(&buf[start..start + m], m, &mut tokens, &mut truncated);
            let pipe = parse(&tokens, tc, &mut truncated);
            if truncated {
                // 词/参数/管道段被截断：命令已残缺，拒绝执行并告警
                crate::uart_send_str("[shell] 警告：命令过长，已截断；未执行\n");
            } else {
                run_pipeline(&pipe);
            }
        }
        if end >= total {
            break;
        }
        start = end + 1;
        seg += 1;
    }
}

/// `uptime`：系统运行时间（秒）。
fn cmd_uptime(_cmd: &Command, sink: &mut Sink) {
    let us = crate::timer::now_us();
    sink.emit_str("运行 ");
    sink.emit_u32((us / 1_000_000) as u32);
    sink.emit_str(" 秒\n");
}

/// `power`：电源 / 热 / DVFS / 电池状态与控制。
///
/// 子命令：
///   `power` / `power status`            显示电源状态
///   `power governor <ondemand|conservative|powersave|performance>`
///   `power thermal force <temp_C>`      注入温度（QEMU 模拟，测试节流）
///   `power thermal clear`               清除注入温度
///   `power battery <pct>`               注入电量（QEMU 虚拟电池）
///   `power plug <0|1>`                  注入电源连接状态
///   `power cap <pid> <0-100>`           设置进程 CPU 上限（Doze）
///   `power freeze <pid>` / `power unfreeze <pid>`
///   `power selftest`                    电源逻辑自检
fn cmd_power(cmd: &Command, sink: &mut Sink) {
    // 收集子命令参数（argv[1..]）
    let mut args: [&str; 4] = ["", "", "", ""];
    let mut n = 0usize;
    for i in 1..cmd.argc {
        if n < args.len() {
            args[n] = word_str(&cmd.argv[i]);
            n += 1;
        }
    }

    // 无子命令 → status
    if n == 0 || args[0] == "status" || args[0] == "help" {
        if args[0] == "help" {
            sink.emit_str(
                "power: status | governor <name> | thermal force <C> | thermal clear |\n\
                 \x20      battery <pct> | plug <0|1> | cap <pid> <0-100> | freeze <pid> |\n\
                 \x20      unfreeze <pid> | selftest\n",
            );
            return;
        }
        let mut buf = [0u8; 2048];
        let len = crate::power::render_status(&mut buf);
        sink.emit(&buf[..len]);
        return;
    }

    let is_selftest = n > 0 && args[0] == "selftest";
    let r = crate::power::handle_cmd(&args[..n]);
    if r < 0 {
        sink.emit_str("power: 参数错误\n");
    } else if is_selftest {
        // 仅 selftest 打印结论汇总（power.rs 的 UART 行打「N 个失败」，
        // 此处不再重复打印，避免双份且措辞不一致的输出）。
        sink.emit_str("[power] 自检完成：");
        sink.emit_u32(r as u32);
        sink.emit_str(" 处失败\n");
    }
}

/// `date`：显示墙钟日历时间（有 RTC 时）；无 RTC 退化为运行时长。
fn cmd_date(_cmd: &Command, sink: &mut Sink) {
    let epoch = crate::rtc::boot_epoch_secs();
    if epoch != 0 {
        let secs = crate::rtc::wall_time_us() / 1_000_000;
        let mut c = [0i32; 6];
        crate::rtc::civil_from_epoch(secs, &mut c);
        let (y, mo, d, h, mi, s) = (c[0], c[1], c[2], c[3], c[4], c[5]);
        sink.emit_u32(y as u32);
        sink.emit_str("-");
        if mo < 10 { sink.emit_str("0"); }
        sink.emit_u32(mo as u32);
        sink.emit_str("-");
        if d < 10 { sink.emit_str("0"); }
        sink.emit_u32(d as u32);
        sink.emit_str(" ");
        if h < 10 { sink.emit_str("0"); }
        sink.emit_u32(h as u32);
        sink.emit_str(":");
        if mi < 10 { sink.emit_str("0"); }
        sink.emit_u32(mi as u32);
        sink.emit_str(":");
        if s < 10 { sink.emit_str("0"); }
        sink.emit_u32(s as u32);
        sink.emit_str("\n");
    } else {
        let secs = crate::timer::now_us() / 1_000_000;
        let (days, rem) = (secs / 86400, secs % 86400);
        let (hours, rem) = (rem / 3600, rem % 3600);
        let (mins, s) = (rem / 60, rem % 60);
        sink.emit_str("运行 ");
        sink.emit_u32(days as u32);
        sink.emit_str(" 天，");
        sink.emit_u32(hours as u32);
        sink.emit_str(":");
        if mins < 10 {
            sink.emit_str("0");
        }
        sink.emit_u32(mins as u32);
        sink.emit_str(":");
        if s < 10 {
            sink.emit_str("0");
        }
        sink.emit_u32(s as u32);
        sink.emit_str("\n");
    }
}

/// `which <cmd>`：判断命令是否为内置命令或 PATH 中的可执行文件。
fn cmd_which(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：which <cmd>\n");
        return;
    }
    for i in 1..cmd.argc {
        let name = word_str(&cmd.argv[i]);
        if BUILTINS.iter().any(|b| *b == name) {
            sink.emit_str(name);
            sink.emit_str(": shell 内置命令\n");
            continue;
        }
        // PATH 搜索：/bin 与 /usr/bin（busybox 等应用所在）
        let mut found = false;
        for dir in ["/bin/", "/usr/bin/"] {
            let mut p = [0u8; 128];
            let dl = dir.len();
            p[..dl].copy_from_slice(dir.as_bytes());
            let nl = name.len().min(128 - dl - 1);
            p[dl..dl + nl].copy_from_slice(&name.as_bytes()[..nl]);
            p[dl + nl] = 0;
            let path = core::str::from_utf8(&p[..dl + nl]).unwrap_or("");
            if vfs::stat_path(path).is_some() {
                sink.emit_str(path);
                sink.emit_str("\n");
                found = true;
                break;
            }
        }
        if !found {
            sink.emit_str(name);
            sink.emit_str(": 未找到\n");
        }
    }
}

/// `uniq`：去除输入中相邻的重复行（`uniq` 与 `sort | uniq` 管道常用）。
fn cmd_uniq(_cmd: &Command, input: &[u8], sink: &mut Sink) {
    if input.is_empty() {
        return;
    }
    let mut line_start = 0usize;
    let mut prev: Option<&[u8]> = None;
    for i in 0..=input.len() {
        if i == input.len() || input[i] == b'\n' {
            let line = &input[line_start..i];
            if prev != Some(line) {
                sink.emit(line);
                sink.emit_str("\n");
            }
            prev = Some(line);
            line_start = i + 1;
        }
    }
}

/// `sha1sum [file...]`：计算 SHA-1 校验和（RFC 3174）。无参数时读管道输入。
/// 用于文件完整性校验（与 pkg 的 FNV-1a 签名互补）。
/// `true` / `false`：无操作命令（脚本条件占位）。
fn cmd_true(_cmd: &Command, _sink: &mut Sink) {}

fn cmd_false(_cmd: &Command, _sink: &mut Sink) {
    _sink.emit_str("");
}

/// `basename <path>`：输出路径最后一级分量。
fn cmd_basename(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：basename <path>
");
        return;
    }
    let p = word_str(&cmd.argv[1]);
    match p.rfind('/') {
        Some(i) if i + 1 < p.len() => sink.emit_str(&p[i + 1..]),
        _ => sink.emit_str(p),
    }
    sink.emit_str("\n");
}

/// `dirname <path>`：输出路径的目录部分（无分隔符时输出 "."）。
fn cmd_dirname(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：dirname <path>\n");
        return;
    }
    let p = word_str(&cmd.argv[1]);
    match p.rfind('/') {
        Some(0) => sink.emit_str("/"),
        Some(i) => sink.emit_str(&p[..i]),
        None => sink.emit_str("."),
    }
    sink.emit_str("\n");
}

/// `verbose [on|off]`：切换内核控制台安静模式。
///
/// 默认安静（简洁 CLI）：信息性日志（启动过程 / 进程诊断 / 电源切换等）不上串口。
/// `verbose on` 恢复完整日志输出；`verbose` 无参数显示当前状态。
/// 错误、崩溃报告与命令输出始终可见，不受此开关影响。
fn cmd_verbose(cmd: &Command, sink: &mut Sink) {
    if cmd.argc > 1 {
        match word_str(&cmd.argv[1]) {
            "on" | "1" | "true" => {
                crate::console_set_quiet(false);
                sink.emit_str("verbose: 已开启完整内核日志\n");
            }
            "off" | "0" | "false" => {
                crate::console_set_quiet(true);
                sink.emit_str("verbose: 已关闭（安静控制台）\n");
            }
            _ => sink.emit_str("用法：verbose [on|off]\n"),
        }
    } else {
        sink.emit_str(if crate::console_quiet() {
            "verbose: off（安静控制台，verbose on 开启完整日志）\n"
        } else {
            "verbose: on（完整内核日志）\n"
        });
    }
}

/// `dmesg`：显示内核消息缓冲（启动日志等，环形 16KB，覆盖最旧内容）。
fn cmd_dmesg(cmd: &Command, sink: &mut Sink) {    let mut tail_lines = 0usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        if cmd.argc > 2 {
            tail_lines = word_str(&cmd.argv[2]).parse::<usize>().unwrap_or(0);
        }
    }
    let mut buf = [0u8; 16384];
    let n = crate::kmsg_snapshot(&mut buf);
    if tail_lines == 0 {
        sink.emit(&buf[..n]);
        return;
    }
    // 只显示末尾 N 行
    let mut last_start = n;
    let mut seen = 0usize;
    for i in (0..n).rev() {
        if buf[i] == b'\n' {
            seen += 1;
            if seen > tail_lines {
                last_start = i + 1;
                break;
            }
        }
    }
    if seen <= tail_lines {
        last_start = 0;
    }
    sink.emit(&buf[last_start..n]);
}

/// `od <file> [-n <bytes>]`：十六进制转储文件内容（每行 16 字节 + ASCII 侧栏），
/// 用于检查二进制文件（.pkg / ELF / 磁盘镜像）。
fn cmd_od(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：od <file> [-n <bytes>]\n");
        return;
    }
    let mut limit = usize::MAX;
    let mut file_i = 1usize;
    if cmd.argc > 3 && word_str(&cmd.argv[2]) == "-n" {
        limit = word_str(&cmd.argv[3]).parse::<usize>().unwrap_or(usize::MAX);
        file_i = 1;
    }
    let path = resolve_path(word_str(&cmd.argv[file_i]));
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        sink.emit_str("od: 无此文件：");
        sink.emit_str(path);
        sink.emit_str("\n");
        return;
    }
    let mut off = 0usize;
    let mut tmp = [0u8; 256];
    loop {
        let r = vfs::read(fd, &mut tmp);
        if r <= 0 {
            break;
        }
        for (i, &b) in tmp[..r as usize].iter().enumerate() {
            if off + i >= limit {
                break;
            }
            if (off + i) % 16 == 0 {
                if off + i > 0 {
                    sink.emit_str("\n");
                }
                print_hex_word(sink, (off + i) as u32);
                sink.emit_str("  ");
            }
            const HEX: &[u8; 16] = b"0123456789abcdef";
            sink.emit(&[HEX[(b >> 4) as usize]]);
            sink.emit(&[HEX[(b & 0xF) as usize]]);
            sink.emit_str(" ");
            if (off + i) % 16 == 15 {
                // ASCII 侧栏
                let start = (off + i) / 16 * 16;
                sink.emit_str(" |");
                for j in start..start + 16 {
                    if j >= limit || j >= off + r as usize {
                        break;
                    }
                    let c = if j >= off { tmp[j - off] } else { 0 };
                    sink.emit(&[if (0x20..0x7f).contains(&c) { c } else { b'.' }]);
                }
                sink.emit_str("|");
            }
        }
        off += r as usize;
        if off >= limit {
            break;
        }
    }
    vfs::close(fd);
    sink.emit_str("\n");
}

fn print_hex_word(sink: &mut Sink, mut v: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for _ in 0..8 {
        let b = ((v >> 28) & 0xF) as usize;
        sink.emit(&[HEX[b]]);
        v <<= 4;
    }
}

/// `tee <file>`：把管道输入同时写入文件并输出到终端（`cmd | tee log`）。
fn cmd_tee(cmd: &Command, input: &[u8], sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：tee <file>\n");
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    let fd = vfs::open(path, vfs::O_CREAT | vfs::O_WRONLY | vfs::O_TRUNC);
    if fd >= 0 {
        let mut off = 0usize;
        while off < input.len() {
            let n = vfs::write(fd, &input[off..]);
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        vfs::close(fd);
    } else {
        sink.emit_str("tee: 无法打开：");
        sink.emit_str(path);
        sink.emit_str("\n");
    }
    // 无论写文件成败都透传输入到 stdout
    sink.emit(input);
}

/// `stat <path...>`：显示文件类型与大小（SFS 元数据）。
fn cmd_stat(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：stat <path...>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        match vfs::lstat_path(path) {
            Some((typ, size)) => {
                let name = match typ {
                    1 => "普通文件",
                    2 => "目录",
                    3 => "字符设备",
                    4 => "块设备",
                    5 => "管道",
                    6 => "符号链接",
                    _ => "未知",
                };
                sink.emit_str(path);
                sink.emit_str(": ");
                sink.emit_str(name);
                sink.emit_str(" 大小=");
                sink.emit_u32(size as u32);
                sink.emit_str("\n");
            }
            None => {
                sink.emit_str(path);
                sink.emit_str(": 未找到\n");
            }
        }
    }
}

fn cmd_sha1sum(cmd: &Command, input: &[u8], sink: &mut Sink) {
    if cmd.argc <= 1 {
        // 从管道输入计算
        let mut ctx = Sha1Ctx::new();
        ctx.update(input);
        let digest = ctx.finalize();
        emit_digest(sink, &digest);
        sink.emit_str("  -\n");
        return;
    }    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let digest = sha1_stream_file(path);
        emit_digest(sink, &digest);
        sink.emit_str("  ");
        sink.emit_str(word_str(&cmd.argv[i]));
        sink.emit_str("\n");
    }
}

/// 流式计算文件的 SHA-1（任意大小，不截断）。
fn sha1_stream_file(path: &str) -> [u8; 20] {
    let mut ctx = Sha1Ctx::new();
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd >= 0 {
        let mut tmp = [0u8; 256];
        loop {
            let r = vfs::read(fd, &mut tmp);
            if r <= 0 {
                break;
            }
            ctx.update(&tmp[..r as usize]);
        }
        vfs::close(fd);
    }
    ctx.finalize()
}

fn emit_digest(sink: &mut Sink, d: &[u8; 20]) {
    for &b in d {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        sink.emit(&[HEX[(b >> 4) as usize]]);
        sink.emit(&[HEX[(b & 0xF) as usize]]);
    }
}

/// 流式 SHA-1 上下文（RFC 3174）：支持分块 update + 一次性 finalize。
struct Sha1Ctx {
    h: [u32; 5],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Sha1Ctx {
    fn new() -> Self {
        Sha1Ctx {
            h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            buf: [0u8; 64],
            buf_len: 0,
            total: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut off = 0usize;
        // 先补满当前未完成块
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            off = take;
            if self.buf_len == 64 {
                let mut w = [0u32; 80];
                for i in 0..16 {
                    w[i] = u32::from_be_bytes([
                        self.buf[i * 4],
                        self.buf[i * 4 + 1],
                        self.buf[i * 4 + 2],
                        self.buf[i * 4 + 3],
                    ]);
                }
                sha1_block(&mut self.h, &mut w);
                self.buf_len = 0;
            }
        }
        // 整块直接处理
        while off + 64 <= data.len() {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    data[off + i * 4],
                    data[off + i * 4 + 1],
                    data[off + i * 4 + 2],
                    data[off + i * 4 + 3],
                ]);
            }
            sha1_block(&mut self.h, &mut w);
            off += 64;
        }
        // 尾部缓存
        let rest = data.len() - off;
        if rest > 0 {
            self.buf[..rest].copy_from_slice(&data[off..]);
            self.buf_len = rest;
        }
    }

    fn finalize(&mut self) -> [u8; 20] {
        let bit_len = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        // 补 0 至 56 字节（mod 64）；buf_len 在 57..63 时先填满本块再补 56
        let need = if self.buf_len <= 56 {
            56 - self.buf_len
        } else {
            120 - self.buf_len
        };
        const ZEROS: [u8; 128] = [0u8; 128];
        self.update(&ZEROS[..need]);
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 20];
        for i in 0..5 {
            out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_be_bytes());
        }
        out
    }
}

fn sha1_block(h: &mut [u32; 5], w: &mut [u32; 80]) {
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
    for i in 0..80 {
        let (f, k) = match i / 20 {
            0 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
            1 => (b ^ c ^ d, 0x6ED9_EBA1),
            2 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let tmp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(w[i]);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = tmp;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

fn cmd_help(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str(
        "builtins: cd echo export exit pwd ls cat run busybox pkg ps uname clear mkdir rm touch cp mv wc head tail grep df history uptime ping sysinfo calc sort netstat tcp_echo udp_echo http_serve http_get gdb nslookup poweroff reboot power sleep kill date which uniq sha1sum stat dmesg od tee verbose true false basename dirname help\n",
    );
    sink.emit_str(
        "debug: top mount ifconfig log crash perf irqstat netdiag (see docs/DEBUGGING.md)\n",
    );
}

/// 解析点分十进制 IPv4（"10.0.2.2" → u32），失败返回 None。
fn parse_ip(s: &str) -> Option<u32> {
    let mut parts = s.split('.');
    let mut ip = 0u32;
    for _ in 0..4 {
        let v = parts.next()?.parse::<u32>().ok()?;
        if v > 255 {
            return None;
        }
        ip = (ip << 8) | v;
    }
    if parts.next().is_some() {
        return None; // 多于 4 段
    }
    Some(ip)
}

/// `grep <pattern> [file...]`：输出包含 pattern（子串）的行。
/// 无文件参数时读 stdin/管道（`ls /bin | grep ca`）。
fn cmd_grep(cmd: &Command, input: &[u8], sink: &mut Sink) {
    // `grep [-n] <pattern> [file...]`：-n 显示匹配行号
    let mut numbered = false;
    let mut argi = 1usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        numbered = true;
        argi = 2;
    }
    if cmd.argc <= argi {
        sink.emit_str("用法：grep [-n] <pattern> [file...]\n");
        return;
    }
    let pattern = word_str(&cmd.argv[argi]);
    let pb = pattern.as_bytes();
    // 从 stdin/管道过滤
    if cmd.argc <= argi + 1 {
        let mut line_no = 1u32;
        let mut line_start = 0usize;
        for (i, &b) in input.iter().enumerate() {
            if b == b'\n' {
                let line = &input[line_start..i];
                if line.windows(pb.len()).any(|w| w == pb) {
                    if numbered {
                        sink.emit_u32(line_no);
                        sink.emit_str(":");
                    }
                    sink.emit(line);
                    sink.emit_str("\n");
                }
                line_no += 1;
                line_start = i + 1;
            }
        }
        if line_start < input.len() {
            let line = &input[line_start..];
            if line.windows(pb.len()).any(|w| w == pb) {
                if numbered {
                    sink.emit_u32(line_no);
                    sink.emit_str(":");
                }
                sink.emit(line);
                sink.emit_str("\n");
            }
        }
        return;
    }
    // 从文件过滤
    for i in argi + 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let fd = vfs::open(path, vfs::O_RDONLY);
        if fd < 0 {
            sink.emit_str("grep: 无此文件：");
            sink.emit_str(path);
            sink.emit_str("\n");
            continue;
        }
        let mut buf = [0u8; 8192];
        let n = read_all(path, &mut buf);
        vfs::close(fd);
        let data = &buf[..n];
        let mut line_no = 1u32;
        let mut line_start = 0usize;
        for (j, &b) in data.iter().enumerate() {
            if b == b'\n' {
                let line = &data[line_start..j];
                if line.windows(pb.len()).any(|w| w == pb) {
                    if numbered {
                        sink.emit_u32(line_no);
                        sink.emit_str(":");
                    }
                    sink.emit(line);
                    sink.emit_str("\n");
                }
                line_no += 1;
                line_start = j + 1;
            }
        }
        if line_start < data.len() {
            let line = &data[line_start..];
            if line.windows(pb.len()).any(|w| w == pb) {
                if numbered {
                    sink.emit_u32(line_no);
                    sink.emit_str(":");
                }
                sink.emit(line);
                sink.emit_str("\n");
            }
        }
    }
}

/// `df`：列出已挂载文件系统（设备 / 挂载点 / 容量 / 已用 / 空闲）。
fn cmd_df(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("设备  挂载点        块数    已用  空闲   inode空闲/总量\n");
    let s = &mut *sink;
    vfs::for_each_mount(&mut |mp: &str, dev: u32| {
        // 按挂载设备真实文件系统类型分派：此前直调 fs_simple 版本，FAT32
        // 挂载点也会进入 SFS 的 dev_mut —— major 不存在即新建 DevState，
        // 表满时"复用 0 号"会把 root 卷的状态改写成其它设备的（df 触发即可达）。
        s.emit_str("fs");
        s.emit_u32(dev);
        // 挂载点补空格对齐
        s.emit_str("   ");
        s.emit_str(mp);
        let pad = 11usize.saturating_sub(mp.len());
        for _ in 0..pad {
            s.emit_str(" ");
        }
        let total = crate::fsops::fs_total_blocks(dev);
        let free = crate::fsops::fs_free_blocks(dev);
        s.emit_u32(total);
        s.emit_str("  ");
        s.emit_u32(total.saturating_sub(free));
        s.emit_str("  ");
        s.emit_u32(free);
        s.emit_str("  ");
        // inode 余量（SFS 卷；inode 耗尽时 create 全体失败，此前只能靠
        // 串口日志反推——第二十轮 /data 耗尽排查的直接出口）。FAT 无此概念。
        match crate::fsops::fs_free_inodes(dev) {
            Some(fi) => {
                s.emit_u32(fi);
                s.emit_str("/");
                match crate::fsops::fs_total_inodes(dev) {
                    Some(ti) => s.emit_u32(ti),
                    None => s.emit_str("?"),
                }
            }
            None => s.emit_str("-"),
        }
        s.emit_str("\n");
    });
}

/// `tcp_echo <port>`：TCP 回显服务器（接受连接并原样返回收到的数据）。
/// 配合 QEMU `hostfwd` 可从宿主机测试：`nc 127.0.0.1 <映射端口>`。
/// 把点分 IPv4 字符串解析为 u32（大端）；失败返回 None。
fn parse_ipv4(s: &[u8]) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut pi = 0usize;
    let mut cur = 0u32;
    let mut has_digit = false;
    for &b in s {
        if b == b'.' {
            if !has_digit {
                return None;
            }
            if pi >= 3 {
                return None;
            }
            parts[pi] = cur;
            pi += 1;
            cur = 0;
            has_digit = false;
        } else if b.is_ascii_digit() {
            cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
            if cur > 255 {
                return None;
            }
            has_digit = true;
        } else {
            return None;
        }
    }
    if !has_digit || pi != 3 {
        return None;
    }
    parts[3] = cur;
    Some(((parts[0] as u32) << 24) | ((parts[1] as u32) << 16) | ((parts[2] as u32) << 8) | parts[3] as u32)
}

/// `http_get <host> <port> <path>`：HTTP/1.0 GET 客户端，打印响应体。
/// host 支持点分 IPv4（如 10.0.2.2）；用于测试 HTTP 客户端与抓取页面。
fn cmd_http_get(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 4 {
        sink.emit_str("用法：http_get <host> <port> <path>\n");
        return;
    }
    let host = word_str(&cmd.argv[1]);
    let port = match word_str(&cmd.argv[2]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("http_get: 端口无效\n");
            return;
        }
    };
    let path = word_str(&cmd.argv[3]);
    let ip = match parse_ipv4(host.as_bytes()) {
        Some(i) => i,
        None => {
            sink.emit_str("http_get: 仅支持点分 IPv4 主机\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("http_get: 网络未初始化\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 {
        sink.emit_str("http_get: 套接字创建失败\n");
        return;
    }
    sink.emit_str("http_get: 正在连接 ");
    sink.emit_str(host);
    sink.emit_str(":");
    sink.emit_u32(port as u32);
    sink.emit_str("\n");
    sink.flush_uart();
    if crate::socket::connect(h, ip, port) != 0 {
        sink.emit_str("http_get: 连接失败\n");
        return;
    }
    // 构造 GET 请求
    let mut req = [0u8; 512];
    let mut n = 0usize;
    for &b in b"GET " {
        req[n] = b;
        n += 1;
    }
    let pn = path.len().min(128);
    req[n..n + pn].copy_from_slice(&path.as_bytes()[..pn]);
    n += pn;
    for &b in b" HTTP/1.0\r\nHost: " {
        req[n] = b;
        n += 1;
    }
    let hn = host.len().min(64);
    req[n..n + hn].copy_from_slice(&host.as_bytes()[..hn]);
    n += hn;
    for &b in b"\r\n\r\n" {
        req[n] = b;
        n += 1;
    }
    if crate::socket::send(h, &req[..n]) < 0 {
        sink.emit_str("http_get: 发送失败\n");
        return;
    }
    // 读响应：全部累积到 BODY_BUF，最后再找头部/正文分隔（空行）
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    let start = crate::timer::now_us();
    loop {
        let r = crate::socket::recv(h, &mut buf);
        if r < 0 {
            break;
        }
        if r == 0 {
            break; // 对端关闭
        }
        let chunk = &buf[..r as usize];
        if total + chunk.len() > unsafe { BODY_BUF.len() } {
            break; // 缓冲满
        }
        unsafe {
            BODY_BUF[total..total + chunk.len()].copy_from_slice(chunk);
        }
        total += chunk.len();
        if crate::timer::now_us().saturating_sub(start) > 5_000_000 {
            break; // 5s 超时
        }
    }
    // 找 "\r\n\r\n"（或 "\n\n"）定位正文起点
    let mut body_start = 0usize;
    let mut j = 0usize;
    unsafe {
        while j + 3 < total {
            if BODY_BUF[j] == b'\r'
                && BODY_BUF[j + 1] == b'\n'
                && BODY_BUF[j + 2] == b'\r'
                && BODY_BUF[j + 3] == b'\n'
            {
                body_start = j + 4;
                break;
            }
            j += 1;
        }
        if body_start == 0 {
            j = 0;
            while j + 1 < total {
                if BODY_BUF[j] == b'\n' && BODY_BUF[j + 1] == b'\n' {
                    body_start = j + 2;
                    break;
                }
                j += 1;
            }
        }
    }
    if body_start > 0 {
        sink.emit_str("[http_get] 正文（");
        sink.emit_u32((total - body_start) as u32);
        sink.emit_str(" 字节）：\n");
        unsafe {
            sink.emit(&BODY_BUF[body_start..total]);
        }
    } else {
        sink.emit_str("[http_get] 未收到响应正文（");
        sink.emit_u32(total as u32);
        sink.emit_str(" 字节）\n");
        unsafe {
            if total > 0 {
                sink.emit(&BODY_BUF[..total]);
            }
        }
    }
    sink.emit_str("\n");
    crate::socket::close(h);
}

/// http_get 响应缓冲（避免栈上大数组）。
static mut BODY_BUF: [u8; 2048] = [0u8; 2048];

/// `gdb`：进入 GDB 调试循环（配合宿主 `gdb -ex "target remote <serial>"`）。
fn cmd_gdb(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("gdb: 进入调试循环（宿主：target remote）
");
    sink.flush_uart();
    crate::gdbstub::gdb_break();
    sink.emit_str("gdb: 已恢复
");
}

fn cmd_tcp_echo(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：tcp_echo <port>\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("tcp_echo: 端口无效\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("tcp_echo: 网络未初始化\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 || crate::socket::bind(h, port) != 0 || crate::socket::listen(h, 4) != 0 {
        sink.emit_str("tcp_echo: 监听失败\n");
        return;
    }
    sink.emit_str("tcp_echo: 正在监听端口 ");
    sink.emit_u32(port as u32);
    sink.emit_str("\n");
    sink.flush_uart();
    loop {
        let c = crate::socket::accept(h);
        if c < 0 {
            sink.emit_str("tcp_echo: 接受连接失败\n");
            break;
        }
        sink.emit_str("tcp_echo: 已接受连接，正在回显\n");
        // 回显直到对端关闭
        loop {
            let mut buf = [0u8; 512];
            let n = crate::socket::recv(c, &mut buf);
            if n <= 0 {
                break;
            }
            if crate::socket::send(c, &buf[..n as usize]) != n {
                break;
            }
        }
        crate::socket::close(c);
        sink.emit_str("tcp_echo: 连接已关闭\n");
    }
}

/// `http_serve <port> [root_dir]`：极简 HTTP 服务器。
/// 有 root_dir 时提供目录下静态文件（`GET /path` → root_dir/path，含
/// 基础 MIME 推断与 404）；无 root_dir 时返回内置欢迎页。
/// 配合 QEMU `hostfwd` 可从宿主机用 curl/浏览器访问。
fn cmd_http_serve(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：http_serve <port> [root_dir]\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("http_serve: 端口无效\n");
            return;
        }
    };
    let root = if cmd.argc > 2 { resolve_path(word_str(&cmd.argv[2])) } else { "" };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("http_serve: 网络未初始化\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 || crate::socket::bind(h, port) != 0 || crate::socket::listen(h, 4) != 0 {
        sink.emit_str("http_serve: 监听失败\n");
        return;
    }
    sink.emit_str("http_serve: 正在监听端口 ");
    sink.emit_u32(port as u32);
    if !root.is_empty() {
        sink.emit_str(" 根目录=");
        sink.emit_str(root);
    }
    sink.emit_str("\n");
    sink.flush_uart();

    const PAGE: &str = "<html><head><title>KunxOS</title></head>\
<body style='font-family:sans-serif;background:#1e2028;color:#eee;\
text-align:center;padding-top:20vh'>\
<h1>KunxOS HTTP Server</h1>\
<p>Hello from the KunxOS microkernel! &mdash; served by the built-in HTTP server</p>\
<p style='color:#4285f4'><b>server: kunxos 1.0.0</b></p>\
</body></html>\n";

    loop {
        let c = crate::socket::accept(h);
        if c < 0 {
            break;
        }
        // 读取请求（首行即请求行，含 GET 路径）
        let mut req = [0u8; 2048];
        let mut got = 0usize;
        loop {
            let n = crate::socket::recv(c, &mut req[got..]);
            if n <= 0 {
                break;
            }
            got += n as usize;
            if got >= 4 && &req[got - 4..got] == b"\r\n\r\n" {
                break;
            }
            if got >= req.len() {
                break;
            }
        }
        // 解析请求路径：GET <path> HTTP/1.x
        let mut path = "/";
        let sline = core::str::from_utf8(&req[..got]).unwrap_or("");
        if let Some(rest) = sline.strip_prefix("GET ") {
            if let Some(sp) = rest.find(' ') {
                path = &rest[..sp];
            }
        }
        let mut resp = [0u8; 12288];
        let mut body: Option<(&[u8], &str)> = None;
        // 读文件（root 模式）；拒绝路径穿越；"/" 映射到 index.html
        if !root.is_empty() && !path.contains("..") {
            let rel = if path == "/" {
                "index.html"
            } else {
                path.trim_start_matches('/')
            };
            let mut full = [0u8; 128];
            let mut off = 0usize;
            for &ch in root.as_bytes() {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = ch;
                off += 1;
            }
            if off > 0 && full[off - 1] != b'/' && off + 1 < full.len() {
                full[off] = b'/';
                off += 1;
            }
            for &ch in rel.as_bytes() {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = ch;
                off += 1;
            }
            full[off] = 0;
            // 安全 UTF-8 校验，非法字节降级为空串，避免 from_utf8_unchecked 的 UB
            let fpath = core::str::from_utf8(&full[..off]).unwrap_or("");
            // 静态文件缓冲（http_serve 单线程事件循环，无并发竞争）
            static mut FBUF: [u8; 8192] = [0; 8192];
            let fbuf: &mut [u8] = unsafe { &mut FBUF };
            let n = read_all(fpath, fbuf);
            if n > 0 {
                // 由扩展名猜 MIME
                let ctype = if fpath.ends_with(".html") || fpath.ends_with(".htm") {
                    "text/html; charset=utf-8"
                } else if fpath.ends_with(".css") {
                    "text/css"
                } else if fpath.ends_with(".js") {
                    "text/javascript"
                } else if fpath.ends_with(".json") {
                    "application/json"
                } else if fpath.ends_with(".txt") {
                    "text/plain"
                } else if fpath.ends_with(".png") {
                    "image/png"
                } else if fpath.ends_with(".jpg") || fpath.ends_with(".jpeg") {
                    "image/jpeg"
                } else {
                    "application/octet-stream"
                };
                let body_slice: &'static [u8] =
                    unsafe { core::slice::from_raw_parts(FBUF.as_ptr(), n) };
                body = Some((body_slice, ctype));
            }
        }
        // 构造响应
        let mut len;
        let (status, ctype, body_len, body_src): (&[u8], &str, usize, &[u8]) = match body {
            Some((b, ct)) => (b"HTTP/1.1 200 OK\r\n", ct, b.len(), b),
            None => {
                if root.is_empty() {
                    (
                        b"HTTP/1.1 200 OK\r\n",
                        "text/html; charset=utf-8",
                        PAGE.len(),
                        PAGE.as_bytes(),
                    )
                } else {
                    (
                        b"HTTP/1.1 404 Not Found\r\n",
                        "text/plain",
                        13,
                        b"404 Not Found",
                    )
                }
            }
        };
        resp[..status.len()].copy_from_slice(status);
        len = status.len();
        let hdr = b"Content-Type: ";
        resp[len..len + hdr.len()].copy_from_slice(hdr);
        len += hdr.len();
        for &ch in ctype.as_bytes() {
            if len + 1 < resp.len() {
                resp[len] = ch;
                len += 1;
            }
        }
        let lh = b"\r\nContent-Length: ";
        resp[len..len + lh.len()].copy_from_slice(lh);
        len += lh.len();
        let mut v = body_len;
        let mut digits = [0u8; 8];
        let mut nd = 0usize;
        if v == 0 {
            digits[0] = b'0';
            nd = 1;
        } else {
            while v > 0 {
                digits[nd] = b'0' + (v % 10) as u8;
                nd += 1;
                v /= 10;
            }
        }
        for i in (0..nd).rev() {
            if len + 1 < resp.len() {
                resp[len] = digits[i];
                len += 1;
            }
        }
        let suf = b"\r\nConnection: close\r\n\r\n";
        resp[len..len + suf.len()].copy_from_slice(suf);
        len += suf.len();
        if len + body_len <= resp.len() {
            resp[len..len + body_len].copy_from_slice(body_src);
            len += body_len;
            let _ = crate::socket::send(c, &resp[..len]);
        }
        crate::socket::close(c);
    }
}

/// `netstat`：列出已打开的套接字（句柄/协议/状态/本地端口/远端）。
fn cmd_netstat(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("FD  协议  状态      本地端口  对端地址\n");
    let s = &mut *sink;
    crate::socket::for_each_socket(&mut |fd, proto, state, lport, rip, rport| {
        s.emit_u32(fd as u32);
        s.emit_str("  ");
        s.emit_str(proto);
        s.emit_str("  ");
        s.emit_str(state);
        let pad = 9usize.saturating_sub(state.len());
        for _ in 0..pad {
            s.emit_str(" ");
        }
        s.emit_u32(lport as u32);
        s.emit_str("  ");
        if rip != 0 {
            // 点分十进制
            let b = rip.to_be_bytes();
            s.emit_u32(b[0] as u32);
            s.emit_str(".");
            s.emit_u32(b[1] as u32);
            s.emit_str(".");
            s.emit_u32(b[2] as u32);
            s.emit_str(".");
            s.emit_u32(b[3] as u32);
            s.emit_str(":");
            s.emit_u32(rport as u32);
        } else {
            s.emit_str("-");
        }
        s.emit_str("\n");
    });
}

/// `nslookup <hostname>`：经内核 DNS 客户端解析域名（QEMU slirp DNS 10.0.2.3）。
fn cmd_nslookup(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：nslookup <hostname>\n");
        return;
    }
    let name = word_str(&cmd.argv[1]);
    if crate::net_stack::local_ip() == 0 || crate::net_stack::dns_ip() == 0 {
        sink.emit_str("nslookup: 网络/DNS 未初始化\n");
        return;
    }
    sink.emit_str("nslookup: 正在解析 ");
    sink.emit_str(name);
    sink.emit_str("……\n");
    match crate::net_stack::dns_resolve(name) {
        Some(ip) => {
            let b = ip.to_be_bytes();
            sink.emit_str(name);
            sink.emit_str(" = ");
            sink.emit_u32(b[0] as u32);
            sink.emit_str(".");
            sink.emit_u32(b[1] as u32);
            sink.emit_str(".");
            sink.emit_u32(b[2] as u32);
            sink.emit_str(".");
            sink.emit_u32(b[3] as u32);
            sink.emit_str("\n");
        }
        None => {
            sink.emit_str("nslookup: 无应答：");
            sink.emit_str(name);
            sink.emit_str("\n");
        }
    }
}

/// `udp_echo <port>`：UDP 回显服务器（把收到的数据报原样发回）。
fn cmd_udp_echo(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：udp_echo <port>\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("udp_echo: 端口无效\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("udp_echo: 网络未初始化\n");
        return;
    }
    let h = crate::socket::socket_udp();
    if h < 0 || crate::socket::bind(h, port) != 0 {
        sink.emit_str("udp_echo: 绑定失败\n");
        return;
    }
    sink.emit_str("udp_echo: 正在监听 UDP 端口 ");
    sink.emit_u32(port as u32);
    sink.emit_str("\n");
    sink.flush_uart();
    loop {
        let mut buf = [0u8; 1472];
        let (n, ip, sport) = crate::socket::recv_from(h, &mut buf);
        if n <= 0 {
            continue;
        }
        // 回显到来源
        crate::socket::send_to(h, &buf[..n as usize], ip, sport);
    }
}

/// `sort [file]`：按字节序排序行（默认读 stdin/管道）。
fn cmd_sort(cmd: &Command, input: &[u8], sink: &mut Sink) {
    const MAX_LINES: usize = 200;
    const MAX_LINE_LEN: usize = 64;
    static mut LINES: [[u8; MAX_LINE_LEN]; MAX_LINES] = [[0; MAX_LINE_LEN]; MAX_LINES];
    static mut LLEN: [usize; MAX_LINES] = [0; MAX_LINES];
    let mut data: [u8; 16384] = [0; 16384];
    let dlen: usize;
    if cmd.argc > 1 {
        let path = resolve_path(word_str(&cmd.argv[1]));
        dlen = read_all(path, &mut data);
    } else {
        dlen = input.len().min(data.len());
        data[..dlen].copy_from_slice(&input[..dlen]);
    }
    let mut n = 0usize;
    let mut line_start = 0usize;
    for i in 0..dlen {
        if data[i] == b'\n' {
            if n < MAX_LINES {
                let l = (i - line_start).min(MAX_LINE_LEN - 1);
                unsafe {
                    LINES[n][..l].copy_from_slice(&data[line_start..line_start + l]);
                    LLEN[n] = l;
                }
                n += 1;
            }
            line_start = i + 1;
        }
    }
    if line_start < dlen && n < MAX_LINES {
        let l = (dlen - line_start).min(MAX_LINE_LEN - 1);
        unsafe {
            LINES[n][..l].copy_from_slice(&data[line_start..line_start + l]);
            LLEN[n] = l;
        }
        n += 1;
    }
    // 插入排序
    unsafe {
        for i in 1..n {
            let mut j = i;
            while j > 0 {
                let a = &LINES[j - 1][..LLEN[j - 1]];
                let b = &LINES[j][..LLEN[j]];
                if a <= b {
                    break;
                }
                let tmp = LINES[j - 1];
                let tl = LLEN[j - 1];
                LINES[j - 1] = LINES[j];
                LLEN[j - 1] = LLEN[j];
                LINES[j] = tmp;
                LLEN[j] = tl;
                j -= 1;
            }
        }
        for i in 0..n {
            sink.emit(&LINES[i][..LLEN[i]]);
            sink.emit_str("\n");
        }
    }
}

/// `calc <expr>`：整数算术求值（+ - * / % 与括号，如 `calc (2+3)*4`）。
fn cmd_calc(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：calc <expr>\n");
        return;
    }
    // 拼表达式（可能被空格分词）
    let mut expr = [0u8; 128];
    let mut elen = 0usize;
    for i in 1..cmd.argc {
        let w = word_str(&cmd.argv[i]);
        if elen + w.len() + 1 > expr.len() {
            break;
        }
        if i > 1 && !w.starts_with(['+', '-', '*', '/', '%', ')']) {
            // 词间补空格（除非以运算符开头）
            expr[elen] = b' ';
            elen += 1;
        }
        for &c in w.as_bytes() {
            expr[elen] = c;
            elen += 1;
        }
    }
    match eval_expr(&expr[..elen]) {
        Some(v) => {
            // 打印十进制
            if v < 0 {
                sink.emit_str("-");
                sink.emit_u32((-(v as i64)) as u32);
            } else {
                sink.emit_u32(v as u32);
            }
            sink.emit_str("\n");
        }
        None => sink.emit_str("calc: 解析错误\n"),
    }
}

/// 表达式求值（递归下降）：expr := term (('+'|'-') term)*
fn eval_expr(s: &[u8]) -> Option<i32> {
    let mut pos = 0usize;
    let v = expr(s, &mut pos)?;
    // 跳过尾部空白
    while pos < s.len() && s[pos] == b' ' {
        pos += 1;
    }
    if pos == s.len() {
        Some(v)
    } else {
        None
    }
}

fn skip_sp(s: &[u8], pos: &mut usize) {
    while *pos < s.len() && s[*pos] == b' ' {
        *pos += 1;
    }
}

fn expr(s: &[u8], pos: &mut usize) -> Option<i32> {
    let mut v = term(s, pos)?;
    loop {
        skip_sp(s, pos);
        if *pos < s.len() && (s[*pos] == b'+' || s[*pos] == b'-') {
            let op = s[*pos];
            *pos += 1;
            let rhs = term(s, pos)?;
            v = if op == b'+' { v.checked_add(rhs)? } else { v.checked_sub(rhs)? };
        } else {
            return Some(v);
        }
    }
}

fn term(s: &[u8], pos: &mut usize) -> Option<i32> {
    let mut v = factor(s, pos)?;
    loop {
        skip_sp(s, pos);
        if *pos < s.len() && (s[*pos] == b'*' || s[*pos] == b'/' || s[*pos] == b'%') {
            let op = s[*pos];
            *pos += 1;
            let rhs = factor(s, pos)?;
            v = match op {
                b'*' => v.checked_mul(rhs)?,
                b'/' => {
                    if rhs == 0 {
                        return None;
                    }
                    v.checked_div(rhs)?
                }
                _ => {
                    if rhs == 0 {
                        return None;
                    }
                    v.checked_rem(rhs)?
                }
            };
        } else {
            return Some(v);
        }
    }
}

fn factor(s: &[u8], pos: &mut usize) -> Option<i32> {
    skip_sp(s, pos);
    if *pos >= s.len() {
        return None;
    }
    if s[*pos] == b'(' {
        *pos += 1;
        let v = expr(s, pos)?;
        skip_sp(s, pos);
        if *pos < s.len() && s[*pos] == b')' {
            *pos += 1;
            Some(v)
        } else {
            None
        }
    } else if s[*pos] == b'-' {
        *pos += 1;
        factor(s, pos).and_then(|v| v.checked_neg())
    } else if s[*pos].is_ascii_digit() {
        let mut v = 0i32;
        let start = *pos;
        while *pos < s.len() && s[*pos].is_ascii_digit() {
            v = v.checked_mul(10)?.checked_add((s[*pos] - b'0') as i32)?;
            *pos += 1;
        }
        if *pos == start {
            None
        } else {
            Some(v)
        }
    } else {
        None
    }
}

/// `poweroff`：经 SBI 关机（QEMU virt 下优雅退出模拟器）。
fn cmd_poweroff(_cmd: &Command, _sink: &mut Sink) {
    crate::uart_send_str("poweroff: 正在关机\n");
    // SBI SRST（shutdown）→ 旧式 shutdown → sifive test 器件（0x5555）兜底
    crate::sbi::shutdown();
}

/// `reboot`：经 SBI 重启（QEMU `-no-reboot` 下等价退出）。
fn cmd_reboot(_cmd: &Command, _sink: &mut Sink) {
    crate::uart_send_str("reboot: 系统重置\n");
    crate::sbi::system_reset();
}

/// `head [-n N] [file]`：输出前 N 行（默认 10；无文件时读 stdin/管道）。
fn cmd_head(cmd: &Command, input: &[u8], sink: &mut Sink) {
    let mut n = 10usize;
    let mut argi = 1usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        if cmd.argc > 2 {
            n = word_str(&cmd.argv[2]).parse::<usize>().unwrap_or(10);
            argi = 3;
        }
    }
    let mut data: [u8; 16384] = [0; 16384];
    let dlen: usize;
    if cmd.argc > argi {
        let path = resolve_path(word_str(&cmd.argv[argi]));
        dlen = read_all(path, &mut data);
    } else {
        dlen = input.len().min(data.len());
        data[..dlen].copy_from_slice(&input[..dlen]);
    }
    let mut lines = 0usize;
    let mut emitted = 0usize;
    for (i, &b) in data[..dlen].iter().enumerate() {
        if lines >= n {
            break;
        }
        sink.emit(&data[emitted..i + 1]);
        emitted = i + 1;
        if b == b'\n' {
            lines += 1;
        }
    }
    // 不足 N 行时把剩余一并输出（保持完整行）
    if lines < n && emitted < dlen {
        sink.emit(&data[emitted..dlen]);
    }
}

/// `ping <ip>`：经内核网络栈发送 ICMP echo 并等待应答。
fn cmd_ping(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：ping <ip>\n");
        return;
    }
    let Some(ip) = parse_ip(word_str(&cmd.argv[1])) else {
        sink.emit_str("ping: IP 地址无效\n");
        return;
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("ping: 网络未初始化\n");
        return;
    }
    sink.emit_str("PING ");
    sink.emit_str(word_str(&cmd.argv[1]));
    sink.emit_str("……\n");
    if crate::net_stack::ping(ip) {
        sink.emit_str("64 字节来自 ");
        sink.emit_str(word_str(&cmd.argv[1]));
        sink.emit_str(": icmp_seq=1 ttl=64 time=0.0 ms\n");
    } else {
        sink.emit_str("ping: 无应答：");
        sink.emit_str(word_str(&cmd.argv[1]));
        sink.emit_str("\n");
    }
}

/// 统计字节流的行数与词数（空白分隔）。
fn count_stats(data: &[u8]) -> (usize, usize) {
    let mut lines = 0usize;
    let mut words = 0usize;
    let mut in_word = false;
    for &b in data {
        if b == b'\n' {
            lines += 1;
        }
        let sep = b == b' ' || b == b'\t' || b == b'\n' || b == b'\r';
        if sep {
            in_word = false;
        } else if !in_word {
            words += 1;
            in_word = true;
        }
    }
    (lines, words)
}

/// `wc [file...]`：统计行数 / 词数 / 字节数（无参数时读 stdin/管道）。
/// 按 `flags`（bit0=行, bit1=词, bit2=字节）输出 wc 统计。
fn emit_wc_stats(sink: &mut Sink, flags: u8, lines: usize, words: usize, bytes: usize) {
    if flags & 1 != 0 {
        sink.emit_u32(lines as u32);
        sink.emit_str(" ");
    }
    if flags & 2 != 0 {
        sink.emit_u32(words as u32);
        sink.emit_str(" ");
    }
    if flags & 4 != 0 {
        sink.emit_u32(bytes as u32);
        sink.emit_str(" ");
    }
}

/// `wc [-l|-w|-c] [file...]`：统计行数 / 词数 / 字节数（标志可组合，默认三者全出）；
/// 无文件参数时读 stdin/管道。
fn cmd_wc(cmd: &Command, input: &[u8], sink: &mut Sink) {
    let mut flags = 0u8;
    let mut argi = 1usize;
    while argi < cmd.argc {
        let a = word_str(&cmd.argv[argi]);
        if a.len() >= 2 && a.as_bytes()[0] == b'-' {
            let mut any = false;
            for &f in &a.as_bytes()[1..] {
                match f {
                    b'l' => {
                        flags |= 1;
                        any = true;
                    }
                    b'w' => {
                        flags |= 2;
                        any = true;
                    }
                    b'c' => {
                        flags |= 4;
                        any = true;
                    }
                    _ => {}
                }
            }
            if any {
                argi += 1;
                continue;
            }
        }
        break;
    }
    if flags == 0 {
        flags = 7;
    }
    let mut total_lines = 0usize;
    let mut total_words = 0usize;
    let mut total_bytes = 0usize;
    // 无文件参数：统计 stdin（来自管道）
    if cmd.argc <= argi {
        let (lines, words) = count_stats(input);
        emit_wc_stats(sink, flags, lines, words, input.len());
        sink.emit_str("\n");
        return;
    }
    for i in argi..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let mut buf = [0u8; 16384];
        let n = read_all(path, &mut buf);
        let data = &buf[..n];
        let (lines, words) = count_stats(data);
        emit_wc_stats(sink, flags, lines, words, n);
        sink.emit_str(word_str(&cmd.argv[i]));
        sink.emit_str("\n");
        total_lines += lines;
        total_words += words;
        total_bytes += n;
    }
    if cmd.argc - argi > 1 {
        emit_wc_stats(sink, flags, total_lines, total_words, total_bytes);
        sink.emit_str("总计\n");
    }
}

/// `uname`：打印操作系统信息（内核版本/架构）。
fn cmd_uname(cmd: &Command, sink: &mut Sink) {
    let all = cmd.argc > 1 && word_str(&cmd.argv[1]) == "-a";
    if all {
        sink.emit_str("KunxOS kunxos 1.0.0 aarch64 (KunxOS 微内核)\n");
    } else {
        sink.emit_str("KunxOS 1.0.0\n");
    }
}

/// `clear`：发送 ANSI 清屏序列。
fn cmd_clear(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("\x1b[2J\x1b[H");
}

/// `mkdir <dir>`：创建目录。
fn cmd_mkdir(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：mkdir <dir>\n");
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    if vfs::mkdir(path) != 0 {
        sink.emit_str("mkdir: 无法创建目录：");
        sink.emit_str(path);
        sink.emit_str("\n");
    }
}

/// `rm <file>`：删除文件（目录删除暂不支持，与内核 fs_unlink 语义一致）。
fn cmd_rm(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：rm <file>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        if vfs::unlink(path) != 0 {
            sink.emit_str("rm: 无法删除：");
            sink.emit_str(path);
            sink.emit_str("\n");
        }
    }
}

/// `touch <file...>`：文件不存在时创建空文件，已存在则空操作（本 FS 无 mtime）。
fn cmd_touch(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：touch <file>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let fd = vfs::open(path, vfs::O_CREAT | vfs::O_WRONLY);
        if fd < 0 {
            sink.emit_str("touch: 无法创建：");
            sink.emit_str(path);
            sink.emit_str("\n");
        } else {
            vfs::close(fd);
        }
    }
}

/// `ln -s <target> <link>`：创建符号链接（仅支持 -s；硬链接未实现）。
fn cmd_ln(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：ln -s <target> <link>\n");
        return;
    }
    if word_str(&cmd.argv[1]) != "-s" {
        sink.emit_str("ln: 仅支持 -s（符号链接）\n");
        return;
    }
    if cmd.argc < 4 {
        sink.emit_str("用法：ln -s <target> <link>\n");
        return;
    }
    // target 原样保存（可为相对路径），link 解析为绝对路径
    let mut tbuf = [0u8; 240];
    let ts = word_str(&cmd.argv[2]);
    let tl = ts.len().min(tbuf.len() - 1);
    tbuf[..tl].copy_from_slice(&ts.as_bytes()[..tl]);
    tbuf[tl] = 0;
    let target = static_str(&tbuf);
    let link = resolve_path(word_str(&cmd.argv[3]));
    if vfs::symlink(target, link) != 0 {
        sink.emit_str("ln: 无法创建链接：");
        sink.emit_str(link);
        sink.emit_str("\n");
    }
}

/// 取路径最后一段（basename），写入 `out`，返回长度；空路径返回 0。
fn path_basename(p: &str, out: &mut [u8]) -> usize {
    let b = p.as_bytes();
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'/' {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && b[start - 1] != b'/' {
        start -= 1;
    }
    let n = (end - start).min(out.len());
    out[..n].copy_from_slice(&b[start..start + n]);
    n
}

/// 拼接 `dir` + "/" + `name` 到 `out`，返回总长度；放不下返回 0。
fn path_join(dir: &str, name: &str, out: &mut [u8]) -> usize {
    let db = dir.as_bytes();
    let nb = name.as_bytes();
    if db.len() + 1 + nb.len() >= out.len() {
        return 0;
    }
    out[..db.len()].copy_from_slice(db);
    let mut n = db.len();
    if n > 0 && out[n - 1] != b'/' {
        out[n] = b'/';
        n += 1;
    }
    out[n..n + nb.len()].copy_from_slice(nb);
    n + nb.len()
}

/// `cp <src> <dst>`：复制文件；`dst` 为目录时复制为 `dst/basename(src)`。
fn cmd_cp(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 3 {
        sink.emit_str("用法：cp <src> <dst>\n");
        return;
    }
    // resolve_path 复用同一 PATH_BUF：先复制源路径到本地，避免被目标路径覆盖
    let mut srcbuf = [0u8; 128];
    let sp = resolve_path(word_str(&cmd.argv[1]));
    let sl = sp.len().min(srcbuf.len() - 1);
    srcbuf[..sl].copy_from_slice(&sp.as_bytes()[..sl]);
    let src = core::str::from_utf8(&srcbuf[..sl]).unwrap_or("");
    let dst_arg = resolve_path(word_str(&cmd.argv[2]));
    // 目标为目录：在目录后追加源 basename
    let mut dstbuf = [0u8; 128];
    let dst: &str = match vfs::stat_path(dst_arg) {
        Some((t, _)) if t == crate::fs_simple::T_DIR => {
            let mut bn = [0u8; 64];
            let bl = path_basename(src, &mut bn);
            if bl == 0 {
                sink.emit_str("cp: 源路径无效\n");
                return;
            }
            let n = path_join(dst_arg, core::str::from_utf8(&bn[..bl]).unwrap_or(""), &mut dstbuf);
            if n == 0 {
                sink.emit_str("cp: 目标路径过长\n");
                return;
            }
            core::str::from_utf8(&dstbuf[..n]).unwrap_or("")
        }
        _ => dst_arg,
    };
    if src == dst {
        sink.emit_str("cp: 不能把文件复制到自身\n");
        return;
    }
    let sfd = vfs::open(src, vfs::O_RDONLY);
    if sfd < 0 {
        sink.emit_str("cp: 无法打开源：");
        sink.emit_str(src);
        sink.emit_str("\n");
        return;
    }
    let dfd = vfs::open(dst, vfs::O_CREAT | vfs::O_WRONLY | vfs::O_TRUNC);
    if dfd < 0 {
        vfs::close(sfd);
        sink.emit_str("cp: 无法创建目标：");
        sink.emit_str(dst);
        sink.emit_str("\n");
        return;
    }
    let mut buf = [0u8; 4096];
    loop {
        let r = vfs::read(sfd, &mut buf);
        if r <= 0 {
            break;
        }
        if vfs::write(dfd, &buf[..r as usize]) != r {
            sink.emit_str("cp: 写入错误\n");
            break;
        }
    }
    vfs::close(sfd);
    vfs::close(dfd);
}

/// `mv <src> <dst>`：重命名/移动文件（可跨目录，SFS 迁移同一 inode）。
fn cmd_mv(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 3 {
        sink.emit_str("用法：mv <src> <dst>\n");
        return;
    }
    let mut srcbuf = [0u8; 128];
    let sp = resolve_path(word_str(&cmd.argv[1]));
    let sl = sp.len().min(srcbuf.len() - 1);
    srcbuf[..sl].copy_from_slice(&sp.as_bytes()[..sl]);
    let src = core::str::from_utf8(&srcbuf[..sl]).unwrap_or("");
    let dst_arg = resolve_path(word_str(&cmd.argv[2]));
    let mut dstbuf = [0u8; 128];
    let dst: &str = match vfs::stat_path(dst_arg) {
        Some((t, _)) if t == crate::fs_simple::T_DIR => {
            let mut bn = [0u8; 64];
            let bl = path_basename(src, &mut bn);
            if bl == 0 {
                sink.emit_str("mv: 源路径无效\n");
                return;
            }
            let n = path_join(dst_arg, core::str::from_utf8(&bn[..bl]).unwrap_or(""), &mut dstbuf);
            if n == 0 {
                sink.emit_str("mv: 目标路径过长\n");
                return;
            }
            core::str::from_utf8(&dstbuf[..n]).unwrap_or("")
        }
        _ => dst_arg,
    };
    if src == dst {
        return; // 同名同路径：空操作
    }
    if vfs::rename(src, dst) != 0 {
        sink.emit_str("mv: 无法移动：");
        sink.emit_str(src);
        sink.emit_str(" -> ");
        sink.emit_str(dst);
        sink.emit_str("（不存在、是目录，或目标已存在？）\n");
    }
}

/// `tail [-n N] [file]`：输出末尾 N 行（默认 10），无文件参数时读 stdin/管道。
fn cmd_tail(cmd: &Command, input: &[u8], sink: &mut Sink) {
    let mut n = 10usize;
    let mut argi = 1usize;
    if cmd.argc > 1 && word_str(&cmd.argv[1]) == "-n" {
        if cmd.argc > 2 {
            n = word_str(&cmd.argv[2]).parse::<usize>().unwrap_or(10);
            argi = 3;
        }
    }
    if n == 0 {
        return;
    }
    let mut data: [u8; 16384] = [0; 16384];
    let dlen: usize;
    if cmd.argc > argi {
        let path = resolve_path(word_str(&cmd.argv[argi]));
        dlen = read_all(path, &mut data);
    } else {
        dlen = input.len().min(data.len());
        data[..dlen].copy_from_slice(&input[..dlen]);
    }
    let d = &data[..dlen];
    // 从末尾向前找第 n+1 个换行；末尾无换行时把文件尾视为一个隐式换行
    let mut count = if dlen > 0 && d[dlen - 1] != b'\n' { 1 } else { 0 };
    let mut start = 0usize;
    for i in (0..dlen).rev() {
        if d[i] == b'\n' {
            count += 1;
            if count > n {
                start = i + 1;
                break;
            }
        }
    }
    sink.emit(&d[start..dlen]);
    if dlen > 0 && d[dlen - 1] != b'\n' {
        sink.emit_str("\n");
    }
}

fn cmd_ps(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("PID  用户  名称\n");
    let s = &mut *sink;
    process::ps(&mut |pid: u32, name: &str| {
        s.emit_u32(pid);
        s.emit_str("    ");
        s.emit_u32(process::uid_of(pid));
        s.emit_str("  ");
        s.emit_str(name);
        s.emit_str("\n");
    });
}

/// `kill <pid>`：向进程发送 SIGKILL（9）。目标进程下一次进入内核（任意系统
/// 调用）时被终止；阻塞中的进程先被唤醒。
fn cmd_kill(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：kill <pid>\n");
        return;
    }
    let Ok(pid) = word_str(&cmd.argv[1]).parse::<u32>() else {
        sink.emit_str("kill: 进程号无效\n");
        return;
    };
    if pid == crate::process::current_pid() {
        sink.emit_str("kill: 不能结束 shell 自身\n");
        return;
    }
    if !crate::process::process_exists(pid) {
        sink.emit_str("kill: 无此进程：");
        sink.emit_str(word_str(&cmd.argv[1]));
        sink.emit_str("\n");
        return;
    }
    // 直接终止（与 sys_kill 的 SIGKILL 同款）：挂起位图只在 syscall 入口
    // 检查，长阻塞在 wait4/read 里的任务被唤醒后会在系统调用内部继续
    // 等待，永远到不了入口检查——SIGKILL 必须当场兑现。
    crate::process::terminate(pid);
    sink.emit_str("kill: 信号 9 已发送至进程 ");
    sink.emit_u32(pid);
    sink.emit_str("\n");
}

/// `sleep <seconds>`：让 shell 睡眠指定秒数（基于内核单调时钟，可被定时器
/// 唤醒后按剩余时间继续）。
fn cmd_sleep(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("用法：sleep <seconds>\n");
        return;
    }
    let Ok(secs) = word_str(&cmd.argv[1]).parse::<u64>() else {
        sink.emit_str("sleep: 时间无效\n");
        return;
    };
    let deadline = crate::timer::now_us() + secs.saturating_mul(1_000_000);
    crate::sched_cfs::sleep_until(deadline);
}

/// `beep [freq] [ms]`：经 AC97 播放指定频率的正弦波音调（默认 440Hz / 500ms）。
fn cmd_beep(cmd: &Command, sink: &mut Sink) {
    let mut freq: u32 = 440;
    let mut ms: u32 = 500;
    if cmd.argc >= 2 {
        match word_str(&cmd.argv[1]).parse::<u32>() {
            Ok(f) if f > 0 => freq = f,
            _ => {
                sink.emit_str("beep: 频率无效\n");
                return;
            }
        }
    }
    if cmd.argc >= 3 {
        match word_str(&cmd.argv[2]).parse::<u32>() {
            Ok(m) if m > 0 => ms = m,
            _ => {
                sink.emit_str("beep: 时长无效\n");
                return;
            }
        }
    }
    sink.emit_str("beep: 播放 ");
    sink.emit_u32(freq);
    sink.emit_str(" Hz，持续 ");
    sink.emit_u32(ms);
    sink.emit_str(" 毫秒\n");
    // 优先走新音频栈（virtio-sound 低延迟路径）；无后端时回退 AC97。
    let r = if crate::audio::backend_ready() {
        crate::audio::beep(freq, ms)
    } else {
        crate::ac97::play_tone(freq, ms)
    };
    if r != 0 {
        sink.emit_str("beep: 无音频设备或播放失败\n");
    } else {
        sink.emit_str("beep: 完成\n");
    }
}

/// `audio`：打印音频子系统状态与统计（后端 / 位置 / underrun / IRQ 延迟）。
fn cmd_audio(_cmd: &Command, _sink: &mut Sink) {
    crate::audio::stats_dump();
}

/// `audiolat <secs>`：内核侧延迟测量——连续写入音调块并采样
/// 缓冲延迟直方图 + IRQ/补充/提交消费统计（等价于用户态 audiolat 的
/// --direct 模式，供无用户态工具时快速验证）。
fn cmd_audiolat(cmd: &Command, sink: &mut Sink) {
    let mut secs: u64 = 5;
    if cmd.argc >= 2 {
        match word_str(&cmd.argv[1]).parse::<u64>() {
            Ok(s) if s > 0 => secs = s,
            _ => {
                sink.emit_str("audiolat: 秒数无效\n");
                return;
            }
        }
    }
    secs = secs.clamp(1, 60);
    let total_chunks = secs * 100;
    let mut hist = [0u64; 8];
    let mut samples = 0u64;
    let mut min_ms = u64::MAX;
    let mut max_ms = 0u64;
    let mut sum_ms = 0u64;

    sink.emit_str("audiolat: 内核路径延迟，");
    sink.emit_u32(secs as u32);
    sink.emit_str(" 秒\n");
    crate::audio::stats_dump();

    for c in 0..total_chunks {
        let mut chunk = [0u8; crate::audio::CHUNK_BYTES];
        crate::audio::beep_chunk(440, c as u32, &mut chunk);
        let n = crate::audio::audio_write(0, &chunk);
        if n != crate::audio::CHUNK_BYTES as i32 {
            sink.emit_str("audiolat: 环形缓冲已满，中止\n");
            break;
        }
        if c % 10 == 9 {
            // 每 100ms 采样缓冲延迟
            let st = crate::audio::stats_snapshot();
            let buffered = st.sw_frames.saturating_sub(st.hw_frames);
            let ms = buffered * 1000 / crate::audio::SAMPLE_RATE as u64;
            let idx = match ms {
                0..=4 => 0,
                5..=9 => 1,
                10..=14 => 2,
                15..=19 => 3,
                20..=29 => 4,
                30..=49 => 5,
                50..=99 => 6,
                _ => 7,
            };
            hist[idx] += 1;
            samples += 1;
            if ms < min_ms {
                min_ms = ms;
            }
            if ms > max_ms {
                max_ms = ms;
            }
            sum_ms += ms;
        }
        crate::sched_cfs::yield_now();
        // 粗 10ms 节拍：忙等至下一个 10ms 边界（比 nanosleep 更准）
        let deadline = crate::timer::now_us() + 10_000;
        while crate::timer::now_us() < deadline {
            crate::sched_cfs::yield_now();
        }
    }

    sink.emit_str("audiolat: 缓冲延迟直方图（样本数=");
    sink.emit_u32(samples as u32);
    sink.emit_str("）：\n");
    let labels = ["0-4", "5-9", "10-14", "15-19", "20-29", "30-49", "50-99", ">=100"];
    for i in 0..8 {
        sink.emit_str("  ");
        sink.emit_str(labels[i]);
        sink.emit_str(" 毫秒：");
        sink.emit_u32(hist[i] as u32);
        if samples > 0 {
            sink.emit_str("（");
            sink.emit_u32((hist[i] * 100 / samples) as u32);
            sink.emit_str("%）");
        }
        sink.emit_str("\n");
    }
    sink.emit_str("  最小/平均/最大：");
    if min_ms == u64::MAX {
        sink.emit_str("无数据\n");
    } else {
        sink.emit_u32(min_ms as u32);
        sink.emit_str("/");
        sink.emit_u32((sum_ms / samples) as u32);
        sink.emit_str("/");
        sink.emit_u32(max_ms as u32);
        sink.emit_str(" 毫秒\n");
    }
    crate::audio::stats_dump();
}

/// `sysinfo`：物理内存 / 任务 / 进程使用统计。
fn cmd_sysinfo(_cmd: &Command, sink: &mut Sink) {
    let us = crate::timer::now_us();
    sink.emit_str("运行时长：");
    sink.emit_u32((us / 1_000_000) as u32);
    sink.emit_str(" 秒\n");
    let total_pages = crate::allocator::total_pages();
    let used = crate::allocator::used_pages();
    let total_kb = total_pages * crate::allocator::PAGE_SIZE / 1024;
    let used_kb = used * crate::allocator::PAGE_SIZE / 1024;
    sink.emit_str("内存总量：");
    sink.emit_u32(total_kb as u32);
    sink.emit_str(" kB\n");
    sink.emit_str("内存已用：");
    sink.emit_u32(used_kb as u32);
    sink.emit_str(" kB（");
    if total_pages > 0 {
        sink.emit_u32(((used * 100) / total_pages) as u32);
    } else {
        sink.emit_str("0");
    }
    sink.emit_str("%）\n");
    sink.emit_str("内存空闲：");
    sink.emit_u32((total_kb.saturating_sub(used_kb)) as u32);
    sink.emit_str(" kB\n");
    sink.emit_str("任务数：  ");
    sink.emit_u32(crate::sched_cfs::task_count() as u32);
    sink.emit_str(" 运行中\n");
    sink.emit_str("进程数：");
    let mut n = 0u32;
    process::ps(&mut |_, _| n += 1);
    sink.emit_u32(n);
    sink.emit_str(" 已注册\n");
}

fn run_builtin(cmd: &Command, input: &[u8], sink: &mut Sink) {
    let name = word_str(&cmd.argv[0]);
    match name {
        "cd" => cmd_cd(cmd, sink),
        "echo" => cmd_echo(cmd, sink),
        "export" => cmd_export(cmd, sink),
        "env" => cmd_export(cmd, sink),
        "alias" => cmd_alias(cmd, sink),
        "unalias" => cmd_unalias(cmd, sink),
        "source" | "sh" => cmd_source(cmd, sink),
        "exit" => {
            let code = if cmd.argc > 1 {
                word_str(&cmd.argv[1]).parse::<i32>().unwrap_or(0)
            } else {
                0
            };
            process::exit(code);
        }
        "pwd" => cmd_pwd(cmd, sink),
        "ls" => cmd_ls(cmd, sink),
        "cat" => cmd_cat(cmd, input, sink),
        "run" => cmd_run(cmd, sink),
        "busybox" => cmd_busybox(cmd, sink),
        "pkg" => cmd_pkg(cmd, sink),
        "ota" => cmd_ota(cmd, sink),
        "ps" => cmd_ps(cmd, sink),
        "uname" => cmd_uname(cmd, sink),
        "clear" => cmd_clear(cmd, sink),
        "mkdir" => cmd_mkdir(cmd, sink),
        "rm" => cmd_rm(cmd, sink),
        "touch" => cmd_touch(cmd, sink),
        "cp" => cmd_cp(cmd, sink),
        "mv" => cmd_mv(cmd, sink),
        "ln" => cmd_ln(cmd, sink),
        "tail" => cmd_tail(cmd, input, sink),
        "help" => cmd_help(cmd, sink),
        "wc" => cmd_wc(cmd, input, sink),
        "ping" => cmd_ping(cmd, sink),
        "sysinfo" => cmd_sysinfo(cmd, sink),
        "head" => cmd_head(cmd, input, sink),
        "grep" => cmd_grep(cmd, input, sink),
        "df" => cmd_df(cmd, sink),
        "tcp_echo" => cmd_tcp_echo(cmd, sink),
        "http_get" => cmd_http_get(cmd, sink),
        "gdb" => cmd_gdb(cmd, sink),
        "health" => cmd_health(cmd, sink),
        "http_serve" => cmd_http_serve(cmd, sink),
        "netstat" => cmd_netstat(cmd, sink),
        "nslookup" => cmd_nslookup(cmd, sink),
        "udp_echo" => cmd_udp_echo(cmd, sink),
        "sort" => cmd_sort(cmd, input, sink),
        "calc" => cmd_calc(cmd, sink),
        "poweroff" => cmd_poweroff(cmd, sink),
        "reboot" => cmd_reboot(cmd, sink),
        "power" => cmd_power(cmd, sink),
        "history" => cmd_history(cmd, sink),
        "uptime" => cmd_uptime(cmd, sink),
        "kill" => cmd_kill(cmd, sink),
        "sleep" => cmd_sleep(cmd, sink),
        "date" => cmd_date(cmd, sink),
        "which" => cmd_which(cmd, sink),
        "uniq" => cmd_uniq(cmd, input, sink),
        "sha1sum" => cmd_sha1sum(cmd, input, sink),
        "stat" => cmd_stat(cmd, sink),
        "od" => cmd_od(cmd, sink),
        "tee" => cmd_tee(cmd, input, sink),
        "dmesg" => cmd_dmesg(cmd, sink),
        "beep" => cmd_beep(cmd, sink),
        "audio" => cmd_audio(cmd, sink),
        "audiolat" => cmd_audiolat(cmd, sink),
        "top" => cmd_top(cmd, sink),
        "mount" => cmd_mount(cmd, sink),
        "ifconfig" => cmd_ifconfig(cmd, sink),
        "log" => cmd_log(cmd, sink),
        "crash" => cmd_crash(cmd, sink),
        "perf" => cmd_perf(cmd, sink),
        "irqstat" => cmd_irqstat(cmd, sink),
        "netdiag" => cmd_netdiag(cmd, sink),
        "verbose" => cmd_verbose(cmd, sink),
        "true" => cmd_true(cmd, sink),
        "false" => cmd_false(cmd, sink),
        "basename" => cmd_basename(cmd, sink),
        "dirname" => cmd_dirname(cmd, sink),
        "" => {}
        _ => {
            sink.emit_str("kunxos: 未找到命令：");
            sink.emit_str(name);
            sink.emit_str("\n");
        }
    }
}

// ==================== 读行（含命令历史） ====================

struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

/// 命令历史（环形，最新在末尾；`HIST_POS` 为浏览位置，`HIST_COUNT` 表示最新）。
static mut HIST: [[u8; LINE_MAX]; HIST_MAX] = [[0; LINE_MAX]; HIST_MAX];
static mut HIST_LEN: [usize; HIST_MAX] = [0; HIST_MAX];
static mut HIST_COUNT: usize = 0;
static mut HIST_POS: usize = 0;

/// 把当前行记入历史（与最近一条重复则忽略）。
fn history_push(line: &Line) {
    unsafe {
        if line.len == 0 {
            return;
        }
        if HIST_COUNT > 0 {
            let last = HIST_COUNT - 1;
            if HIST_LEN[last] == line.len && &HIST[last][..line.len] == &line.buf[..line.len] {
                return;
            }
        }
        if HIST_COUNT == HIST_MAX {
            // 满：整体前移，丢弃最旧
            for i in 1..HIST_MAX {
                HIST[i - 1] = HIST[i];
                HIST_LEN[i - 1] = HIST_LEN[i];
            }
            HIST_COUNT -= 1;
        }
        let idx = HIST_COUNT;
        HIST[idx].fill(0);
        HIST[idx][..line.len].copy_from_slice(&line.buf[..line.len]);
        HIST_LEN[idx] = line.len;
        HIST_COUNT += 1;
        HIST_POS = HIST_COUNT; // 浏览位置回到「最新」
    }
}

/// 用历史条目覆盖当前行（并重绘行内容）。
fn history_load(pos: usize, line: &mut Line) {
    unsafe {
        let idx = pos;
        let len = HIST_LEN[idx];
        line.buf.fill(0);
        line.buf[..len].copy_from_slice(&HIST[idx][..len]);
        line.len = len;
    }
    // 清行并回显
    crate::uart_send_str("\r\x1b[K");
    for &c in &line.buf[..line.len] {
        crate::uart_send_char(c);
    }
}

/// 枚举 `dir` 下匹配 `prefix` 的条目，用于 Tab 补全。
///
/// * `print_list`：为 true 时把候选打印到控制台（多候选场景）。
/// * 唯一匹配时把完整匹配名写入 `out`（调用方按前缀截取追加部分）。
/// * 返回匹配数。
fn complete_from_dir(dir: &str, prefix: &str, out: &mut [u8], print_list: bool) -> usize {
    let mut matches = 0usize;
    let mut first: [u8; 32] = [0; 32];
    let mut first_len = 0usize;
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    loop {
        let ino = vfs::readdir(dir, idx, &mut name);
        if ino < 0 {
            break;
        }
        idx += 1;
        let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let entry = core::str::from_utf8(&name[..elen]).unwrap_or("");
        if entry == "." || entry == ".." || entry.is_empty() {
            continue;
        }
        if entry.starts_with(prefix) {
            if matches == 0 {
                first_len = elen.min(first.len());
                first[..first_len].copy_from_slice(&name[..first_len]);
            } else {
                // 多候选：收窄公共前缀
                let mut cl = 0usize;
                while cl < first_len.min(elen) && first[cl] == name[cl] {
                    cl += 1;
                }
                first_len = cl;
            }
            if print_list {
                crate::uart_send_str(entry);
                crate::uart_send_str("  ");
            }
            matches += 1;
        }
    }
    if print_list && matches > 0 {
        crate::uart_send_str("\n");
    }
    if matches == 1 {
        let n = first_len.min(out.len());
        out[..n].copy_from_slice(&first[..n]);
    }
    matches
}

/// 内置命令名列表（供 Tab 补全与「命令位置」判定）。
const BUILTINS: &[&str] = &[
    "cd", "echo", "export", "exit", "pwd", "ls", "cat", "run", "busybox", "pkg",
    "ps", "uname", "clear", "mkdir", "rm", "touch", "cp", "mv", "wc", "head",
    "tail", "grep", "df", "history", "uptime", "ping", "sysinfo", "help", "kill",
    "sleep", "date", "which", "uniq", "sha1sum", "stat", "true",
    "false", "basename", "dirname", "dmesg", "od", "tee", "ln", "beep",
    "verbose",
    "audio", "audiolat",
    "top", "mount", "ifconfig", "log", "crash", "health", "perf", "irqstat", "netdiag",
];

/// 在内置命令中匹配前缀；唯一匹配时写入 `out`，返回匹配数。
fn complete_from_builtins(prefix: &str, out: &mut [u8], print_list: bool) -> usize {
    let mut matches = 0usize;
    let mut first: [u8; 32] = [0; 32];
    let mut first_len = 0usize;
    for b in BUILTINS {
        if b.starts_with(prefix) {
            if matches == 0 {
                first_len = b.len().min(first.len());
                first[..first_len].copy_from_slice(&b.as_bytes()[..first_len]);
            } else {
                let mut cl = 0usize;
                while cl < first_len.min(b.len()) && first[cl] == b.as_bytes()[cl] {
                    cl += 1;
                }
                first_len = cl;
            }
            if print_list {
                crate::uart_send_str(b);
                crate::uart_send_str("  ");
            }
            matches += 1;
        }
    }
    if print_list && matches > 0 {
        crate::uart_send_str("\n");
    }
    if matches == 1 {
        let n = first_len.min(out.len());
        out[..n].copy_from_slice(&first[..n]);
    }
    matches
}

/// 带路径感知的补全：前缀含 `/` 时按「父目录 + 末段」补全；
/// 否则在 `fallback_dir`（cwd）下补全。
fn complete_path(prefix: &str, out: &mut [u8], print_list: bool) -> usize {
    if let Some(slash) = prefix.rfind('/') {
        let (dir_part, last) = prefix.split_at(slash + 1);
        // 目录部分可能相对（"tmp/x"）——统一先解析为绝对路径
        let dir = if dir_part.starts_with('/') {
            resolve_path(dir_part)
        } else {
            resolve_path(dir_part)
        };
        // 完整路径 = dir + "/" + 补全后的末段；这里只补全末段并允许调用方拼接
        let matches = complete_from_dir(dir, last, out, print_list);
        if matches == 1 {
            // 把补全结果改写成完整路径（dir + last 段）
            let mut full = [0u8; 96];
            let mut off = 0usize;
            for &c in dir.as_bytes() {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = c;
                off += 1;
            }
            if off > 0 && full[off - 1] != b'/' && off + 1 < full.len() {
                full[off] = b'/';
                off += 1;
            }
            let seg_len = out.iter().position(|&c| c == 0).unwrap_or(out.len());
            let seg = core::str::from_utf8(&out[..seg_len]).unwrap_or("");
            for &c in seg.as_bytes() {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = c;
                off += 1;
            }
            full[off] = 0;
            // 安全 UTF-8 校验，非法字节降级为空串，避免 from_utf8_unchecked 的 UB
            let full_str = core::str::from_utf8(&full[..off]).unwrap_or("");
            let n = full_str.len().min(out.len());
            out[..n].copy_from_slice(&full_str.as_bytes()[..n]);
        }
        matches
    } else {
        complete_from_dir(cwd_str(), prefix, out, print_list)
    }
}

/// 当前词所在行位置（最后一个空白之后）。
fn current_word_start(line: &[u8]) -> usize {
    // 空白与 ';' 都是词边界（`a; b<Tab>` 补全 b）
    line.iter()
        .rposition(|&c| c == b' ' || c == b';')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// 单行读取（行编辑所在）：输入源 = TTY 会话键码（GUI 终端附接时）或 UART。
///
/// 行编辑能力：
/// - 光标移动：←→（`ESC[D/C`）、Home/End（Ctrl+A/E）；
/// - 删除：Backspace（0x7f/0x08，行中删除）、Ctrl+U（删至行尾）、Ctrl+W（删词）；
/// - 历史：↑/↓ 浏览（16 条，与 Tab 补全共用 HIST）；
/// - Tab 补全：命令名（内置 + /bin）与路径；
/// - 信号：Ctrl+C（SIGINT/丢弃行）、Ctrl+D（EOF/空行 → SIGTERM 前台子进程）、
///   Ctrl+Z（SIGTSTP 预留）。
///
/// 回显通过 `uart_send_*`（镜像到 TTY out_q），向图形终端输出规范 ANSI/退格
/// 序列；GUI 只做终端仿真渲染，不自行本地回显。
fn read_line() -> Line {
    let mut line = Line { buf: [0; LINE_MAX], len: 0 };
    // 光标列（0..=len，位于插入点）。
    let mut cur = 0usize;
    // 提示 UART 输入曾溢出（粘贴大量命令时 shell 忙于执行可能丢字节）。
    let dropped = crate::uart_take_dropped();
    if dropped > 0 {
        crate::uart_send_str("[shell] 警告：");
        crate::print_dec(dropped as u64);
        crate::uart_send_str(" 个输入字节被丢弃（UART rx 溢出）\n");
    }
    // 转义序列状态机：0=普通，1=已见 ESC，2=已见 ESC[。
    let mut esc = 0u8;
    loop {
        if let Some(c) = crate::tty::shell_input() {
            match c {
                b'\n' | b'\r' => {
                    // 回车：光标不在行尾时先擦除尾部（纯显示），随后换行提交。
                    if cur < line.len {
                        crate::uart_send_str("\x1b[0K");
                    }
                    crate::uart_send_str("\r\n");
                    history_push(&line);
                    return line;
                }
                0x1b => esc = 1,
                b'[' if esc == 1 => esc = 2,
                b'C' if esc == 2 => {
                    // 右箭头：光标右移一位。
                    esc = 0;
                    if cur < line.len {
                        cur += 1;
                        crate::uart_send_str("\x1b[C");
                    }
                }
                b'D' if esc == 2 => {
                    // 左箭头：光标左移一位。
                    esc = 0;
                    if cur > 0 {
                        cur -= 1;
                        crate::uart_send_str("\x1b[D");
                    }
                }
                b'A' if esc == 2 => {
                    // 上箭头：浏览更早的历史。
                    esc = 0;
                    unsafe {
                        if HIST_COUNT > 0 && HIST_POS > 0 {
                            HIST_POS -= 1;
                            history_load(HIST_POS, &mut line);
                            cur = line.len;
                        }
                    }
                }
                b'B' if esc == 2 => {
                    // 下箭头：浏览更新的历史（越过最新则回到空行）。
                    esc = 0;
                    unsafe {
                        if HIST_POS < HIST_COUNT {
                            HIST_POS += 1;
                            if HIST_POS == HIST_COUNT {
                                line.buf.fill(0);
                                line.len = 0;
                                crate::uart_send_str("\r\x1b[K");
                            } else {
                                history_load(HIST_POS, &mut line);
                            }
                            cur = line.len;
                        }
                    }
                }
                b'\t' => {
                    // Tab 补全：首个词按 /bin 命令补全，其余按 cwd 路径补全。
                    esc = 0;
                    // 补全语义为"追加到词尾"：先移光标到行尾（同步 GUI 光标）。
                    if cur < line.len {
                        let n = line.len - cur;
                        cur = line.len;
                        for _ in 0..n {
                            crate::uart_send_str("\x1b[C");
                        }
                    }
                    let ws = current_word_start(&line.buf[..line.len]);
                    let prefix = core::str::from_utf8(&line.buf[ws..line.len]).unwrap_or("");
                    let is_first = line.buf[..ws].iter().all(|&c| c == b' ');
                    let mut completion = [0u8; 96];
                    let n = if is_first {
                        // 命令位置：内置命令优先，其次 /bin 可执行。
                        let b = complete_from_builtins(prefix, &mut completion, false);
                        if b > 0 {
                            b
                        } else {
                            complete_from_dir("/bin", prefix, &mut completion, false)
                        }
                    } else {
                        complete_path(prefix, &mut completion, false)
                    };
                    if n == 1 {
                        // 追加缺失部分（前缀后的字符）。
                        let plen = prefix.len();
                        let mut rest_len = 0usize;
                        while plen + rest_len < completion.len()
                            && completion[plen + rest_len] != 0
                            && line.len + rest_len < LINE_MAX - 1
                        {
                            rest_len += 1;
                        }
                        for k in 0..rest_len {
                            let c = completion[plen + k];
                            line.buf[line.len] = c;
                            line.len += 1;
                            crate::uart_send_char(c);
                        }
                    } else if n > 1 {
                        // 多候选：换行列出后重绘。
                        crate::uart_send_str("\n");
                        if is_first {
                            let _ = complete_from_builtins(prefix, &mut completion, true);
                            let _ = complete_from_dir("/bin", prefix, &mut completion, true);
                        } else {
                            let _ = complete_path(prefix, &mut completion, true);
                        }
                        crate::uart_send_str(shell_prompt_str());
                        for &c in &line.buf[..line.len] {
                            crate::uart_send_char(c);
                        }
                    }
                }
                0x7f | 0x08 => {
                    // 退格：删除光标左侧字符（行中删除需要重绘尾部并移回）。
                    esc = 0;
                    if cur > 0 {
                        let mut i = cur;
                        while i < line.len {
                            line.buf[i - 1] = line.buf[i];
                            i += 1;
                        }
                        line.len -= 1;
                        cur -= 1;
                        crate::uart_send_str("\x08");
                        for &ch in &line.buf[cur..line.len] {
                            crate::uart_send_char(ch);
                        }
                        crate::uart_send_str("\x1b[K");
                        let back = line.len - cur;
                        for _ in 0..back {
                            crate::uart_send_str("\x1b[D");
                        }
                    }
                }
                0x01 => {
                    // Ctrl+A：光标到行首。
                    esc = 0;
                    if cur > 0 {
                        for _ in 0..cur {
                            crate::uart_send_str("\x1b[D");
                        }
                        cur = 0;
                    }
                }
                0x05 => {
                    // Ctrl+E：光标到行尾。
                    esc = 0;
                    if cur < line.len {
                        let n = line.len - cur;
                        cur = line.len;
                        for _ in 0..n {
                            crate::uart_send_str("\x1b[C");
                        }
                    }
                }
                0x15 => {
                    // Ctrl+U：删除光标到行尾。
                    esc = 0;
                    if cur < line.len {
                        line.len = cur;
                        crate::uart_send_str("\x1b[0K");
                    }
                }
                0x17 => {
                    // Ctrl+W：删除光标前一个词。
                    esc = 0;
                    if cur > 0 {
                        let mut w = cur;
                        while w > 0 && line.buf[w - 1] == b' ' {
                            w -= 1;
                        }
                        while w > 0 && line.buf[w - 1] != b' ' {
                            w -= 1;
                        }
                        let del = cur - w;
                        let mut i = cur;
                        while i < line.len {
                            line.buf[i - del] = line.buf[i];
                            i += 1;
                        }
                        line.len -= del;
                        if del > 0 {
                            crate::uart_send_str("\x1b[");
                            for _ in 0..del {
                                crate::uart_send_str("\x1b[D");
                            }
                            crate::uart_send_str("\x1b[K");
                            for &ch in &line.buf[w..line.len] {
                                crate::uart_send_char(ch);
                            }
                        }
                        cur = w;
                    }
                }
                0x03 => {
                    // Ctrl+C：丢弃当前行；有前台子进程则发 SIGINT。
                    esc = 0;
                    crate::uart_send_str("^C\r\n");
                    line.buf.fill(0);
                    line.len = 0;
                    cur = 0;
                    if crate::shell::fg_child() != 0 {
                        crate::process::signal_send(crate::shell::fg_child() as u32, 2); // SIGINT
                    }
                    crate::uart_send_str(shell_prompt_str());
                }
                0x04 => {
                    // Ctrl+D：空行 = EOF（有前台子进程发 SIGTERM）；非空行尾忽略。
                    esc = 0;
                    if line.len == 0 {
                        if crate::shell::fg_child() != 0 {
                            crate::uart_send_str("^D\r\n");
                            crate::process::signal_send(crate::shell::fg_child() as u32, 15); // SIGTERM
                            crate::uart_send_str(shell_prompt_str());
                        }
                    }
                }
                0x1a => {
                    // Ctrl+Z：挂起（SIGTSTP）；无前台子进程时按丢弃行处理。
                    esc = 0;
                    crate::uart_send_str("^Z\r\n");
                    if crate::shell::fg_child() != 0 {
                        crate::process::signal_send(crate::shell::fg_child() as u32, 20); // SIGTSTP
                        crate::uart_send_str(shell_prompt_str());
                    }
                    line.buf.fill(0);
                    line.len = 0;
                    cur = 0;
                    crate::uart_send_str(shell_prompt_str());
                }
                _ => {
                    // 普通字符：在光标处插入（行中插入需重绘尾部并移回）。
                    esc = 0;
                    if line.len < LINE_MAX - 1 {
                        let mut i = line.len;
                        while i > cur {
                            line.buf[i] = line.buf[i - 1];
                            i -= 1;
                        }
                        line.buf[cur] = c;
                        line.len += 1;
                        let after = line.len - (cur + 1);
                        crate::uart_send_char(c);
                        for &ch in &line.buf[cur + 1..line.len] {
                            crate::uart_send_char(ch);
                        }
                        if after > 0 {
                            for _ in 0..after {
                                crate::uart_send_str("\x1b[D");
                            }
                        }
                        cur += 1;
                    }
                }
            }
        } else {
            thread::yield_now();
        }
    }
}

// ==================== 入口 ====================

/// Shell 进程入口。
pub fn shell_main(_: usize) -> ! {
    set_cwd("/");
    // 默认 HOME：nsmonkey/NetSurf 等按 ~ 解析资源路径的程序依赖（缺失时
    // 解析成 /~/.netsurf/... 的无害但持续的警告）；用户可 export 覆盖。
    env_set("HOME", "/data");
    loop {
        crate::uart_send_str(shell_prompt_str());
        let mut line = read_line();
        if line.len == 0 {
            continue;
        }
        // 支持 ';' 命令分隔符：按 ';' 切分后逐段执行（`a; b`）。
        // 每段首词先做别名展开（alias 表），再交给词法/管道解析。
        let mut seg_start = 0usize;
        let mut i = 0usize;
        while i <= line.len {
            if i == line.len || line.buf[i] == b';' {
                let seg_len = i - seg_start;
                if seg_len > 0 {
                    let expanded = alias_expand(&mut line.buf[seg_start..i], seg_len);
                    let mut tokens = [Token { word: EMPTY_WORD, op: OP_WORD }; MAX_TOK];
                    let mut truncated = false;
                    let tok_count = tokenize(
                        &line.buf[seg_start..seg_start + expanded],
                        expanded,
                        &mut tokens,
                        &mut truncated,
                    );
                    let pipe = parse(&tokens, tok_count, &mut truncated);
                    if truncated {
                        // 词/参数/管道段被截断：命令已残缺，拒绝执行并告警
                        crate::uart_send_str("[shell] 警告：命令过长，已截断；未执行\n");
                    } else {
                        run_pipeline(&pipe);
                    }
                }
                seg_start = i + 1;
            }
            i += 1;
        }
    }
}

// ==================== 开发者工具命令（top/mount/ifconfig/log/crash/perf） ====================

/// `top [-n 1]`：任务 CPU 用量列表（按累计调度 tick 降序）。
/// 默认每秒刷新（终端清屏重绘）；`-n 1` 单次输出。
fn cmd_top(cmd: &Command, sink: &mut Sink) {
    let mut once = false;
    for i in 1..cmd.argc {
        if word_str(&cmd.argv[i]) == "-n" {
            once = true;
        }
    }
    #[derive(Clone, Copy)]
    struct Row {
        pid: u32,
        tid: u32,
        name: [u8; 16],
        st: u8,
        nice: i32,
        ticks: u64,
        cap: u16,
    }
    static mut ROWS: [Row; 64] = [Row { pid: 0, tid: 0, name: [0; 16], st: 0, nice: 0, ticks: 0, cap: 0 }; 64];
    loop {
        unsafe {
            let mut n = 0usize;
            crate::sched_cfs::for_each_task_full(|pid, tid, st, name, nice, ticks, cap| {
                if n < ROWS.len() {
                    let mut nm = [0u8; 16];
                    let l = name.len().min(15);
                    nm[..l].copy_from_slice(&name.as_bytes()[..l]);
                    ROWS[n] = Row {
                        pid,
                        tid,
                        name: nm,
                        st: st.as_bytes()[0],
                        nice,
                        ticks,
                        cap,
                    };
                    n += 1;
                }
            });
            // 按 ticks 降序插入排序
            for i in 1..n {
                let key = ROWS[i];
                let mut j = i;
                while j > 0 && ROWS[j - 1].ticks < key.ticks {
                    ROWS[j] = ROWS[j - 1];
                    j -= 1;
                }
                ROWS[j] = key;
            }
            if once {
                sink.emit_str("PID  线程  ST 名称             优先级  滴答  配额\n");
            } else {
                sink.emit_str("\x1b[H\x1b[2J");
                sink.emit_str("KunxOS top（");
                sink.emit_u32((crate::timer::now_us() / 1_000_000) as u32);
                sink.emit_str(" 秒） FPS=");
                sink.emit_u32(crate::perf::fps());
                sink.emit_str(" CPU=");
                sink.emit_u32(crate::perf::cpu_pct());
                sink.emit_str("% MEM=");
                sink.emit_u32(crate::perf::mem_pct());
                sink.emit_str("%\n");
                sink.emit_str("PID  线程  ST 名称             优先级  滴答  配额\n");
            }
            for i in 0..n {
                let r = &ROWS[i];
                sink.emit_u32(r.pid);
                sink.emit_str("  ");
                sink.emit_u32(r.tid);
                sink.emit_str("  ");
                sink.emit(&[r.st]);
                sink.emit_str("  ");
                let nl = r.name.iter().position(|&c| c == 0).unwrap_or(r.name.len());
                let name = core::str::from_utf8(&r.name[..nl]).unwrap_or("?");
                sink.emit_str(name);
                let pad = 16usize.saturating_sub(nl);
                for _ in 0..pad {
                    sink.emit_str(" ");
                }
                sink.emit_i32(r.nice);
                sink.emit_str("  ");
                let mut tb = [0u8; 12];
                let mut ti = tb.len();
                let mut v = r.ticks;
                loop {
                    ti -= 1;
                    tb[ti] = b'0' + (v % 10) as u8;
                    v /= 10;
                    if v == 0 {
                        break;
                    }
                }
                sink.emit(&tb[ti..]);
                sink.emit_str("  ");
                sink.emit_u32(r.cap as u32);
                sink.emit_str("\n");
            }
            sink.emit_str("任务数：");
            sink.emit_u32(crate::sched_cfs::task_count() as u32);
            sink.emit_str("\n");
        }
        if once {
            break;
        }
        sink.flush_uart();
        crate::sched_cfs::sleep_until(crate::timer::now_us() + 1_000_000);
    }
}

/// `mount`：列出挂载点与块设备；`mount <dev> <point>` 尽力挂载。
fn cmd_mount(cmd: &Command, sink: &mut Sink) {
    if cmd.argc >= 3 {
        let dev = word_str(&cmd.argv[1]);
        let point = word_str(&cmd.argv[2]);
        // 支持 /dev/sda3 与 sda3 两种写法
        let name = dev.strip_prefix("/dev/").unwrap_or(dev);
        let major = match crate::blkdev::find_by_name(name) {
            Some(m) => m,
            None => {
                sink.emit_str("mount: 无此块设备：");
                sink.emit_str(dev);
                sink.emit_str("\n");
                return;
            }
        };
        // vfs::mount 需要 &'static str（挂载表静态存储）。挂载点字符串必须
        // 每个挂载一份独立缓冲：共用单缓冲时第二次 mount 会原地改写第一次
        // 挂载点的字符串内容，挂载表里出现两个相同前缀，find_mount 把
        // /data/... 之类路径路由到错误设备。缓冲池按 VFS 挂载表容量分配。
        static mut MOUNT_POINT_POOL: [[u8; 64]; crate::vfs::MAX_MOUNT] =
            [[0; 64]; crate::vfs::MAX_MOUNT];
        static MOUNT_POINT_USED: core::sync::atomic::AtomicUsize =
            core::sync::atomic::AtomicUsize::new(0);
        let pl = point.len().min(63);
        let slot = MOUNT_POINT_USED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if slot >= crate::vfs::MAX_MOUNT {
            sink.emit_str("mount: 挂载点缓冲池耗尽\n");
            return;
        }
        unsafe {
            MOUNT_POINT_POOL[slot][..pl].copy_from_slice(&point.as_bytes()[..pl]);
            MOUNT_POINT_POOL[slot][pl] = 0;
        }
        // 安全 UTF-8 校验，非法字节降级为空串，避免 from_utf8_unchecked 的 UB
        let point_static =
            unsafe { core::str::from_utf8(&MOUNT_POINT_POOL[slot][..pl]).unwrap_or("") };
        let r = crate::vfs::mount(major, point_static);
        if r == 0 {
            sink.emit_str("mount: ");
            sink.emit_str(dev);
            sink.emit_str(" -> ");
            sink.emit_str(point);
            sink.emit_str(" 成功\n");
        } else {
            sink.emit_str("mount: 失败（错误 ");
            sink.emit_i32(r);
            sink.emit_str("）\n");
        }
        return;
    }
    // 列出挂载点
    sink.emit_str("挂载点列表：\n");
    crate::vfs::for_each_mount(&mut |point, dev| {
        sink.emit_str("  ");
        sink.emit_str(point);
        sink.emit_str("  设备主号=");
        sink.emit_u32(dev);
        sink.emit_str("\n");
    });
    // 列出块设备
    sink.emit_str("块设备列表：\n");
    crate::blkdev::for_each_blkdev(|major, name, sectors| {
        sink.emit_str("  /dev/");
        sink.emit_str(name);
        sink.emit_str("  主号=");
        sink.emit_u32(major);
        sink.emit_str("  扇区数=");
        sink.emit_u32(sectors as u32);
        sink.emit_str("\n");
    });
}

/// `ifconfig [eth0]`：显示网络接口配置（静态 IP，无 DHCP）。
fn cmd_ifconfig(cmd: &Command, sink: &mut Sink) {
    let _ = cmd;
    let ip = crate::net_stack::local_ip();
    if ip == 0 {
        sink.emit_str("ifconfig: 网络未初始化\n");
        return;
    }
    sink.emit_str("eth0: flags=4163<UP,BROADCAST,RUNNING,MULTICAST>\n");
    let b = ip.to_be_bytes();
    sink.emit_str("        inet ");
    sink.emit_u32(b[0] as u32);
    sink.emit_str(".");
    sink.emit_u32(b[1] as u32);
    sink.emit_str(".");
    sink.emit_u32(b[2] as u32);
    sink.emit_str(".");
    sink.emit_u32(b[3] as u32);
    sink.emit_str("  netmask ");
    let m = crate::net_stack::netmask().to_be_bytes();
    sink.emit_u32(m[0] as u32);
    sink.emit_str(".");
    sink.emit_u32(m[1] as u32);
    sink.emit_str(".");
    sink.emit_u32(m[2] as u32);
    sink.emit_str(".");
    sink.emit_u32(m[3] as u32);
    sink.emit_str("  gateway ");
    let g = crate::net_stack::gateway().to_be_bytes();
    sink.emit_u32(g[0] as u32);
    sink.emit_str(".");
    sink.emit_u32(g[1] as u32);
    sink.emit_str(".");
    sink.emit_u32(g[2] as u32);
    sink.emit_str(".");
    sink.emit_u32(g[3] as u32);
    sink.emit_str("\n");
    sink.emit_str("        dns ");
    let d = crate::net_stack::dns_ip().to_be_bytes();
    sink.emit_u32(d[0] as u32);
    sink.emit_str(".");
    sink.emit_u32(d[1] as u32);
    sink.emit_str(".");
    sink.emit_u32(d[2] as u32);
    sink.emit_str(".");
    sink.emit_u32(d[3] as u32);
    sink.emit_str("\n");
    sink.emit_str("        mac ");
    let mac = crate::net_virtio::mac();
    for i in 0..6 {
        if i > 0 {
            sink.emit_str(":");
        }
        let hi = mac[i] >> 4;
        let lo = mac[i] & 0xF;
        sink.emit(&[if hi < 10 { b'0' + hi } else { b'a' + hi - 10 }]);
        sink.emit(&[if lo < 10 { b'0' + lo } else { b'a' + lo - 10 }]);
    }
    sink.emit_str("\n");
}

/// `log [-l <level>] [tag]`：读取结构化内核/用户日志（logcat 等价物）。
/// level 为 0-7（默认全部）；tag 过滤模块标签（子串匹配）。
fn cmd_log(cmd: &Command, sink: &mut Sink) {
    let mut min_level = 0u8;
    let mut tag_filter: [u8; 16] = [0; 16];
    let mut tag_len = 0usize;
    let mut i = 1usize;
    while i < cmd.argc {
        let a = word_str(&cmd.argv[i]);
        if a == "-l" && i + 1 < cmd.argc {
            if let Ok(v) = word_str(&cmd.argv[i + 1]).parse::<u8>() {
                min_level = v.min(7);
            }
            i += 2;
        } else if a.starts_with('-') && a.len() > 1 {
            i += 1;
        } else {
            let l = a.len().min(15);
            tag_filter[..l].copy_from_slice(&a.as_bytes()[..l]);
            tag_len = l;
            i += 1;
        }
    }
    // 读整个快照到静态缓冲
    static mut LOG_BUF: [u8; 8192] = [0; 8192];
    let n = unsafe { crate::log::log_snapshot(&mut LOG_BUF) };
    let s = unsafe { core::str::from_utf8(&LOG_BUF[..n]).unwrap_or("") };
    // 按行过滤
    let mut printed = 0u32;
    for line in s.lines() {
        // 行格式：<level> <ts> <tag>: <msg>
        let lv = line.as_bytes().get(1).copied().map(|c| c - b'0').unwrap_or(0);
        if lv < min_level {
            continue;
        }
        if tag_len > 0 {
            let has_tag = line.contains(core::str::from_utf8(&tag_filter[..tag_len]).unwrap_or(""));
            if !has_tag {
                continue;
            }
        }
        sink.emit_str(line);
        sink.emit_str("\n");
        printed += 1;
        if printed >= 200 {
            sink.emit_str("[log] 已截断，仅显示 200 行\n");
            break;
        }
    }
    if printed == 0 {
        sink.emit_str("[log] 无匹配记录\n");
    }
}

/// `health`：查看系统健康事件（故障告知 + 修复指引）；`health clear` 清空。
fn cmd_health(cmd: &Command, sink: &mut Sink) {
    let sub = if cmd.argc > 1 { word_str(&cmd.argv[1]) } else { "" };
    if sub == "clear" {
        crate::health::clear();
        sink.emit_str("[health] 已清空健康事件\n");
        return;
    }
    let mut n = 0usize;
    crate::health::for_each(&mut |t, sev, subsys, msg, fix| {
        let sev_name = match sev {
            crate::health::SEV_CRITICAL => "严重",
            crate::health::SEV_WARN => "警告",
            _ => "提示",
        };
        sink.emit_str("[");
        // 手工拼时间戳（sink 无 fmt 支持）
        let mut tb = [0u8; 12];
        let mut i = tb.len();
        let mut v = t;
        loop {
            i -= 1;
            tb[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let _ = core::str::from_utf8(&tb[i..]).map(|s| sink.emit_str(s));
        sink.emit_str("ms][");
        sink.emit_str(sev_name);
        sink.emit_str("][");
        sink.emit_str(subsys);
        sink.emit_str("] ");
        sink.emit_str(msg);
        sink.emit_str("\n");
        if !fix.is_empty() {
            sink.emit_str("    修复建议：");
            sink.emit_str(fix);
            sink.emit_str("\n");
        }
        n += 1;
    });
    if n == 0 {
        sink.emit_str("[health] 无健康事件（系统正常）\n");
    } else {
        sink.emit_str("[health] 共 ");
        // 复用 emit_dec 若存在则更好；此处逐位输出
        let mut pb = [0u8; 8];
        let mut i = pb.len();
        let mut v = n;
        loop {
            i -= 1;
            pb[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let _ = core::str::from_utf8(&pb[i..]).map(|s| sink.emit_str(s));
        sink.emit_str(" 条（health clear 清除）\n");
    }
}

/// `crash`：列出崩溃报告；`crash show <file>` 显示内容；`crash clear` 清空。
fn cmd_crash(cmd: &Command, sink: &mut Sink) {
    let dir = if crate::vfs::stat_path("/data").is_some() {
        "/data/crash"
    } else {
        "/tmp/crash"
    };
    let sub = if cmd.argc > 1 { word_str(&cmd.argv[1]) } else { "" };
    if sub == "clear" {
        let mut name = [0u8; 32];
        let mut idx = 0usize;
        let mut removed = 0u32;
        loop {
            let ino = crate::vfs::readdir(dir, idx, &mut name);
            if ino < 0 {
                break;
            }
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let nm = core::str::from_utf8(&name[..len]).unwrap_or("");
            if nm == "." || nm == ".." || nm.is_empty() {
                idx += 1;
                continue;
            }
            let mut p = [0u8; 96];
            let mut n = 0usize;
            for &c in dir.as_bytes() {
                if n + 1 >= p.len() {
                    break;
                }
                p[n] = c;
                n += 1;
            }
            if p[n - 1] != b'/' && n + 1 < p.len() {
                p[n] = b'/';
                n += 1;
            }
            for &c in nm.as_bytes() {
                if n + 1 >= p.len() {
                    break;
                }
                p[n] = c;
                n += 1;
            }
            let del = crate::vfs::unlink(core::str::from_utf8(&p[..n]).unwrap_or(""));
            // 删除成功后目录条目整体前移：不递增 idx，同位重读（原实现每次
            // 都 idx+=1，隔一条删一条，N 个文件只删掉约一半）。
            // 删除失败才前进，避免同一失败条目死循环。
            if del == 0 {
                removed += 1;
            } else {
                idx += 1;
            }
        }
        sink.emit_str("crash: 已删除 ");
        sink.emit_u32(removed);
        sink.emit_str(" 个墓碑记录\n");
        return;
    }
    if sub.starts_with("show") && cmd.argc > 2 {
        let fname = word_str(&cmd.argv[2]);
        let mut path = [0u8; 96];
        let mut n = 0usize;
        for &c in dir.as_bytes() {
            if n + 1 >= path.len() {
                break;
            }
            path[n] = c;
            n += 1;
        }
        if path[n - 1] != b'/' && n + 1 < path.len() {
            path[n] = b'/';
            n += 1;
        }
        for &c in fname.as_bytes() {
            if n + 1 >= path.len() {
                break;
            }
            path[n] = c;
            n += 1;
        }
        let p = core::str::from_utf8(&path[..n]).unwrap_or("");
        let fd = crate::vfs::open(p, crate::vfs::O_RDONLY);
        if fd < 0 {
            sink.emit_str("crash: 无此文件：");
            sink.emit_str(fname);
            sink.emit_str("\n");
            return;
        }
        static mut CRASH_BUF: [u8; 8192] = [0; 8192];
        let mut total = 0usize;
        loop {
            let r = crate::vfs::read(fd, &mut unsafe { CRASH_BUF });
            if r <= 0 {
                break;
            }
            let s = unsafe { core::str::from_utf8(&CRASH_BUF[..r as usize]).unwrap_or("") };
            sink.emit_str(s);
            total += r as usize;
            if total > 60000 {
                sink.emit_str("\n[crash] 已截断\n");
                break;
            }
        }
        crate::vfs::close(fd);
        return;
    }
    // 列表
    sink.emit_str("崩溃报告位于 ");
    sink.emit_str(dir);
    sink.emit_str("：\n");
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    let mut count = 0u32;
    loop {
        let ino = crate::vfs::readdir(dir, idx, &mut name);
        if ino < 0 {
            break;
        }
        idx += 1;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let nm = core::str::from_utf8(&name[..len]).unwrap_or("");
        if nm == "." || nm == ".." || nm.is_empty() {
            continue;
        }
        sink.emit_str("  ");
        sink.emit_str(nm);
        sink.emit_str("\n");
        count += 1;
    }
    if count == 0 {
        sink.emit_str("  （无）\n");
    }
    sink.emit_str("用法：crash [show <file> | clear]\n");
}

/// `perf [on|off|stats]`：性能 overlay 与统计。
/// Sink 输出 u64（十进制；Sink 仅内建 emit_u32）。
fn emit_u64(sink: &mut Sink, v: u64) {
    if v == 0 {
        sink.emit_str("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = v;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    sink.emit(&buf[i..]);
}

/// Sink 输出置位位列表（如 act0=0x5 → "0,2"；定位 SGI/PPI 活动源）。
fn emit_set_bits(sink: &mut Sink, v: u32) {
    let mut any = false;
    for b in 0..32 {
        if v & (1 << b) != 0 {
            if any {
                sink.emit_str(",");
            }
            sink.emit_u32(b);
            any = true;
        }
    }
    if !any {
        sink.emit_str("-");
    }
}

/// `irqstat`：中断观测点（技术债：中断风暴 act0 待复现）。
///   - 每 IRQ 分发计数：定位高频源 / 风暴贡献者的常态分布；
///   - 风暴 episode 历史：转储阈值（2000/20000/200000 次重入）时刻的
///     GIC 状态快照（含 act0 及其置位位解码），事后回溯不必盯串口抓现场。
fn cmd_irqstat(_cmd: &Command, sink: &mut Sink) {
    sink.emit_str("IRQ 分发计数（irq 次数）：\n");
    crate::interrupt::for_each_irq_count(&mut |irq, n| {
        sink.emit_str("  ");
        sink.emit_u32(irq);
        sink.emit_str("  ");
        emit_u64(sink, n);
        sink.emit_str("\n");
    });
    sink.emit_str("风暴 episode（新→旧；act0/pend0 bit N = irq N）：\n");
    let mut eps = 0u32;
    crate::interrupt::for_each_storm_ep(&mut |e| {
        eps += 1;
        sink.emit_str("  irq=");
        sink.emit_u32(e.irq);
        sink.emit_str(" 重入=");
        emit_u64(sink, e.n);
        sink.emit_str(" t=");
        emit_u64(sink, e.t_us);
        sink.emit_str("us pend0=");
        sink.emit_u32(e.pend0);
        sink.emit_str("(");
        emit_set_bits(sink, e.pend0);
        sink.emit_str(") pend1=");
        sink.emit_u32(e.pend1);
        sink.emit_str(" act0=");
        sink.emit_u32(e.act0);
        sink.emit_str("(");
        emit_set_bits(sink, e.act0);
        sink.emit_str(") act1=");
        sink.emit_u32(e.act1);
        sink.emit_str(" rpr=");
        sink.emit_u32(e.rpr);
        sink.emit_str(" pid=");
        emit_u64(sink, e.pid as u64);
        sink.emit_str("\n");
    });
    if eps == 0 {
        sink.emit_str("  （无：尚未触发转储阈值，连续同号重入 2000 次）\n");
    }
}

/// `netdiag`：virtio-net DMA 缓存行（64B）隔离审计（技术债：DMA 64B 缓存行
/// 隔离，Pi4/KVM）。输出环布局与 DMA 缓冲对齐的运行时事实；「共享行」为真
/// 即真实缓存平台上缓存维护越界覆盖的前提（TCG 无缓存仿真不可复现，诊断
/// 在任何平台确定性可得）。
fn cmd_netdiag(_cmd: &Command, sink: &mut Sink) {
    let d = crate::net_virtio::cache_line_diag();
    if !d.ready {
        sink.emit_str("netdiag: 网卡未就绪（无 virtio-net 设备）\n");
        return;
    }
    sink.emit_str("virtio-net DMA 缓存行(64B)隔离审计：\n");
    sink.emit_str("  RX 环 ");
    emit_u64(sink, d.rx_ring as u64);
    sink.emit_str(" TX 环 ");
    emit_u64(sink, d.tx_ring as u64);
    sink.emit_str("（页对齐");
    sink.emit_str(if d.rx_ring & 63 == 0 && d.tx_ring & 63 == 0 { "，独立成行）\n" } else { "，[风险] 非 64B 对齐）\n" });
    sink.emit_str("  used_off=");
    sink.emit_u32(d.used_off as u32);
    sink.emit_str(" %64=");
    sink.emit_u32((d.used_off & 63) as u32);
    if d.used_line_shared {
        sink.emit_str("  [风险] used 头与 avail 尾共享行");
        if d.avail_clean_overreach {
            sink.emit_str("，且 clean(avail) 行取整越过 avail 末尾：回写可把 CPU 陈旧 used 头字节覆盖设备 DMA 写入（真机/KVM）\n");
        } else {
            sink.emit_str("\n");
        }
    } else {
        sink.emit_str("  [安全] used 头独立成行\n");
    }
    sink.emit_str("  RX DMA 缓冲 64B 对齐：");
    sink.emit_u32(d.rx_buf_aligned as u32);
    sink.emit_str("/");
    sink.emit_u32(d.rx_buf_total as u32);
    if d.rx_buf_aligned < d.rx_buf_total {
        sink.emit_str("  [风险] 未对齐缓冲 invalidate 可丢弃共享行上相邻所有者脏数据（堆损坏，真机/KVM）\n");
    } else {
        sink.emit_str("  [安全] 全部独占行\n");
    }
    sink.emit_str("  TX DMA 缓冲 64B 对齐：");
    sink.emit_u32(d.tx_buf_aligned as u32);
    sink.emit_str("/");
    sink.emit_u32(d.tx_buf_total as u32);
    if d.tx_buf_aligned < d.tx_buf_total {
        sink.emit_str("  [提示] TX 仅 clean（回写自身数据不丢弃），共享行无数据丢失风险\n");
    } else {
        sink.emit_str("  [安全] 全部独占行\n");
    }
}

fn cmd_perf(cmd: &Command, sink: &mut Sink) {
    let sub = if cmd.argc > 1 { word_str(&cmd.argv[1]) } else { "stats" };
    match sub {
        "on" => {
            crate::perf::set_overlay(true);
            sink.emit_str("perf: 叠加层已启用\n");
        }
        "off" => {
            crate::perf::set_overlay(false);
            sink.emit_str("perf: 叠加层已禁用\n");
        }
        "stats" | "" => {
            sink.emit_str("FPS: ");
            sink.emit_u32(crate::perf::fps());
            sink.emit_str("\nCPU: ");
            sink.emit_u32(crate::perf::cpu_pct());
            sink.emit_str("%\nMEM: ");
            sink.emit_u32(crate::perf::mem_pct());
            sink.emit_str("%（已用 ");
            sink.emit_u32(crate::perf::mem_used_kb());
            sink.emit_str(" kB）\n总帧数: ");
            let mut tb = [0u8; 16];
            let mut ti = tb.len();
            let mut v = crate::perf::total_frames();
            loop {
                ti -= 1;
                tb[ti] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            sink.emit(&tb[ti..]);
            sink.emit_str("\n叠加层: ");
            sink.emit_str(if crate::perf::overlay_enabled() { "开" } else { "关" });
            sink.emit_str("\n");
        }
        _ => {
            sink.emit_str("用法：perf [on|off|stats]\n");
        }
    }
}
