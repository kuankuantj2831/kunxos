//! 第二组页面渲染与命中：网络/蓝牙/移动数据/VPN/NFC/勿扰/权限/账户/云/
//! 系统/关于/更新/日期时间/语言/开发者/电池/存储/辅助功能。

use super::*;

// ==================== 网络页 ====================

pub(crate) unsafe fn net_enabled() -> bool {
    cfg::cfg_get_int(cfg::CONF_NET, b"wifi_enabled", 0) != 0
}

pub(crate) unsafe fn bt_enabled() -> bool {
    cfg::cfg_get_int(cfg::CONF_NET, b"bt_enabled", 0) != 0
}

pub(crate) unsafe fn data_enabled() -> bool {
    cfg::cfg_get_int(cfg::CONF_NET, b"data_enabled", 0) != 0
}

pub(crate) unsafe fn render_net() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE7\xBD\x91\xE7\xBB\x9C\xE4\xB8\x8E\xE8\xBF\x9E\xE6\x8E\xA5");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let card_h = ui::card_h(5, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_row(ry, I_WIFI, b"Wi-Fi", b"", false, false);
    // Wi-Fi 状态行右侧文字。
    let state: &[u8] = if net_enabled() { b"\xE5\xB7\xB2\xE5\xBC\x80\xE5\x90\xAF" } else { b"\xE5\xB7\xB2\xE5\x85\xB3\xE9\x97\xAD" };
    let w = text_width_px(state, 14.0);
    draw_text_px(
        DISPLAY_W as i32 - MARGIN - w,
        text_top(14.0, ry + ROW_H / 2),
        state,
        14.0,
        C_SUB,
    );
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_SAVE, b"IP \xE8\xAE\xBE\xE7\xBD\xAE", b"DHCP", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_BT, b"\xE8\x93\x9D\xE7\x89\x99", b"", false, false);
    let st: &[u8] = if bt_enabled() { b"\xE5\xB7\xB2\xE5\xBC\x80\xE5\x90\xAF" } else { b"\xE5\xB7\xB2\xE5\x85\xB3\xE9\x97\xAD" };
    let w = text_width_px(st, 14.0);
    draw_text_px(
        DISPLAY_W as i32 - MARGIN - w,
        text_top(14.0, ry + ROW_H / 2),
        st,
        14.0,
        C_SUB,
    );
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_MOBILE, b"\xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE", b"", false, false);
    let st: &[u8] = if data_enabled() { b"\xE5\xB7\xB2\xE5\xBC\x80\xE5\x90\xAF" } else { b"\xE5\xB7\xB2\xE5\x85\xB3\xE9\x97\xAD" };
    let w = text_width_px(st, 14.0);
    draw_text_px(
        DISPLAY_W as i32 - MARGIN - w,
        text_top(14.0, ry + ROW_H / 2),
        st,
        14.0,
        C_SUB,
    );
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_VPN, b"VPN", b"\xE9\xA2\x84\xE7\x95\x99", true, false);
    y += card_h + HEADER_H;

    // 预留说明。
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"VPN / NFC \xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE6\x8E\xA5\xE5\x8F\xA3\xEF\xBC\x8C\xE4\xBE\x9D\xE8\xB5\x96\xE7\xBD\x91\xE7\xBB\x9C\xE5\x8D\x8F\xE8\xAE\xAE\xE6\xA0\x88\xE6\x8E\xA5\xE5\x85\xA5",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_wifi() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"Wi-Fi");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    // 开关行。
    ui::draw_row(ry, I_WIFI, b"Wi-Fi", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, net_enabled());
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_SAVE, b"\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98\xE7\xBD\x91\xE7\xBB\x9C", b"", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_INPUT, b"\xE6\x89\x8B\xE5\x8A\xA8\xE8\xBF\x9E\xE6\x8E\xA5", b"", true, false);
    y += card_h + HEADER_H;

    // 扫描 / 自动连接。
    ui::draw_group_header(y, b"\xE8\xBF\x9E\xE6\x8E\xA5");
    y += HEADER_H;
    let card_h2 = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_row(ry, I_WIFI, b"\xE6\x89\xAB\xE6\x8F\x8F\xE7\xBD\x91\xE7\xBB\x9C", b"", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_SYNC, b"\xE8\x87\xAA\xE5\x8A\xA8\xE8\xBF\x9E\xE6\x8E\xA5\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98", b"", true, false);
    y += card_h2 + HEADER_H;

    // 已保存网络清单。
    ui::draw_group_header(y, b"\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98\xE7\xBD\x91\xE7\xBB\x9C");
    y += HEADER_H;
    if SAVED_COUNT == 0 {
        let card_h3 = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h3, CARD_R);
        ui::draw_row(y + CARD_PAD, I_INFO, b"\xE6\x97\xA0\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98\xE7\xBD\x91\xE7\xBB\x9C", b"", false, false);
        y += card_h3 + HEADER_H;
    } else {
        let card_h3 = ui::card_h(SAVED_COUNT, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h3, CARD_R);
        for i in 0..SAVED_COUNT {
            let name = &SAVED_SSIDS[i];
            let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let ry = y + CARD_PAD + i as i32 * ROW_H;
            ui::draw_row(ry, I_SAVE, &name[..ln], b"", true, false);
            if i + 1 < SAVED_COUNT {
                ui::draw_divider(ry);
            }
        }
        y += card_h3 + HEADER_H;
    }
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_ip() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"IP \xE8\xAE\xBE\xE7\xBD\xAE");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let mode_dhcp = ip_mode_dhcp();
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_radio_row(y + CARD_PAD, b"\xE8\x87\xAA\xE5\x8A\xA8\xEF\xBC\x88DHCP\xEF\xBC\x89", mode_dhcp);
    y += card_h + HEADER_H;

    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    let mut v0 = [0u8; 32];
    let v0l = cfg::cfg_get_str(cfg::CONF_NET, b"ip_addr", b"192.168.1.100", &mut v0);
    ui::draw_picker_row(ry, b"IP \xE5\x9C\xB0\xE5\x9D\x80", &v0[..v0l], !mode_dhcp);
    ui::draw_divider(ry);
    ry += ROW_H;
    let mut v1 = [0u8; 32];
    let v1l = cfg::cfg_get_str(cfg::CONF_NET, b"ip_gw", b"192.168.1.1", &mut v1);
    ui::draw_picker_row(ry, b"\xE7\xBD\x91\xE5\x85\xB3", &v1[..v1l], !mode_dhcp);
    ui::draw_divider(ry);
    ry += ROW_H;
    let mut v2 = [0u8; 32];
    let v2l = cfg::cfg_get_str(cfg::CONF_NET, b"ip_dns", b"10.0.2.3", &mut v2);
    ui::draw_picker_row(ry, b"DNS", &v2[..v2l], !mode_dhcp);
    y += card_h + HEADER_H;

    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h / 2),
        b"\xE9\x9D\x99\xE6\x80\x81 IP \xE9\x85\x8D\xE7\xBD\xAE\xE8\x90\xBD\xE7\x9B\x98\xEF\xBC\x9B\xE5\x86\x85\xE6\xA0\xB8\xE7\xBD\x91\xE7\xBB\x9C\xE6\xA0\x88\xE6\x8E\xA5\xE5\x85\xA5\xE9\xA2\x84\xE7\x95\x99\xEF\xBC\x88\xE5\xBD\x93\xE5\x89\x8D\xE7\x94\xA8 QEMU slirp \xE9\xBB\x98\xE8\xAE\xA4\xE5\x80\xBC\xEF\xBC\x89",
        13.0,
        C_SUB,
    );
    y += card_h + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

