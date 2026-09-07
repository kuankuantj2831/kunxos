/* KunxOS PDU 编解码库主机测试（gcc -o test_pdu test_pdu.c pdu.c）。
 *
 * 覆盖：GSM-7 往返（含转义）、UCS-2 往返、中文、长短信分段一致性、
 * SUBMIT 编码结构性检查、DELIVER 解析（含级联重组输入生成）。
 *
 * 附带 `--deliver <text> [number] [ref total part]` 模式：输出 SMS-DELIVER
 * PDU 十六进制（供 QEMU 虚拟调制解调器 SMS_IN 注入，端到端验证接收路径）。
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "pdu.h"

static int failures = 0;

#define CHECK(cond, msg) do { \
    if (!(cond)) { printf("FAIL: %s (line %d)\n", msg, __LINE__); failures++; } \
    else { printf("ok: %s\n", msg); } \
} while (0)

static void hexdump(const unsigned char *p, int n) {
    for (int i = 0; i < n; i++) printf("%02X", p[i]);
    printf("\n");
}

/* 正确的 7-bit LSB-first 打包（与 pdu.c pack7_at 一致，测试侧独立实现） */
static void pack7_ref(const unsigned char *sept, int n, unsigned char *out) {
    for (int i = 0; i < n; i++) {
        int bit = i * 7;
        int idx = bit / 8, sh = bit % 8;
        out[idx] |= (unsigned char)((sept[i] << sh) & 0xFF);
        if (sh > 1) out[idx + 1] |= (unsigned char)(sept[i] >> (8 - sh));
    }
}

static void test_gsm7(void) {
    unsigned char sept[200], utf8[200];
    const char *t1 = "Hello, world! 12345";
    int ns = pdu_gsm7_encode((const unsigned char *)t1, (int)strlen(t1), sept, sizeof(sept));
    CHECK(ns == (int)strlen(t1), "gsm7 ascii count == strlen");
    int tl = pdu_gsm7_decode(sept, ns, utf8, sizeof(utf8));
    CHECK(tl == (int)strlen(t1) && memcmp(utf8, t1, tl) == 0, "gsm7 ascii roundtrip");

    const char *t2 = "Hello [world] ~ test \\ | ^ { } €";
    ns = pdu_gsm7_encode((const unsigned char *)t2, (int)strlen(t2), sept, sizeof(sept));
    CHECK(ns > 0, "gsm7 escape encodable");
    tl = pdu_gsm7_decode(sept, ns, utf8, sizeof(utf8));
    CHECK(tl == (int)strlen(t2) && memcmp(utf8, t2, tl) == 0, "gsm7 escape roundtrip");

    const char *t3 = "你好";
    ns = pdu_gsm7_encode((const unsigned char *)t3, (int)strlen(t3), sept, sizeof(sept));
    CHECK(ns < 0, "gsm7 rejects CJK");

    /* 已知向量：hello → E8 32 9B FD 06（前 5 字节） */
    const char *t4 = "hello";
    ns = pdu_gsm7_encode((const unsigned char *)t4, 5, sept, sizeof(sept));
    CHECK(ns == 5, "hello 5 septets");
    unsigned char packed[8] = {0};
    pack7_ref(sept, ns, packed);
    CHECK(packed[0] == 0xE8 && packed[1] == 0x32 && packed[2] == 0x9B
          && packed[3] == 0xFD && packed[4] == 0x06,
          "known vector E8 32 9B FD 06");
}

static void test_ucs2(void) {
    unsigned char ucs[400], utf8[400];
    const char *t = "你好，世界！KunxOS 短信测试";
    int nb = pdu_utf8_to_ucs2((const unsigned char *)t, (int)strlen(t), ucs, sizeof(ucs));
    CHECK(nb > 0 && (nb & 1) == 0, "utf8->ucs2 ok");
    int tl = pdu_ucs2_to_utf8(ucs, nb, utf8, sizeof(utf8));
    CHECK(tl == (int)strlen(t) && memcmp(utf8, t, tl) == 0, "ucs2 roundtrip");
}

