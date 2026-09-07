/* KunxOS SMS PDU 编解码库（C，freestanding，无 libc / 无堆）。
 *
 * 实现 3GPP TS 23.040 / 23.038：
 *   - GSM 7-bit 默认字母表（128 基本字符 + 9 个转义扩展字符）
 *   - UTF-8 ⇄ UCS-2（大端）双向转换
 *   - SMS-SUBMIT TPDU 构造（可选 UDH 级联 = 长短信分段）
 *   - SMS-DELIVER TPDU 解析（发件人 / 时间戳 / 数据编码 / 级联信息）
 *
 * 7-bit + UDH 的打包遵循 TS 23.040 惯例（与 Android/业界一致）：
 *   TP-UD = [UDHL][UDH] + 填充位到 septet 边界 + 消息 septets（从该位继续打包），
 *   TP-UDL = headerSeptets + 消息 septets，
 *   headerSeptets = ceil(headerOctets * 8 / 7)。
 *
 * 约定：所有函数无状态；输出缓冲由调用方提供；失败返回 -1。
 */

#include "pdu.h"

/* ==================== GSM 7-bit 默认字母表 ==================== */

/* 7-bit 码 → Unicode（TS 23.038 默认字母表，128 项） */
static const unsigned short GSM7_TO_UC[128] = {
    /* 0x00..0x1F */
    0x0040, 0x00A3, 0x0024, 0x00A5, 0x00E8, 0x00E9, 0x00F9, 0x00EC,
    0x00F2, 0x00C7, 0x000A, 0x00D8, 0x00F8, 0x000D, 0x00C5, 0x00E5,
    0x0394, 0x005F, 0x03A6, 0x0393, 0x039B, 0x03A9, 0x03A0, 0x03A8,
    0x03A3, 0x0398, 0x039E, 0x001B, 0x00C6, 0x00E6, 0x00DF, 0x00C9,
    /* 0x20..0x3F */
    0x0020, 0x0021, 0x0022, 0x0023, 0x00A4, 0x0025, 0x0026, 0x0027,
    0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037,
    0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F,
    /* 0x40..0x5F */
    0x00A1, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047,
    0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F,
    0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057,
    0x0058, 0x0059, 0x005A, 0x00C4, 0x00D6, 0x00D1, 0x00DC, 0x00A7,
    /* 0x60..0x7F */
    0x00BF, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067,
    0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F,
    0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077,
    0x0078, 0x0079, 0x007A, 0x00E4, 0x00F6, 0x00F1, 0x00FC, 0x00E0,
};

/* 转义扩展字符表：{(转义后 7-bit 码, Unicode)} */
static const struct {
    unsigned char code;
    unsigned short uc;
} GSM7_EXT[9] = {
    {0x0A, 0x000C}, /* FF 换页 */
    {0x14, 0x005E}, /* ^ */
    {0x28, 0x007B}, /* { */
    {0x29, 0x007D}, /* } */
    {0x2F, 0x005C}, /* \ */
    {0x3C, 0x005B}, /* [ */
    {0x3D, 0x007E}, /* ~ */
    {0x3E, 0x005D}, /* ] */
    {0x40, 0x007C}, /* | */
};

/* Unicode → 7-bit 码：基本表返回 0..127；扩展字符返回 0x100 | code；
 * 欧元返回 0x100 | 0x65；不可编码返回 -1。 */
static int gsm7_code_of(unsigned short uc) {
    int i;
    if (uc == 0x20AC) {
        return 0x100 | 0x65;
    }
    for (i = 0; i < 9; i++) {
        if (GSM7_EXT[i].uc == uc) {
            return 0x100 | GSM7_EXT[i].code;
        }
    }
    for (i = 0; i < 128; i++) {
        if (GSM7_TO_UC[i] == uc) {
            return i;
        }
    }
    return -1;
}

/* ==================== UTF-8 ⇄ 码点 ==================== */