unsafe fn ip_mode_dhcp() -> bool {
    let mut v = [0u8; 8];
    let n = cfg::cfg_get_str(cfg::CONF_NET, b"ip_mode", b"dhcp", &mut v);
    n > 0 && v[0] == b'd'
}

pub(crate) unsafe fn render_bt() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\x93\x9D\xE7\x89\x99");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_BT, b"\xE8\x93\x9D\xE7\x89\x99", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, y + CARD_PAD + (ROW_H - 28) / 2, bt_enabled());
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE8\x93\x9D\xE7\x89\x99\xE4\xBE\x9D\xE8\xB5\x96\xE6\x97\xA0\xE7\xBA\xBF\xE5\x8D\x8F\xE8\xAE\xAE\xE6\xA0\x88\xEF\xBC\x88/dev/hci\xEF\xBC\x89\xEF\xBC\x8C\xE9\x85\x8D\xE5\xAF\xB9/\xE5\x8F\xAF\xE8\xA7\x81\xE6\x80\xA7\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE6\x8E\xA5\xE5\x8F\xA3",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_mobile() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_MOBILE, b"\xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, y + CARD_PAD + (ROW_H - 28) / 2, data_enabled());
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE/APN \xE4\xBE\x9D\xE8\xB5\x96\xE7\x94\xB5\xE8\xAF\x9D\xE5\xAD\x90\xE7\xB3\xBB\xE7\xBB\x9F\xEF\xBC\x88rild \xE6\x95\xB0\xE6\x8D\xAE PDN\xEF\xBC\x89\xEF\xBC\x8C\xE5\xBD\x93\xE5\x89\x8D\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE5\xBC\x80\xE5\x85\xB3",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_vpn() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"VPN");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_VPN, b"VPN \xE9\x85\x8D\xE7\xBD\xAE\xE6\x96\x87\xE4\xBB\xB6", b"\xE6\x97\xA0", false, false);
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"VPN \xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE6\x8E\xA5\xE5\x8F\xA3\xEF\xBC\x8C\xE5\x90\x8E\xE7\xBB\xAD\xE6\x8E\xA5\xE5\x85\xA5\xE5\x86\x85\xE6\xA0\xB8\xE7\xBD\x91\xE7\xBB\x9C\xE6\xA0\x88\xE8\xB7\xAF\xE7\x94\xB1",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_nfc() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"NFC");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_WIFI, b"NFC", b"\xE6\x9C\xAA\xE5\x90\xAF\xE7\x94\xA8", false, false);
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"NFC \xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE6\x8E\xA5\xE5\x8F\xA3\xEF\xBC\x88\xE6\x97\xA0\xE7\xA1\xAC\xE4\xBB\xB6\xEF\xBC\x89",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 勿扰页 ====================

pub(crate) unsafe fn render_dnd() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\x8B\xBF\xE6\x89\xB0\xE6\xA8\xA1\xE5\xBC\x8F");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_MOON, b"\xE5\x8B\xBF\xE6\x89\xB0\xE6\xA8\xA1\xE5\xBC\x8F", b"", false, false);
    ui::draw_switch(
        DISPLAY_W as i32 - MARGIN - 48,
        y + CARD_PAD + (ROW_H - 28) / 2,
        cfg::cfg_get_int(cfg::CONF_SOUND, b"dnd_enabled", 0) != 0,
    );
    y += card_h + HEADER_H;

    let st = cfg::cfg_get_int(cfg::CONF_SOUND, b"dnd_start", 22);
    let en = cfg::cfg_get_int(cfg::CONF_SOUND, b"dnd_end", 7);
    let mut v1 = [0u8; 2];
    v1[0] = b'0' + (st / 10) as u8;
    v1[1] = b'0' + (st % 10) as u8;
    let mut v2 = [0u8; 2];
    v2[0] = b'0' + (en / 10) as u8;
    v2[1] = b'0' + (en % 10) as u8;
    let card_h2 = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    ui::draw_stepper_row(y + CARD_PAD, b"\xE5\xBC\x80\xE5\xA7\x8B\xE6\x97\xB6\xE9\x97\xB4", &v1[..2]);
    ui::draw_divider(y + CARD_PAD);
    ui::draw_stepper_row(y + CARD_PAD + ROW_H, b"\xE7\xBB\x93\xE6\x9D\x9F\xE6\x97\xB6\xE9\x97\xB4", &v2[..2]);
    y += card_h2 + HEADER_H;

    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h3 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h3, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h3 / 2),
        b"\xE5\x8B\xBF\xE6\x89\xB0\xE5\xAE\x9A\xE6\x97\xB6\xE4\xBE\x9D\xE8\xB5\x96 lifed\xEF\xBC\x88\xE4\xBA\x8B\xE4\xBB\xB6\xE4\xBF\xA1\xE7\xAE\xB1\xEF\xBC\x89\xEF\xBC\x8C\xE5\xBD\x93\xE5\x89\x8D\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE9\x85\x8D\xE7\xBD\xAE",
        13.0,
        C_SUB,
    );
    y += card_h3 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

unsafe fn fmt2(v: i32, out: &mut [u8; 2]) -> [u8; 2] {
    out[0] = b'0' + (v / 10) as u8;
    out[1] = b'0' + (v % 10) as u8;
    *out
}

// ==================== 权限页 ====================

pub(crate) unsafe fn render_perms() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xBA\x94\xE7\x94\xA8\xE6\x9D\x83\xE9\x99\x90");
    let s = unsafe { SCROLL };
    let y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    if APP_COUNT == 0 {
        let card_h = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
        ui::draw_row(y + CARD_PAD, I_INFO, b"\xE6\x97\xA0\xE5\xBA\x94\xE7\x94\xA8\xE6\xB8\x85\xE5\x8D\x95", b"", false, false);
        CONTENT_H = card_h;
        return;
    }
    let card_h = ui::card_h(APP_COUNT, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    for i in 0..APP_COUNT {
        let name = &APP_NAMES[i];
        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let ry = y + CARD_PAD + i as i32 * ROW_H;
        ui::draw_row(ry, I_APPS, &name[..ln], b"\xE6\x9F\xA5\xE7\x9C\x8B", true, false);
        if i + 1 < APP_COUNT {
            ui::draw_divider(ry);
        }
    }
    CONTENT_H = card_h;
}

pub(crate) unsafe fn render_perm() {
    ui::draw_gradient_bg();
    let app = PERM_APP;
    let name = &APP_NAMES[app];
    let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    ui::draw_title_bar(&name[..ln]);
    let s = unsafe { SCROLL };
    let y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    if PERM_LEN == 0 {
        let card_h = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
        ui::draw_row(y + CARD_PAD, I_INFO, b"\xE6\x97\xA0\xE6\x9D\x83\xE9\x99\x90\xE5\xA3\xB0\xE6\x98\x8E", b"", false, false);
        CONTENT_H = card_h;
        return;
    }
    // 解析 PERM_BUF 行为行（冒号前为权限名）。
    let mut lines = 0usize;
    let mut i = 0;
    while i < PERM_LEN {
        let e = PERM_BUF[i..PERM_LEN]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| i + p)
            .unwrap_or(PERM_LEN);
        let line = &PERM_BUF[i..e];
        if !line.is_empty() {
            lines += 1;
        }
        i = e + 1;
    }
    let card_h = ui::card_h(lines, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ri = 0;
    let mut i = 0;
    while i < PERM_LEN {
        let e = PERM_BUF[i..PERM_LEN]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| i + p)
            .unwrap_or(PERM_LEN);
        let line = &PERM_BUF[i..e];
        if !line.is_empty() {
            // 权限名 = "Perm:" 前的部分；去掉 "granted"。
            let label: &[u8] = if let Some(pos) = line.iter().position(|&c| c == b':') {
                &line[..pos]
            } else {
                line
            };
            let ry = y + CARD_PAD + ri as i32 * ROW_H;
            ui::draw_row(ry, I_SAVE, label, b"\xE6\x92\xA4\xE9\x94\x80", true, false);
            if ri + 1 < lines {
                ui::draw_divider(ry);
            }
            ri += 1;
        }
        i = e + 1;
    }
    CONTENT_H = card_h;
}

