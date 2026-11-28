//! Small helpers for digging through Innertube JSON.
use serde_json::Value;

/// Extracts text from {simpleText}, {runs:[{text}]}, {content}, or a plain string.
pub fn text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(s) = v.get("simpleText").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(runs) = v.get("runs").and_then(Value::as_array) {
        return runs
            .iter()
            .filter_map(|r| r.get("text").and_then(Value::as_str))
            .collect::<String>();
    }
    if let Some(s) = v.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    String::new()
}

pub fn opt_text(v: Option<&Value>) -> Option<String> {
    v.map(text).filter(|s| !s.is_empty())
}

pub fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Value::as_str).map(str::to_string)
}

/// Depth-first search for the first value under `key`.
pub fn find<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                return Some(x);
            }
            m.values().find_map(|c| find(c, key))
        }
        Value::Array(a) => a.iter().find_map(|c| find(c, key)),
        _ => None,
    }
}

pub fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                if k == key {
                    out.push(c);
                } else {
                    find_all(c, key, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| find_all(c, key, out)),
        _ => {}
    }
}

/// Renderers that only ever hold ads or paid promotions. Their contents look like
/// ordinary videos (an ad's own video, usually unlisted and unplayable), so the
/// whole subtree is skipped.
pub const AD_RENDERERS: &[&str] = &[
    "adSlotRenderer",
    "inFeedAdLayoutRenderer",
    "adSlotAndLayoutMetadata",
    "promotedVideoRenderer",
    "compactPromotedVideoRenderer",
    "promotedSparklesWebRenderer",
    "promotedSparklesTextSearchRenderer",
    "searchPyvRenderer",
    "bannerPromoRenderer",
    "statementBannerRenderer",
    "brandVideoShelfRenderer",
    "brandVideoSingletonRenderer",
    "videoMastheadAdV3Renderer",
    "mastheadAdV4Renderer",
    "primetimePromoRenderer",
    "playerLegacyDesktopWatchAdsRenderer",
    "adBreakServiceRenderer",
    "displayAdRenderer",
    "actionCompanionAdRenderer",
    "feedAdMetadataViewModel",
];

/// Fields that only appear on ads, also when YouTube draws an ad with a regular
/// video renderer or lockup.
const AD_MARKERS: &[&str] = &["adSlotLoggingData", "adLayoutLoggingData", "adClientParams", "adVideoId", "promotedVideoRenderer"];

/// Whether a renderer is an ad: it carries an ad-only field or an "Ad"/"Sponsored" badge.
pub fn is_ad(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, c)| {
            AD_MARKERS.contains(&k.as_str())
                || AD_RENDERERS.contains(&k.as_str())
                || ((k == "style" || k == "badgeStyle") && matches!(c.as_str(), Some("BADGE_STYLE_TYPE_AD" | "BADGE_STYLE_TYPE_AD_INFO")))
                || (k == "iconType" && c.as_str() == Some("AD_BADGE"))
                || is_ad(c)
        }),
        Value::Array(a) => a.iter().any(is_ad),
        _ => false,
    }
}

/// Largest thumbnail url in a {thumbnails:[...]} or {sources:[...]} object.
pub fn best_thumb(v: &Value) -> Option<String> {
    let arr = v
        .get("thumbnails")
        .or_else(|| v.get("sources"))
        .and_then(Value::as_array)?;
    let w = |t: &&Value| t.get("width").and_then(Value::as_u64).unwrap_or(0);
    let h = |t: &&Value| t.get("height").and_then(Value::as_u64).unwrap_or(0);
    // big enough for a card on a high-DPI screen, and no bigger: every image is
    // kept decoded in memory (portrait Shorts covers count by height)
    let best = arr
        .iter()
        .filter(|t| w(t) >= 600 || h(t) >= 700)
        .min_by_key(|t| w(t) * h(t).max(1))
        .or_else(|| arr.iter().max_by_key(w))?;
    best.get("url").and_then(Value::as_str).map(fix_url).map(|u| limit_avatar(&u))
}

