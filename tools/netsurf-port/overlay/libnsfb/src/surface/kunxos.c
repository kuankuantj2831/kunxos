/*
 * KunxOS surface backend for libnsfb.
 *
 * 输出：/dev/fb0（XRGB8888，内核合成器 + FBIO_PRESENT 脏区增量提交）。
 *       软件光标：claim 清精灵 → 应用重绘 → update 重画并入脏区。
 * 输入：/dev/input/event0（Linux evdev：触摸 = BTN_TOUCH/BTN_LEFT + ABS_X/Y，
 *       键盘 = EV_KEY）。按 Linux input_event(24B) 读取。
 */

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <time.h>

#include "libnsfb.h"
#include "libnsfb_plot.h"
#include "libnsfb_plot_util.h"
#include "libnsfb_event.h"

#include "nsfb.h"
#include "surface.h"
#include "plot.h"
#include "cursor.h"

/* ---- 内核 fb ioctl（与 KunxOS devfs 约定一致，24 字节 varinfo） ---- */
#define KX_FBIOGET_VSCREENINFO 0x4600
#define KX_FBIO_PRESENT        0x4603

struct kx_varinfo {
    unsigned int xres, yres, xres_virtual, yres_virtual, xoffset, yoffset;
};

/* ---- evdev（与 linux/input.h 一致） ---- */
struct kx_input_event {
    long long tv_sec;   /* timeval：aarch64 上 16 字节 */
    long long tv_usec;
    unsigned short type;
    unsigned short code;
    int value;
};
#define KX_EV_KEY  0x01
#define KX_EV_ABS  0x03
#define KX_EV_SYN  0x00
#define KX_ABS_X   0x00
#define KX_ABS_Y   0x01
#define KX_BTN_LEFT  0x110
#define KX_BTN_TOUCH 0x14a

static int kx_fbfd = -1;
static int kx_evfd = -1;
static unsigned char *fbin = NULL;
static unsigned int kx_stride_px = 0;
/* 屏幕几何（present 裁剪用），几何锁定时记录 */
static unsigned int kx_width = 0, kx_height = 0;

/* 触摸状态（跨 read 调用保持） */
static int kx_have_pos = 0;
static int kx_cur_x = 0, kx_cur_y = 0;
static int kx_btn_down = 0;
/* 待派发的翻译后事件队列（每次 input 至多消费一个） */
static nsfb_event_t kx_queue[16];
static int kx_q_head = 0, kx_q_tail = 0;

static void kx_emit_move(void)
{
    nsfb_event_t *ev;
    if (kx_q_tail >= (int)(sizeof(kx_queue) / sizeof(kx_queue[0]))) {
        return;
    }
    ev = &kx_queue[kx_q_tail++];
    ev->type = NSFB_EVENT_MOVE_ABSOLUTE;
    ev->value.vector.x = kx_cur_x;
    ev->value.vector.y = kx_cur_y;
    ev->value.vector.z = 0;
}

static void kx_emit_key(int down, int keycode)
{
    nsfb_event_t *ev;
    if (kx_q_tail >= (int)(sizeof(kx_queue) / sizeof(kx_queue[0]))) {
        return;
    }
    ev = &kx_queue[kx_q_tail++];
    ev->type = down ? NSFB_EVENT_KEY_DOWN : NSFB_EVENT_KEY_UP;
    ev->value.keycode = (enum nsfb_key_code_e)keycode;
}

/* evdev keycodes → NSFB keycodes（覆盖常用 ASCII 键） */
static int kx_map_key(unsigned short code)
{
    if (code == 1)  return 27;   /* ESC */
    if (code == 14) return 8;    /* BACKSPACE */
    if (code == 15) return 9;    /* TAB */
    if (code == 28) return 13;   /* ENTER */
    if (code == 57) return 32;   /* SPACE */
    if (code >= 2 && code <= 10)  return '1' + (code - 2);   /* 1..9 */
    if (code == 11) return '0';
    if (code >= 16 && code <= 25) return 'q' + (code - 16);  /* QWERTYUIOP */
    if (code >= 30 && code <= 38) return 'a' + (code - 30);  /* ASDFGHJKL */
    if (code >= 44 && code <= 50) return 'z' + (code - 44);  /* ZXCVBNM */
    if (code == 12) return '-';
    if (code == 13) return '=';
    if (code == 51) return ',';
    if (code == 52) return '.';
    if (code == 53) return '/';
    if (code == 103) return NSFB_KEY_UP;
    if (code == 108) return NSFB_KEY_DOWN;
    if (code == 105) return NSFB_KEY_LEFT;
    if (code == 106) return NSFB_KEY_RIGHT;
    return -1;
}

