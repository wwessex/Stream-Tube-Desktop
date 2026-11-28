//! Normalize VOD captions before handing them to mpv.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use unicode_segmentation::UnicodeSegmentation;

const MAX_CUE_MS: u64 = 10_000;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CUE_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTIVE: usize = 256;
const MAX_LINE_LEN: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cue {
    start: u64,
    end: u64,
    text: String,
    positioned: bool,
}

#[derive(Default)]
struct Normalizer {
    buffer: Option<String>,
}

impl Normalizer {
    // SmartTube's repeated-line fix (MIT); keep its whitespace and replacement semantics.
    // https://github.com/yuliskov/SmartTube/blob/b6a1c8221fa395f91786d29202454b864df2a91e/common/src/main/java/com/liskovsoft/smartyoutubetv2/common/exoplayer/other/SubtitleManager.java#L98-L129
    fn apply(&mut self, cues: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for text in cues {
            if text.ends_with(['\n', ' ']) {
                self.buffer = Some(text.clone());
            } else if text.contains('\n') {
                let value = match &self.buffer {
                    Some(previous) if text.contains(previous.as_str()) => {
                        text.replace(previous.as_str(), "").replace('\n', "")
                    }
                    _ => text.clone(),
                };
                out.push(value);
                let mut split = text.split('\n');
                let _ = split.next();
                let second = split.next().unwrap();
                let last = if split.next().is_none() { second } else { text };
                self.buffer = Some(last.to_owned());
            } else {
                let value = self.buffer.as_ref().map_or_else(
                    || text.clone(),
                    |previous| text.replace(previous.as_str(), ""),
                );
                self.buffer = Some(value.clone());
                out.push(value);
            }
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Frame {
    ms: u64,
    decoded: Vec<String>,
    normalized: Vec<String>,
}

// SmartTube's SubtitleView keeps the last cue to avoid overlapping captions.
fn visible(cues: &[String]) -> &[String] {
    &cues[cues.len().saturating_sub(1)..]
}

fn decode_active(cues: &[Cue], active: impl Iterator<Item = usize>) -> (Vec<String>, Option<usize>) {
    let mut decoded = Vec::new();
    let mut normal = Vec::new();
    let (mut positioned, mut unpositioned) = (None, None);
    for i in active {
        let cue = &cues[i];
        if cue.positioned {
            decoded.push(cue.text.clone());
            positioned = Some(i);
        } else {
            normal.push(cue.text.as_str());
            unpositioned = Some(i);
        }
    }
    // ExoPlayer merges unpositioned cues and appends them after positioned ones.
    if !normal.is_empty() {
        decoded.push(normal.join("\n"));
    }
    (decoded, unpositioned.or(positioned))
}

// Precompute the callbacks in playback order so seeking does not depend on playback history.
fn timeline(cues: &[Cue]) -> Option<Vec<Frame>> {
    #[derive(Default)]
    struct Event {
        starts: Vec<usize>,
        ends: Vec<usize>,
    }
    let mut events = BTreeMap::<u64, Event>::new();
    for (i, cue) in cues.iter().enumerate() {
        let end = cue.end.min(cue.start.saturating_add(MAX_CUE_MS));
        if cue.start < end {
            events.entry(cue.start).or_default().starts.push(i);
            events.entry(end).or_default().ends.push(i);
        }
    }
    let mut normalizer = Normalizer::default();
    let mut active = BTreeSet::new();
    struct Displayed {
        text: String,
        buffer: Option<String>,
        standalone: bool,
    }
    let mut displayed = BTreeMap::<usize, Displayed>::new();
    let mut frames: Vec<Frame> = Vec::with_capacity(events.len());
    let mut previous_winner = None;
    let mut bytes = 0usize;
    for (ms, event) in events {
        for i in event.ends {
            active.remove(&i);
            displayed.remove(&i);
        }
        for &i in &event.starts {
            active.insert(i);
        }
        if active.len() > MAX_ACTIVE {
            return None;
        }
        let (decoded, winner) = decode_active(cues, active.iter().copied());
        let (normalized, standalone) = if let Some(i) = winner {
            if event.starts.contains(&i) {
                // Feeding old, still-active cues back into the buffer can replay or erase them.
                let (incoming, _) = decode_active(cues, event.starts.iter().copied());
                (normalizer.apply(&incoming), event.starts.len() == 1)
            } else if previous_winner == winner && frames.last().is_some_and(|f| f.decoded == decoded) {
                (frames.last()?.normalized.clone(), displayed.get(&i).is_some_and(|saved| saved.standalone))
            } else if let Some(saved) = displayed.get(&i).filter(|saved| {
                !saved.text.is_empty() && (saved.standalone || decoded.last().is_some_and(|s| s.ends_with(&saved.text)))
            }) {
                // Restore both the text and its repetition state when a surviving cue returns.
                normalizer.buffer.clone_from(&saved.buffer);
                (vec![saved.text.clone()], saved.standalone)
            } else {
                // An expiry is not new speech; do not remove words using the expired cue's buffer.
                normalizer = Normalizer::default();
                (normalizer.apply(&decoded), active.len() == 1)
            }
        } else {
            // Do not deduplicate speech across a period without captions.
            normalizer = Normalizer::default();
            (Vec::new(), false)
        };
        if let Some(i) = winner {
            if let Some(text) = visible(&normalized).last() {
                displayed.insert(i, Displayed {
                    text: text.clone(), buffer: normalizer.buffer.clone(), standalone,
                });
            } else {
                displayed.remove(&i);
            }
        }
        previous_winner = winner;
        bytes = bytes.checked_add(decoded.iter().chain(&normalized).map(String::len).sum())?;
        if bytes > MAX_OUTPUT_BYTES {
            return None;
        }
        frames.push(Frame { ms, decoded, normalized });
    }
    Some(frames)
}

// Java trim leaves non-breaking spaces intact.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= '\u{20}')
}

fn timestamp(s: &str) -> Option<u64> {
    let (seconds, frac) = s.split_once('.')?;
    if frac.len() != 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<_> = seconds.split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let mut total = 0u64;
    for part in parts {
        total = total.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    total.checked_mul(1000)?.checked_add(frac.parse().ok()?)
}

// Match SmartTube's percentage parsing and its Float.MIN_VALUE unset sentinel.
// https://github.com/yuliskov/SmartTube/blob/b6a1c8221fa395f91786d29202454b864df2a91e/exoplayer-amzn-2.10.6/library/core/src/main/java/com/google/android/exoplayer2/text/webvtt/WebvttParserUtil.java
fn java_float(s: &str) -> Option<f32> {
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned == "NaN" { return Some(f32::NAN); }
    if unsigned == "Infinity" {
        return Some(if s.starts_with('-') { f32::NEG_INFINITY } else { f32::INFINITY });
    }
    let s = s.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(s);
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if let Some(hex) = unsigned.strip_prefix("0x").or_else(|| unsigned.strip_prefix("0X")) {
        let (mantissa, exponent) = hex.split_once(['p', 'P'])?;
        let exponent = exponent.parse::<i32>().ok()?;
        let mut value = 0.0f64;
        let mut fraction = None;
        let mut digits = 0;
        for c in mantissa.chars() {
            if c == '.' && fraction.is_none() {
                fraction = Some(0i32);
            } else {
                value = value * 16.0 + f64::from(c.to_digit(16)?);
                digits += 1;
                if let Some(n) = &mut fraction { *n = n.saturating_add(4); }
            }
        }
        if digits == 0 { return None; }
        let value = value * 2.0f64.powi(exponent.saturating_sub(fraction.unwrap_or(0)));
        return Some(if s.starts_with('-') { -value as f32 } else { value as f32 });
    }
    if !s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')) {
        return None;
    }
    s.parse().ok()
}

fn positioned(settings: &str) -> bool {
    let (mut line, mut position) = (false, false);
    for setting in settings.split_ascii_whitespace() {
        let Some((name, value)) = setting.split_once(':') else { continue; };
        let value = value.split(',').next().unwrap_or("");
        let percent = value.strip_suffix('%').and_then(java_float);
        match name {
            "line" => {
                if let Some(v) = percent {
                    line = v / 100.0 != f32::from_bits(1);
                } else if value.parse::<i32>().is_ok() {
                    line = true;
                }
            }
            "position" => {
                if let Some(v) = percent {
                    position = v / 100.0 != f32::from_bits(1);
                }
            }
            _ => {}
        }
    }
    line || position
}

fn html_entity(entity: &str) -> Option<char> {
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "copy" => '©',
        "reg" => '®',
        _ => {
            let n = entity.strip_prefix('#')?;
            let n = if let Some(hex) = n.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                n.parse().ok()?
            };
            char::from_u32(n).filter(|&c| c != '\0')?
        }
    })
}