// ==================== 账户页 ====================

pub(crate) unsafe fn render_acct() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\xB4\xA6\xE6\x88\xB7");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let mut dn = [0u8; 32];
    let dnl = cfg::cfg_get_str(cfg::CONF_ACCT, b"device_name", b"KunxPhone", &mut dn);
    let mut lu = [0u8; 32];
    let lul = cfg::cfg_get_str(cfg::CONF_ACCT, b"local_user", b"user", &mut lu);

    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_row(ry, I_USER, b"\xE8\xAE\xBE\xE5\xA4\x87\xE5\x90\x8D\xE7\xA7\xB0", &dn[..dnl], true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_USER, b"\xE6\x9C\xAC\xE5\x9C\xB0\xE8\xB4\xA6\xE6\x88\xB7", &lu[..lul], true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    let on = cfg::cfg_get_int(cfg::CONF_ACCT, b"sync_enabled", 1) != 0;
    ui::draw_row(ry, I_SYNC, b"\xE8\xB4\xA6\xE6\x88\xB7\xE5\x90\x8C\xE6\xAD\xA5", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
    y += card_h + HEADER_H;

    // 云服务。
    ui::draw_group_header(y, b"\xE4\xBA\x91\xE6\x9C\x8D\xE5\x8A\xA1");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    let mut ca = [0u8; 64];
    let cal = cfg::cfg_get_str(cfg::CONF_ACCT, b"cloud_account", b"", &mut ca);
    let status: &[u8] = if cal > 0 { b"\xE5\xB7\xB2\xE7\x99\xBB\xE5\xBD\x95" } else { b"\xE6\x9C\xAA\xE7\x99\xBB\xE5\xBD\x95" };
    ui::draw_row(y + CARD_PAD, I_CLOUD, b"\xE4\xBA\x91\xE6\x9C\x8D\xE5\x8A\xA1\xE8\xB4\xA6\xE6\x88\xB7", status, true, false);
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_cloud() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE4\xBA\x91\xE6\x9C\x8D\xE5\x8A\xA1\xE7\x99\xBB\xE5\xBD\x95");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_row(ry, I_CLOUD, b"\xE7\x99\xBB\xE5\xBD\x95", b"\xE8\xBE\x93\xE5\x85\xA5\xE7\x94\xA8\xE6\x88\xB7\xE5\x90\x8D/\xE5\xAF\x86\xE7\xA0\x81", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_RESET, b"\xE9\x80\x80\xE5\x87\xBA\xE7\x99\xBB\xE5\xBD\x95", b"", true, false);
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE4\xBA\x91\xE6\x9C\x8D\xE5\x8A\xA1\xE4\xBE\x9D\xE8\xB5\x96 apps/cloud \xE7\xBD\x91\xE5\x85\xB3\xEF\xBC\x9B\xE7\x99\xBB\xE5\xBD\x95\xE5\x90\x8E\xE5\x86\x99\xE5\x9B\x9E /data/oobe/cloud_account",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 系统页 ====================

pub(crate) unsafe fn render_system() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE7\xB3\xBB\xE7\xBB\x9F");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let rows: [(&[u8], usize); 6] = [
        (b"\xE5\x85\xB3\xE4\xBA\x8E", I_PHONE),
        (b"\xE8\xBD\xAF\xE4\xBB\xB6\xE6\x9B\xB4\xE6\x96\xB0", I_UPDATE),
        (b"\xE6\x97\xA5\xE6\x9C\x9F\xE4\xB8\x8E\xE6\x97\xB6\xE9\x97\xB4", I_CLOCK),
        (b"\xE8\xAF\xAD\xE8\xA8\x80", I_LANG),
        (b"\xE9\x94\xAE\xE7\x9B\x98", I_KBD),
        (b"\xE5\xAF\xBC\xE8\x88\xAA\xE6\x96\xB9\xE5\xBC\x8F", I_NAV),
    ];
    let card_h = ui::card_h(6, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon)) in rows.iter().enumerate() {
        let value: &[u8] = match i {
            0 => b"KUNXOS 0.9.0",
            1 => b"\xE5\xB7\xB2\xE6\x98\xAF\xE6\x9C\x80\xE6\x96\xB0",
            3 => if lang_is_en() { b"English" } else { b"\xE7\xAE\x80\xE4\xBD\x93\xE4\xB8\xAD\xE6\x96\x87" },
            5 => if sys_nav::nav_mode() == sys_nav::MODE_BUTTONS { b"\xE4\xB8\x89\xE9\x94\xAE" } else { b"\xE6\x89\x8B\xE5\x8A\xBF" },
            _ => b"",
        };
        ui::draw_row(ry, *icon, title, value, true, false);
        if i + 1 < 6 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;

    // 应用管理 / 重置 / 开发者选项。
    let card_h2 = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_row(ry, I_APPS, b"\xE5\xBA\x94\xE7\x94\xA8\xE7\xAE\xA1\xE7\x90\x86", b"", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_row(ry, I_RESET, b"\xE9\x87\x8D\xE7\xBD\xAE", b"", true, false);
    ui::draw_divider(ry);
    ry += ROW_H;
    let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"usb_debug", 0) != 0;
    ui::draw_row(ry, I_GEAR, b"\xE5\xBC\x80\xE5\x8F\x91\xE8\x80\x85\xE9\x80\x89\xE9\xA1\xB9", if on { b"\xE5\xB7\xB2\xE5\xBC\x80\xE5\x90\xAF" } else { b"\xE5\xB7\xB2\xE5\x85\xB3\xE9\x97\xAD" }, true, false);
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

unsafe fn lang_is_en() -> bool {
    let mut v = [0u8; 8];
    let n = cfg::cfg_get_str(cfg::CONF_SYSTEM, b"lang", b"zh_CN", &mut v);
    n > 0 && find_sub(&v[..n], b"en")
}

pub(crate) unsafe fn render_about() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\x85\xB3\xE4\xBA\x8E");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let rows: [(&[u8], &[u8]); 6] = [
        (b"\xE8\xAE\xBE\xE5\xA4\x87\xE5\x90\x8D\xE7\xA7\xB0", b"KunxPhone"),
        (b"\xE5\x9E\x8B\xE5\x8F\xB7", b"Kunx-AC (QEMU virt)"),
        (b"\xE7\xB3\xBB\xE7\xBB\x9F\xE7\x89\x88\xE6\x9C\xAC", b"KUNXOS 0.9.0"),
        (b"\xE5\x86\x85\xE6\xA0\xB8\xE7\x89\x88\xE6\x9C\xAC", b"KunxOS 0.1.0 aarch64"),
        (b"\xE6\x9E\x84\xE5\xBB\xBA\xE5\x8F\xB7", b"OTA-42"),
        (b"Android \xE7\x89\x88\xE6\x9C\xAC", b"-"),
    ];
    let card_h = ui::card_h(6, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, value)) in rows.iter().enumerate() {
        ui::draw_picker_row(ry, title, value, false);
        if i + 1 < 6 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_update() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\xBD\xAF\xE4\xBB\xB6\xE6\x9B\xB4\xE6\x96\xB0");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    if OTA_COUNT == 0 {
        let card_h = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
        ui::draw_row(y + CARD_PAD, I_UPDATE, b"\xE5\xB7\xB2\xE6\x98\xAF\xE6\x9C\x80\xE6\x96\xB0\xE7\x89\x88\xE6\x9C\xAC", b"", false, false);
        y += card_h + HEADER_H;
    } else {
        let card_h = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
        let name = &OTA_PKGS[0];
        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        ui::draw_row(y + CARD_PAD, I_UPDATE, &name[..ln], b"\xE5\x8F\x91\xE7\x8E\xB0\xE6\x9B\xB4\xE6\x96\xB0\xE5\x8C\x85", false, false);
        y += card_h + HEADER_H;
    }

    // 按钮。
    let by = y;
    ui::draw_button(by, b"\xE6\xA3\x80\xE6\x9F\xA5\xE6\x9B\xB4\xE6\x96\xB0", true);
    let by = by + BTN_H + CARD_PAD;
    ui::draw_button_plain(by, b"\xE5\xAE\x89\xE8\xA3\x85\xE6\x9B\xB4\xE6\x96\xB0\xEF\xBC\x88.kxup\xEF\xBC\x89");
    let by = by + BTN_H + CARD_PAD;
    ui::draw_button_plain(by, b"\xE6\x9F\xA5\xE7\x9C\x8B OTA \xE7\x8A\xB6\xE6\x80\x81");
    let by = by + BTN_H + CARD_PAD;
    ui::draw_button(by, b"\xE7\xAB\x8B\xE5\x8D\xB3\xE9\x87\x8D\xE5\x90\xAF\xE7\xB3\xBB\xE7\xBB\x9F", false);
    y = by + BTN_H + HEADER_H;

    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h / 2),
        b"OTA \xE4\xBE\x9D\xE8\xB5\x96 /data/ota/*.kxup \xE5\x8C\x85\xE4\xB8\x8E\xE5\x86\x85\xE6\xA0\xB8 syscall 430",
        13.0,
        C_SUB,
    );
    y += card_h + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_dt() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE6\x97\xA5\xE6\x9C\x9F\xE4\xB8\x8E\xE6\x97\xB6\xE9\x97\xB4");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let rows: [(&[u8], usize); 3] = [
        (b"\xE8\x87\xAA\xE5\x8A\xA8\xE8\xAE\xBE\xE7\xBD\xAE", I_SYNC),
        (b"\xE6\x97\xB6\xE5\x8C\xBA", I_CLOCK),
        (b"24 \xE5\xB0\x8F\xE6\x97\xB6\xE5\x88\xB6", I_CLOCK),
    ];
    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon)) in rows.iter().enumerate() {
        match i {
            0 => {
                let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"auto_time", 1) != 0;
                ui::draw_row(ry, *icon, title, b"", false, false);
                ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
            }
            1 => {
                let mut tz = [0u8; 32];
                let tzl = cfg::cfg_get_str(cfg::CONF_SYSTEM, b"timezone", b"Asia/Shanghai", &mut tz);
                ui::draw_row(ry, *icon, title, &tz[..tzl], true, false);
            }
            _ => {
                let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"fmt_24h", 1) != 0;
                ui::draw_row(ry, *icon, title, if on { b"24 \xE5\xB0\x8F\xE6\x97\xB6" } else { b"12 \xE5\xB0\x8F\xE6\x97\xB6" }, false, false);
            }
        }
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_lang() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\xAF\xAD\xE8\xA8\x80");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    ui::draw_radio_row(y + CARD_PAD, b"\xE7\xAE\x80\xE4\xBD\x93\xE4\xB8\xAD\xE6\x96\x87", !lang_is_en());
    ui::draw_divider(y + CARD_PAD);
    ui::draw_radio_row(y + CARD_PAD + ROW_H, b"English", lang_is_en());
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE8\xAF\xAD\xE8\xA8\x80\xE5\x86\x99\xE5\x9B\x9E /data/oobe/lang\xEF\xBC\x9B\xE6\x9C\xAC\xE5\xBA\x94\xE7\x94\xA8\xE4\xB8\x8E\xE5\x85\xA8\xE7\xB3\xBB\xE7\xBB\x9F\xE5\xA4\x9A\xE8\xAF\xAD\xE8\xA8\x80\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