/* 读取并翻译 evdev 事件；返回翻译后事件数（0 = 无输入） */
static int kx_pump_evdev(void)
{
    struct kx_input_event evs[8];
    unsigned char *p = (unsigned char *)evs;
    int want = (int)sizeof(evs);
    int got = 0, i;
    int produced = 0;

    if (kx_evfd < 0) {
        return 0;
    }
    while (got < want) {
        int n = (int)read(kx_evfd, (char *)evs + got, (size_t)(want - got));
        if (n <= 0) {
            break;
        }
        got += n;
    }
    if (got <= 0) {
        return 0;
    }
    for (i = 0; i + (int)sizeof(struct kx_input_event) <= got;
         i += (int)sizeof(struct kx_input_event)) {
        const struct kx_input_event *e =
            (const struct kx_input_event *)(p + i);
        if (e->type == KX_EV_ABS) {
            if (e->code == KX_ABS_X) {
                kx_cur_x = e->value;
                kx_have_pos |= 1;
            } else if (e->code == KX_ABS_Y) {
                kx_cur_y = e->value;
                kx_have_pos |= 2;
                if (kx_have_pos == 3) {
                    kx_emit_move();
                    produced++;
                }
            }
        } else if (e->type == KX_EV_KEY) {
            if (e->code == KX_BTN_TOUCH || e->code == KX_BTN_LEFT) {
                if ((e->value != 0) != kx_btn_down) {
                    kx_btn_down = (e->value != 0);
                    if (kx_have_pos == 3) {
                        kx_emit_move();
                        produced++;
                    }
                    kx_emit_key(kx_btn_down, NSFB_KEY_MOUSE_1);
                    produced++;
                }
            } else {
                int k = kx_map_key(e->code);
                if (k > 0) {
                    kx_emit_key(e->value != 0, k);
                    produced++;
                }
            }
        }
    }
    return produced;
}

/* 打开 /dev/fb0 并把 nsfb 几何锁定为内核报告的真实分辨率 */
static int kx_open_and_lock_geometry(nsfb_t *nsfb)
{
    struct kx_varinfo vi;

    if (kx_fbfd >= 0) {
        return 0; /* 已打开 */
    }
    kx_fbfd = open("/dev/fb0", O_RDWR);
    if (kx_fbfd < 0) {
        fprintf(stderr, "kunxos surface: cannot open /dev/fb0\n");
        return -1;
    }
    memset(&vi, 0, sizeof vi);
    if (ioctl(kx_fbfd, KX_FBIOGET_VSCREENINFO, &vi) != 0) {
        fprintf(stderr, "kunxos surface: fb ioctl failed\n");
        return -1;
    }
    nsfb->width = (int)vi.xres;
    nsfb->height = (int)vi.yres;
    nsfb->format = NSFB_FMT_XRGB8888;
    kx_stride_px = vi.xres_virtual ? vi.xres_virtual : vi.xres;
    kx_width = vi.xres;
    kx_height = vi.yres;
    select_plotters(nsfb);
    return 0;
}

static int kunxos_defaults(nsfb_t *nsfb)
{
    nsfb->width = 0;
    nsfb->height = 0;
    nsfb->format = NSFB_FMT_XRGB8888;
    select_plotters(nsfb);
    return 0;
}

