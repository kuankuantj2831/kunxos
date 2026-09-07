//! 天气应用 JSON 提取器宿主单测：直接复用 apps/weather/src/json.rs（#[path]），
//! 用真实 Open-Meteo 响应快照 + 合成用例验证提取与 WMO 映射。
//!
//! 用法：cargo run --release --manifest-path tests/weather_json/Cargo.toml

#[path = "../../../apps/weather/src/json.rs"]
mod json;

use json::*;

/// 2026-08-27 抓取的北京真实响应（当前：26.5°C / code 3；
/// daily codes [3,61,3,0,0]，max [29.7,24.3,30.3,31.1,27.2]，min [22.4,20.8,19.2,18.9,19.2]）。
const BEIJING: &str = r#"{"latitude":39.89455,"longitude":116.35983,"generationtime_ms":0.17786026000976562,"utc_offset_seconds":28800,"timezone":"Asia/Shanghai","timezone_abbreviation":"GMT+8","elevation":47.0,"current_units":{"time":"iso8601","interval":"seconds","temperature_2m":"°C","relative_humidity_2m":"%","apparent_temperature":"°C","is_day":"","weather_code":"wmo code","wind_speed_10m":"km/h"},"current":{"time":"2026-08-27T19:00","interval":900,"temperature_2m":26.5,"relative_humidity_2m":71,"apparent_temperature":30.2,"is_day":0,"weather_code":3,"wind_speed_10m":3.7},"daily_units":{"time":"iso8601","weather_code":"wmo code","temperature_2m_max":"°C","temperature_2m_min":"°C"},"daily":{"time":["2026-08-27","2026-08-28","2026-08-29","2026-08-30","2026-08-31"],"weather_code":[3,61,3,0,0],"temperature_2m_max":[29.7,24.3,30.3,31.1,27.2],"temperature_2m_min":[22.4,20.8,19.2,18.9,19.2]}}"#;

/// 哈尔滨真实响应（当前：19.1°C / code 0；daily codes [51,51,95,80,51]）。
const HARBIN: &str = r#"{"latitude":45.79965,"longitude":126.54029,"generationtime_ms":0.1863241195678711,"utc_offset_seconds":28800,"timezone":"Asia/Shanghai","timezone_abbreviation":"GMT+8","elevation":122.0,"current_units":{"time":"iso8601","interval":"seconds","temperature_2m":"°C","relative_humidity_2m":"%","apparent_temperature":"°C","is_day":"","weather_code":"wmo code","wind_speed_10m":"km/h"},"current":{"time":"2026-08-27T19:00","interval":900,"temperature_2m":19.1,"relative_humidity_2m":83,"apparent_temperature":20.3,"is_day":0,"weather_code":0,"wind_speed_10m":6.4},"daily_units":{"time":"iso8601","weather_code":"wmo code","temperature_2m_max":"°C","temperature_2m_min":"°C"},"daily":{"time":["2026-08-27","2026-08-28","2026-08-29","2026-08-30","2026-08-31"],"weather_code":[51,51,95,80,51],"temperature_2m_max":[22.9,23.9,24.0,22.0,19.1],"temperature_2m_min":[15.3,14.2,16.1,16.7,14.0]}}"#;

/// 合成：负温度、雪/雷码、夜间。
const SYNTH: &str = r#"{"current":{"time":"x","temperature_2m":-5.7,"relative_humidity_2m":80,"apparent_temperature":-8.2,"is_day":0,"weather_code":71,"wind_speed_10m":10.0},"daily":{"weather_code":[71,73,75,85,95],"temperature_2m_max":[-1.2,0.4,3.1,-2.5,7.6],"temperature_2m_min":[-9.3,-7.8,-4.2,-8.1,2.4]}}"#;

/// 合成：current_units 先出现（键前缀陷阱），真实字段只在 current 中。
const PREFIX_TRAP: &str = r#"{"current_units":{"temperature_2m":"°C","weather_code":"wmo code"},"current":{"temperature_2m":25.5,"relative_humidity_2m":50,"apparent_temperature":26.0,"is_day":1,"weather_code":0,"wind_speed_10m":5.0},"daily":{"time":["a","b","c","d","e"],"weather_code":[0,1,2,3,61],"temperature_2m_max":[30.0,31.0,32.0,33.0,34.0],"temperature_2m_min":[20.0,21.0,22.0,23.0,24.0]}}"#;

fn parse(s: &str) -> (CurrentWeather, [DayForecast; 5], bool) {
    let mut cur = CurrentWeather::empty();
    let mut days = [DayForecast::empty(); 5];
    let ok = json_parse(s.as_bytes(), &mut cur, &mut days);
    (cur, days, ok)
}

fn check(name: &str, cond: bool) {
    if !cond {
        eprintln!("FAIL: {name}");
        std::process::exit(1);
    }
    println!("ok: {name}");
}