pub(crate) unsafe fn render_devops() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xBC\x80\xE5\x8F\x91\xE8\x80\x85\xE9\x80\x89\xE9\xA1\xB9");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }
    let card_h = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"usb_debug", 0) != 0;
    ui::draw_row(ry, I_GEAR, b"USB \xE8\xB0\x83\xE8\xAF\x95", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
    ui::draw_divider(ry);
    ry += ROW_H;
    let on2 = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"oem_unlock", 0) != 0;
    ui::draw_row(ry, I_GEAR, b"OEM \xE8\xA7\xA3\xE9\x94\x81", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on2);
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"USB \xE8\xB0\x83\xE8\xAF\x95 / OEM \xE8\xA7\xA3\xE9\x94\x81\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE9\x85\x8D\xE7\xBD\xAE\xEF\xBC\x88\xE4\xBE\x9D\xE8\xB5\x96\xE5\x86\x85\xE6\xA0\xB8/\xE5\xBC\x95\xE5\xAF\xBC\xE9\x93\xBE\xE6\x94\xAF\xE6\x8C\x81\xEF\xBC\x89",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 电池页 ====================

pub(crate) unsafe fn render_battery() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE7\x94\xB5\xE6\xB1\xA0");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    // 状态卡：syscall 410 打包位。
    let pm = cfg::refresh_pm();
    let pct = ((pm >> 8) & 0xFF) as i32;
    let bstate = ((pm >> 35) & 0x3) as i32;
    let state_label: &[u8] = match bstate {
        1 => b"\xE5\x85\x85\xE7\x94\xB5\xE4\xB8\xAD",
        2 => b"\xE5\xB7\xB2\xE5\x85\x85\xE6\xBB\xA1",
        _ => b"\xE6\x94\xBE\xE7\x94\xB5\xE4\xB8\xAD",
    };
    let mut num = [0u8; 12];
    let bn = fmt_dec(&mut num, pct as u32);
    let mut vv = [0u8; 16];
    vv[..bn].copy_from_slice(&num[..bn]);
    vv[bn] = b'%';
    vv[bn + 1] = b' ';
    let mut sn = 0;
    for &c in state_label {
        vv[bn + 2 + sn] = c;
        sn += 1;
    }
    let card_h = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let w = text_width_px(&vv[..bn + 2 + sn], 14.0);
    draw_text_px(
        DISPLAY_W as i32 - MARGIN - w,
        text_top(14.0, y + card_h / 2),
        &vv[..bn + 2 + sn],
        14.0,
        C_SUB,
    );
    draw_text_px(
        MARGIN + 4,
        text_top(font_px(), y + card_h / 2),
        b"\xE7\x94\xB5\xE9\x87\x8F",
        font_px(),
        ui::c_text(),
    );
    y += card_h + HEADER_H;

    let card_h2 = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    let on = cfg::cfg_get_int(cfg::CONF_BATTERY, b"show_percent", 1) != 0;
    ui::draw_row(ry, I_BATT, b"\xE7\x94\xB5\xE9\x87\x8F\xE7\x99\xBE\xE5\x88\x86\xE6\xAF\x94", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
    ui::draw_divider(ry);
    ry += ROW_H;
    let on2 = cfg::cfg_get_int(cfg::CONF_BATTERY, b"low_power", 0) != 0;
    ui::draw_row(ry, I_BATT, b"\xE4\xBD\x8E\xE7\x94\xB5\xE9\x87\x8F\xE6\xA8\xA1\xE5\xBC\x8F", b"", false, false);
    ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on2);
    y += card_h2 + HEADER_H;

    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h3 = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h3, CARD_R);
    let ry = y + CARD_PAD;
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, ry + ROW_H / 2),
        b"\xE7\x94\xB5\xE9\x87\x8F\xE7\xBB\x8F syscall 410 \xE5\xAE\x9E\xE6\x97\xB6\xE8\xAF\xBB\xE5\x8F\x96\xEF\xBC\x9B\xE4\xBD\x8E\xE7\x94\xB5\xE9\x87\x8F\xE6\xA8\xA1\xE5\xBC\x8F\xE5\x88\x87 governor \xE4\xB8\xBA powersave",
        13.0,
        C_SUB,
    );
    ui::draw_divider(ry);
    let ry = ry + ROW_H;
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, ry + ROW_H / 2),
        b"\xE6\x8C\x89\xE5\xBA\x94\xE7\x94\xA8\xE8\x80\x97\xE7\x94\xB5\xE7\xBB\x9F\xE8\xAE\xA1\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xEF\xBC\x88\xE9\x9C\x80\xE7\x94\xB5\xE6\xB1\xA0/\xE8\xBF\x9B\xE7\xA8\x8B\xE7\xBB\x9F\xE8\xAE\xA1\xE5\x90\x8E\xE7\xAB\xAF\xEF\xBC\x89",
        13.0,
        C_SUB,
    );
    y += card_h3 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 存储页 ====================

