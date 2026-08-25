// busybox-lite：Phase 9 验证用的最小多调用测试程序。
// 仅依赖 Linux AArch64 系统调用（openat/getdents64/read/write/exit），
// 模拟 busybox 的 applet 分发：`busybox ls <path>` / `busybox cat <file>` /
// `busybox echo <args...>`。
typedef unsigned long u64;
typedef long i64;

static inline i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    register i64 x8 __asm__("x8") = n;
    register i64 x0 __asm__("x0") = a;
    register i64 x1 __asm__("x1") = b;
    register i64 x2 __asm__("x2") = c;
    register i64 x3 __asm__("x3") = d;
    register i64 x4 __asm__("x4") = e;
    register i64 x5 __asm__("x5") = f;
    __asm__ __volatile__("svc #0" : "=r"(x0) : "r"(x8), "0"(x0), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
    return x0;
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

void busybox_main(long argc, char **argv) {
    const char *applet = argc > 1 ? argv[1] : "help";
    if (mycmp(applet, "ls") == 0) {
        const char *p = argc > 2 ? argv[2] : "/";
        ls(p);
    } else if (mycmp(applet, "cat") == 0) {
        for (long i = 2; i < argc; i++) cat(argv[i]);
    } else if (mycmp(applet, "echo") == 0) {
        echo((int)argc, argv);
    } else {
        put("busybox-lite: applets: ls cat echo\n");
    }
    exit_(0);
}

/* 入口：内核在用户栈上放好 argc/argv（sp 指向 argc），此处抽取后调用 C 主体 */
__asm__(
".text\n"
".global _start\n"
".type _start, %function\n"
"_start:\n"
"    mov x2, sp\n"
"    ldr x0, [x2], #8\n"
"    mov x1, x2\n"
"    bl busybox_main\n"
"    mov x8, #93\n"
"    svc #0\n"
"    b .\n"
);