static void test_submit_encode(void) {
    unsigned char pdu[200];
    int udl = 0;
    /* 单条 7-bit：SMSC "+8613800100500"（13 位）→ SMSC 字段 9 字节 */
    int n = pdu_submit_encode(pdu, sizeof(pdu),
                              (const unsigned char *)"+8613800100500", 14,
                              (const unsigned char *)"13800000001", 11,
                              (const unsigned char *)"Hello", 5,
                              0, 1, 0, 0, 0, &udl);
    CHECK(n > 0, "submit encode single ok");
    CHECK(udl == 5, "submit udl septets = 5");
    CHECK(pdu[0] == 9, "submit smsc len = 9");
    /* SMSC 字段 9 字节 → TPDU 从 pdu[10] 起 */
    CHECK((pdu[10] & 0x03) == 0x01, "submit MTI");
    CHECK(pdu[11] == 1, "submit MR=1");
    CHECK(pdu[12] == 11, "submit DA len 11");
    CHECK(pdu[13] == 0x81, "submit DA TON national (no +)");
    CHECK(pdu[14] == 0x31, "submit DA first octet 31");
    /* 11 位 → 6 个半八位字节 → PID/DCS 在 20/21 */
    CHECK(pdu[20] == 0x00, "submit PID");
    CHECK(pdu[21] == 0x00, "submit DCS 7bit");
    CHECK(pdu[22] == 5, "submit UDL 5");
    /* 'H' = 0x48 打包后首字节 0xC8 */
    CHECK(pdu[23] == 0xC8, "submit first UD octet C8");

    /* 无 SMSC 时首字节 00 */
    n = pdu_submit_encode(pdu, sizeof(pdu),
                          NULL, 0,
                          (const unsigned char *)"13800000001", 11,
                          (const unsigned char *)"Hi", 2,
                          0, 2, 0, 0, 0, &udl);
    CHECK(n > 0 && pdu[0] == 0x00, "submit no-smsc");

    /* UCS-2 单条 */
    n = pdu_submit_encode(pdu, sizeof(pdu),
                          NULL, 0,
                          (const unsigned char *)"13800000001", 11,
                          (const unsigned char *)"你好", 6,
                          8, 3, 0, 0, 0, &udl);
    CHECK(n > 0 && udl == 4, "submit ucs2 udl=4");
    CHECK(pdu[0] == 0x00 && (pdu[1] & 0x03) == 0x01, "submit ucs2 head");

    /* 长短信分段：180 字符 'A' → 2 段（7-bit），每段含 UDH */
    static char long_text[180];
    for (int i = 0; i < 180; i++) long_text[i] = 'A';
    int parts = pdu_concat_parts((const unsigned char *)long_text, 180, 0);
    CHECK(parts == 2, "concat parts = 2 for 180 A");
    int off = 0, len = 0;
    CHECK(pdu_concat_slice((const unsigned char *)long_text, 180, 0, 1, &off, &len) == 1
          && off == 0 && len == 152, "concat slice part1 = 152 chars");
    CHECK(pdu_concat_slice((const unsigned char *)long_text, 180, 0, 2, &off, &len) == 1
          && off == 152 && len == 28, "concat slice part2 = 28 chars");
    int n1 = pdu_submit_encode(pdu, sizeof(pdu),
                               (const unsigned char *)"+8613800100500", 14,
                               (const unsigned char *)"13800000001", 11,
                               (const unsigned char *)long_text + 0, 152,
                               0, 3, 0x4A, 2, 1, &udl);
    CHECK(n1 > 0 && udl == 7 + 152, "concat part1 udl includes header septets");
    /* TPDU 起始 pdu[10]；UDHL 在 pdu[22]（10+1+1+2+6+1+1 = 22） */
    CHECK(pdu[22] == 0x05, "concat UDHL=5");
    CHECK(pdu[23] == 0x00 && pdu[24] == 0x03 && pdu[25] == 0x4A
          && pdu[26] == 2 && pdu[27] == 1, "concat UDH fields");
}