unsafe fn statfs_bytes(p: &[u8]) -> (u64, u64) {
    unsafe {
        let mut path = [0u8; 64];
        let n = p.len().min(path.len() - 1);
        path[..n].copy_from_slice(p);
        path[n] = 0;
        let mut st = [0u8; 120];
        let r = syscall2(SYS_STATFS, path.as_ptr() as usize, st.as_mut_ptr() as usize);
        if r != 0 {
            return (0, 0);
        }
        let bsize = i64::from_le_bytes([st[8], st[9], st[10], st[11], st[12], st[13], st[14], st[15]]) as u64;
        let blocks = i64::from_le_bytes([st[16], st[17], st[18], st[19], st[20], st[21], st[22], st[23]]) as u64;
        let bfree = i64::from_le_bytes([st[24], st[25], st[26], st[27], st[28], st[29], st[30], st[31]]) as u64;
        (blocks.saturating_mul(bsize), bfree.saturating_mul(bsize))
    }
}

unsafe fn fmt_bytes(buf: &mut [u8], v: u64) -> usize {
    // 输出如 "12.3 GB"。
    let mut b = [0u8; 16];
    let gb = v / (1024 * 1024 * 1024);
    let frac = (v % (1024 * 1024 * 1024)) * 10 / (1024 * 1024 * 1024);
    if gb >= 100 {
        let n = fmt_dec(&mut b, gb as u32);
        buf[..n].copy_from_slice(&b[..n]);
        return n;
    }
    let n = fmt_dec(&mut b, gb as u32);
    buf[..n].copy_from_slice(&b[..n]);
    buf[n] = b'.';
    buf[n + 1] = b'0' + (frac % 10) as u8;
    for &c in b" GB" {
        buf[n + 2] = c;
        // 越界保护：循环只填充一次。
        break;
    }
    n + 6
}

pub(crate) unsafe fn render_storage() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xAD\x98\xE5\x82\xA8");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let (total, free) = statfs_bytes(b"/data");
    let used = total.saturating_sub(free);
    let pct = if total > 0 { (used * 100 / total) as i32 } else { 0 };

    let mut tbuf = [0u8; 16];
    let tl = fmt_bytes(&mut tbuf, total);
    let mut ubuf = [0u8; 16];
    let ul = fmt_bytes(&mut ubuf, used);
    let mut fbuf = [0u8; 16];
    let fl = fmt_bytes(&mut fbuf, free);

    // 总/已用/剩余。
    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    ui::draw_picker_row(ry, b"\xE6\x80\xBB\xE7\xA9\xBA\xE9\x97\xB4", &tbuf[..tl], false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_picker_row(ry, b"\xE5\xB7\xB2\xE7\x94\xA8", &ubuf[..ul], false);
    ui::draw_divider(ry);
    ry += ROW_H;
    ui::draw_picker_row(ry, b"\xE5\x89\xA9\xE4\xBD\x99", &fbuf[..fl], false);
    y += card_h + HEADER_H;

    // 使用率条。
    let mut num = [0u8; 12];
    let bn = fmt_dec(&mut num, pct as u32);
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 4,
        text_top(font_px(), y + card_h2 / 2),
        b"\xE4\xBD\xBF\xE7\x94\xA8\xE7\x8E\x87",
        font_px(),
        ui::c_text(),
    );
    let mut vv = [0u8; 8];
    vv[..bn].copy_from_slice(&num[..bn]);
    vv[bn] = b'%';
    let w = text_width_px(&vv[..bn + 1], 14.0);
    draw_text_px(DISPLAY_W as i32 - MARGIN - w, text_top(14.0, y + card_h2 / 2), &vv[..bn + 1], 14.0, C_SUB);
    let bx = MARGIN + 8;
    let bw = CARD_W - 16;
    let by = y + card_h2 - 18;
    let track = if dark_on() { T_SW_OFF_D } else { T_SW_OFF_L };
    fill_round_rect(bx, by, bw, 8, 4, track);
    fill_round_rect(bx, by, (bw * pct / 100).max(4), 8, 4, C_PRIMARY);
    y += card_h2 + HEADER_H;

    // 缓存清理。
    ui::draw_group_header(y, b"\xE7\xBC\x93\xE5\xAD\x98");
    y += HEADER_H;
    let card_h3 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h3, CARD_R);
    ui::draw_row(y + CARD_PAD, I_RESET, b"\xE6\xB8\x85\xE9\x99\xA4\xE7\xBC\x93\xE5\xAD\x98", b"", true, false);
    y += card_h3 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 辅助功能页 ====================