/* 从 UTF-8 读一个码点；返回消耗字节数（1..4）或 -1（非法）。 */
static int utf8_next(const unsigned char *s, int len, unsigned long *cp) {
    unsigned char b0 = s[0];
    if (b0 < 0x80) {
        *cp = b0;
        return 1;
    }
    if (b0 >= 0xC2 && b0 <= 0xDF && len >= 2) {
        if ((s[1] & 0xC0) != 0x80) return -1;
        *cp = ((unsigned long)(b0 & 0x1F) << 6) | (s[1] & 0x3F);
        return 2;
    }
    if (b0 >= 0xE0 && b0 <= 0xEF && len >= 3) {
        if ((s[1] & 0xC0) != 0x80 || (s[2] & 0xC0) != 0x80) return -1;
        *cp = ((unsigned long)(b0 & 0x0F) << 12) | ((unsigned long)(s[1] & 0x3F) << 6)
              | (s[2] & 0x3F);
        if (*cp < 0x800) return -1; /* 过长编码 */
        return 3;
    }
    if (b0 >= 0xF0 && b0 <= 0xF4 && len >= 4) {
        if ((s[1] & 0xC0) != 0x80 || (s[2] & 0xC0) != 0x80 || (s[3] & 0xC0) != 0x80) return -1;
        *cp = ((unsigned long)(b0 & 0x07) << 18) | ((unsigned long)(s[1] & 0x3F) << 12)
              | ((unsigned long)(s[2] & 0x3F) << 6) | (s[3] & 0x3F);
        if (*cp < 0x10000) return -1; /* 过长编码 */
        return 4;
    }
    return -1;
}

