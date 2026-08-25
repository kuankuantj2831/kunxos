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
const ARG_LEN: usize = 32;
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
    fn emit(&mut self, data: &[u8]) {
        if self.out_file >= 0 {
            vfs::write(self.out_file, data);
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

fn tokenize(line: &[u8], len: usize, tokens: &mut [Token; MAX_TOK]) -> usize {
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
                    }
                    i += 1;
                }
                tokens[n] = Token { word: w, op: OP_WORD };
                n += 1;
            }
        }
    }
    n
}

// ==================== 语法分析 ====================

fn parse(tokens: &[Token], tok_count: usize) -> Pipeline {
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
                }
            }
            OP_PIPE => {
                if pipe.n < MAX_PIPE - 1 {
                    pipe.n += 1;
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
        if out_file >= 0 && cmd.append {
            vfs::seek(out_file, 0, vfs::SEEK_END);
        }
    }

    let redirected = out_file >= 0;
    let mut sink = Sink { output, out_len: 0, out_file, truncated: false };
    run_builtin(cmd, stdin_data, &mut sink);

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
                crate::uart_send_str("\n[shell] warning: output truncated (limit ");
                crate::print_dec(OUT_MAX as u64);
                crate::uart_send_str(" bytes)\n");
            }
            input_len = 0;
        } else {
            unsafe {
                INPUT_BUF[..out_len].copy_from_slice(&output[..out_len]);
            }
            input_len = out_len;
            if truncated {
                crate::uart_send_str("[shell] warning: pipeline stage output truncated\n");
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
                sink.emit_str("cd: no previous directory\n");
            }
        }
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    if path.is_empty() {
        sink.emit_str("cd: path too long\n");
        return;
    }
    let mut dev = 0u32;
    let mut ino = 0u32;
    if vfs::resolve(path, &mut dev, &mut ino) == 0 {
        // 目标必须是目录（此前只查存在性，`cd 普通文件` 会"成功"）
        if vfs::stat_path(path).map_or(true, |(t, _)| t != crate::fs_simple::T_DIR) {
            sink.emit_str("cd: not a directory: ");
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
        sink.emit_str("cd: no such directory: ");
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
                sink.emit_u32(size);
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
        sink.emit_str("ls: cannot access: ");
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
                        sink.emit_u32(size);
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
            sink.emit_str("cat: no such file: ");
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

fn cmd_run(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: run <elf> [args...]\n");
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    let mut argv: [&str; MAX_ARGC] = [""; MAX_ARGC];
    let mut n = 0usize;
    for i in 1..cmd.argc {
        argv[n] = word_str(&cmd.argv[i]);
        n += 1;
    }
    let pid = elf::spawn_from_path(path, &argv[..n]);
    if pid < 0 {
        sink.emit_str("run: failed to load: ");
        sink.emit_str(path);
        sink.emit_str("\n");
        return;
    }
    let (rpid, code) = process::wait(pid);
    elf::release(pid as u32);
    if rpid > 0 {
        sink.emit_str("[run] pid ");
        sink.emit_u32(rpid as u32);
        sink.emit_str(" exited code=");
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
    let pid = elf::spawn_from_path("/bin/busybox", &argv[..n]);
    if pid < 0 {
        sink.emit_str("busybox: not found\n");
        return;
    }
    let _ = process::wait(pid);
    elf::release(pid as u32);
}

fn cmd_pkg(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: pkg <install|uninstall|list>\n");
        return;
    }
    let sub = word_str(&cmd.argv[1]);
    match sub {
        "install" => {
            if cmd.argc < 3 {
                sink.emit_str("usage: pkg install <file>\n");
                return;
            }
            let path = resolve_path(word_str(&cmd.argv[2]));
            let mut buf = [0u8; 16384];
            let total = read_all(path, &mut buf);
            if total == 0 {
                sink.emit_str("pkg: cannot read package\n");
                return;
            }
            match pkg::parse(&buf[..total]) {
                Ok(p) => {
                    if pkg::install(&p, &buf[..total]) == 0 {
                        sink.emit_str("pkg: installed\n");
                    } else {
                        sink.emit_str("pkg: install failed (signature mismatch?)\n");
                    }
                }
                Err(_) => sink.emit_str("pkg: invalid package\n"),
            }
        }
        "uninstall" => {
            if cmd.argc < 3 {
                sink.emit_str("usage: pkg uninstall <name>\n");
                return;
            }
            pkg::uninstall(word_str(&cmd.argv[2]));
            sink.emit_str("pkg: uninstalled\n");
        }
        "list" => pkg::list_to(&mut |s| sink.emit_str(s)),
        _ => sink.emit_str("pkg: unknown subcommand\n"),
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

/// `uptime`：系统运行时间（秒）。
fn cmd_uptime(_cmd: &Command, sink: &mut Sink) {
    let us = crate::timer::now_us();
    sink.emit_str("up ");
    sink.emit_u32((us / 1_000_000) as u32);
    sink.emit_str(" seconds\n");
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
        sink.emit_str("up ");
        sink.emit_u32(days as u32);
        sink.emit_str(" days, ");
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
        sink.emit_str("usage: which <cmd>\n");
        return;
    }
    for i in 1..cmd.argc {
        let name = word_str(&cmd.argv[i]);
        if BUILTINS.iter().any(|b| *b == name) {
            sink.emit_str(name);
            sink.emit_str(": shell builtin\n");
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
            sink.emit_str(": not found\n");
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
        sink.emit_str("usage: basename <path>
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
        sink.emit_str("usage: dirname <path>\n");
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

/// `dmesg`：显示内核消息缓冲（启动日志等，环形 16KB，覆盖最旧内容）。
fn cmd_dmesg(cmd: &Command, sink: &mut Sink) {
    let mut tail_lines = 0usize;
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
        sink.emit_str("usage: od <file> [-n <bytes>]\n");
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
        sink.emit_str("od: no such file: ");
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
        sink.emit_str("usage: tee <file>\n");
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
        sink.emit_str("tee: cannot open: ");
        sink.emit_str(path);
        sink.emit_str("\n");
    }
    // 无论写文件成败都透传输入到 stdout
    sink.emit(input);
}

/// `stat <path...>`：显示文件类型与大小（SFS 元数据）。
fn cmd_stat(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: stat <path...>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        match vfs::lstat_path(path) {
            Some((typ, size)) => {
                let name = match typ {
                    1 => "regular",
                    2 => "directory",
                    3 => "char-device",
                    4 => "block-device",
                    5 => "fifo",
                    6 => "symlink",
                    _ => "unknown",
                };
                sink.emit_str(path);
                sink.emit_str(": ");
                sink.emit_str(name);
                sink.emit_str(" size=");
                sink.emit_u32(size);
                sink.emit_str("\n");
            }
            None => {
                sink.emit_str(path);
                sink.emit_str(": not found\n");
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
        "builtins: cd echo export exit pwd ls cat run busybox pkg ps uname clear mkdir rm touch cp mv wc head tail grep df history uptime ping sysinfo calc sort netstat tcp_echo udp_echo http_serve http_get gdb nslookup poweroff reboot kill sleep date which uniq sha1sum stat dmesg od tee true false basename dirname help\n",
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
        sink.emit_str("usage: grep [-n] <pattern> [file...]\n");
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
            sink.emit_str("grep: no such file: ");
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
    sink.emit_str("DEV   MOUNT       BLOCKS  USED  FREE\n");
    let s = &mut *sink;
    vfs::for_each_mount(&mut |mp: &str, dev: u32| {
        s.emit_str("sfs");
        s.emit_u32(dev);
        // 挂载点补空格对齐
        s.emit_str("   ");
        s.emit_str(mp);
        let pad = 11usize.saturating_sub(mp.len());
        for _ in 0..pad {
            s.emit_str(" ");
        }
        let total = crate::fs_simple::fs_total_blocks(dev);
        let free = crate::fs_simple::fs_free_blocks(dev);
        s.emit_u32(total);
        s.emit_str("  ");
        s.emit_u32(total - free);
        s.emit_str("  ");
        s.emit_u32(free);
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
        sink.emit_str("usage: http_get <host> <port> <path>\n");
        return;
    }
    let host = word_str(&cmd.argv[1]);
    let port = match word_str(&cmd.argv[2]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("http_get: invalid port\n");
            return;
        }
    };
    let path = word_str(&cmd.argv[3]);
    let ip = match parse_ipv4(host.as_bytes()) {
        Some(i) => i,
        None => {
            sink.emit_str("http_get: only dotted IPv4 hosts supported\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("http_get: network not initialized\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 {
        sink.emit_str("http_get: socket failed\n");
        return;
    }
    sink.emit_str("http_get: connecting ");
    sink.emit_str(host);
    sink.emit_str(":");
    sink.emit_u32(port as u32);
    sink.emit_str("\n");
    sink.flush_uart();
    if crate::socket::connect(h, ip, port) != 0 {
        sink.emit_str("http_get: connect failed\n");
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
        sink.emit_str("http_get: send failed\n");
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
        sink.emit_str("[http_get] body (");
        sink.emit_u32((total - body_start) as u32);
        sink.emit_str(" bytes):\n");
        unsafe {
            sink.emit(&BODY_BUF[body_start..total]);
        }
    } else {
        sink.emit_str("[http_get] no response body (");
        sink.emit_u32(total as u32);
        sink.emit_str(" bytes received)\n");
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
    sink.emit_str("gdb: entering debug loop (host: target remote)
");
    sink.flush_uart();
    crate::gdbstub::gdb_break();
    sink.emit_str("gdb: resumed
");
}

fn cmd_tcp_echo(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: tcp_echo <port>\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("tcp_echo: invalid port\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("tcp_echo: network not initialized\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 || crate::socket::bind(h, port) != 0 || crate::socket::listen(h, 4) != 0 {
        sink.emit_str("tcp_echo: listen failed\n");
        return;
    }
    sink.emit_str("tcp_echo: listening on port ");
    sink.emit_u32(port as u32);
    sink.emit_str("\n");
    sink.flush_uart();
    loop {
        let c = crate::socket::accept(h);
        if c < 0 {
            sink.emit_str("tcp_echo: accept failed\n");
            break;
        }
        sink.emit_str("tcp_echo: accepted, echoing\n");
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
        sink.emit_str("tcp_echo: connection closed\n");
    }
}

/// `http_serve <port> [root_dir]`：极简 HTTP 服务器。
/// 有 root_dir 时提供目录下静态文件（`GET /path` → root_dir/path，含
/// 基础 MIME 推断与 404）；无 root_dir 时返回内置欢迎页。
/// 配合 QEMU `hostfwd` 可从宿主机用 curl/浏览器访问。
fn cmd_http_serve(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: http_serve <port> [root_dir]\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("http_serve: invalid port\n");
            return;
        }
    };
    let root = if cmd.argc > 2 { resolve_path(word_str(&cmd.argv[2])) } else { "" };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("http_serve: network not initialized\n");
        return;
    }
    let h = crate::socket::socket_tcp();
    if h < 0 || crate::socket::bind(h, port) != 0 || crate::socket::listen(h, 4) != 0 {
        sink.emit_str("http_serve: listen failed\n");
        return;
    }
    sink.emit_str("http_serve: listening on port ");
    sink.emit_u32(port as u32);
    if !root.is_empty() {
        sink.emit_str(" root=");
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
            let fpath = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(full.as_ptr(), off))
            };
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
    sink.emit_str("FD  PROTO  STATE     LPORT  REMOTE\n");
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
        sink.emit_str("usage: nslookup <hostname>\n");
        return;
    }
    let name = word_str(&cmd.argv[1]);
    if crate::net_stack::local_ip() == 0 || crate::net_stack::dns_ip() == 0 {
        sink.emit_str("nslookup: network/DNS not initialized\n");
        return;
    }
    sink.emit_str("nslookup: resolving ");
    sink.emit_str(name);
    sink.emit_str(" ...\n");
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
            sink.emit_str("nslookup: no answer for ");
            sink.emit_str(name);
            sink.emit_str("\n");
        }
    }
}

/// `udp_echo <port>`：UDP 回显服务器（把收到的数据报原样发回）。
fn cmd_udp_echo(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: udp_echo <port>\n");
        return;
    }
    let port = match word_str(&cmd.argv[1]).parse::<u16>() {
        Ok(p) => p,
        Err(_) => {
            sink.emit_str("udp_echo: invalid port\n");
            return;
        }
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("udp_echo: network not initialized\n");
        return;
    }
    let h = crate::socket::socket_udp();
    if h < 0 || crate::socket::bind(h, port) != 0 {
        sink.emit_str("udp_echo: bind failed\n");
        return;
    }
    sink.emit_str("udp_echo: listening on UDP port ");
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
        sink.emit_str("usage: calc <expr>\n");
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
        None => sink.emit_str("calc: parse error\n"),
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

/// `poweroff`：经 PSCI SYSTEM_OFF 关机（QEMU virt 下优雅退出模拟器）。
fn cmd_poweroff(_cmd: &Command, _sink: &mut Sink) {
    crate::uart_send_str("poweroff: shutting down\n");
    // PSCI 0x84000008 = SYSTEM_OFF。QEMU virt 的 PSCI conduit 是 hvc
    //（DTB psci 节点 method="hvc"）：HVC 交给 EL2/固件后 QEMU 退出（-no-reboot）。
    unsafe {
        let func: u64 = 0x8400_0008;
        core::arch::asm!("hvc #0", in("x0") func, options(noreturn));
    }
}

/// `reboot`：经 PSCI SYSTEM_RESET 重启（QEMU `-no-reboot` 下等价退出）。
fn cmd_reboot(_cmd: &Command, _sink: &mut Sink) {
    crate::uart_send_str("reboot: system reset\n");
    unsafe {
        let func: u64 = 0x8400_0009; // PSCI SYSTEM_RESET
        core::arch::asm!("hvc #0", in("x0") func, options(noreturn));
    }
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
        sink.emit_str("usage: ping <ip>\n");
        return;
    }
    let Some(ip) = parse_ip(word_str(&cmd.argv[1])) else {
        sink.emit_str("ping: invalid IP address\n");
        return;
    };
    if crate::net_stack::local_ip() == 0 {
        sink.emit_str("ping: network not initialized\n");
        return;
    }
    sink.emit_str("PING ");
    sink.emit_str(word_str(&cmd.argv[1]));
    sink.emit_str(" ...\n");
    if crate::net_stack::ping(ip) {
        sink.emit_str("64 bytes from ");
        sink.emit_str(word_str(&cmd.argv[1]));
        sink.emit_str(": icmp_seq=1 ttl=64 time=0.0 ms\n");
    } else {
        sink.emit_str("ping: no reply from ");
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
        sink.emit_str("total\n");
    }
}

/// `uname`：打印操作系统信息（内核版本/架构）。
fn cmd_uname(cmd: &Command, sink: &mut Sink) {
    let all = cmd.argc > 1 && word_str(&cmd.argv[1]) == "-a";
    if all {
        sink.emit_str("KunxOS kunxos 1.0.0 aarch64 (KunxOS microkernel)\n");
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
        sink.emit_str("usage: mkdir <dir>\n");
        return;
    }
    let path = resolve_path(word_str(&cmd.argv[1]));
    if vfs::mkdir(path) != 0 {
        sink.emit_str("mkdir: cannot create directory: ");
        sink.emit_str(path);
        sink.emit_str("\n");
    }
}

/// `rm <file>`：删除文件（目录删除暂不支持，与内核 fs_unlink 语义一致）。
fn cmd_rm(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: rm <file>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        if vfs::unlink(path) != 0 {
            sink.emit_str("rm: cannot remove: ");
            sink.emit_str(path);
            sink.emit_str("\n");
        }
    }
}

/// `touch <file...>`：文件不存在时创建空文件，已存在则空操作（本 FS 无 mtime）。
fn cmd_touch(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: touch <file>\n");
        return;
    }
    for i in 1..cmd.argc {
        let path = resolve_path(word_str(&cmd.argv[i]));
        let fd = vfs::open(path, vfs::O_CREAT | vfs::O_WRONLY);
        if fd < 0 {
            sink.emit_str("touch: cannot create: ");
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
        sink.emit_str("usage: ln -s <target> <link>\n");
        return;
    }
    if word_str(&cmd.argv[1]) != "-s" {
        sink.emit_str("ln: only -s (symbolic link) is supported\n");
        return;
    }
    if cmd.argc < 4 {
        sink.emit_str("usage: ln -s <target> <link>\n");
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
        sink.emit_str("ln: cannot create link: ");
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
        sink.emit_str("usage: cp <src> <dst>\n");
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
                sink.emit_str("cp: invalid source path\n");
                return;
            }
            let n = path_join(dst_arg, core::str::from_utf8(&bn[..bl]).unwrap_or(""), &mut dstbuf);
            if n == 0 {
                sink.emit_str("cp: destination too long\n");
                return;
            }
            core::str::from_utf8(&dstbuf[..n]).unwrap_or("")
        }
        _ => dst_arg,
    };
    if src == dst {
        sink.emit_str("cp: cannot copy a file onto itself\n");
        return;
    }
    let sfd = vfs::open(src, vfs::O_RDONLY);
    if sfd < 0 {
        sink.emit_str("cp: cannot open source: ");
        sink.emit_str(src);
        sink.emit_str("\n");
        return;
    }
    let dfd = vfs::open(dst, vfs::O_CREAT | vfs::O_WRONLY | vfs::O_TRUNC);
    if dfd < 0 {
        vfs::close(sfd);
        sink.emit_str("cp: cannot create destination: ");
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
            sink.emit_str("cp: write error\n");
            break;
        }
    }
    vfs::close(sfd);
    vfs::close(dfd);
}

/// `mv <src> <dst>`：重命名/移动文件（可跨目录，SFS 迁移同一 inode）。
fn cmd_mv(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 3 {
        sink.emit_str("usage: mv <src> <dst>\n");
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
                sink.emit_str("mv: invalid source path\n");
                return;
            }
            let n = path_join(dst_arg, core::str::from_utf8(&bn[..bl]).unwrap_or(""), &mut dstbuf);
            if n == 0 {
                sink.emit_str("mv: destination too long\n");
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
        sink.emit_str("mv: cannot move: ");
        sink.emit_str(src);
        sink.emit_str(" -> ");
        sink.emit_str(dst);
        sink.emit_str(" (missing, directory, or target exists?)\n");
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
    sink.emit_str("PID  NAME\n");
    let s = &mut *sink;
    process::ps(&mut |pid: u32, name: &str| {
        s.emit_u32(pid);
        s.emit_str("    ");
        s.emit_str(name);
        s.emit_str("\n");
    });
}

/// `kill <pid>`：向进程发送 SIGKILL（9）。目标进程下一次进入内核（任意系统
/// 调用）时被终止；阻塞中的进程先被唤醒。
fn cmd_kill(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: kill <pid>\n");
        return;
    }
    let Ok(pid) = word_str(&cmd.argv[1]).parse::<u32>() else {
        sink.emit_str("kill: invalid pid\n");
        return;
    };
    if pid == crate::process::current_pid() {
        sink.emit_str("kill: cannot kill the shell itself\n");
        return;
    }
    if !crate::process::process_exists(pid) {
        sink.emit_str("kill: no such process: ");
        sink.emit_str(word_str(&cmd.argv[1]));
        sink.emit_str("\n");
        return;
    }
    crate::process::signal_send(pid, 9);
    // 唤醒阻塞中的目标：被唤醒后其睡眠/等待系统调用返回，下一次系统调用
    // 入口处 SIGKILL 位图被检查并终止进程。
    crate::sched_cfs::wake_pid(pid);
    sink.emit_str("kill: signal 9 sent to pid ");
    sink.emit_u32(pid);
    sink.emit_str("\n");
}

/// `sleep <seconds>`：让 shell 睡眠指定秒数（基于内核单调时钟，可被定时器
/// 唤醒后按剩余时间继续）。
fn cmd_sleep(cmd: &Command, sink: &mut Sink) {
    if cmd.argc < 2 {
        sink.emit_str("usage: sleep <seconds>\n");
        return;
    }
    let Ok(secs) = word_str(&cmd.argv[1]).parse::<u64>() else {
        sink.emit_str("sleep: invalid time\n");
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
                sink.emit_str("beep: invalid frequency\n");
                return;
            }
        }
    }
    if cmd.argc >= 3 {
        match word_str(&cmd.argv[2]).parse::<u32>() {
            Ok(m) if m > 0 => ms = m,
            _ => {
                sink.emit_str("beep: invalid duration\n");
                return;
            }
        }
    }
    sink.emit_str("beep: playing ");
    sink.emit_u32(freq);
    sink.emit_str(" Hz for ");
    sink.emit_u32(ms);
    sink.emit_str(" ms\n");
    let r = crate::ac97::play_tone(freq, ms);
    if r != 0 {
        sink.emit_str("beep: no audio device or playback failed\n");
    } else {
        sink.emit_str("beep: done\n");
    }
}

/// `sysinfo`：物理内存 / 任务 / 进程使用统计。
fn cmd_sysinfo(_cmd: &Command, sink: &mut Sink) {
    let us = crate::timer::now_us();
    sink.emit_str("Uptime: ");
    sink.emit_u32((us / 1_000_000) as u32);
    sink.emit_str(" s\n");
    let total_pages = crate::allocator::total_pages();
    let used = crate::allocator::used_pages();
    let total_kb = total_pages * crate::allocator::PAGE_SIZE / 1024;
    let used_kb = used * crate::allocator::PAGE_SIZE / 1024;
    sink.emit_str("MemTotal: ");
    sink.emit_u32(total_kb as u32);
    sink.emit_str(" kB\n");
    sink.emit_str("MemUsed:  ");
    sink.emit_u32(used_kb as u32);
    sink.emit_str(" kB (");
    if total_pages > 0 {
        sink.emit_u32(((used * 100) / total_pages) as u32);
    } else {
        sink.emit_str("0");
    }
    sink.emit_str("%)\n");
    sink.emit_str("MemFree:  ");
    sink.emit_u32((total_kb.saturating_sub(used_kb)) as u32);
    sink.emit_str(" kB\n");
    sink.emit_str("Tasks:   ");
    sink.emit_u32(crate::sched_cfs::task_count() as u32);
    sink.emit_str(" running\n");
    sink.emit_str("Processes: ");
    let mut n = 0u32;
    process::ps(&mut |_, _| n += 1);
    sink.emit_u32(n);
    sink.emit_str(" registered\n");
}

fn run_builtin(cmd: &Command, input: &[u8], sink: &mut Sink) {
    let name = word_str(&cmd.argv[0]);
    match name {
        "cd" => cmd_cd(cmd, sink),
        "echo" => cmd_echo(cmd, sink),
        "export" => cmd_export(cmd, sink),
        "env" => cmd_export(cmd, sink),
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
        "http_serve" => cmd_http_serve(cmd, sink),
        "netstat" => cmd_netstat(cmd, sink),
        "nslookup" => cmd_nslookup(cmd, sink),
        "udp_echo" => cmd_udp_echo(cmd, sink),
        "sort" => cmd_sort(cmd, input, sink),
        "calc" => cmd_calc(cmd, sink),
        "poweroff" => cmd_poweroff(cmd, sink),
        "reboot" => cmd_reboot(cmd, sink),
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
        "true" => cmd_true(cmd, sink),
        "false" => cmd_false(cmd, sink),
        "basename" => cmd_basename(cmd, sink),
        "dirname" => cmd_dirname(cmd, sink),
        "" => {}
        _ => {
            sink.emit_str("kunxos: command not found: ");
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
            let full_str = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(full.as_ptr(), off)) };
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

fn read_line() -> Line {
    let mut line = Line { buf: [0; LINE_MAX], len: 0 };
    // 提示 UART 输入曾溢出（粘贴大量命令时 shell 忙于执行可能丢字节）
    let dropped = crate::uart_take_dropped();
    if dropped > 0 {
        crate::uart_send_str("[shell] warning: ");
        crate::print_dec(dropped as u64);
        crate::uart_send_str(" input byte(s) dropped (UART rx overflow)\n");
    }
    // 转义序列状态机：0=普通，1=已见 ESC，2=已见 ESC[
    let mut esc = 0u8;
    loop {
        if let Some(c) = crate::uart_try_read_char() {
            match c {
                b'\n' | b'\r' => {
                    crate::uart_send_str("\n");
                    history_push(&line);
                    return line;
                }
                0x1b => esc = 1,
                b'[' if esc == 1 => esc = 2,
                b'A' if esc == 2 => {
                    // 上箭头：浏览更早的历史
                    esc = 0;
                    unsafe {
                        if HIST_COUNT > 0 && HIST_POS > 0 {
                            HIST_POS -= 1;
                            history_load(HIST_POS, &mut line);
                        }
                    }
                }
                b'B' if esc == 2 => {
                    // 下箭头：浏览更新的历史（越过最新则回到空行）
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
                        }
                    }
                }
                b'\t' => {
                    // Tab 补全：首个词按 /bin 命令补全，其余按 cwd 路径补全
                    esc = 0;
                    let ws = current_word_start(&line.buf[..line.len]);
                    let prefix = core::str::from_utf8(&line.buf[ws..line.len]).unwrap_or("");
                    let is_first = line.buf[..ws].iter().all(|&c| c == b' ');
                    let mut completion = [0u8; 96];
                    let n = if is_first {
                        // 命令位置：内置命令优先，其次 /bin 可执行
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
                        // 追加缺失部分（前缀后的字符）
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
                        // 多候选：换行列出后重绘
                        crate::uart_send_str("\n");
                        if is_first {
                            let _ = complete_from_builtins(prefix, &mut completion, true);
                            let _ = complete_from_dir("/bin", prefix, &mut completion, true);
                        } else {
                            let _ = complete_path(prefix, &mut completion, true);
                        }
                        crate::uart_send_str("kunxos$ ");
                        for &c in &line.buf[..line.len] {
                            crate::uart_send_char(c);
                        }
                    }
                }
                0x7f | 0x08 => {
                    esc = 0;
                    if line.len > 0 {
                        line.len -= 1;
                        crate::uart_send_str("\x08 \x08");
                    }
                }
                _ => {
                    esc = 0;
                    if line.len < LINE_MAX - 1 {
                        line.buf[line.len] = c;
                        line.len += 1;
                        crate::uart_send_char(c);
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
    loop {
        crate::uart_send_str("kunxos$ ");
        let line = read_line();
        if line.len == 0 {
            continue;
        }
        // 支持 ';' 命令分隔符：按 ';' 切分后逐段执行（`a; b`）
        let mut seg_start = 0usize;
        for i in 0..=line.len {
            if i == line.len || line.buf[i] == b';' {
                let seg_len = i - seg_start;
                if seg_len > 0 {
                    let mut tokens = [Token { word: EMPTY_WORD, op: OP_WORD }; MAX_TOK];
                    let tok_count = tokenize(&line.buf[seg_start..i], seg_len, &mut tokens);
                    let pipe = parse(&tokens, tok_count);
                    run_pipeline(&pipe);
                }
                seg_start = i + 1;
            }
        }
    }
}