pub(crate) unsafe fn render_access() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\xBE\x85\xE5\x8A\xA9\xE5\x8A\x9F\xE8\x83\xBD");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let rows: [(&[u8], usize, &[u8]); 3] = [
        (b"\xE6\x94\xBE\xE5\xA4\xA7\xE5\x99\xA8", I_ACC, b"magnifier"),
        (b"\xE9\xA2\x9C\xE8\x89\xB2\xE5\x8F\x8D\xE8\xBD\xAC", I_EYE, b"invert_colors"),
        (b"\xE9\xAB\x98\xE5\xAF\xB9\xE6\xAF\x94\xE5\xBA\xA6", I_EYE, b"high_contrast"),
    ];
    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon, key)) in rows.iter().enumerate() {
        let on = cfg::cfg_get_int(cfg::CONF_ACCESS, key, 0) != 0;
        ui::draw_row(ry, *icon, title, b"", false, false);
        ui::draw_switch(DISPLAY_W as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, CARD_W, card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE9\xA2\x9C\xE8\x89\xB2\xE5\x8F\x8D\xE8\xBD\xAC/\xE9\xAB\x98\xE5\xAF\xB9\xE6\xAF\x94\xE5\xBA\xA6\xE5\x9C\xA8\xE6\x9C\xAC\xE5\xBA\x94\xE7\x94\xA8\xE5\x86\x85\xE5\x8D\xB3\xE6\x97\xB6\xE7\x94\x9F\xE6\x95\x88\xEF\xBC\x88flip LUT\xEF\xBC\x89\xEF\xBC\x8C\xE5\x85\xA8\xE7\xB3\xBB\xE7\xBB\x9F\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 第二组命中 ====================

/// 由 pages::handle_tap 转发到这里。
pub(crate) unsafe fn tap(page: usize, x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let _ = y;
    match page {
        PG_NET => tap_net(x, y),
        PG_WIFI => tap_wifi(x, y),
        PG_IP => tap_ip(x, y),
        PG_BT | PG_MOBILE | PG_VPN | PG_NFC => tap_toggle_page(page, x, y),
        PG_DND => tap_dnd(x, y),
        PG_PERMS => tap_perms(x, y),
        PG_PERM => tap_perm(x, y),
        PG_ACCT => tap_acct(x, y),
        PG_CLOUD => tap_cloud(x, y),
        PG_SYSTEM => tap_system(x, y),
        PG_ABOUT | PG_UPDATE | PG_DT | PG_LANG | PG_DEVOPS => tap_simple(page, x, y),
        PG_BATTERY => tap_battery(x, y),
        PG_STORAGE => tap_storage(x, y),
        PG_ACCESS => tap_access(x, y),
        _ => TapEffect::None,
    }
}

unsafe fn tap_simple(page: usize, x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let back = ui::back_hit(x, y);
    if back {
        page_pop();
        return TapEffect::Redraw;
    }
    match page {
        PG_UPDATE => tap_update(x, y),
        PG_DT => tap_dt(x, y),
        PG_LANG => tap_lang(x, y),
        PG_DEVOPS => tap_devops(x, y),
        _ => TapEffect::None,
    }
}

unsafe fn tap_net(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                page_push(PG_WIFI);
                TapEffect::Redraw
            }
            1 => {
                page_push(PG_IP);
                TapEffect::Redraw
            }
            2 => {
                page_push(PG_BT);
                TapEffect::Redraw
            }
            3 => {
                page_push(PG_MOBILE);
                TapEffect::Redraw
            }
            4 => {
                page_push(PG_VPN);
                TapEffect::Redraw
            }
            _ => TapEffect::None,
        }
    } else {
        TapEffect::None
    }
}

unsafe fn tap_wifi(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    // 开关行。
    if y >= top + CARD_PAD && y < top + CARD_PAD + ROW_H {
        let on = !cfg::cfg_get_int(cfg::CONF_NET, b"wifi_enabled", 0) != 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_NET, b"wifi_enabled", if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    let dy = y - (top + 3 * ROW_H + HEADER_H + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                // 扫描网络：wifid SCAN。
                unsafe { cfg::wifi_cmd(b"SCAN"); }
                toast(b"\xE5\xB7\xB2\xE8\xAF\xB7\xE6\xB1\x82\xE6\x89\xAB\xE6\x8F\x8F");
                TapEffect::Redraw
            }
            1 => {
                // 自动连接已保存。
                unsafe { cfg::wifi_cmd(b"AUTO"); }
                toast(b"\xE5\xB7\xB2\xE8\xAF\xB7\xE6\xB1\x82\xE8\x87\xAA\xE5\x8A\xA8\xE8\xBF\x9E\xE6\x8E\xA5");
                TapEffect::Redraw
            }
            _ => TapEffect::None,
        }
    } else {
        // 已保存网络区。
        let base = LIST_TOP - unsafe { SCROLL } + 5 * ROW_H + 2 * HEADER_H;
        let dy = y - (base + CARD_PAD);
        if dy >= 0 {
            let r = (dy / ROW_H) as usize;
            if r < SAVED_COUNT {
                // 直接连接该已保存网络。
                let ssid = &SAVED_SSIDS[r];
                let ln = ssid.iter().position(|&c| c == 0).unwrap_or(ssid.len());
                let mut line = [0u8; 128];
                let mut n = 0;
                for &c in b"CONNECT " {
                    line[n] = c;
                    n += 1;
                }
                for &c in &ssid[..ln] {
                    line[n] = c;
                    n += 1;
                    if n >= line.len() - 2 {
                        break;
                    }
                }
                unsafe { cfg::wifi_cmd(&line[..n]); }
                toast(b"\xE5\xB7\xB2\xE8\xAF\xB7\xE6\xB1\x82\xE8\xBF\x9E\xE6\x8E\xA5");
                return TapEffect::Redraw;
            }
        }
        TapEffect::None
    }
}