static int kunxos_initialise(nsfb_t *nsfb)
{
    size_t size;

    if (kx_open_and_lock_geometry(nsfb) != 0) {
        return -1;
    }

    size = (size_t)kx_stride_px * (unsigned)nsfb->height * 4;

    fbin = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, kx_fbfd, 0);
    if (fbin == (unsigned char *)-1) {
        fprintf(stderr, "kunxos surface: fb mmap failed\n");
        fbin = NULL;
        return -1;
    }
    /* framebuffer 直接作为 nsfb 绘制面：跳过拷贝 */
    nsfb->ptr = fbin;
    nsfb->linelen = (int)(kx_stride_px * 4);

    kx_evfd = open("/dev/input/event0", O_RDWR | O_NONBLOCK);
    if (kx_evfd < 0) {
        fprintf(stderr, "kunxos surface: no input device (mouse disabled)\n");
    }
    return 0;
}

static int kunxos_finalise(nsfb_t *nsfb)
{
    if (kx_evfd >= 0) {
        close(kx_evfd);
        kx_evfd = -1;
    }
    if (kx_fbfd >= 0) {
        close(kx_fbfd);
        kx_fbfd = -1;
    }
    nsfb->ptr = NULL;
    return 0;
}

static int
kunxos_set_geometry(nsfb_t *nsfb, int width, int height, enum nsfb_format_e format)
{
    (void)width;
    (void)height;
    (void)format;
    /* 几何锁定为内核 fb 真实分辨率：前端请求的 800x600 一律忽略，
     * 否则前端按 800 宽绘图而 surface 是 540 宽 → 行内越界写 */
    if (kx_open_and_lock_geometry(nsfb) != 0) {
        return -1;
    }
    nsfb->linelen = (int)(kx_stride_px * 4);
    return 0;
}

static bool kunxos_input(nsfb_t *nsfb, nsfb_event_t *event, int timeout)
{
    int waited = 0;

    (void)nsfb;

    for (;;) {
        /* 先派发已排队的事件 */
        while (kx_q_head < kx_q_tail) {
            *event = kx_queue[kx_q_head++];
            if (kx_q_head >= kx_q_tail) {
                kx_q_head = kx_q_tail = 0;
            }
            return true;
        }
        kx_q_head = kx_q_tail = 0;

        if (kx_pump_evdev() > 0) {
            continue;
        }
        /* 队列里有翻译好的事件则返回 */
        if (kx_q_tail > kx_q_head) {
            *event = kx_queue[kx_q_head++];
            return true;
        }
        if (timeout == 0) {
            return false;
        }
        if (timeout > 0 && waited >= timeout) {
            return false;
        }
        {
            struct timespec ts;
            int slice = 8; /* ms；内核输入到达会提前唤醒 nanosleep，
                            * 切片只是唤醒竞态丢失时的延迟上界 */
            if (timeout > 0 && timeout - waited < slice) {
                slice = timeout - waited;
            }
            ts.tv_sec = slice / 1000;
            ts.tv_nsec = (long)(slice % 1000) * 1000000L;
            nanosleep(&ts, NULL);
            waited += slice;
        }
    }
}

/* ---- 增量呈现 + 软件光标 ---- */

/* 把屏幕坐标 bbox 作为脏区增量提交（FBIO_PRESENT 用户缓冲布局：
 * [count:u32][rect × count]，rect = {x:i32, y:i32, w:u32, h:u32}，
 * 与内核 devfs/linux_compat 约定一致）。空区域直接跳过。 */