static void test_deliver_decode(void) {
    /* 构造 DELIVER：SMSC 00 + DELIVER（无 UDH） */
    unsigned char pdu[200];
    int pos = 0;
    pdu[pos++] = 0x00; /* 无 SMSC */
    pdu[pos++] = 0x00; /* DELIVER，无 UDHI */
    /* OA：7 位 "1380001" */
    pdu[pos++] = 7;
    pdu[pos++] = 0x81;
    pdu[pos++] = 0x31; /* 1,3 */
    pdu[pos++] = 0x08; /* 8,0 */
    pdu[pos++] = 0x00; /* 0,0 */
    pdu[pos++] = 0xF1; /* 1,F */
    pdu[pos++] = 0x00; /* PID */
    pdu[pos++] = 0x00; /* DCS 7bit */
    /* SCTS：24/08/27 12:34:56 +08 */
    pdu[pos++] = 0x42; pdu[pos++] = 0x80; pdu[pos++] = 0x72; pdu[pos++] = 0x21;
    pdu[pos++] = 0x43; pdu[pos++] = 0x65; pdu[pos++] = 0x20;
    /* UDL + UD：5 septets "Hello" */
    unsigned char sept[8];
    int ns = pdu_gsm7_encode((const unsigned char *)"Hello", 5, sept, sizeof(sept));
    pdu[pos++] = (unsigned char)ns;
    {
        unsigned char packed[8] = {0};
        pack7_ref(sept, ns, packed);
        for (int i = 0; i < (ns * 7 + 7) / 8; i++) pdu[pos++] = packed[i];
    }
    unsigned char oa[24], scts[24], text[200];
    int oa_len = 0, dcs = -1, text_len = 0, cref = 0, ctot = 0, cpart = 0;
    int r = pdu_deliver_decode(pdu, pos, oa, sizeof(oa), &oa_len,
                               scts, sizeof(scts), &dcs,
                               text, sizeof(text), &text_len,
                               &cref, &ctot, &cpart);
    CHECK(r == 0, "deliver decode ok");
    CHECK(oa_len == 7 && memcmp(oa, "1380001", 7) == 0, "deliver OA");
    CHECK(text_len == 5 && memcmp(text, "Hello", 5) == 0, "deliver text Hello");
    CHECK(dcs == 0, "deliver dcs 0");
    CHECK(ctot == 1 && cpart == 0, "deliver no concat");
}

static void test_deliver_concat(void) {
    /* 2 段级联 DELIVER（UCS-2），第 1 段 "你好"，第 2 段 "世界" */
    const char *part1 = "你好";
    const char *part2 = "世界";
    unsigned char u1[16], u2[16];
    int n1 = pdu_utf8_to_ucs2((const unsigned char *)part1, (int)strlen(part1), u1, sizeof(u1));
    int n2 = pdu_utf8_to_ucs2((const unsigned char *)part2, (int)strlen(part2), u2, sizeof(u2));
    CHECK(n1 == 4 && n2 == 4, "concat ucs2 byte lens");
    for (int seg = 1; seg <= 2; seg++) {
        unsigned char pdu[200];
        int pos = 0;
        const unsigned char *u = (seg == 1) ? u1 : u2;
        pdu[pos++] = 0x00; /* SMSC */
        pdu[pos++] = 0x10; /* DELIVER + UDHI */
        pdu[pos++] = 3;    /* OA len */
        pdu[pos++] = 0x81;
        pdu[pos++] = 0x31; /* 1,3 */
        pdu[pos++] = 0xF8; /* 8,F */
        pdu[pos++] = 0x00; /* PID */
        pdu[pos++] = 0x08; /* DCS UCS-2 */
        pdu[pos++] = 0x42; pdu[pos++] = 0x80; pdu[pos++] = 0x72; pdu[pos++] = 0x21;
        pdu[pos++] = 0x43; pdu[pos++] = 0x65; pdu[pos++] = 0x20; /* SCTS */
        pdu[pos++] = (unsigned char)(6 + 4); /* UDL = UDHL(1)+UDH(5)+4 */
        pdu[pos++] = 0x05; /* UDHL */
        pdu[pos++] = 0x00; pdu[pos++] = 0x03; pdu[pos++] = 0x2A; /* IEI IEDL ref */
        pdu[pos++] = 2;    /* total */
        pdu[pos++] = (unsigned char)seg; /* part */
        for (int i = 0; i < 4; i++) pdu[pos++] = u[i];
        unsigned char oa[24], scts[24], text[200];
        int oa_len = 0, dcs = -1, text_len = 0, cref = 0, ctot = 0, cpart = 0;
        int r = pdu_deliver_decode(pdu, pos, oa, sizeof(oa), &oa_len,
                                   scts, sizeof(scts), &dcs,
                                   text, sizeof(text), &text_len,
                                   &cref, &ctot, &cpart);
        CHECK(r == 0, "concat deliver decode ok");
        CHECK(ctot == 2 && cpart == seg && cref == 0x2A, "concat meta");
        const char *expect = (seg == 1) ? part1 : part2;
        CHECK(text_len == (int)strlen(expect) && memcmp(text, expect, text_len) == 0,
              "concat segment text");
    }
}