unsafe fn tap_ip(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        // 切换 DHCP / 静态。
        let v: &[u8] = if ip_mode_dhcp() { b"static" } else { b"dhcp" };
        unsafe { cfg::cfg_set(cfg::CONF_NET, b"ip_mode", v); }
        return TapEffect::Redraw;
    }
    let base = top + ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        let ctx = match r {
            0 => crate::pages::Ctx::IpAddr,
            1 => crate::pages::Ctx::IpGw,
            2 => crate::pages::Ctx::IpDns,
            _ => return TapEffect::None,
        };
        crate::pages::open_input(ctx);
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_toggle_page(page: usize, x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        let key: &[u8] = match page {
            PG_BT => b"bt_enabled",
            PG_MOBILE => b"data_enabled",
            _ => return TapEffect::None,
        };
        let on = cfg::cfg_get_int(cfg::CONF_NET, key, 0) == 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_NET, key, if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_dnd(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        let on = cfg::cfg_get_int(cfg::CONF_SOUND, b"dnd_enabled", 0) == 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_SOUND, b"dnd_enabled", if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    let base = top + ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        let key: &[u8] = if r == 0 { b"dnd_start" } else { b"dnd_end" };
        // 步进：左侧 -，右侧 +。
        let bx = DISPLAY_W as i32 - MARGIN - 150;
        let val = cfg::cfg_get_int(cfg::CONF_SOUND, key, 22);
        let new: i32 = if x < bx + 46 {
            if val > 0 { val - 1 } else { 23 }
        } else {
            if val < 23 { val + 1 } else { 0 }
        };
        unsafe { cfg::cfg_set_int(cfg::CONF_SOUND, key, new); }
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_perms(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r < APP_COUNT {
            load_perms(r);
            PERM_APP = r;
            page_push(PG_PERM);
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

/// 加载 /var/lib/pkg/permissions/<app> 到 PERM_BUF。
unsafe fn load_perms(app: usize) {
    let name = &APP_NAMES[app];
    let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let mut path = [0u8; 80];
    let mut n = 0;
    for &c in PERM_DIR.as_bytes() {
        path[n] = c;
        n += 1;
    }
    path[n] = b'/';
    n += 1;
    for &c in &name[..ln] {
        path[n] = c;
        n += 1;
    }
    let path_str = core::str::from_utf8(&path[..n]).unwrap_or("/var/lib/pkg/permissions/x");
    unsafe {
        let len = read_file(path_str, &mut PERM_BUF);
        PERM_LEN = if len > 0 { len as usize } else { 0 };
    }
}

unsafe fn tap_perm(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r < PERM_LEN {
            revoke_perm(r);
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

/// 撤销第 r 条权限（从 PERM_BUF 移除该行并重新写盘）。
unsafe fn revoke_perm(r: usize) {
    // 找行起止。
    let mut i = 0usize;
    let mut line = 0usize;
    let mut start = 0usize;
    let mut end = 0usize;
    while i < PERM_LEN && line <= r {
        let e = PERM_BUF[i..PERM_LEN]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| i + p)
            .unwrap_or(PERM_LEN);
        if line == r {
            start = i;
            end = e;
            if end < PERM_LEN && PERM_BUF[end] == b'\n' {
                end += 1;
            }
            break;
        }
        i = e + 1;
        line += 1;
    }
    let mut out = [0u8; 1024];
    let mut n = 0;
    // 复制 start 之前的。
    if start > 0 && start <= PERM_LEN {
        out[..start].copy_from_slice(&PERM_BUF[..start]);
        n = start;
    }
    let tail = PERM_LEN.saturating_sub(end);
    if tail > 0 && end <= PERM_LEN {
        out[n..n + tail].copy_from_slice(&PERM_BUF[end..PERM_LEN]);
        n += tail;
    }
    unsafe {
        PERM_LEN = n;
        PERM_BUF[..n].copy_from_slice(&out[..n]);
        // 写回原文件。
        let name = &APP_NAMES[PERM_APP];
        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let mut path = [0u8; 80];
        let mut pn = 0;
        for &c in PERM_DIR.as_bytes() {
            path[pn] = c;
            pn += 1;
        }
        path[pn] = b'/';
        pn += 1;
        for &c in &name[..ln] {
            path[pn] = c;
            pn += 1;
        }
        let path_str = core::str::from_utf8(&path[..pn]).unwrap_or("/var/lib/pkg/permissions/x");
        write_file(path_str, &PERM_BUF[..n]);
    }
    toast(b"\xE6\x9D\x83\xE9\x99\x90\xE5\xB7\xB2\xE6\x92\xA4\xE9\x94\x80");
}

unsafe fn tap_acct(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                crate::pages::open_input(crate::pages::Ctx::DevName);
                TapEffect::Redraw
            }
            1 => {
                crate::pages::open_input(crate::pages::Ctx::UserRename);
                TapEffect::Redraw
            }
            2 => {
                let on = cfg::cfg_get_int(cfg::CONF_ACCT, b"sync_enabled", 1) == 0;
                unsafe { cfg::cfg_set_int(cfg::CONF_ACCT, b"sync_enabled", if on { 1 } else { 0 }); }
                TapEffect::Redraw
            }
            _ => TapEffect::None,
        }
    } else {
        let base = top + 4 * ROW_H + HEADER_H;
        let dy = y - (base + CARD_PAD);
        if dy >= 0 && dy < ROW_H {
            page_push(PG_CLOUD);
            return TapEffect::Redraw;
        }
        TapEffect::None
    }
}

unsafe fn tap_cloud(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        crate::pages::open_input(crate::pages::Ctx::CloudUser);
        return TapEffect::Redraw;
    }
    if dy >= ROW_H && dy < 2 * ROW_H {
        // 退出登录。
        unsafe {
            cfg::cfg_set_int(cfg::CONF_ACCT, b"cloud_enabled", 0);
            cfg::cfg_set(cfg::CONF_ACCT, b"cloud_account", b"");
        }
        toast(b"\xE5\xB7\xB2\xE9\x80\x80\xE5\x87\xBA\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7");
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_system(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                page_push(PG_ABOUT);
                TapEffect::Redraw
            }
            1 => {
                page_push(PG_UPDATE);
                TapEffect::Redraw
            }
            2 => {
                page_push(PG_DT);
                TapEffect::Redraw
            }
            3 => {
                page_push(PG_LANG);
                TapEffect::Redraw
            }
            4 => {
                toast(b"\xE8\xBE\x93\xE5\x85\xA5\xE6\xB3\x95\xE9\x80\x89\xE6\x8B\xA9\xE4\xB8\xBA\xE9\xA2\x84\xE7\x95\x99\xE6\x8E\xA5\xE5\x8F\xA3\xEF\xBC\x88IME\xEF\xBC\x89");
                TapEffect::Redraw
            }
            5 => {
                // 切换导航方式。
                let new = if sys_nav::nav_mode() == sys_nav::MODE_BUTTONS {
                    b"gesture"
                } else {
                    b"buttons"
                };
                unsafe { write_file(crate::cfg::NAV_MODE_PATH, new); }
                TapEffect::Redraw
            }
            _ => {
                let base = top + 6 * ROW_H + HEADER_H;
                let dy = y - (base + CARD_PAD);
                if dy >= 0 {
                    let r2 = (dy / ROW_H) as usize;
                    match r2 {
                        0 => {
                            page_push(PG_PERMS);
                            TapEffect::Redraw
                        }
                        1 => {
                            page_push(PG_RESET);
                            TapEffect::Redraw
                        }
                        2 => {
                            page_push(PG_DEVOPS);
                            TapEffect::Redraw
                        }
                        _ => TapEffect::None,
                    }
                } else {
                    TapEffect::None
                }
            }
        }
    } else {
        TapEffect::None
    }
}

unsafe fn tap_update(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let btn_y = top + (if OTA_COUNT > 0 { ROW_H + HEADER_H } else { ROW_H + HEADER_H });
    let dy = y - btn_y;
    if dy >= 0 {
        let i = (dy / (BTN_H + CARD_PAD)) as usize;
        match i {
            0 => {
                unsafe {
                    cfg::refresh_ota();
                    if OTA_COUNT > 0 {
                        let name = &OTA_PKGS[0];
                        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                        let mut path = [0u8; 64];
                        let mut n = 0;
                        for &c in b"/data/ota/" {
                            path[n] = c;
                            n += 1;
                        }
                        for &c in &name[..ln] {
                            path[n] = c;
                            n += 1;
                        }
                        let path_str = core::str::from_utf8(&path[..n]).unwrap_or("/data/ota/x.kxup");
                        let mut pbuf = [0u8; 256];
                        let pn = path_str.as_bytes().len().min(pbuf.len() - 1);
                        pbuf[..pn].copy_from_slice(path_str.as_bytes());
                        pbuf[pn] = 0;
                        // syscall2(430, 2, path) 检查更新。
                        syscall2(SYS_KUNX_OTA, 2, pbuf.as_ptr() as usize);
                    }
                }
                toast(if OTA_COUNT > 0 { b"\xE5\xB7\xB2\xE6\xA3\x80\xE6\x9F\xA5\xE6\x9B\xB4\xE6\x96\xB0" } else { b"\xE6\x9C\xAA\xE5\x8F\x91\xE7\x8E\xB0\xE6\x9B\xB4\xE6\x96\xB0\xE5\x8C\x85" });
                TapEffect::Redraw
            }
            1 => {
                unsafe {
                    if OTA_COUNT > 0 {
                        let name = &OTA_PKGS[0];
                        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                        let mut path = [0u8; 64];
                        let mut n = 0;
                        for &c in b"/data/ota/" {
                            path[n] = c;
                            n += 1;
                        }
                        for &c in &name[..ln] {
                            path[n] = c;
                            n += 1;
                        }
                        let path_str = core::str::from_utf8(&path[..n]).unwrap_or("/data/ota/x.kxup");
                        let mut pbuf = [0u8; 256];
                        let pn = path_str.as_bytes().len().min(pbuf.len() - 1);
                        pbuf[..pn].copy_from_slice(path_str.as_bytes());
                        pbuf[pn] = 0;
                        syscall2(SYS_KUNX_OTA, 1, pbuf.as_ptr() as usize);
                        toast(b"\xE5\xB7\xB2\xE6\x8F\x90\xE4\xBA\xA4\xE5\xAE\x89\xE8\xA3\x85\xEF\xBC\x8C\xE9\x87\x8D\xE5\x90\xAF\xE7\x94\x9F\xE6\x95\x88");
                    } else {
                        toast(b"\xE6\x97\xA0\xE6\x9B\xB4\xE6\x96\xB0\xE5\x8C\x85");
                    }
                }
                TapEffect::Redraw
            }
            2 => {
                unsafe { syscall1(SYS_KUNX_OTA, 3); }
                toast(b"\xE7\x8A\xB6\xE6\x80\x81\xE5\xB7\xB2\xE8\xBE\x93\xE5\x87\xBA\xE5\x88\xB0\xE6\x97\xA5\xE5\xBF\x97");
                TapEffect::Redraw
            }
            3 => {
                // 重启确认：居中模态对话框（不再走页栈）。
                return crate::pages::open_confirm(crate::pages::Ctx::ConfirmRestart);
            }
            _ => TapEffect::None,
        }
    } else {
        TapEffect::None
    }
}