#[cfg(test)]
mod avatar_tests {
    #[test]
    fn avatar_size() {
        assert_eq!(super::limit_avatar("https://yt3.ggpht.com/abc=s900-c-k-c0x00ffffff-no-rj"), "https://yt3.ggpht.com/abc=s240-c-k-c0x00ffffff-no-rj");
        assert_eq!(super::limit_avatar("https://yt3.ggpht.com/abc=s88-c-k"), "https://yt3.ggpht.com/abc=s88-c-k");
        assert_eq!(super::limit_avatar("https://i.ytimg.com/vi/x/hq720.jpg"), "https://i.ytimg.com/vi/x/hq720.jpg");
    }
}

/// A thumbnail sized for a 168 pt recommendation card on a Retina display.
pub fn compact_thumb(v: &Value) -> Option<String> {
    let images = v.get("thumbnails").or_else(|| v.get("sources"))?.as_array()?;
    images
        .iter()
        .filter_map(|image| Some((image.get("width")?.as_u64()?, image.get("url")?.as_str()?)))
        .filter(|(width, _)| (320..=480).contains(width))
        .min_by_key(|(width, _)| width.abs_diff(336))
        .map(|(_, url)| fix_url(url))
}

pub fn small_thumb(v: &Value) -> Option<String> {
    let arr = v
        .get("thumbnails")
        .or_else(|| v.get("sources"))
        .and_then(Value::as_array)?;
    arr.first()
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .map(fix_url)
}

/// Channel avatars come as 900x900 but are drawn at 24-160 px: ask for 240 px.
pub fn limit_avatar(u: &str) -> String {
    if !(u.contains("yt3.ggpht.com") || u.contains("yt3.googleusercontent.com")) {
        return u.to_string();
    }
    // "...=s900-c-k-c0x00ffffff-no-rj" -> "...=s240-c-k-c0x00ffffff-no-rj"
    if let Some(i) = u.rfind("=s") {
        let rest = &u[i + 2..];
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && rest[..digits].parse::<u32>().map(|n| n > 240).unwrap_or(false) {
            return format!("{}=s240{}", &u[..i], &rest[digits..]);
        }
    }
    u.to_string()
}

pub fn fix_url(u: &str) -> String {
    if let Some(r) = u.strip_prefix("//") {
        format!("https://{r}")
    } else {
        u.to_string()
    }
}

/// "1:02:03" -> 3723
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0u64;
    for part in s.split(':') {
        let n: u64 = part.trim().parse().ok()?;
        total = total * 60 + n;
    }
    Some(total)
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Converts "3 days ago" / "3d ago" / "Streamed 2 weeks ago" into an approximate unix timestamp.
/// "3 days ago", "3d", and the same in the common European languages YouTube
/// answers in ("il y a 3 jours", "vor 3 Tagen", "hace 3 días", "3 giorni fa", ...).
pub fn parse_relative_time(s: &str) -> Option<i64> {
    relative_secs(s).map(|secs| now() - secs)
}