/* 码点 → UTF-8（仅 BMP，>0xFFFF 返回 -1）；返回字节数。 */
static int utf8_put(unsigned long cp, unsigned char *out, int cap) {
    if (cp < 0x80) {
        if (cap < 1) return -1;
        out[0] = (unsigned char)cp;
        return 1;
    }
    if (cp < 0x800) {
        if (cap < 2) return -1;
        out[0] = (unsigned char)(0xC0 | (cp >> 6));
        out[1] = (unsigned char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        if (cap < 3) return -1;
        out[0] = (unsigned char)(0xE0 | (cp >> 12));
        out[1] = (unsigned char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (unsigned char)(0x80 | (cp & 0x3F));
        return 3;
    }
    return -1;
}

/* ==================== GSM 7-bit 编解码 ==================== */

/* 统计文本的 septet 数（不写缓冲）；不可编码返回 -1。 */
static int gsm7_count(const unsigned char *utf8, int utf8_len) {
    int n = 0, i = 0;
    while (i < utf8_len) {
        unsigned long cp;
        int adv = utf8_next(utf8 + i, utf8_len - i, &cp);
        int code;
        if (adv < 0) return -1;
        code = gsm7_code_of((unsigned short)cp);
        if (code < 0) return -1;
        n += (code & 0x100) ? 2 : 1;
        i += adv;
    }
    return n;
}

int pdu_gsm7_encode(const unsigned char *utf8, int utf8_len,
                    unsigned char *septets, int septets_cap) {
    int n = 0, i = 0;
    while (i < utf8_len) {
        unsigned long cp;
        int adv = utf8_next(utf8 + i, utf8_len - i, &cp);
        int code;
        if (adv < 0) return -1;
        code = gsm7_code_of((unsigned short)cp);
        if (code < 0) return -1;
        if (code & 0x100) {
            if (n + 2 > septets_cap) return -1;
            septets[n++] = 0x1B;
            septets[n++] = (unsigned char)(code & 0xFF);
        } else {
            if (n + 1 > septets_cap) return -1;
            septets[n++] = (unsigned char)code;
        }
        i += adv;
    }
    return n;
}

int pdu_gsm7_decode(const unsigned char *septets, int n_septets,
                    unsigned char *utf8, int utf8_cap) {
    int i = 0, out = 0;
    while (i < n_septets) {
        unsigned short uc;
        if (septets[i] == 0x1B && i + 1 < n_septets) {
            unsigned char code = septets[i + 1];
            int j;
            uc = 0xFFFF;
            for (j = 0; j < 9; j++) {
                if (GSM7_EXT[j].code == code) {
                    uc = GSM7_EXT[j].uc;
                    break;
                }
            }
            if (code == 0x65) uc = 0x20AC;
            i += 2;
        } else {
            uc = GSM7_TO_UC[septets[i] & 0x7F];
            i += 1;
        }
        if (uc == 0xFFFF) {
            uc = 0x003F; /* 未知转义 → '?' */
        }
        {
            int w = utf8_put(uc, utf8 + out, utf8_cap - out);
            if (w < 0) return -1;
            out += w;
        }
    }
    return out;
}

/* ==================== UTF-8 ⇄ UCS-2 ==================== */

/* 统计 UCS-2 字节数（2 × 字符数）；含 >0xFFFF 或非法 UTF-8 返回 -1。 */
static int ucs2_count(const unsigned char *utf8, int utf8_len) {
    int i = 0, n = 0;
    while (i < utf8_len) {
        unsigned long cp;
        int adv = utf8_next(utf8 + i, utf8_len - i, &cp);
        if (adv < 0) return -1;
        if (cp > 0xFFFF) return -1;
        n += 2;
        i += adv;
    }
    return n;
}

int pdu_utf8_to_ucs2(const unsigned char *utf8, int utf8_len,
                     unsigned char *ucs2, int ucs2_cap) {
    int i = 0, out = 0;
    while (i < utf8_len) {
        unsigned long cp;
        int adv = utf8_next(utf8 + i, utf8_len - i, &cp);
        if (adv < 0) return -1;
        if (cp > 0xFFFF) return -1;
        if (out + 2 > ucs2_cap) return -1;
        ucs2[out++] = (unsigned char)(cp >> 8);
        ucs2[out++] = (unsigned char)(cp & 0xFF);
        i += adv;
    }
    return out;
}

int pdu_ucs2_to_utf8(const unsigned char *ucs2, int ucs2_len,
                     unsigned char *utf8, int utf8_cap) {
    int i = 0, out = 0;
    if (ucs2_len & 1) return -1;
    while (i < ucs2_len) {
        unsigned long cp = ((unsigned long)ucs2[i] << 8) | ucs2[i + 1];
        int w = utf8_put(cp, utf8 + out, utf8_cap - out);
        if (w < 0) return -1;
        out += w;
        i += 2;
    }
    return out;
}

/* ==================== 用户数据单元统计 ==================== */

int pdu_ud_units(const unsigned char *text, int text_len, int dcs) {
    if (dcs == 0) {
        return gsm7_count(text, text_len);
    }
    if (dcs == 8) {
        return ucs2_count(text, text_len);
    }
    return -1;
}

int pdu_concat_parts(const unsigned char *text, int text_len, int dcs) {
    int units = pdu_ud_units(text, text_len, dcs);
    if (units < 0) return -1;
    if (dcs == 0) {
        if (units <= PDU_GSM7_MAX_SEPTETS) return 1;
        return (units + PDU_GSM7_CONCAT_SEPTETS - 1) / PDU_GSM7_CONCAT_SEPTETS;
    }
    if (units <= PDU_UCS2_MAX_CHARS * 2) return 1;
    return (units / 2 + PDU_UCS2_CONCAT_CHARS - 1) / PDU_UCS2_CONCAT_CHARS;
}

int pdu_concat_slice(const unsigned char *text, int text_len, int dcs,
                     int part, int *off, int *len) {
    int budget = (dcs == 0) ? PDU_GSM7_CONCAT_SEPTETS : PDU_UCS2_CONCAT_CHARS * 2;
    int units = 0, i = 0, cur = 1;
    *off = 0;
    *len = 0;
    while (i < text_len) {
        unsigned long cp;
        int adv = utf8_next(text + i, text_len - i, &cp);
        int u;
        if (adv < 0) return 0;
        if (dcs == 0) {
            int code = gsm7_code_of((unsigned short)cp);
            if (code < 0) return 0;
            u = (code & 0x100) ? 2 : 1;
        } else {
            u = 2;
        }
        if (cur == part) {
            if (units + u > budget) break; /* 本段已满 */
            *len += adv;
            units += u;
        } else {
            units += u;
            if (units >= budget) {
                units = 0;
                cur += 1;
                if (cur == part) {
                    *off = i + adv;
                }
            }
        }
        i += adv;
    }
    if (cur != part) return 0;
    return 1;
}

/* ==================== 半八位（BCD）地址编解码 ==================== */

/* 数字串 → 半八位地址字段（[数字个数][TON/NPI][半八位字节]）。返回字段字节数。 */
static int addr_encode(const unsigned char *digits, int nd, int ton,
                       unsigned char *out, int cap) {
    int i, n = (nd + 1) / 2;
    if (nd > PDU_ADDR_MAX_DIGITS) return -1;
    if (n + 2 > cap) return -1;
    out[0] = (unsigned char)nd;
    out[1] = (unsigned char)(0x80 | ((ton & 0x07) << 4) | 0x01); /* TON<<4 | NPI=1 */
    for (i = 0; i < n; i++) {
        unsigned char hi = (i * 2 < nd) ? (unsigned char)(digits[i * 2] - '0') : 0x0F;
        unsigned char lo = (i * 2 + 1 < nd) ? (unsigned char)(digits[i * 2 + 1] - '0') : 0x0F;
        out[2 + i] = (unsigned char)((lo << 4) | hi);
    }
    return n + 2;
}

/* 半八位地址字段 → 数字串（国际格式带 '+'）。返回 0 成功 / -1。 */
static int addr_decode(const unsigned char *in, int in_len,
                       unsigned char *digits, int cap, int *nd_out) {
    int nd, i, out = 0;
    if (in_len < 2) return -1;
    nd = in[0];
    if (nd > PDU_ADDR_MAX_DIGITS) return -1;
    if (cap < nd + 2) return -1;
    if ((in[1] & 0x70) == 0x10) { /* TON=1 国际 */
        digits[out++] = '+';
    }
    for (i = 0; i < nd; i++) {
        unsigned char oct = in[i / 2 + 2];
        unsigned char nib = (i & 1) ? (oct >> 4) : (oct & 0x0F);
        if (nib > 9) return -1;
        digits[out++] = (unsigned char)('0' + nib);
    }
    digits[out] = 0;
    *nd_out = out;
    return 0;
}

/* ==================== 7-bit 打包 / 解包（支持起始位偏移） ==================== */

/* 把 septets 从 bit_offset 位开始写入 octets（octets 须已清零）。 */
static void pack7_at(const unsigned char *septets, int n, int bit_offset,
                     unsigned char *octets, int octet_cap) {
    int i;
    for (i = 0; i < n; i++) {
        int bit = bit_offset + i * 7;
        int idx = bit / 8;
        int sh = bit % 8;
        if (idx >= octet_cap) break;
        octets[idx] |= (unsigned char)((septets[i] << sh) & 0xFF);
        if (sh > 1 && idx + 1 < octet_cap) {
            octets[idx + 1] |= (unsigned char)(septets[i] >> (8 - sh));
        }
    }
}

/* 从 bit_offset 位开始解出 n 个 septets。 */
static void unpack7_at(const unsigned char *octets, int n, int bit_offset,
                       unsigned char *septets) {
    int i;
    for (i = 0; i < n; i++) {
        int bit = bit_offset + i * 7;
        int idx = bit / 8;
        int sh = bit % 8;
        unsigned int v = (unsigned int)octets[idx] >> sh;
        if (sh > 1) {
            v |= (unsigned int)octets[idx + 1] << (8 - sh);
        }
        septets[i] = (unsigned char)(v & 0x7F);
    }
}

/* ==================== SMS-SUBMIT 构造 ==================== */

int pdu_submit_encode(unsigned char *out, int cap,
                      const unsigned char *sc, int sc_len,
                      const unsigned char *da, int da_len,
                      const unsigned char *text, int text_len,
                      int dcs, unsigned char mr,
                      int concat_ref, int concat_total, int concat_part,
                      int *udl_out) {
    int pos = 0;
    unsigned char tmp[PDU_ADDR_MAX_DIGITS + 4];
    unsigned char septets[PDU_GSM7_MAX_SEPTETS + 16];
    unsigned char ud[160];
    int ud_len = 0;
    int udhi = (concat_total > 1) ? 1 : 0;
    int i;

    /* 1. SMSC 字段 */
    if (sc_len == 0) {
        if (cap < 1) return -1;
        out[pos++] = 0x00;
    } else {
        int ton = (sc[0] == '+') ? 1 : 0;
        const unsigned char *digits = (sc[0] == '+') ? sc + 1 : sc;
        int nd = (sc[0] == '+') ? sc_len - 1 : sc_len;
        int n = addr_encode(digits, nd, ton, tmp, (int)sizeof(tmp));
        if (n < 0) return -1;
        if (pos + 1 + n > cap) return -1;
        out[pos++] = (unsigned char)n; /* SMSC 长度 = 后续字段字节数 */
        for (i = 0; i < n; i++) out[pos++] = tmp[i];
    }

    /* 2. TPDU 首字节 / MR / 目的地址 / PID / DCS */
    {
        unsigned char first = 0x01; /* SMS-SUBMIT，RD=0，无 VP */
        if (udhi) first |= 0x10;
        if (pos + 2 > cap) return -1;
        out[pos++] = first;
        out[pos++] = mr;

        int ton = (da_len > 0 && da[0] == '+') ? 1 : 0;
        const unsigned char *ddigits = ton ? da + 1 : da;
        int dnd = ton ? da_len - 1 : da_len;
        int n = addr_encode(ddigits, dnd, ton, tmp, (int)sizeof(tmp));
        if (n < 0) return -1;
        if (pos + n > cap) return -1;
        for (i = 0; i < n; i++) out[pos++] = tmp[i];

        if (pos + 2 > cap) return -1;
        out[pos++] = 0x00; /* PID */
        out[pos++] = (unsigned char)dcs;
    }

    /* 3. UDHL + UDH（级联） */
    if (udhi) {
        /* UDH = 00 03 ref total part（5 字节），UDHL = 5 */
        if (pos + 6 > cap) return -1;
        out[pos++] = 0x05;
        out[pos++] = 0x00; /* IEI：8 位级联引用 */
        out[pos++] = 0x03; /* IEDL */
        out[pos++] = (unsigned char)concat_ref;
        out[pos++] = (unsigned char)concat_total;
        out[pos++] = (unsigned char)concat_part;
    }

    /* 4. 用户数据 */
    if (dcs == 0) {
        int ns = pdu_gsm7_encode(text, text_len, septets, (int)sizeof(septets));
        if (ns < 0) return -1;
        for (i = 0; i < 160; i++) ud[i] = 0;
        if (udhi) {
            /* 头 6 字节（UDHL+UDH）+ 填充位到 septet 边界，消息从 bit 49 起 */
            int header_septets = (6 * 8 + 6) / 7; /* = 7 */
            for (i = 0; i < 6; i++) ud[i] = out[pos - 6 + i];
            pack7_at(septets, ns, header_septets * 7, ud, (int)sizeof(ud));
            ud_len = header_septets + ns;
        } else {
            pack7_at(septets, ns, 0, ud, (int)sizeof(ud));
            ud_len = ns;
        }
    } else if (dcs == 8) {
        int nb = pdu_utf8_to_ucs2(text, text_len, ud + (udhi ? 6 : 0), (int)sizeof(ud) - 6);
        if (nb < 0) return -1;
        if (udhi) {
            for (i = 0; i < 6; i++) ud[i] = out[pos - 6 + i];
            ud_len = 6 + nb;
        } else {
            ud_len = nb;
        }
    } else {
        return -1;
    }

    /* 5. UDL + UD */
    if (pos + 1 > cap) return -1;
    out[pos++] = (unsigned char)ud_len;
    if (pos + ((dcs == 0) ? (ud_len * 7 + 7) / 8 : ud_len) > cap) return -1;
    if (dcs == 0) {
        int octets = (ud_len * 7 + 7) / 8;
        for (i = 0; i < octets; i++) out[pos++] = ud[i];
    } else {
        for (i = 0; i < ud_len; i++) out[pos++] = ud[i];
    }

    if (udl_out) *udl_out = ud_len;
    return pos;
}

/* ==================== SMS-DELIVER 解析 ==================== */

int pdu_deliver_decode(const unsigned char *pdu, int pdu_len,
                       unsigned char *oa, int oa_cap, int *oa_len,
                       unsigned char *scts, int scts_cap,
                       int *dcs,
                       unsigned char *text, int text_cap, int *text_len,
                       int *concat_ref, int *concat_total, int *concat_part) {
    const unsigned char *t;
    int tlen;
    int i;

    if (concat_ref) *concat_ref = 0;
    if (concat_total) *concat_total = 1;
    if (concat_part) *concat_part = 0;

    /* 1. 跳过 SMSC 字段 */
    if (pdu_len < 1) return -1;
    {
        int slen = pdu[0];
        if (slen == 0) {
            t = pdu + 1;
            tlen = pdu_len - 1;
        } else {
            if (1 + slen > pdu_len) return -1;
            t = pdu + 1 + slen;
            tlen = pdu_len - 1 - slen;
        }
    }

    /* 2. 首字节：MTI=00（DELIVER） */
    if (tlen < 1) return -1;
    {
        unsigned char first = t[0];
        int udhi;
        int p;
        int d = 0; /* DCS */
        if ((first & 0x03) != 0x00) return -1;
        udhi = (first >> 4) & 1;
        p = 1;

        /* 3. 发件人地址 */
        if (tlen < p + 1) return -1;
        {
            int oa_nd = t[p];
            int oa_field;
            if (oa_nd > PDU_ADDR_MAX_DIGITS) return -1;
            oa_field = 2 + (oa_nd + 1) / 2;
            if (tlen < p + oa_field) return -1;
            if (addr_decode(t + p, oa_field, oa, oa_cap, oa_len) < 0) return -1;
            p += oa_field;
        }

        /* 4. PID / DCS / SCTS */
        if (tlen < p + 10) return -1;
        {
            d = t[p + 1];
            if (dcs) *dcs = d;
            p += 2;
            if (scts && scts_cap >= 20) {
            static const unsigned char hexd[] = "0123456789";
            unsigned char tz = t[p + 6];
            int neg = (tz & 0x80) != 0;
            unsigned char tzv = tz & 0x7F;
            int n = 0;
            for (i = 0; i < 6; i++) {
                unsigned char oct = t[p + i];
                scts[n++] = hexd[oct & 0x0F];
                scts[n++] = hexd[(oct >> 4) & 0x0F];
                if (i == 1) scts[n++] = '/';
                else if (i == 2) scts[n++] = ',';
                else if (i >= 3 && i <= 4) scts[n++] = ':';
            }
            scts[n++] = neg ? '-' : '+';
            scts[n++] = hexd[tzv >> 4];
            scts[n++] = hexd[tzv & 0x0F];
            scts[n] = 0;
            }
        }
        p += 7;

        /* 5. UDL + UD */
        if (tlen < p + 1) return -1;
        {
            int udl = t[p];
            int ud_octets;
            const unsigned char *ud;
            p += 1;
            if (tlen < p) return -1;
            ud = t + p;
            ud_octets = tlen - p; /* 实际可用字节数（7-bit 时 UDL 是 septets） */

            /* 5.1 UDH 提取（7-bit 时 UDL 含 header septets） */
            if (udhi) {
                int udhl, header_octets, header_septets;
                if (ud_octets < 1) return -1;
                udhl = ud[0];
                header_octets = 1 + udhl;
                if (header_octets > ud_octets) return -1;
                /* 查找级联 IE（IEI=0x00，8 位引用） */
                {
                    int q = 1;
                    while (q + 1 < 1 + udhl) {
                        unsigned char iei = ud[q];
                        unsigned char iel = ud[q + 1];
                        if (q + 2 + iel > 1 + udhl) break;
                        if (iei == 0x00 && iel == 3) {
                            if (concat_ref) *concat_ref = ud[q + 2];
                            if (concat_total) *concat_total = ud[q + 3];
                            if (concat_part) *concat_part = ud[q + 4];
                        }
                        q += 2 + iel;
                    }
                }
                if (d == 0) {
                    header_septets = (header_octets * 8 + 6) / 7;
                    {
                        int need = (udl * 7 + 7) / 8;
                        int msg_septets = udl - header_septets;
                        unsigned char septets[PDU_GSM7_MAX_SEPTETS + 16];
                        if (ud_octets < need) return -1;
                        if (msg_septets < 0 || msg_septets > (int)sizeof(septets)) return -1;
                        unpack7_at(ud, msg_septets, header_septets * 7, septets);
                        {
                            int tl = pdu_gsm7_decode(septets, msg_septets, text, text_cap);
                            if (tl < 0) return -1;
                            if (text_len) *text_len = tl;
                        }
                    }
                } else if (d == 8) {
                    int msg_bytes = udl - header_octets;
                    int tl;
                    if (msg_bytes < 0 || ud_octets < udl) return -1;
                    tl = pdu_ucs2_to_utf8(ud + header_octets, msg_bytes, text, text_cap);
                    if (tl < 0) return -1;
                    if (text_len) *text_len = tl;
                } else {
                    return -1;
                }
            } else {
                /* 无 UDH */
                if (d == 0) {
                    int need = (udl * 7 + 7) / 8;
                    unsigned char septets[PDU_GSM7_MAX_SEPTETS + 16];
                    if (ud_octets < need) return -1;
                    if (udl > (int)sizeof(septets)) return -1;
                    unpack7_at(ud, udl, 0, septets);
                    {
                        int tl = pdu_gsm7_decode(septets, udl, text, text_cap);
                        if (tl < 0) return -1;
                        if (text_len) *text_len = tl;
                    }
                } else if (d == 8) {
                    int tl;
                    if (ud_octets < udl) return -1;
                    tl = pdu_ucs2_to_utf8(ud, udl, text, text_cap);
                    if (tl < 0) return -1;
                    if (text_len) *text_len = tl;
                } else {
                    return -1;
                }
            }
        }
    }
    return 0;
}