unsafe fn tap_dt(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"auto_time", 1) == 0;
                unsafe { cfg::cfg_set_int(cfg::CONF_SYSTEM, b"auto_time", if on { 1 } else { 0 }); }
                TapEffect::Redraw
            }
            1 => {
                // 时区选择：循环切换 TZDB 列表。
                let mut tz = [0u8; 32];
                let tzl = cfg::cfg_get_str(cfg::CONF_SYSTEM, b"timezone", b"Asia/Shanghai", &mut tz);
                let cur = &tz[..tzl];
                let nz = if let Some(idx) = sys_tz::zone_by_name(core::str::from_utf8(cur).unwrap_or("UTC"))
                {
                    (idx + 1) % sys_tz::TZDB.len()
                } else {
                    0
                };
                let next = sys_tz::zone_name(nz);
                unsafe { cfg::cfg_set(cfg::CONF_SYSTEM, b"timezone", next.as_bytes()); }
                TapEffect::Redraw
            }
            _ => {
                let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, b"fmt_24h", 1) == 0;
                unsafe { cfg::cfg_set_int(cfg::CONF_SYSTEM, b"fmt_24h", if on { 1 } else { 0 }); }
                TapEffect::Redraw
            }
        }
    } else {
        TapEffect::None
    }
}

unsafe fn tap_lang(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        unsafe { cfg::cfg_set(cfg::CONF_SYSTEM, b"lang", b"zh_CN"); }
        toast(b"\xE8\xAF\xAD\xE8\xA8\x80\xE5\x88\x87\xE6\x8D\xA2\xE5\x9C\xA8\xE6\x9C\xAC\xE5\xBA\x94\xE7\x94\xA8\xE5\x86\x85\xE6\x9A\x82\xE4\xB8\x8D\xE5\x8D\xB3\xE6\x97\xB6\xE7\x94\x9F\xE6\x95\x88\xEF\xBC\x88\xE9\xA2\x84\xE7\x95\x99\xE5\xA4\x9A\xE8\xAF\xAD\xE8\xA8\x80\xEF\xBC\x89");
        return TapEffect::Redraw;
    }
    if dy >= ROW_H && dy < 2 * ROW_H {
        unsafe { cfg::cfg_set(cfg::CONF_SYSTEM, b"lang", b"en_US"); }
        toast(b"\xE8\xAF\xAD\xE8\xA8\x80\xE5\x88\x87\xE6\x8D\xA2\xE9\xA2\x84\xE7\x95\x99\xEF\xBC\x88\xE5\xA4\x9A\xE8\xAF\xAD\xE8\xA8\x80\xEF\xBC\x89");
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_devops(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        let key: &[u8] = if r == 0 { b"usb_debug" } else { b"oem_unlock" };
        let on = cfg::cfg_get_int(cfg::CONF_SYSTEM, key, 0) == 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_SYSTEM, key, if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_battery(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + ROW_H + HEADER_H + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        let key: &[u8] = if r == 0 { b"show_percent" } else { b"low_power" };
        let on = cfg::cfg_get_int(cfg::CONF_BATTERY, key, 0) == 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_BATTERY, key, if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_storage(x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    // 清除缓存按钮行（第三卡）。
    let base = top + 3 * ROW_H + HEADER_H + ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 && dy < ROW_H {
        clear_caches();
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn clear_caches() {
    // 遍历每个应用 cache 目录删除顶层文件。
    let mut removed = 0u32;
    for a in 0..APP_COUNT {
        let name = &APP_NAMES[a];
        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let mut dir = [0u8; 96];
        let mut n = 0;
        for &c in b"/data/app/" {
            dir[n] = c;
            n += 1;
        }
        for &c in &name[..ln] {
            dir[n] = c;
            n += 1;
        }
        for &c in b"/cache" {
            dir[n] = c;
            n += 1;
        }
        let dir_str = core::str::from_utf8(&dir[..n]).unwrap_or("/data/app/x/cache");
        let mut raw = [0u8; 1024];
        unsafe {
            let rn = list_dir(dir_str, &mut raw);
            let mut i = 0;
            while i < rn {
                let e = raw[i..rn].iter().position(|&c| c == 0).map(|p| i + p).unwrap_or(rn);
                let fname = &raw[i..e];
                if !fname.is_empty() {
                    let mut path = [0u8; 128];
                    let mut pn = 0;
                    for &c in dir_str.as_bytes() {
                        path[pn] = c;
                        pn += 1;
                    }
                    path[pn] = b'/';
                    pn += 1;
                    let fl = fname.len().min(path.len() - pn - 1);
                    for &c in &fname[..fl] {
                        path[pn] = c;
                        pn += 1;
                    }
                    path[pn] = 0;
                    let r = syscall3(SYS_UNLINKAT, AT_FDCWD, path.as_ptr() as usize, 0);
                    if (r as isize) >= 0 {
                        removed += 1;
                    }
                }
                i = e + 1;
            }
        }
    }
    let mut msg = [0u8; 32];
    let mut mn = 0;
    for &c in b"\xE5\xB7\xB2\xE6\xB8\x85\xE7\x90\x86\xE7\xBC\x93\xE5\xAD\x98\xE6\x96\x87\xE4\xBB\xB6 " {
        msg[mn] = c;
        mn += 1;
    }
    let mut num = [0u8; 12];
    let bn = fmt_dec(&mut num, removed);
    msg[mn..mn + bn].copy_from_slice(&num[..bn]);
    mn += bn;
    msg[mn] = b' ';
    mn += 1;
    for &c in b"\xE4\xB8\xAA" {
        msg[mn] = c;
        mn += 1;
    }
    toast(&msg[..mn]);
    unsafe { cfg::cfg_set_int(cfg::CONF_STORAGE, b"cache_cleaned", removed as i32); }
}

unsafe fn tap_access(_x: i32, y: i32) -> crate::pages::TapEffect {
    use crate::pages::TapEffect;
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        let key: &[u8] = match r {
            0 => b"magnifier",
            1 => b"invert_colors",
            2 => b"high_contrast",
            _ => return TapEffect::None,
        };
        let on = cfg::cfg_get_int(cfg::CONF_ACCESS, key, 0) == 0;
        unsafe { cfg::cfg_set_int(cfg::CONF_ACCESS, key, if on { 1 } else { 0 }); }
        return TapEffect::Redraw;
    }
    TapEffect::None
}