/* --deliver 模式：输出 DELIVER PDU hex（含 SMSC=00，UCS-2 若含中文） */
static void gen_deliver(const char *text, const char *number,
                        int ref, int total, int part) {
    unsigned char pdu[200];
    int pos = 0;
    int dcs = 0;
    unsigned char sept[200], ucs[400];
    int udhi = (total > 1) ? 1 : 0;
    if (pdu_gsm7_encode((const unsigned char *)text, (int)strlen(text), sept, sizeof(sept)) < 0) {
        dcs = 8;
    }
    pdu[pos++] = 0x00; /* SMSC */
    pdu[pos++] = (unsigned char)(udhi ? 0x10 : 0x00);
    {
        const unsigned char *d = (const unsigned char *)number;
        if (d[0] == '+') d++;
        int nd = (int)strlen((const char *)d);
        pdu[pos++] = (unsigned char)nd;
        pdu[pos++] = (number[0] == '+') ? 0x91 : 0x81;
        int i = 0;
        while (i < nd) {
            unsigned char hi = d[i] - '0';
            unsigned char lo = (i + 1 < nd) ? d[i + 1] - '0' : 0x0F;
            pdu[pos++] = (unsigned char)((lo << 4) | hi);
            i += 2;
        }
    }
    pdu[pos++] = 0x00; /* PID */
    pdu[pos++] = (unsigned char)dcs;
    pdu[pos++] = 0x42; pdu[pos++] = 0x80; pdu[pos++] = 0x72; pdu[pos++] = 0x21;
    pdu[pos++] = 0x43; pdu[pos++] = 0x65; pdu[pos++] = 0x20; /* SCTS */
    if (udhi) {
        unsigned char ud[180] = {0};
        int header_septets;
        ud[0] = 0x05; ud[1] = 0x00; ud[2] = 0x03;
        ud[3] = (unsigned char)ref; ud[4] = (unsigned char)total; ud[5] = (unsigned char)part;
        if (dcs == 0) {
            int ns = pdu_gsm7_encode((const unsigned char *)text, (int)strlen(text), sept, sizeof(sept));
            header_septets = (6 * 8 + 6) / 7;
            for (int i = 0; i < ns; i++) {
                int bit = header_septets * 7 + i * 7;
                int idx = bit / 8, sh = bit % 8;
                ud[idx] |= (unsigned char)((sept[i] << sh) & 0xFF);
                if (sh > 1) ud[idx + 1] |= (unsigned char)(sept[i] >> (8 - sh));
            }
            pdu[pos++] = (unsigned char)(header_septets + ns);
            for (int i = 0; i < (header_septets * 7 + ns * 7 + 7) / 8; i++) pdu[pos++] = ud[i];
        } else {
            int nb = pdu_utf8_to_ucs2((const unsigned char *)text, (int)strlen(text), ucs, sizeof(ucs));
            pdu[pos++] = (unsigned char)(6 + nb);
            for (int i = 0; i < 6; i++) pdu[pos++] = ud[i];
            for (int i = 0; i < nb; i++) pdu[pos++] = ucs[i];
        }
    } else {
        if (dcs == 0) {
            int ns = pdu_gsm7_encode((const unsigned char *)text, (int)strlen(text), sept, sizeof(sept));
            unsigned char ud[140] = {0};
            for (int i = 0; i < ns; i++) {
                int bit = i * 7;
                int idx = bit / 8, sh = bit % 8;
                ud[idx] |= (unsigned char)((sept[i] << sh) & 0xFF);
                if (sh > 1) ud[idx + 1] |= (unsigned char)(sept[i] >> (8 - sh));
            }
            pdu[pos++] = (unsigned char)ns;
            for (int i = 0; i < (ns * 7 + 7) / 8; i++) pdu[pos++] = ud[i];
        } else {
            int nb = pdu_utf8_to_ucs2((const unsigned char *)text, (int)strlen(text), ucs, sizeof(ucs));
            pdu[pos++] = (unsigned char)nb;
            for (int i = 0; i < nb; i++) pdu[pos++] = ucs[i];
        }
    }
    hexdump(pdu, pos);
}

int main(int argc, char **argv) {
    if (argc >= 3 && strcmp(argv[1], "--deliver") == 0) {
        const char *number = (argc >= 4) ? argv[3] : "13800000001";
        int ref = (argc >= 5) ? atoi(argv[4]) : 0;
        int total = (argc >= 6) ? atoi(argv[5]) : 0;
        int part = (argc >= 7) ? atoi(argv[6]) : 0;
        gen_deliver(argv[2], number, ref, total, part);
        return 0;
    }
    printf("== pdu unit tests ==\n");
    test_gsm7();
    test_ucs2();
    test_submit_encode();
    test_deliver_decode();
    test_deliver_concat();
    if (failures) {
        printf("FAILED: %d failures\n", failures);
        return 1;
    }
    printf("ALL PASS\n");
    return 0;
}
