// busybox-lite：Phase 9 验证用的最小多调用测试程序。
// 仅依赖 Linux RISC-V 系统调用（openat/getdents64/read/write/exit），
// 模拟 busybox 的 applet 分发：`busybox ls <path>` / `busybox cat <file>` /
// `busybox echo <args...>` / `busybox bench [n]`（syscall 微基准）。
typedef unsigned long u64;
typedef long i64;

static inline i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    register i64 a7 __asm__("a7") = n;
    register i64 a0 __asm__("a0") = a;
    register i64 a1 __asm__("a1") = b;
    register i64 a2 __asm__("a2") = c;
    register i64 a3 __asm__("a3") = d;
    register i64 a4 __asm__("a4") = e;
    register i64 a5 __asm__("a5") = f;
    __asm__ __volatile__("ecall" : "=r"(a0) : "r"(a7), "0"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5) : "memory");
    return a0;
}
#define sys1(n,a) sys6(n,a,0,0,0,0,0)
#define sys3(n,a,b,c) sys6(n,a,b,c,0,0,0)

static i64 write(i64 fd, const char *buf, i64 len) { return sys3(64, fd, (i64)buf, len); }
static i64 read(i64 fd, char *buf, i64 len) { return sys3(63, fd, (i64)buf, len); }
static i64 openat(i64 dirfd, const char *path, i64 flags, i64 mode) { return sys6(56, dirfd, (i64)path, flags, mode, 0, 0); }
static i64 close(i64 fd) { return sys1(57, fd); }
static i64 getdents64(i64 fd, char *buf, i64 len) { return sys3(61, fd, (i64)buf, len); }
static void exit_(i64 code) { sys1(93, code); for (;;) {} }

static i64 mylen(const char *s) { i64 n = 0; while (s[n]) n++; return n; }
static i64 mycmp(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}
static void put(const char *s) { write(1, s, mylen(s)); }

struct dirent64 {
    u64 d_ino;
    u64 d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[];
};

static void ls(const char *path) {
    i64 fd = openat(-100, path, 0, 0); /* O_RDONLY */
    if (fd < 0) { put("ls: cannot open "); put(path); put("\n"); return; }
    char buf[4096];
    i64 n = getdents64(fd, buf, sizeof buf);
    while (n > 0) {
        i64 pos = 0;
        while (pos < n) {
            struct dirent64 *d = (struct dirent64 *)(buf + pos);
            if (d->d_name[0] != '.') {
                put(d->d_name);
                put("\n");
            }
            pos += d->d_reclen;
        }
        n = getdents64(fd, buf, sizeof buf);
    }
    close(fd);
}

static void cat(const char *path) {
    i64 fd = openat(-100, path, 0, 0);
    if (fd < 0) { put("cat: cannot open "); put(path); put("\n"); return; }
    char buf[512];
    i64 r;
    while ((r = read(fd, buf, sizeof buf)) > 0) {
        write(1, buf, r);
    }
    close(fd);
}

static void echo(int argc, char **argv) {
    for (int i = 2; i < argc; i++) { /* 跳过 argv[0]=busybox, argv[1]=echo */
        if (i > 2) put(" ");
        put(argv[i]);
    }
    put("\n");
}

/* ---- bench：用户态 syscall 微基准（优化轮 028 起的常备观测点）----
 * 用户↔内核往返开销（ecall 陷阱+分发+返回），clock_gettime(MONOTONIC)
 * 自计时，volatile sink 防循环被优化。用法：busybox bench [n]。 */
struct ts { i64 s; i64 ns; };
static u64 now_ns(void) {
    struct ts t;
    sys3(113, 1, (i64)&t, 0);
    return (u64)t.s * 1000000000ul + (u64)t.ns;
}
static void put_u64(u64 v) {
    char b[24];
    int i = 24;
    if (v == 0) b[--i] = '0';
    while (v) { b[--i] = (char)('0' + v % 10); v /= 10; }
    write(1, b + i, 24 - i);
}
static void bench(long n) {
    volatile i64 sink = 0;
    put("[bench] n=");
    put_u64((u64)n);
    put("\n");

    u64 t0 = now_ns();
    for (long i = 0; i < n; i++) sink += sys1(172, 0); /* getpid */
    u64 t1 = now_ns();
    put("getpid        ");
    put_u64((t1 - t0) / (u64)n);
    put(" ns/op (sink=");
    put_u64((u64)sink);
    put(")\n");

    t0 = now_ns();
    for (long i = 0; i < n; i++) { struct ts t; sys3(113, 1, (i64)&t, 0); }
    t1 = now_ns();
    put("clock_gettime ");
    put_u64((t1 - t0) / (u64)n);
    put(" ns/op\n");

    long m = n / 10; /* open+close 较重，跑 1/10 次数 */
    t0 = now_ns();
    for (long i = 0; i < m; i++) {
        i64 fd = sys6(56, -100, (i64)"/", 0, 0, 0, 0);
        if (fd >= 0) sys1(57, fd);
    }
    t1 = now_ns();
    put("open+close /  ");
    put_u64(m ? (t1 - t0) / (u64)m : 0);
    put(" ns/op\n");

    /* 长路径（33 字符）：read_cstr 逐页校验 + 路径解析的受益观测 */
    t0 = now_ns();
    for (long i = 0; i < m; i++) {
        i64 fd = sys6(56, -100, (i64)"/var/lib/pkg/repo/main/Packages", 0, 0, 0, 0);
        if (fd >= 0) sys1(57, fd);
    }
    t1 = now_ns();
    put("open+close 长 ");
    put_u64(m ? (t1 - t0) / (u64)m : 0);
    put(" ns/op\n");
}

void busybox_main(long argc, char **argv) {
    const char *applet = argc > 1 ? argv[1] : "help";
    if (mycmp(applet, "ls") == 0) {
        const char *p = argc > 2 ? argv[2] : "/";
        ls(p);
    } else if (mycmp(applet, "cat") == 0) {
        for (long i = 2; i < argc; i++) cat(argv[i]);
    } else if (mycmp(applet, "echo") == 0) {
        echo((int)argc, argv);
    } else if (mycmp(applet, "bench") == 0) {
        long n = 20000;
        if (argc > 2) {
            n = 0;
            for (const char *p = argv[2]; *p >= '0' && *p <= '9'; p++)
                n = n * 10 + (*p - '0');
            if (n <= 0) n = 20000;
        }
        bench(n);
    } else {
        put("busybox-lite: applets: ls cat echo bench\n");
    }
    exit_(0);
}

/* 入口：内核在用户栈上放好 argc/argv（sp 指向 argc），此处抽取后调用 C 主体 */
__asm__(
".text\n"
".global _start\n"
".type _start, %function\n"
"_start:\n"
"    mv a1, sp\n"
"    ld a0, 0(a1)\n"
"    addi a1, a1, 8\n"
"    call busybox_main\n"
"    li a7, 93\n"
"    ecall\n"
"    j .\n"
);