fn relative_secs(s: &str) -> Option<i64> {
    let lower = s.to_lowercase();
    // view and subscriber counts also start with a number ("3 M de vues")
    const COUNTS: [&str; 15] = [
        "view", "vue", "aufruf", "visualiz", "vista", "weergave", "watching", "spectat", "zuschau", "subscri", "abonn", "visning", "просмотр",
        "перегляд", "vizion",
    ];
    if COUNTS.iter().any(|c| lower.contains(c)) {
        return None;
    }
    let start = lower.find(|c: char| c.is_ascii_digit())?;
    let rest = &lower[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().ok()?;
    let after = &rest[digits.len()..];
    // "1.2K" / "3,4 M" are counts, not times
    if after.starts_with('.') || after.starts_with(',') {
        return None;
    }
    // "5m" (English compact) touches the number; "21 M" on its own is a count
    let attached = !after.starts_with(char::is_whitespace);
    // Romanian puts "de" between the number and the unit ("acum 20 de ore")
    let word = after.trim_start();
    let word = word.strip_prefix("de ").unwrap_or(word);
    let unit: String = word.chars().take_while(|c| c.is_alphabetic()).collect();
    let u = unit.as_str();
    if u.is_empty() || u == "k" {
        return None;
    }
    if u == "m" {
        return if attached {
            Some(n * 60)
        } else if lower.contains("il y a") {
            // "il y a 1 m." is a month in French's short form
            Some(n * 30 * 86400)
        } else {
            None
        };
    }
    if u == "ay" {
        // Turkish month ("2 ay önce")
        return Some(n * 30 * 86400);
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    // Most specific first: the units of languages whose words would otherwise
    // match a shorter prefix of another language ("minggu" is a week, not
    // minutes; "hari" a day and "jam" an hour, not "h"/"j"; Ukrainian "годин"
    // hours, not Russian "год" years). The generic one-letter prefixes come last.
    const UNITS: &[(&[&str], i64)] = &[
        (&["minggu"], 7 * DAY),
        (&["hari", "ngày", "gün", "päiv", "nap", "døgn", "zi", "ден", "дн", "dzie", "ημέρ", "يوم", "أيام", "दिन", "วัน", "日", "天", "일"], DAY),
        (
            &["jam", "giờ", "saat", "tunt", "tim", "hod", "ór", "час", "годин", "ώρ", "ساع", "घंट", "ชั", "時間", "小时", "小時", "시간"],
            HOUR,
        ),
        (
            &[
                "mo", "mois", "mes", "mês", "monat", "maand", "mies", "mån", "kuuk", "bulan", "tháng", "hónap", "lun", "měs", "месяц", "мес", "міс",
                "μήν", "شهر", "أشهر", "महीन", "माह", "เดือน", "か月", "ヶ月", "カ月", "个月", "個月", "개월",
            ],
            30 * DAY,
        ),
        (&["min", "menit", "dak", "perc", "phút", "мин", "хвил", "λεπτ", "دق", "मिनट", "นาที", "分", "분"], MIN),
        (
            &[
                "sem", "settiman", "woche", "week", "wk", "w", "tydz", "tyg", "veck", "uge", "uke", "viik", "hafta", "tuần", "hét", "týd", "săpt",
                "недел", "тиж", "εβδ", "أسبوع", "أسابيع", "सप्ताह", "हफ", "สัปดา", "週", "周", "주",
            ],
            7 * DAY,
        ),
        (&["sec", "seg", "sek", "seconde", "secondo", "detik", "san", "giây", "másod", "сек", "δευτ", "ثان", "ثوان", "सेकंड", "วินาที", "秒", "초", "s"], 1),
        (&["h", "heure", "stund", "ora", "ore", "uur", "godz"], HOUR),
        (&["d", "jour", "j", "tag", "giorn", "dag", "dzie"], DAY),
        (
            &[
                "y", "an", "añ", "jahr", "jaar", "ann", "rok", "lat", "år", "vuo", "év", "yıl", "tahun", "năm", "let", "год", "лет", "рік", "рок", "έτ",
                "سن", "वर", "साल", "ปี", "年", "년", "a",
            ],
            365 * DAY,
        ),
    ];
    for (prefixes, secs) in UNITS {
        // "s" and "a" only as the whole unit ("5s", "il y a 16 a"), as before
        if prefixes.iter().any(|x| if *x == "s" || *x == "a" { u == *x } else { u.starts_with(x) }) {
            return Some(n * secs);
        }
    }
    None
}

#[cfg(test)]
mod relative_time_tests {
    use super::relative_secs;
    #[test]
    fn languages() {
        let day = 86400;
        for (s, want) in [
            ("3 days ago", 3 * day),
            ("Streamed 2 weeks ago", 14 * day),
            ("17y", 17 * 365 * day),
            ("2mo", 60 * day),
            ("5 minutes ago", 300),
            ("il y a 3 jours", 3 * day),
            ("il y a 2 semaines", 14 * day),
            ("il y a 1 mois", 30 * day),
            ("il y a 4 ans", 4 * 365 * day),
            ("vor 3 Tagen", 3 * day),
            ("vor 5 Stunden", 5 * 3600),
            ("hace 2 días", 2 * day),
            ("hace 1 año", 365 * day),
            ("há 3 meses", 90 * day),
            ("3 giorni fa", 3 * day),
            ("2 ore fa", 7200),
            ("1 uur geleden", 3600),
            ("il y a 1 m.", 30 * day),
            ("il y a 16 a", 16 * 365 * day),
            ("il y a 2\u{a0}j", 2 * day),
            ("5m", 300),
            // as YouTube writes them (TV client, Home and Watch later)
            ("för 20 timmar sedan", 20 * 3600),
            ("för 4 dagar sedan", 4 * day),
            ("för 1 år sedan", 365 * day),
            ("for 20 timer siden", 20 * 3600),
            ("for 11 dage siden", 11 * day),
            ("for 4 døgn siden", 4 * day),
            ("20 часов назад", 20 * 3600),
            ("4 дня назад", 4 * day),
            ("11 дней назад", 11 * day),
            ("2 года назад", 2 * 365 * day),
            ("6 лет назад", 6 * 365 * day),
            ("20 годин тому", 20 * 3600),
            ("4 дні тому", 4 * day),
            ("1 рік тому", 365 * day),
            ("6 років тому", 6 * 365 * day),
            ("20 godzin temu", 20 * 3600),
            ("1 dzień temu", day),
            ("2 lata temu", 2 * 365 * day),
            ("20 saat önce", 20 * 3600),
            ("4 gün önce", 4 * day),
            ("2 yıl önce", 2 * 365 * day),
            ("20 jam yang lalu", 20 * 3600),
            ("4 hari yang lalu", 4 * day),
            ("2 minggu yang lalu", 14 * day),
            ("2 tahun yang lalu", 2 * 365 * day),
            ("20 時間前", 20 * 3600),
            ("4 日前", 4 * day),
            ("1 か月前", 30 * day),
            ("2 年前", 2 * 365 * day),
            ("20小时前", 20 * 3600),
            ("4天前", 4 * day),
            ("2年前", 2 * 365 * day),
            ("20시간 전", 20 * 3600),
            ("4일 전", 4 * day),
            ("2년 전", 2 * 365 * day),
            ("20 tuntia sitten", 20 * 3600),
            ("4 päivää sitten", 4 * day),
            ("1 vuosi sitten", 365 * day),
            ("2 vuotta sitten", 2 * 365 * day),
            ("20 giờ trước", 20 * 3600),
            ("4 ngày trước", 4 * day),
            ("2 năm trước", 2 * 365 * day),
            ("há 20 horas", 20 * 3600),
            ("há 1 ano", 365 * day),
            ("před 20 hodinami", 20 * 3600),
            ("před 4 dny", 4 * day),
            ("před 1 rokem", 365 * day),
            ("před 2 lety", 2 * 365 * day),
            ("20 órával ezelőtt", 20 * 3600),
            ("4 nappal ezelőtt", 4 * day),
            ("2 évvel ezelőtt", 2 * 365 * day),
            ("acum 20 de ore", 20 * 3600),
            ("acum 4 zile", 4 * day),
            ("acum 2 ani", 2 * 365 * day),
            ("πριν από 20 ώρες", 20 * 3600),
            ("πριν από 4 ημέρες", 4 * day),
            ("πριν από 2 έτη", 2 * 365 * day),
            ("قبل 20 ساعة", 20 * 3600),
            ("قبل 11 يومًا", 11 * day),
            ("قبل 4 أيام", 4 * day),
            ("قبل 3 سنوات", 3 * 365 * day),
            ("20 घंटे पहले", 20 * 3600),
            ("4 दिन पहले", 4 * day),
            ("2 वर्ष पहले", 2 * 365 * day),
            ("20 ชั่วโมงที่ผ่านมา", 20 * 3600),
            ("4 วันที่ผ่านมา", 4 * day),
            ("2 ปีที่แล้ว", 2 * 365 * day),
        ] {
            assert_eq!(relative_secs(s), Some(want), "{s}");
        }
        for s in ["1.2M views", "3 M de vues", "12 k vues", "1,5 Mio. Aufrufe", "450 watching", "Ed Sheeran", "100% DOCS", "21\u{a0}M", "4,8\u{a0}k", "2,5\u{a0}Md", "7,5 k spectateurs",
            "108\u{a0}799 visningar", "108 тыс. просмотров", "23 просмотра", "108 тис. переглядів", "108 tys. wyświetleń", "108 B görüntüleme",
            "108 rb x ditonton", "10万回視聴", "10万次观看", "조회수 10만회", "108 t. katselukertaa", "108k avspillinger", "108 N lượt xem",
            "108 mil visualizações", "108 tis. zhlédnutí", "108 E megtekintés", "108 K de vizionări", "108 χιλ. προβολές", "108 ألف مشاهدة",
            "23 हज़ार व्यू", "การดู 1 แสน ครั้ง"] {
            assert_eq!(relative_secs(s), None, "{s}");
        }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn relative_from_ts(ts: i64) -> String {
    use crate::i18n::{self, AppLanguage};

    let d = (now() - ts).max(0);
    let (n, messages, ru, uk) = if d < 60 {
        return i18n::t("just now").into();
    } else if d < 3600 {
        (
            d / 60,
            ["{0} minute ago", "{0} minutes ago"],
            ["минуту", "минуты", "минут"],
            ["хвилину", "хвилини", "хвилин"],
        )
    } else if d < 86400 {
        (
            d / 3600,
            ["{0} hour ago", "{0} hours ago"],
            ["час", "часа", "часов"],
            ["годину", "години", "годин"],
        )
    } else if d < 7 * 86400 {
        (
            d / 86400,
            ["{0} day ago", "{0} days ago"],
            ["день", "дня", "дней"],
            ["день", "дні", "днів"],
        )
    } else if d < 30 * 86400 {
        (
            d / (7 * 86400),
            ["{0} week ago", "{0} weeks ago"],
            ["неделю", "недели", "недель"],
            ["тиждень", "тижні", "тижнів"],
        )
    } else if d < 365 * 86400 {
        (
            d / (30 * 86400),
            ["{0} month ago", "{0} months ago"],
            ["месяц", "месяца", "месяцев"],
            ["місяць", "місяці", "місяців"],
        )
    } else {
        (
            d / (365 * 86400),
            ["{0} year ago", "{0} years ago"],
            ["год", "года", "лет"],
            ["рік", "роки", "років"],
        )
    };

    let (forms, suffix) = match i18n::language() {
        AppLanguage::Russian => (ru, "назад"),
        AppLanguage::Ukrainian => (uk, "тому"),
        _ => return i18n::format(messages[usize::from(n != 1)], &[&n]),
    };
    let form = match (n % 100, n % 10) {
        (11..=14, _) => forms[2],
        (_, 1) => forms[0],
        (_, 2..=4) => forms[1],
        _ => forms[2],
    };
    format!("{n} {form} {suffix}")
}

/// "27K", "1.2M", "27,124", "3 likes" -> a number; None without digits.
pub fn parse_count(s: &str) -> Option<u64> {
    let t: String = s.trim().chars().filter(|c| *c != ',' && !c.is_whitespace()).collect();
    let end = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let num: f64 = t[..end].parse().ok()?;
    let mult = match t[end..].chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('K') => 1e3,
        Some('M') => 1e6,
        Some('B') => 1e9,
        _ => 1.0,
    };
    Some((num * mult).round() as u64)
}

pub fn compact_number(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim_dec(n as f64 / 1e3, "K"),
        1_000_000..=999_999_999 => trim_dec(n as f64 / 1e6, "M"),
        _ => trim_dec(n as f64 / 1e9, "B"),
    }
}

fn trim_dec(v: f64, suf: &str) -> String {
    if v >= 100.0 { format!("{:.0}{suf}", v) } else { format!("{:.1}{suf}", v).replace(".0", "") }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rel() {
        let n = now();
        assert!((parse_relative_time("17y ago").unwrap() - (n - 17 * 365 * 86400)).abs() < 5);
        assert!((parse_relative_time("2mo ago").unwrap() - (n - 60 * 86400)).abs() < 5);
        assert!((parse_relative_time("3 days ago").unwrap() - (n - 3 * 86400)).abs() < 5);
        assert!((parse_relative_time("Streamed 1 hour ago").unwrap() - (n - 3600)).abs() < 5);
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(compact_number(1_234_567), "1.2M");
    }
}

#[cfg(test)]
mod count_tests {
    #[test]
    fn counts() {
        use super::parse_count;
        assert_eq!(parse_count("27K"), Some(27_000));
        assert_eq!(parse_count("1.2M"), Some(1_200_000));
        assert_eq!(parse_count("27,124"), Some(27_124));
        assert_eq!(parse_count("403"), Some(403));
        assert_eq!(parse_count("Like"), None);
    }
}
