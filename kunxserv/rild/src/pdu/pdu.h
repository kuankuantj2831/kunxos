/* KunxOS SMS PDU 编解码库（C，freestanding，无 libc 依赖）。
 *
 * 实现 3GPP TS 23.040 / 23.038 的 SMS-SUBMIT / SMS-DELIVER TPDU 编解码：
 *   - GSM 7-bit 默认字母表（含转义扩展字符）打包 / 解包
 *   - UTF-8 ⇄ UCS-2（BE）转换
 *   - SMS-SUBMIT TPDU 构造（可选 UDH 级联 = 长短信分段）
 *   - SMS-DELIVER TPDU 解析（来电号码 / 时间戳 / 数据编码 / 级联信息）
 *
 * 所有函数均为无状态、调用方提供输出缓冲；失败返回 -1。
 * 与 rild 的 Rust FFI 包装（pdu.rs）配套使用。
 */
#ifndef KUNXOS_PDU_H
#define KUNXOS_PDU_H

#ifdef __cplusplus
extern "C" {
#endif

/* 单条短信上限（单位：用户数据单元）：GSM-7 160 septets / UCS-2 70 字符 */
#define PDU_GSM7_MAX_SEPTETS 160
#define PDU_UCS2_MAX_CHARS   70
/* 级联（UDH 6 字节 + UDHL 1 字节）后的单段上限 */
#define PDU_GSM7_CONCAT_SEPTETS 152
#define PDU_UCS2_CONCAT_CHARS   66
/* 地址字段最大位数 */
#define PDU_ADDR_MAX_DIGITS 20

/* GSM 7-bit 编码：UTF-8 文本 → septets 数组。
 * 返回 septet 数；文本含不可编码字符（需退化为 UCS-2）时返回 -1。
 * septets_cap 应 ≥ PDU_GSM7_MAX_SEPTETS（调用方负责容量）。 */
int pdu_gsm7_encode(const unsigned char *utf8, int utf8_len,
                    unsigned char *septets, int septets_cap);

/* GSM 7-bit 解码：septets → UTF-8。返回 UTF-8 字节数或 -1。 */
int pdu_gsm7_decode(const unsigned char *septets, int n_septets,
                    unsigned char *utf8, int utf8_cap);

/* UTF-8 → UCS-2（大端字节）。返回字节数（偶数）或 -1（含 >0xFFFF 或非法 UTF-8）。 */
int pdu_utf8_to_ucs2(const unsigned char *utf8, int utf8_len,
                     unsigned char *ucs2, int ucs2_cap);

/* UCS-2（大端）→ UTF-8。返回字节数或 -1。 */
int pdu_ucs2_to_utf8(const unsigned char *ucs2, int ucs2_len,
                     unsigned char *utf8, int utf8_cap);

/* 文本占用的用户数据单元数：dcs=0 → septets；dcs=8 → 字节数（= 字符数×2）。
 * 文本不可编码为所选 DCS 时返回 -1。 */
int pdu_ud_units(const unsigned char *text, int text_len, int dcs);

/* 长短信分段数（1 = 不需要级联）。 */
int pdu_concat_parts(const unsigned char *text, int text_len, int dcs);

/* 计算第 part 段（1 起）覆盖的文本字节区间。返回 1 成功 / 0 失败。 */
int pdu_concat_slice(const unsigned char *text, int text_len, int dcs,
                     int part, int *off, int *len);

/* 构造 SMS-SUBMIT PDU（含 SMSC 字段）。
 *   sc / da：短信中心与目的号码（ASCII 数字，可带前导 '+'）；
 *   text：UTF-8 消息体；dcs：0 = GSM-7，8 = UCS-2；
 *   mr：消息引用号；concat_total <= 1 时不加 UDH，否则加入级联 UDH
 *   （ref 为 8 位级联引用，part 从 1 起）。
 * 返回 PDU 总长度（含 SMSC 长度字节）或 -1；*udl_out 返回用户数据长度
 * （septets / 字节，供 AT+CMGS 校验）。 */
int pdu_submit_encode(unsigned char *out, int cap,
                      const unsigned char *sc, int sc_len,
                      const unsigned char *da, int da_len,
                      const unsigned char *text, int text_len,
                      int dcs, unsigned char mr,
                      int concat_ref, int concat_total, int concat_part,
                      int *udl_out);

/* 解析 SMS-DELIVER PDU（含 SMSC 字段，即 AT+CMGR/CMGL 返回的完整 PDU）。
 *   输出 oa（ASCII 号码，国际格式带 '+'）、scts（"YY/MM/DD,HH:MM:SS+ZZ"）、
 *   dcs、text（UTF-8 解码后的正文）、级联信息（无级联时 concat_total=1）。
 * 返回 0 成功 / -1 解析失败。 */
int pdu_deliver_decode(const unsigned char *pdu, int pdu_len,
                       unsigned char *oa, int oa_cap, int *oa_len,
                       unsigned char *scts, int scts_cap,
                       int *dcs,
                       unsigned char *text, int text_cap, int *text_len,
                       int *concat_ref, int *concat_total, int *concat_part);

#ifdef __cplusplus
}
#endif

#endif /* KUNXOS_PDU_H */