// Approximate Android Html.fromHtml for YouTube markup; fall back on unknown tags/entities.
fn html_plain(markup: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = markup;
    while !rest.is_empty() {
        let c = rest.chars().next()?;
        if c == '<' {
            let end = rest.find('>')?;
            let tag = rest[1..end].trim();
            let name = tag.trim_start_matches('/').split([' ', '.', '\t', '/']).next()?;
            if name.eq_ignore_ascii_case("br") {
                out.push('\n');
            } else if !matches!(
                name.to_ascii_lowercase().as_str(),
                "b" | "i" | "u" | "c" | "v" | "lang" | "span" | "font" | "strong" | "em"
            ) && timestamp(tag).is_none() {
                return None;
            }
            rest = &rest[end + 1..];
            continue;
        }
        let mut consumed = c.len_utf8();
        let c = if c == '&' {
            if let Some(end) = rest.find(';').filter(|&i| i <= 32 && !rest[1..i].contains([' ', '\n', '<', '&'])) {
                consumed = end + 1;
                html_entity(&rest[1..end])?
            } else {
                c
            }
        } else {
            c
        };
        if matches!(c, ' ' | '\n') {
            if !out.is_empty() && !out.ends_with([' ', '\n']) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        rest = &rest[consumed..];
    }
    Some(out)
}

// SmartTube parses WebVTT markup again after Html.fromHtml, even if entities were decoded.
fn parse_payload(markup: &str) -> Option<String> {
    let html = html_plain(markup)?;
    let mut out = String::new();
    let mut rest = html.as_str();
    while let Some(c) = rest.chars().next() {
        match c {
            '<' => {
                if rest.len() == 1 { break; }
                let Some(end) = rest.find('>') else { break; };
                rest = &rest[end + 1..];
            }
            '&' => {
                if let Some(end) = rest.find([';', ' ']) {
                    match &rest[1..end] {
                        "lt" => out.push('<'),
                        "gt" => out.push('>'),
                        "nbsp" => out.push(' '),
                        "amp" => out.push('&'),
                        _ => {}
                    }
                    if rest.as_bytes()[end] == b' ' {
                        out.push(' ');
                    }
                    rest = &rest[end + 1..];
                } else {
                    out.push(c);
                    rest = &rest[1..];
                }
            }
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Some(out)
}

fn parse_vtt(raw: &str) -> Option<Vec<Cue>> {
    if raw.len() > MAX_INPUT_BYTES { return None; }
    let raw = raw.replace("\r\n", "\n").replace('\r', "\n");
    // ParsableByteArray.readLine discards a BOM at every line start.
    let mut lines = raw.split_terminator('\n').map(|line| line.strip_prefix('\u{feff}').unwrap_or(line));
    if !lines.next()?.starts_with("WEBVTT") { return None; }
    for line in lines.by_ref() {
        if line.starts_with("X-TIMESTAMP-MAP") { return None; } // segmented/live VTT
        if line.is_empty() { break; }
    }
    let mut cues = Vec::new();
    let mut skipped = false;
    while let Some(first) = lines.next() {
        if first.is_empty() { continue; }
        if first.starts_with("NOTE") || first == "STYLE" || first == "REGION" {
            if first == "STYLE" && !cues.is_empty() { return None; }
            for line in lines.by_ref() {
                if line.is_empty() { break; }
            }
            continue;
        }
        let header = if first.contains("-->") { first } else { lines.next()? };
        let (start, rest) = header.split_once("-->")?;
        let rest = java_trim(rest);
        let end_index = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (Some(start), Some(end)) = (timestamp(java_trim(start)), timestamp(&rest[..end_index])) else {
            // The original decoder skips bad headers rather than losing the whole track.
            skipped = true;
            continue;
        };
        if end < start { return None; }
        let mut markup = String::new();
        for line in lines.by_ref() {
            if line.is_empty() { break; }
            if !markup.is_empty() { markup.push('\n'); }
            markup.push_str(java_trim(line));
            if markup.len() > MAX_CUE_BYTES { return None; }
        }
        cues.push(Cue {
            start,
            end,
            text: parse_payload(&markup)?,
            positioned: positioned(&rest[end_index..]),
        });
        if cues.len() > 100_000 { return None; }
    }
    if skipped && cues.is_empty() { return None; }
    Some(cues)
}

fn format_timestamp(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn escape_payload(s: &str) -> String {
    // An empty line would terminate the WebVTT cue.
    s.split('\n')
        .map(|line| {
            if line.is_empty() {
                "&nbsp;".into()
            } else {
                line.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 { out.push('\n'); }
        let offsets: Vec<_> = line.grapheme_indices(true).map(|(i, _)| i)
            .chain(std::iter::once(line.len())).collect();
        let len = offsets.len() - 1;
        let space = |i: usize| matches!(&line[offsets[i]..offsets[i + 1]], " " | "\t");
        let mut breaks = unicode_linebreak::linebreaks(line)
            .filter_map(|(at, _)| offsets.binary_search(&at).ok()).peekable();
        let mut start = 0;
        while len - start > MAX_LINE_LEN {
            let limit = start + MAX_LINE_LEN;
            let mut after = limit;
            while after < len && space(after) { after += 1; }
            let mut next = limit;
            while let Some(at) = breaks.peek().copied().filter(|&at| at <= after) {
                breaks.next();
                if at > start { next = at; }
            }
            let mut end = next;
            while end > start && space(end - 1) { end -= 1; }
            out.push_str(&line[offsets[start]..offsets[end]]);
            while next < len && space(next) { next += 1; }
            if end > start && next < len { out.push('\n'); }
            start = next;
        }
        out.push_str(&line[offsets[start]..]);
    }
    out
}

// Android breaks oversized words at grapheme boundaries; libass needs break hints.
// https://android.googlesource.com/platform/frameworks/minikin/+/27b5e0fac2bde35a0b1f4d7b4dbff884471ec190/libs/minikin/GreedyLineBreaker.cpp
fn wrap_points(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    for (end, _) in unicode_linebreak::linebreaks(text) {
        let run = &text[start..end];
        let core = run.trim_end_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{200b}'));
        if core.graphemes(true).take(16).count() == 16 {
            for (i, g) in core.graphemes(true).enumerate() {
                if i > 0 && i % 4 == 0 { out.push('\u{200b}'); }
                out.push_str(g);
            }
            out.push_str(&run[core.len()..]);
        } else {
            out.push_str(run);
        }
        start = end;
    }
    out.push_str(&text[start..]);
    out
}

/// Strip source geometry so mpv can wrap captions to fit the window.
/// Return `None` for unsupported tracks.
pub(super) fn render_webvtt(raw: &str) -> Option<String> {
    let frames = timeline(&parse_vtt(raw)?)?;
    let mut out = String::from("WEBVTT\n\n");
    for pair in frames.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        for text in visible(&a.normalized) {
            if text.trim().is_empty() { continue; }
            let text = wrap_points(&wrap_lines(text));
            writeln!(
                out,
                "{} --> {}\n{}\n",
                format_timestamp(a.ms),
                format_timestamp(b.ms),
                escape_payload(&text)
            ).ok()?;
            if out.len() > MAX_OUTPUT_BYTES { return None; }
        }
    }
    Some(out)
}

pub(super) fn write_http_response(
    out: &mut impl std::io::Write,
    body: &[u8],
    range: Option<(u64, Option<u64>)>,
    head: bool,
) -> std::io::Result<()> {
    let len = body.len() as u64;
    let bytes = if let Some((start, end)) = range {
        let end = end.unwrap_or(len.saturating_sub(1)).min(len.saturating_sub(1));
        if start >= len || end < start {
            write!(out, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return out.flush();
        }
        write!(out, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\n")?;
        &body[start as usize..=end as usize]
    } else {
        out.write_all(b"HTTP/1.1 200 OK\r\n")?;
        body
    };
    write!(out, "Content-Type: text/vtt; charset=utf-8\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len())?;
    if !head { out.write_all(bytes)?; }
    out.flush()
}