fn main() {
    // ---- 北京真实响应 ----
    let (cur, days, ok) = parse(BEIJING);
    check("北京解析成功", ok);
    check("北京温度 26.5→27", cur.temp == 27);
    check("北京体感 30.2→30", cur.feels == 30);
    check("北京湿度 71", cur.humidity == 71);
    check("北京风速 4.2→4", cur.wind == 4);
    check("北京天气码 3", cur.code == 3);
    check("北京夜间(is_day=0)", !cur.is_day);
    check("北京 day0 码 3", days[0].code == 3);
    check("北京 day1 码 61（小雨）", days[1].code == 61);
    check("北京 day0 高 29.7→30", days[0].high == 30);
    check("北京 day0 低 22.4→22", days[0].low == 22);
    check("北京 day2 高 30.3→30", days[2].high == 30);
    check("北京 5 天填满", days[4].high != 0);

    // ---- 哈尔滨真实响应 ----
    let (cur, days, ok) = parse(HARBIN);
    check("哈尔滨解析成功", ok);
    check("哈尔滨温度 19.1→19", cur.temp == 19);
    check("哈尔滨码 0（晴）", cur.code == 0);
    check("哈尔滨夜间", !cur.is_day);
    check("哈尔滨 day1 码 51", days[1].code == 51);
    check("哈尔滨 day2 码 95（雷雨）", days[2].code == 95);
    check("哈尔滨 day0 高 22.9→23", days[0].high == 23);

    // ---- 合成：负数 / 雪 / 雷 ----
    let (cur, days, ok) = parse(SYNTH);
    check("合成解析成功", ok);
    check("负温 -5.7→-6", cur.temp == -6);
    check("负体感 -8.2→-8", cur.feels == -8);
    check("雪码 71", cur.code == 71);
    check("夜间", !cur.is_day);
    check("day0 高 -1.2→-1", days[0].high == -1);
    check("day0 低 -9.3→-9", days[0].low == -9);
    check("day4 高 7.6→8", days[4].high == 8);
    check("day4 码 95", days[4].code == 95);

    // ---- 键前缀陷阱：current_units 不污染 current ----
    let (cur, days, ok) = parse(PREFIX_TRAP);
    check("前缀陷阱解析成功", ok);
    check("温度取自 current 25.5→26", cur.temp == 26);
    check("湿度 50", cur.humidity == 50);
    check("day0 码 0（晴）", days[0].code == 0);

    // ---- 垃圾 / 截断输入 ----
    let (_, _, ok) = parse("hello world");
    check("垃圾输入返回 false", !ok);
    let (_, _, ok) = parse(r#"{"current":{"temperature_2m":26.5,"weather_code":3}}"#);
    check("缺 daily 返回 false", !ok);
    let (_, _, ok) = parse(r#"{"current":{"temperature_2m":26.5,"weather_code":3,"relative_humidity_2m":60,"apparent_temperature":28,"is_day":1,"wind_speed_10m":3.0},"daily":{"weather_code":[0],"temperature_2m_max":[30.0],"temperature_2m_min":[20.0]}}"#);
    check("daily 不足 5 天也接受（取到 1 天）", ok);
    let (_, _, ok) = parse(r#"{"current":{"temperature_2m":26.5}}"#);
    check("只有 current 返回 false", !ok);

    // ---- WMO 映射 ----
    check("wmo_icon 0=sun", wmo_icon(0) == 0);
    check("wmo_icon 3=cloud", wmo_icon(3) == 1);
    check("wmo_icon 45=cloud(雾)", wmo_icon(45) == 1);
    check("wmo_icon 61=rain", wmo_icon(61) == 2);
    check("wmo_icon 71=snow", wmo_icon(71) == 3);
    check("wmo_icon 95=thunder", wmo_icon(95) == 4);
    check("wmo_icon 82=rain", wmo_icon(82) == 2);
    check("wmo_label 0=晴", wmo_label(0) == "晴");
    check("wmo_label 2=多云", wmo_label(2) == "多云");
    check("wmo_label 3=阴", wmo_label(3) == "阴");
    check("wmo_label 61=小雨", wmo_label(61) == "小雨");
    check("wmo_label 71=小雪", wmo_label(71) == "小雪");
    check("wmo_label 95=雷雨", wmo_label(95) == "雷雨");
    check("wmo_widget 0=sunny", wmo_widget(0) == b"sunny");
    check("wmo_widget 2=cloudy", wmo_widget(2) == b"cloudy");
    check("wmo_widget 61=rain", wmo_widget(61) == b"rain");
    check("wmo_widget 71=snow", wmo_widget(71) == b"snow");
    check("wmo_widget 95=rain", wmo_widget(95) == b"rain");
    check("wmo_widget 45=cloudy", wmo_widget(45) == b"cloudy");

    println!("ALL PASS");
}
