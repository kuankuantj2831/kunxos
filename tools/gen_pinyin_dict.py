#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 KunxOS 拼音输入法字典 shared/pinyin_dict.rs。

用法:
    python3 tools/gen_pinyin_dict.py

输出:
    <repo>/shared/pinyin_dict.rs   （Rust 源码，静态表，no_std 可编译）

依赖:
    pip install pypinyin

数据来源:
    - GB2312-80 一级字库（区 16..=55，位 1..=94；区 55 仅位 1..=89，共 3755 字）。
      字符由 bytes([qu + 0xA0, wei + 0xA0]).decode('gb2312') 得到。
    - tools/cjk/hzk16.bin 仅用于存在性/大小抽查（本任务不读字形）。
    - pypinyin 提供无声调拼音（多音字取默认常用音）。
"""
from __future__ import annotations

import os
import re
import sys
from collections import OrderedDict

try:
    from pypinyin import Style, pinyin
except ImportError as exc:  # pragma: no cover
    sys.exit(
        "错误：缺少 pypinyin 库，请先安装：\n"
        "    pip install --break-system-packages --user pypinyin\n"
        "原始错误: %s" % exc
    )

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_DIR = os.path.dirname(SCRIPT_DIR)
HZK16_PATH = os.path.join(SCRIPT_DIR, "cjk", "hzk16.bin")
OUT_PATH = os.path.join(REPO_DIR, "shared", "pinyin_dict.rs")

SYLLABLE_CAP = 12

# ---------------------------------------------------------------------------
# 常见字表：出现顺序越靠前，同音节候选排序越靠前（常见字优先启发式）。
# ---------------------------------------------------------------------------
COMMON_CHARS_RAW = (
    "你、我、他、好、是、了、不、在、有、和、人、这、中、大、为、上、个、国、我、以、要、他、时、来、用、们、生、到、作、地、"
    "于、出、就、分、对、成、会、可、主、发、年、动、同、工、也、能、下、过、子、说、产、种、面、而、方、后、多、定、行、学、法、"
    "所、民、得、经、十、三、之、进、着、等、部、度、家、电、力、里、如、水、化、高、自、二、理、起、小、物、现、实、加、量、都、"
    "两、体、制、机、当、使、点、从、业、本、去、把、性、好、应、开、它、合、还、因、由、其、些、然、前、外、天、政、四、日、那、"
    "社、义、事、平、形、相、全、表、间、样、与、关、各、重、新、线、内、数、正、心、反、你、明、看、原、又、么、利、比、或、但、"
    "质、气、第、向、道、命、此、变、条、只、没、结、解、问、意、建、月、公、无、系、军、很、情、者、最、立、代、想、已、通、并、"
    "提、直、题、党、程、展、五、果、料、象、员、革、位、入、常、文、总、次、品、式、活、设、及、管、特、件、长、求、老、头、基、"
    "资、边、流、路、级、少、图、山、统、接、知、较、将、组、见、计、别、她、手、角、期、根、论、运、农、指、几、九、区、强、放、"
    "决、西、被、干、做、必、战、先、回、则、任、取、据、处、队、南、给、色、光、门、即、保、治、北、造、百、规、热、领、七、海、"
    "口、东、导、器、压、志、世、金、增、争、济、阶、油、思、术、极、交、受、联、什、认、六、共、权、收、证、改、清、己、美、再、"
    "采、转、更、单、风、切、打、白、教、速、花、带、安、场、身、车、例、真、务、具、万、每、目、至、达、走、积、示、议、声、报、"
    "斗、完、类、八、离、华、名、确、才、科、张、信、马、节、话、米、整、空、元、况、今、集、温、传、土、许、步、群、广、石、记、"
    "需、段、研、界、拉、林、律、叫、且、究、观、越、织、装、影、算、低、持、音、众、书、布、复、容、儿、须、际、商、非、验、连、"
    "断、深、难、近、矿、千、周、委、素、技、备、半、办、青、省、列、习、响、约、支、般、史、感、劳、便、团、往、酸、历、市、克、"
    "何、除、消、构、府、称、太、准、精、值、号、率、族、维、划、选、标、写、存、候、毛、亲、快、效、斯、院、查、江、型、眼、王、"
    "按、格、养、易、置、派、层、片、始、却、专、状、育、厂、京、识、适、属、圆、包、火、住、调、满、县、局、照、参、红、细、引、"
    "听、该、铁、价、严、龙、飞"
)

# ---------------------------------------------------------------------------
# 常用词表（至少 250 个常用双字/三字词；单字词亦保留）。
# ---------------------------------------------------------------------------
WORDS_RAW = (
    "你好、谢谢、再见、请问、知道、手机、电脑、天气、今天、明天、昨天、星期、时间、中国、北京、上海、广州、深圳、老师、学生、"
    "朋友、家人、吃饭、喝水、睡觉、工作、学习、开会、电话、短信、消息、照片、视频、音乐、电影、电视、游戏、商店、银行、医院、"
    "学校、公园、汽车、火车、飞机、地铁、公交、打车、走路、快乐、难过、高兴、生气、喜欢、讨厌、爱、恨、想、要、买、卖、贵、"
    "便宜、大、小、多、少、好、坏、新、旧、快、慢、热、冷、晴天、下雨、下雪、刮风、多云、阴天、温度、多少、怎么、什么、为什么、"
    "哪里、谁、几、一、二、三、十、百、千、万、零、上午、下午、晚上、中午、早上、现在、以后、以前、未来、过去、开始、结束、"
    "继续、停止、打开、关闭、发送、接收、输入、输出、保存、删除、复制、粘贴、撤销、重做、搜索、浏览、下载、上传、连接、断开、"
    "登录、退出、注册、密码、账号、用户名、设置、帮助、关于、菜单、页面、返回、首页、刷新、加载、错误、成功、失败、警告、信息、"
    "通知、提醒、日历、闹钟、计算、测量、转换、比较、分析、统计、记录、报告、计划、安排、预约、取消、确认、提交、发布、分享、"
    "收藏、关注、评论、点赞、转发、安装、更新、版本、升级、修复、崩溃、重启、关机、开机、电池、充电、电量、信号、网络、蓝牙、"
    "无线、流量、话费、余额、支付、收款、转账、红包、快递、外卖、超市、商场、菜市场、厨房、客厅、卧室、卫生间、阳台、窗户、门、"
    "桌子、椅子、床、沙发、灯、空调、冰箱、洗衣机、电视柜、衣柜、书架、电脑桌、黑板、粉笔、课本、作业、考试、成绩、分数、排名、"
    "奖学金、毕业、开学、放假、假期、周末、节日、春节、中秋、国庆、生日、礼物、蛋糕、蜡烛、许愿、祝福、恭喜、不客气、对不起、"
    "没关系、早安、晚安"
)

WORDS_EXTRA = (
    "爸爸、妈妈、哥哥、弟弟、姐姐、妹妹、爷爷、奶奶、外公、外婆、叔叔、阿姨、男孩、女孩、男人、女人、小孩、大人、同学、同事、"
    "领导、老板、员工、医生、护士、司机、警察、军人、工人、农民、服务员、厨师、律师、记者、作家、画家、歌手、演员、运动员、"
    "没有、还有、可以、不能、可能、应该、一定、必须、非常、特别、真的、确实、当然、马上、经常、总是、偶尔、然后、但是、因为、"
    "所以、如果、虽然、而且、或者、那么、于是、红色、黄色、蓝色、绿色、黑色、白色、紫色、橙色、颜色、苹果、香蕉、西瓜、葡萄、"
    "草莓、橘子、梨、桃子、米饭、面条、饺子、包子、馒头、鸡蛋、牛奶、咖啡、面包、蔬菜、水果、番茄、土豆、白菜、萝卜、左边、"
    "右边、上面、下面、里面、外面、前面、后面、旁边、中间、跑步、游泳、跳舞、唱歌、画画、看书、写字、散步、旅行、爬山、钥匙、"
    "钱包、雨伞、帽子、衣服、裤子、鞋子、袜子、手套、眼镜、手表、背包、行李、毛巾、牙刷、牙膏、漂亮、美丽、可爱、聪明、勇敢、"
    "认真、努力、辛苦、幸福、健康、平安、顺利、安全、危险、重要、简单、复杂、方便、麻烦、干净、安静、星期一、星期二、星期三、"
    "星期四、星期五、星期六、星期日、周一、周二、周三、周四、周五、周六、周日、一月、二月、三月、四月、五月、六月、七月、八月、"
    "九月、十月、十一月、十二月、春天、夏天、秋天、冬天、季节、早上好、下午好、晚上好、生日快乐、新年快乐、恭喜发财、万事如意、"
    "图书馆、电影院、体育馆、火车站、飞机场、停车场、办公室、宿舍、食堂、书店、说话、聊天、沟通、交流、讨论、商量、同意、反对、"
    "支持、关心、照顾、准备、休息、放松、旅游、参观、空气、阳光、月亮、星星、太阳、地球、海洋、森林、城市、农村、街道、马路、"
    "电梯、楼梯、热水、凉水、酒店、宾馆、广场、长城、故宫、天安门、机票、护照、签证、身份证、人民币、美元、欧元、日元、公斤、"
    "千米、小时、分钟、秒钟、第一、第二、第三、首先、最后、另外、其他、全部、部分、许多、一点、一些、很多、最近、将来、平时、"
    "回家、上班、下班、上学、放学、出门、出去、进来、回来、离开"
)

# 语气词等手动补充：pypinyin 可能不返回或读音特殊。
MANUAL_SYLLABLES = [("en", "嗯"), ("o", "哦"), ("ei", "诶"), ("ng", "嗯"), ("n", "嗯")]

# 必须出现在词表里的高频词。
REQUIRED_WORDS = [
    "你好", "谢谢", "再见", "中国", "北京", "上海", "今天", "明天", "时间",
    "手机", "电脑", "天气", "老师", "学生", "朋友", "家人", "什么", "怎么",
    "为什么", "知道", "请问", "晚安", "早安",
]

_PY_RE = re.compile(r"^[a-z]+$")


def common_chars() -> list[str]:
    """去重（保留首次出现顺序）后的常见字列表。"""
    return list(OrderedDict.fromkeys(c for c in COMMON_CHARS_RAW.split("、") if c))


def words() -> list[str]:
    """去重（保留首次出现顺序）后的常用词列表。"""
    return list(OrderedDict.fromkeys(
        c for c in (WORDS_RAW + "、" + WORDS_EXTRA).split("、") if c
    ))


def gb2312_level1_chars() -> list[tuple[str, int, int]]:
    """GB2312-80 一级字库（区 16..=55，位 1..=94，区 55 只到位 89）。"""
    chars: list[tuple[str, int, int]] = []
    for qu in range(16, 56):
        max_wei = 89 if qu == 55 else 94
        for wei in range(1, max_wei + 1):
            try:
                ch = bytes([qu + 0xA0, wei + 0xA0]).decode("gb2312")
            except Exception:
                continue
            if ch:
                chars.append((ch, qu, wei))
    return chars


def check_hzk16() -> None:
    """对 hzk16.bin 做存在性/大小抽查（本任务不读字形，仅供确认数据源）。"""
    if not os.path.isfile(HZK16_PATH):
        print("警告: 未找到 %s，将按 GB2312-80 一级字库区位范围直接枚举字符。" % HZK16_PATH)
        return
    size = os.path.getsize(HZK16_PATH)
    need = ((55 - 1) * 94 + 94) * 32  # 需覆盖到 55 区 94 位（本任务只用到位 89）
    if size < need:
        print("警告: %s 大小 %d 字节 < 覆盖一级字库所需 %d 字节（本任务不使用字形）。" % (HZK16_PATH, size, need))
        return
    off = ((16 - 1) * 94 + (1 - 1)) * 32  # 区16位1 = 啊
    with open(HZK16_PATH, "rb") as f:
        f.seek(off)
        glyph = f.read(32)
    if not any(glyph):
        print("警告: %s 中 (区16,位1) 字形全空，文件可能损坏。" % HZK16_PATH)


def build_syllables() -> list[tuple[str, str]]:
    """单音节表：拼音 -> 候选汉字（常见字在前，其余按 GB2312 区码排序），每音节最多 SYLLABLE_CAP 个。"""
    common_idx = {ch: i for i, ch in enumerate(common_chars())}
    groups: dict[str, OrderedDict[str, tuple[int | None, int | None, int | None]]] = {}

    missing_common: list[str] = []
    for ch, qu, wei in gb2312_level1_chars():
        try:
            res = pinyin(ch, style=Style.NORMAL, heteronym=False)
            py = res[0][0].strip().lower() if res and res[0] else ""
        except Exception as exc:  # noqa: BLE001
            print("警告: pinyin(%r) 出错: %s" % (ch, exc))
            py = ""
        if not _PY_RE.fullmatch(py):
            print("警告: %r (%s) 无有效拼音，跳过。" % (ch, py or "<空>"))
            continue
        groups.setdefault(py, OrderedDict())
        groups[py][ch] = (qu, wei, common_idx.get(ch))
    # 手动补充（en/ng/n 等语气词读音），去重。
    for py, ch in MANUAL_SYLLABLES:
        groups.setdefault(py, OrderedDict())
        if ch not in groups[py]:
            groups[py][ch] = (None, None, None)

    all_chars = {ch for g in groups.values() for ch in g}
    missing_common = [c for c in common_chars() if c not in all_chars]
    if missing_common:
        print("警告: 以下常见字未覆盖（不在一级字库或 pypinyin 无音）: %s" % "".join(missing_common))

    def _key(item: tuple[str, tuple[int | None, int | None, int | None]]) -> tuple:
        _ch, (qu, wei, cidx) = item
        if cidx is not None:
            return (0, cidx)
        return (1, qu if qu is not None else 9999, wei if wei is not None else 9999)

    syllables: list[tuple[str, str]] = []
    for py in sorted(groups):
        items = sorted(groups[py].items(), key=_key)[:SYLLABLE_CAP]
        syllables.append((py, "".join(ch for ch, _ in items)))
    return syllables


def build_words() -> list[tuple[str, str]]:
    """词语表：全拼 -> 词语候选（同音词合并），按全拼字母序排序。"""
    word_groups: OrderedDict[str, list[str]] = OrderedDict()
    for w in words():
        try:
            res = pinyin(w, style=Style.NORMAL, heteronym=False)
            py = "".join(part[0] for part in res if part).strip().lower()
        except Exception as exc:  # noqa: BLE001
            print("警告: pinyin(%r) 出错: %s" % (w, exc))
            py = ""
        if not _PY_RE.fullmatch(py):
            print("警告: 词语 %r 无有效全拼（%r），跳过。" % (w, py or "<空>"))
            continue
        word_groups.setdefault(py, [])
        if w not in word_groups[py]:
            word_groups[py].append(w)
    return [(py, "".join(ws)) for py, ws in sorted(word_groups.items())]


def render(syllables: list[tuple[str, str]], word_entries: list[tuple[str, str]]) -> str:
    lines = [
        "//! 拼音输入法字典（由 tools/gen_pinyin_dict.py 生成，勿手改）。",
        "//! 单音节表：拼音（无声调、小写）→ 候选汉字列表（常用在前）。",
        "//! 词语表：全拼 → 词语候选。",
        "",
        "/// 单音节候选数上限。",
        "pub const SYLLABLE_CAP: usize = %d;" % SYLLABLE_CAP,
        "",
        "/// 单音节条目：(拼音字节串, 候选汉字 UTF-8 字节串)。",
        '/// 例：("ni", "你尼泥拟呢倪妮霓逆溺腻") — 注意是拼接的字符串，不是数组。',
        "pub static SYLLABLES: &[(&str, &str)] = &[",
    ]
    for py, cands in syllables:
        lines.append('    ("%s", "%s"),' % (py, cands))
    lines.append("];")
    lines.append("")
    lines.append("/// 词语条目：(全拼, 词语 UTF-8)。")
    lines.append("pub static WORDS: &[(&str, &str)] = &[")
    for py, cands in word_entries:
        lines.append('    ("%s", "%s"),' % (py, cands))
    lines.append("];")
    return "\n".join(lines) + "\n"


def main() -> int:
    check_hzk16()

    syllables = build_syllables()
    word_entries = build_words()

    # ---- 校验 ----
    if len(syllables) <= 350:
        sys.exit("错误：单音节条目 %d 条，未达到 >350 的要求。" % len(syllables))
    if len(word_entries) <= 200:
        sys.exit("错误：词语条目 %d 条，未达到 >200 的要求。" % len(word_entries))
    if len(words()) < 250:
        sys.exit("错误：常用词仅 %d 个，未达到 >=250 的要求。" % len(words()))

    all_cands = [c for _, c in syllables] + [c for _, c in word_entries]
    unique_chars = len(set("".join(all_cands)))
    unique_words = len(words())

    missing_required = [w for w in REQUIRED_WORDS if w not in words()]
    if missing_required:
        sys.exit("错误：词表缺少必需高频词: %s" % "、".join(missing_required))

    src = render(syllables, word_entries)
    with open(OUT_PATH, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)

    print("已生成 %s" % OUT_PATH)
    print("  SYLLABLES 条目数: %d" % len(syllables))
    print("  WORDS 条目数:    %d（来自 %d 个去重常用词）" % (len(word_entries), unique_words))
    print("  涉及汉字数:      %d" % unique_chars)
    print("  单音节示例:      %r" % (syllables[0],))
    print("  词语示例:        %r" % (word_entries[0],))
    return 0


if __name__ == "__main__":
    sys.exit(main())