static void kx_present_box(const nsfb_bbox_t *box)
{
    unsigned char buf[4 + 16];
    int x0 = box->x0;
    int y0 = box->y0;
    int x1 = box->x1;
    int y1 = box->y1;
    unsigned int w, h;
    int i;

    if (kx_fbfd < 0) {
        return;
    }
    if (x0 < 0) {
        x0 = 0;
    }
    if (y0 < 0) {
        y0 = 0;
    }
    if (kx_width != 0 && x1 > (int)kx_width) {
        x1 = (int)kx_width;
    }
    if (kx_height != 0 && y1 > (int)kx_height) {
        y1 = (int)kx_height;
    }
    if (x1 <= x0 || y1 <= y0) {
        return; /* 无有效损伤：不触发呈现 */
    }
    w = (unsigned int)(x1 - x0);
    h = (unsigned int)(y1 - y0);

    buf[0] = 1; /* count = 1（小端） */
    buf[1] = 0;
    buf[2] = 0;
    buf[3] = 0;
    for (i = 0; i < 4; i++) {
        buf[4 + i] = (unsigned char)(((unsigned int)x0 >> (i * 8)) & 0xff);
        buf[8 + i] = (unsigned char)(((unsigned int)y0 >> (i * 8)) & 0xff);
        buf[12 + i] = (unsigned char)((w >> (i * 8)) & 0xff);
        buf[16 + i] = (unsigned char)((h >> (i * 8)) & 0xff);
    }
    ioctl(kx_fbfd, KX_FBIO_PRESENT, buf);
}

/* 应用（浏览器重绘）即将直接写入该区域：若软件光标与其相交，先恢复
 * 底图清除光标精灵，由随后的 kunxos_update 重画 —— SDL surface 同款
 * claim/clear → 重绘 → update/plot 时序。 */
static int kunxos_claim(nsfb_t *nsfb, nsfb_bbox_t *box)
{
    struct nsfb_cursor_s *cursor = nsfb->cursor;

    if ((cursor != NULL) && cursor->plotted &&
        nsfb_plot_bbox_intersect(box, &cursor->loc)) {
        nsfb_cursor_clear(nsfb, cursor);
    }
    return 0;
}

/* 增量提交：本帧应用重绘区域（box）∪ 需重画的光标区域，按脏区呈现。
 * 取代旧版「每次整帧 FBIO_PRESENT(NULL)」——内核对无脏区呈现回退
 * prev_app 整屏拷贝（启动转场后 prev_app = 全屏，TCG 下每次鼠标
 * 移动 ~2MB canvas→back 拷贝）是浏览器卡顿主因。 */
static int kunxos_update(nsfb_t *nsfb, nsfb_bbox_t *box)
{
    struct nsfb_cursor_s *cursor = nsfb->cursor;
    nsfb_bbox_t allbox;

    if ((cursor != NULL) && !cursor->plotted) {
        nsfb_cursor_plot(nsfb, cursor);
        if (box != NULL) {
            nsfb_plot_add_rect(box, &cursor->savloc, &allbox);
            box = &allbox;
        } else {
            box = &cursor->savloc;
        }
    }
    if (box != NULL) {
        kx_present_box(box);
    }
    return 0;
}

/* 光标位置/形状变更（nsfb_cursor_set / nsfb_cursor_loc_set 触发）：
 * 恢复旧位置底图、在新位置画精灵，两处并集作为小脏区增量呈现 ——
 * 光标移动的呈现成本从整屏 2MB 拷贝降到 ~2×精灵面积。 */
static int kunxos_cursor(nsfb_t *nsfb, struct nsfb_cursor_s *cursor)
{
    nsfb_bbox_t box;
    bool was_plotted = cursor->plotted;

    if (was_plotted) {
        box = cursor->savloc;
        nsfb_cursor_clear(nsfb, cursor);
    }
    if (!nsfb_cursor_plot(nsfb, cursor)) {
        return 0; /* OOM 等异常：本帧不呈现 */
    }
    if (was_plotted) {
        nsfb_plot_add_rect(&box, &cursor->savloc, &box);
    } else {
        box = cursor->savloc;
    }
    kx_present_box(&box);
    return 1; /* 已处理（bool 语义） */
}

const nsfb_surface_rtns_t kunxos_rtns = {
    .defaults = kunxos_defaults,
    .initialise = kunxos_initialise,
    .finalise = kunxos_finalise,
    .input = kunxos_input,
    .geometry = kunxos_set_geometry,
    .claim = kunxos_claim,
    .update = kunxos_update,
    .cursor = kunxos_cursor,
};

NSFB_SURFACE_DEF(kunxos, NSFB_SURFACE_KUNXOS, &kunxos_rtns)

/*
 * Local variables:
 *  c-basic-offset: 4
 *  tab-width: 8
 * End:
 */
