//! Download manager (yt-dlp when available, native fallback) and external player launching.
pub(crate) mod transcript;

use crate::api::streams::{self, hide_window};
use crate::i18n::{t as translate, tf};
use crate::api::types::{CaptionTrack, VideoItem};
use crate::store;
use crate::ui::toast;
use gpui_kit::*;
use parking_lot::Mutex;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq)]
pub enum JobStatus {
    Queued,
    Running,
    Done(String),
    Failed(String),
    Cancelled,
}

#[derive(Clone)]
pub struct Job {
    pub id: u64,
    pub video: VideoItem,
    pub format: String,
    pub status: JobStatus,
    pub progress: f32,
    pub detail: String,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Downloads {
    pub jobs: Arc<Mutex<Vec<Job>>>,
    next: u64,
    ticking: bool,
}

impl Global for Downloads {}

pub const TRANSCRIPTS: &[(&str, &str)] = &[("txt", "Transcript (.txt)"), ("srt", "Subtitles (.srt)")];

pub const FORMATS: &[(&str, &str)] = &[
    ("1080", "1080p (recommended)"),
    ("best", "Best available (up to 4K, large files)"),
    ("720", "Up to 720p"),
    ("480", "Up to 480p"),
    ("audio", "Audio only"),
];

/// Starts several downloads, asking for the folder at most once.
pub fn start_many(videos: Vec<VideoItem>, cx: &mut App) {
    if videos.is_empty() {
        return;
    }
    if !store::settings_snapshot().download_dir_chosen {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some(translate("Save downloads here").into()) });
        cx.spawn(async move |cx| {
            let picked = rx.await.ok().and_then(|r| r.ok()).flatten().and_then(|p| p.into_iter().next());
            let _ = cx.update(|cx| match picked {
                Some(dir) => {
                    store::update_settings(|s| {
                        s.download_dir = dir.to_string_lossy().into_owned();
                        s.download_dir_chosen = true;
                    });
                    start_many(videos, cx);
                }
                None => toast(cx, translate("Download cancelled")),
            });
        })
        .detach();
        return;
    }
    let n = videos.len();
    for v in videos {
        start(v, None, cx);
    }
    if n > 1 {
        toast(cx, tf!("Downloading {0} videos", n));
    }
}

pub fn start(video: VideoItem, format: Option<String>, cx: &mut App) {
    start_with_track(video, format, None, cx);
}

pub(crate) fn start_with_track(video: VideoItem, format: Option<String>, track: Option<CaptionTrack>, cx: &mut App) {
    // First download: ask where to save (once), like a browser does.
    if !store::settings_snapshot().download_dir_chosen {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(translate("Save downloads here").into()),
        });
        cx.spawn(async move |cx| {
            let picked = rx.await.ok().and_then(|r| r.ok()).flatten().and_then(|p| p.into_iter().next());
            let _ = cx.update(|cx| match picked {
                Some(dir) => {
                    store::update_settings(|s| {
                        s.download_dir = dir.to_string_lossy().into_owned();
                        s.download_dir_chosen = true;
                    });
                    start_with_track(video, format, track, cx);
                }
                None => toast(cx, translate("Download cancelled")),
            });
        })
        .detach();
        return;
    }
    let fmt = format.unwrap_or_else(|| store::settings_snapshot().download_format.clone());
    // without yt-dlp only 360p (one combined stream) or audio can be saved
    let transcript = TRANSCRIPTS.iter().any(|(format, _)| *format == fmt);
    if !transcript && fmt != "audio" && fmt != "single" && streams::ytdlp_path().is_none() {
        if YTDLP_FETCHING.load(Ordering::SeqCst) {
            PENDING.lock().push((video, Some(fmt)));
            return;
        }
        let v2 = video.clone();
        crate::ui::toast_with_actions(
            cx,
            translate("Higher quality downloads use yt-dlp"),
            vec![
                (
                    translate(GET_YTDLP).into(),
                    Arc::new(move |_, cx: &mut App| {
                        PENDING.lock().push((video.clone(), Some(fmt.clone())));
                        fetch_ytdlp(cx);
                    }),
                ),
                (translate("Download 360p").into(), Arc::new(move |_, cx: &mut App| start(v2.clone(), Some("single".into()), cx))),
            ],
        );
        return;
    }
    // separate video + audio streams need FFmpeg to be joined into one file
    if !transcript && fmt != "audio" && fmt != "single" && streams::ytdlp_path().is_some() && ffmpeg_path().is_none() {
        if FFMPEG_FETCHING.load(Ordering::SeqCst) {
            // queued behind the FFmpeg download
            PENDING.lock().push((video, Some(fmt)));
            return;
        }
        let v2 = video.clone();
        let mut actions: Vec<(SharedString, Arc<dyn Fn(&mut Window, &mut App)>)> = Vec::new();
        if let Some(label) = GET_FFMPEG {
            actions.push((translate(label).into(), Arc::new(move |_, cx| {
                PENDING.lock().push((video.clone(), Some(fmt.clone())));
                fetch_ffmpeg(cx);
            })));
        }
        actions.push((translate("Download 360p").into(), Arc::new(move |_, cx| start(v2.clone(), Some("single".into()), cx))));
        crate::ui::toast_with_actions(cx, translate(GET_FFMPEG_HELP), actions);
        return;
    }
    let d = cx.default_global::<Downloads>();
    d.next += 1;
    let job = Job {
        id: d.next,
        video: video.clone(),
        format: fmt,
        status: JobStatus::Queued,
        progress: 0.0,
        detail: String::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let jobs = d.jobs.clone();
    jobs.lock().push(job.clone());
    toast(cx, tf!("Downloading “{0}”", video.title));
    let jid = job.id;
    let cancel = job.cancel.clone();
    let jobs2 = jobs.clone();
    let (saved, completion) = futures::channel::oneshot::channel();
    if let Some(track) = &track {
        let name = transcript::display_name(track).to_owned();
        let srt = job.format == "srt";
        cx.spawn(async move |cx| {
            if completion.await.is_ok() {
                let _ = cx.update(|cx| toast(cx, if srt { tf!("Subtitles saved · {0}", name) } else { tf!("Transcript saved · {0}", name) }));
            }
        }).detach();
    }
    std::thread::spawn(move || {
        let update = |f: &dyn Fn(&mut Job)| {
            if let Some(j) = jobs2.lock().iter_mut().find(|j| j.id == jid) {
                f(j);
            }
        };
        // at most 3 downloads at a time; the rest wait their turn
        loop {
            if cancel.load(Ordering::SeqCst) {
                update(&|j| j.status = JobStatus::Cancelled);
                return;
            }
            let running = jobs2.lock().iter().filter(|j| j.status == JobStatus::Running).count();
            if running < 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        update(&|j| j.status = JobStatus::Running);
        let res = run_job(&job, track.as_ref(), &cancel, &|p, detail| {
            update(&|j| {
                j.progress = p;
                j.detail = detail.to_string();
            })
        })
        .map(|path| resolve_output(&path, &job.video.id).unwrap_or(path));
        match res {
            Ok(path) => {
                update(&|j| {
                    j.status = JobStatus::Done(path.clone());
                    j.progress = 1.0;
                });
                let _ = saved.send(());
            }
            Err(e) if cancel.load(Ordering::SeqCst) => {
                let _ = e;
                update(&|j| j.status = JobStatus::Cancelled)
            }
            Err(e) => update(&|j| j.status = JobStatus::Failed(e.to_string())),
        }
    });
    ensure_ticking(cx);
}

fn ensure_ticking(cx: &mut App) {
    if cx.default_global::<Downloads>().ticking {
        return;
    }
    cx.default_global::<Downloads>().ticking = true;
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            let active = cx
                .update(|cx| {
                    cx.refresh_windows();
                    let d = cx.default_global::<Downloads>();
                    let active = d.jobs.lock().iter().any(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running));
                    if !active {
                        d.ticking = false;
                    }
                    if !active {
                        save_history(cx);
                    }
                    active
                });
            if !active {
                break;
            }
        }
    })
    .detach();
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(150)
        .collect()
}

/// The user's file name template filled in for a video, or None for the default name.
fn custom_name(video: &crate::api::types::VideoItem, template: &str) -> Option<String> {
    if template.trim().is_empty() {
        return None;
    }
    let name = template
        .replace("{title}", &video.title)
        .replace("{channel}", &video.channel_name)
        .replace("{id}", &video.id);
    let name = sanitize(&name).trim_end_matches('.').trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn run_job(job: &Job, track: Option<&CaptionTrack>, cancel: &AtomicBool, progress: &dyn Fn(f32, &str)) -> anyhow::Result<String> {
    let s = store::settings_snapshot();
    // yt-dlp reads % in the output name as a template
    let custom = custom_name(&job.video, &s.download_name);
    let ytdlp_name = custom.as_ref().map(|n| n.replace('%', "%%"));
    let dir = std::path::PathBuf::from(if s.download_dir.is_empty() { ".".into() } else { s.download_dir.clone() });
    std::fs::create_dir_all(&dir)?;
    if TRANSCRIPTS.iter().any(|(format, _)| *format == job.format) {
        return transcript::export(job, &dir, track, cancel);
    }
    let url = format!("https://www.youtube.com/watch?v={}", job.video.id);
    if let Some(bin) = streams::ytdlp_path() {
        let fmt = match job.format.as_str() {
            "audio" => "bestaudio[ext=m4a]/bestaudio".to_string(),
            // one file with video and audio (no FFmpeg needed)
            "single" => "b".to_string(),
            "best" => "bv*+ba/b".to_string(),
            h => format!("bv*[height<={h}]+ba/b[height<={h}]/b"),
        };
        let res = ytdlp_download(&bin, &dir, &url, &fmt, job, &s, ytdlp_name.as_deref(), None, cancel, progress);
        return match res {
            // YouTube wants a signed-in viewer: retry with the browser sign-in's
            // cookies, the way playback does (#260)
            Err(e) if (streams::needs_sign_in(&e.to_string()) || e.to_string().contains("bot check")) && !cancel.load(Ordering::SeqCst) => match crate::api::web::write_cookies_file() {
                Some(cookies) => {
                    progress(0.0, translate("Retrying…"));
                    let res = ytdlp_download(&bin, &dir, &url, &fmt, job, &s, ytdlp_name.as_deref(), Some(&cookies), cancel, progress);
                    let _ = std::fs::remove_file(&cookies);
                    res
                }
                None => Err(e),
            },
            other => other,
        };
    }
    native_download(job, &dir, custom.as_deref(), cancel, progress)
}

/// One download through yt-dlp, with the fallbacks for refused stream URLs and
/// videos made for kids.
#[allow(clippy::too_many_arguments)]
fn ytdlp_download(bin: &str, dir: &std::path::Path, url: &str, fmt: &str, job: &Job, s: &store::Settings, ytdlp_name: Option<&str>, cookies: Option<&std::path::Path>, cancel: &AtomicBool, progress: &dyn Fn(f32, &str)) -> anyhow::Result<String> {
        let res = match ytdlp_run(bin, dir, url, fmt, s, &[], ytdlp_name, cookies, cancel, progress) {
            Err(e) if e.to_string().contains("403") && !cancel.load(Ordering::SeqCst) => {
                // a stream URL was refused: retry preferring the HLS formats, which don't need a PO token
                progress(0.0, translate("Retrying…"));
                let hls = format!("{}/{}", fmt.replace("bv*", "bv*[protocol^=m3u8]"), fmt);
                ytdlp_run(bin, dir, url, &hls, s, &[], ytdlp_name, cookies, cancel, progress)
            }
            other => other,
        };
        match res {
            Err(e) if retry_with_ios(&e.to_string()) && !cancel.load(Ordering::SeqCst) => {
                // videos made for kids are refused to yt-dlp's default clients;
                // the iOS client still serves them (its HLS formats need no PO token)
                progress(0.0, translate("Retrying…"));
                match ytdlp_run(bin, dir, url, fmt, s, &["--extractor-args", YTDLP_IOS], ytdlp_name, cookies, cancel, progress) {
                    Err(e2) if e2.to_string() == "cancelled" => Err(e2),
                    Err(_) if !cancel.load(Ordering::SeqCst) => {
                        // yt-dlp found no formats either: download the HLS manifest the
                        // app itself plays these videos from
                        match streams::ios_hls_manifest(&job.video.id) {
                            Ok(manifest) => {
                                let fmt = if job.format == "single" { "bv*+ba/b".to_string() } else { fmt.to_string() };
                                let name = ytdlp_name.map(str::to_string).unwrap_or_else(|| format!("{} [{}]", sanitize(&job.video.title).trim_end_matches('.').replace('%', "%%"), job.video.id));
                                ytdlp_run(bin, dir, &manifest, &fmt, s, &[], Some(&name), cookies, cancel, progress)
                                    .map_err(|e3| if e3.to_string() == "cancelled" { e3 } else { e })
                            }
                            Err(_) => Err(e),
                        }
                    }
                    other => other,
                }
            }
            other => other,
        }
}

/// yt-dlp extractor arguments for the iOS client, including the formats yt-dlp
/// would otherwise hold back for lack of a PO token.
// the Android client is the one still served videos made for kids (360p), see streams::android
pub const YTDLP_IOS: &str = "youtube:player_client=ios,android;formats=missing_pot";

/// Whether a failed yt-dlp run is worth retrying through the iOS client.
pub fn retry_with_ios(err: &str) -> bool {
    let e = err.to_lowercase();
    !e.contains("cancelled") && (e.contains("not available") || e.contains("unavailable") || e.contains("requested format") || e.contains("reloaded"))
}

/// `name`: the file name without extension, when the URL isn't a YouTube page
/// (yt-dlp would otherwise name the file after the manifest).
#[allow(clippy::too_many_arguments)]
fn ytdlp_run(bin: &str, dir: &std::path::Path, url: &str, fmt: &str, s: &store::Settings, extra: &[&str], name: Option<&str>, cookies: Option<&std::path::Path>, cancel: &AtomicBool, progress: &dyn Fn(f32, &str)) -> anyhow::Result<String> {
    {
        let mut cmd = Command::new(bin);
        cmd.args(extra);
        if let Some(c) = cookies {
            cmd.arg("--cookies").arg(c);
        }
        cmd.args(crate::api::jsc::ytdlp_runtime_args());
        // yt-dlp writes in the console code page otherwise, and its error text was lost (#260)
        cmd.env("PYTHONIOENCODING", "utf-8");
        if let Some(p) = crate::store::proxy_setting() {
            cmd.arg("--proxy").arg(p);
        }
        if let Some(ff) = ffmpeg_path().filter(|f| f != "ffmpeg") {
            cmd.arg("--ffmpeg-location").arg(ff);
        }
        if s.ipv4_only {
            // YouTube's bot check is triggered far more often on IPv6
            cmd.arg("--force-ipv4");
        }
        // --print makes yt-dlp quiet, so progress is asked for explicitly in a fixed format.
        // Fragments and chunks download in parallel, which is much faster than one stream.
        cmd.args(["--newline", "--no-warnings", "--no-playlist", "--progress", "--progress-delta", "0.4"])
            .args(["--progress-template", "download:[dl] %(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s"])
            .args(["-N", "8", "--http-chunk-size", "10M", "-f", fmt, "-o"])
            .arg(match name {
                Some(n) => dir.join(format!("{n}.%(ext)s")),
                None => dir.join("%(title).150B [%(id)s].%(ext)s"),
            })
            .args(["--print", "after_move:filepath"])
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if s.sponsorblock && ffmpeg_path().is_some() {
            cmd.args(["--sponsorblock-mark", "all"]);
        }
        hide_window(&mut cmd);
        let mut child = cmd.spawn()?;
        progress(0.0, translate("Getting video info…"));
        let stdout = child.stdout.take().unwrap();
        // drain stderr on its own thread so a chatty yt-dlp can't block on a full pipe
        let stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = std::io::Read::read_to_end(&mut BufReader::new(stderr), &mut b);
            String::from_utf8_lossy(&b).into_owned()
        });
        let mut final_path = String::new();
        // video and audio arrive one after the other: weight them 90 / 10
        let (mut phase, mut last_pct) = (0u32, 0.0f32);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                anyhow::bail!("cancelled");
            }
            if let Some(rest) = line.strip_prefix("[dl]") {
                let mut parts = rest.trim().split('|');
                let pct = parts.next().and_then(|p| p.trim().trim_end_matches('%').parse::<f32>().ok()).unwrap_or(0.0);
                let speed = parts.next().unwrap_or("").trim();
                let eta = parts.next().unwrap_or("").trim();
                if pct + 20.0 < last_pct {
                    phase += 1;
                }
                last_pct = pct;
                let overall = if phase == 0 { pct / 100.0 * 0.9 } else { 0.9 + pct / 100.0 * 0.1 };
                let what = if phase == 0 { "" } else { translate("Audio, ") };
                let detail = if pct >= 100.0 { translate("Finishing…").to_string() } else { tf!("{0}{1}%, {2}, {3} left", what, format_args!("{:.0}", pct), speed, eta) };
                progress(overall.min(0.99), &detail);
            } else if !line.starts_with('[') && !line.trim().is_empty() {
                final_path = line.trim().to_string();
            }
        }
        let status = child.wait()?;
        let err = err_reader.join().unwrap_or_default();
        if !status.success() {
            let last = err.lines().rev().find(|l| l.contains("ERROR")).or(err.lines().last()).unwrap_or("yt-dlp failed").to_string();
            if last.contains("confirm you") && last.contains("bot") {
                anyhow::bail!("YouTube asked for a bot check. Turn on \"Use IPv4 only\" in Settings → Network, or update yt-dlp.");
            }
            anyhow::bail!("{}", last.trim_start_matches("ERROR: ").trim());
        }
        Ok(if final_path.is_empty() { dir.to_string_lossy().into_owned() } else { final_path })
    }
}

/// Without yt-dlp: progressive (muxed) stream or audio-only stream.
fn native_download(job: &Job, dir: &std::path::Path, name: Option<&str>, cancel: &AtomicBool, progress: &dyn Fn(f32, &str)) -> anyhow::Result<String> {
    let info = streams::get_streams(&job.video.id)?;
    let (fmt, ext) = if job.format == "audio" {
        let a = info.audio.iter().find(|a| a.mime.contains("mp4")).or(info.audio.first()).ok_or_else(|| anyhow::anyhow!("no audio stream"))?;
        (a.clone(), if a.mime.contains("mp4") { "m4a" } else { "webm" })
    } else {
        let m = info.muxed.iter().max_by_key(|m| m.height).ok_or_else(|| anyhow::anyhow!("no combined stream (install yt-dlp for high quality downloads)"))?;
        (m.clone(), "mp4")
    };
    let stem = name.map(str::to_string).unwrap_or_else(|| format!("{} [{}]", sanitize(&info.title), job.video.id));
    let path = dir.join(format!("{stem}.{ext}"));
    let mut f = std::fs::File::create(&path)?;
    crate::api::http::download_to(&fmt.url, &mut f, |done, total| {
        let p = total.map(|t| done as f32 / t as f32).unwrap_or(0.0);
        progress(p, &format!("{:.1} MB", done as f64 / 1e6));
        !cancel.load(Ordering::SeqCst)
    })?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn cancel(id: u64, cx: &mut App) {
    if let Some(j) = cx.default_global::<Downloads>().jobs.lock().iter().find(|j| j.id == id) {
        j.cancel.store(true, Ordering::SeqCst);
    }
}

pub fn open_external(id: &str, cx: &mut App) {
    open_external_at(id, 0.0, cx)
}

pub fn open_external_at(id: &str, time: f64, cx: &mut App) {
    let custom = store::settings_snapshot().external_player.trim().to_string();
    let id = id.to_string();
    toast(cx, translate("Opening in your video player…"));
    cx.spawn(async move |cx| {
        let res = crate::ui::blocking(move || launch_external(&id, time, &custom)).await;
        let _ = cx.update(|cx| match res {
            Ok(name) => toast(cx, tf!("Playing in {0}. Keep Stream Tube open while it plays", name)),
            Err(e) => toast(cx, tf!("Couldn't open a video player: {0}", e)),
        });
    })
    .detach();
}

/// The external player "Automatic" would use: VLC or mpv when installed.
pub fn detect_player() -> Option<(&'static str, String)> {
    let on_path = |exe: &str| -> Option<String> {
        let name = if cfg!(windows) { format!("{exe}.exe") } else { exe.to_string() };
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
    };
    let mut found: Vec<(&'static str, String)> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(root) = std::env::var_os(var) {
                let p = std::path::Path::new(&root).join("VideoLAN").join("VLC").join("vlc.exe");
                if p.is_file() {
                    found.push(("VLC", p.to_string_lossy().into_owned()));
                }
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let p = std::path::Path::new(&local).join("Programs").join("mpv").join("mpv.exe");
            if p.is_file() {
                found.push(("mpv", p.to_string_lossy().into_owned()));
            }
        }
    }
    found.extend(on_path("vlc").map(|p| ("VLC", p)));
    found.extend(on_path("mpv").map(|p| ("mpv", p)));
    found.extend(on_path("celluloid").map(|p| ("Celluloid", p)));
    found.into_iter().next()
}

/// Resolves the streams and starts a player on them. The streams are served by the
/// app's local proxy (YouTube refuses the open-ended requests media players make),
/// so playback keeps working only while Stream Tube is open.
fn launch_external(id: &str, time: f64, custom: &str) -> anyhow::Result<String> {
    let info = streams::get_streams(id)?;
    let s = store::settings_snapshot();
    // H.264 + AAC plays in every player
    let (v, a) = streams::pick_formats(&info, s.default_quality, "h264", None);
    let local = |f: &crate::api::types::StreamFormat| crate::player::proxy::local_url(&f.url, f.content_length, &f.mime);
    let live = info.is_live || (info.video.is_empty() && info.muxed.is_empty());
    let hls = info.hls_url.as_deref().map(|u| crate::player::proxy::local_hls(u, s.default_quality, None));
    let (video, audio) = if live || info.hls_only {
        (hls.clone().ok_or_else(|| anyhow::anyhow!("no stream for this video"))?, None)
    } else {
        match (v, a) {
            (Some(v), a) => (local(v), a.map(local)),
            (None, Some(a)) => (local(a), None),
            _ => (info.muxed.last().map(local).ok_or_else(|| anyhow::anyhow!("no stream for this video"))?, None),
        }
    };
    // at the very end there's nothing left to play: start from the top
    let start = (time > 1.0 && (info.length_secs == 0 || time + 5.0 < info.length_secs as f64)).then_some(time as u64);
    let title = info.title.clone();

    let (name, exe) = if custom.is_empty() {
        match detect_player() {
            Some((n, p)) => (n.to_string(), p),
            None => return open_with_default(&info, start),
        }
    } else {
        let n = std::path::Path::new(custom).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| custom.to_string());
        (n, custom.to_string())
    };
    let lower = exe.to_lowercase();
    let mut cmd = Command::new(&exe);
    if lower.contains("vlc") {
        if let Some(a) = &audio {
            cmd.arg(format!("--input-slave={a}"));
        }
        if let Some(t) = start {
            cmd.arg(format!("--start-time={t}"));
        }
        cmd.arg(format!("--meta-title={title}")).arg(&video);
    } else if lower.contains("celluloid") {
        if let Some(a) = &audio {
            cmd.arg(format!("--mpv-audio-file={a}"));
        }
        if let Some(t) = start {
            cmd.arg(format!("--mpv-start={t}"));
        }
        cmd.arg(&video);
    } else if lower.contains("mpv") {
        if let Some(a) = &audio {
            cmd.arg(format!("--audio-file={a}"));
        }
        if let Some(t) = start {
            cmd.arg(format!("--start={t}"));
        }
        cmd.arg(format!("--force-media-title={title}")).arg(&video);
    } else {
        // any other player: one stream with sound
        let one = match (&audio, info.muxed.last()) {
            (Some(_), Some(m)) => local(m),
            _ => video.clone(),
        };
        cmd.arg(one);
    }
    cmd.spawn().map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
    Ok(name)
}

/// No known player installed: a playlist with one stream (video and sound
/// together), opened by whatever the system uses for playlists.
fn open_with_default(info: &crate::api::types::StreamInfo, start: Option<u64>) -> anyhow::Result<String> {
    let s = store::settings_snapshot();
    let url = if info.is_live || info.hls_only {
        info.hls_url.as_deref().map(|u| crate::player::proxy::local_hls(u, s.default_quality, None))
    } else {
        info.muxed.last().map(|m| crate::player::proxy::local_url(&m.url, m.content_length, &m.mime))
    }
    .ok_or_else(|| anyhow::anyhow!("this video has no stream a default player can open. Install VLC or mpv"))?;
    let mut m3u = format!("#EXTM3U\n#EXTINF:{},{}\n", info.length_secs, info.title.replace(['\n', '\r'], " "));
    if let Some(t) = start {
        m3u.push_str(&format!("#EXTVLCOPT:start-time={t}\n"));
    }
    m3u.push_str(&url);
    m3u.push('\n');
    // in the app's own (private) folder: a fixed name in the shared /tmp could be a
    // symlink another user planted
    let path = store::data_dir().join("streamtube-play.m3u");
    std::fs::write(&path, m3u)?;
    open::that(&path)?;
    Ok(translate("your default player").into())
}

/// Removes a finished/failed job from the list (the file stays).
pub fn remove(id: u64, cx: &mut App) {
    cx.default_global::<Downloads>().jobs.lock().retain(|j| j.id != id || matches!(j.status, JobStatus::Running | JobStatus::Queued));
    save_history(cx);
    cx.refresh_windows();
}

/// Clears every finished/failed/cancelled entry.
pub fn clear_history(cx: &mut App) {
    cx.default_global::<Downloads>().jobs.lock().retain(|j| matches!(j.status, JobStatus::Running | JobStatus::Queued));
    save_history(cx);
    cx.refresh_windows();
}

/// Deletes the downloaded file and removes the entry.
pub fn delete_file(id: u64, cx: &mut App) {
    let path = cx.default_global::<Downloads>().jobs.lock().iter().find(|j| j.id == id).and_then(|j| match &j.status {
        JobStatus::Done(p) => Some(p.clone()),
        _ => None,
    });
    if let Some(p) = path {
        match std::fs::remove_file(&p) {
            Ok(()) => toast(cx, translate("Deleted from disk")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                toast(cx, tf!("Could not delete: {0}", e));
                return;
            }
        }
    }
    remove(id, cx);
}

/// The real file for a finished download. yt-dlp reports the merged name even
/// when the merge didn't happen, and older entries may only hold the folder, so
/// fall back to the largest media file in the folder tagged with the video id.
pub fn resolve_output(path: &str, video_id: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.is_file() {
        return Some(path.to_string());
    }
    if matches!(p.extension().and_then(|e| e.to_str()), Some("txt" | "srt")) {
        return None; // A video with the same ID is not a missing transcript.
    }
    let dir = if p.is_dir() { p.to_path_buf() } else { p.parent()?.to_path_buf() };
    let tag = format!("[{video_id}]");
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.contains(&tag) && !n.ends_with(".txt") && !n.ends_with(".srt") && !n.ends_with(".part") && !n.ends_with(".ytdl") && !n.ends_with(".tmp")
        })
        .filter_map(|e| Some((e.metadata().ok()?.len(), e.path())))
        .max_by_key(|(len, _)| *len)
        .map(|(_, p)| p.to_string_lossy().into_owned())
}

/// Opens a downloaded file with the system's default player.
pub fn open_file(path: &str, cx: &mut App) {
    let Some(path) = std::path::Path::new(path).is_file().then(|| path.to_string()).or_else(|| {
        // find the id from the "[id]" in the file name
        let name = std::path::Path::new(path).file_name()?.to_string_lossy().into_owned();
        let id = name.rsplit_once('[')?.1.split_once(']')?.0.to_string();
        resolve_output(path, &id)
    }) else {
        toast(cx, translate("File not found (moved or deleted)"));
        return;
    };
    // the default app is launched off the UI thread; Explorer is the most
    // reliable way to hand a file to its default app on Windows
    std::thread::spawn(move || {
        #[cfg(windows)]
        {
            let mut c = Command::new("explorer");
            c.arg(&path);
            if c.spawn().is_ok() {
                return;
            }
        }
        let _ = open::that(&path);
    });
}

/// (active jobs, overall progress 0..1 of active jobs)
pub fn summary(cx: &App) -> (usize, f32) {
    let Some(d) = cx.try_global::<Downloads>() else { return (0, 0.0) };
    let jobs = d.jobs.lock();
    let active: Vec<&Job> = jobs.iter().filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running)).collect();
    if active.is_empty() {
        return (0, 0.0);
    }
    let p = active.iter().map(|j| j.progress).sum::<f32>() / active.len() as f32;
    (active.len(), p)
}

// ── history: finished downloads survive restarts ──

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    video: VideoItem,
    format: String,
    path: Option<String>,
    error: Option<String>,
}

const HISTORY_KEY: &str = "download_history";

fn save_history(cx: &mut App) {
    let jobs = cx.default_global::<Downloads>().jobs.lock().clone();
    let saved: Vec<Saved> = jobs
        .iter()
        .filter_map(|j| match &j.status {
            JobStatus::Done(p) => Some(Saved { video: j.video.clone(), format: j.format.clone(), path: Some(p.clone()), error: None }),
            JobStatus::Failed(e) => Some(Saved { video: j.video.clone(), format: j.format.clone(), path: None, error: Some(e.clone()) }),
            _ => None,
        })
        .collect();
    if let Ok(s) = serde_json::to_string(&saved) {
        store::kv_set(HISTORY_KEY, &s);
    }
}

/// Loads the saved download list at startup.
pub fn load_history(cx: &mut App) {
    let Some(raw) = store::kv_get(HISTORY_KEY) else { return };
    let Ok(saved) = serde_json::from_str::<Vec<Saved>>(&raw) else { return };
    let d = cx.default_global::<Downloads>();
    let mut jobs = d.jobs.lock();
    for s in saved {
        d.next += 1;
        jobs.push(Job {
            id: d.next,
            video: s.video,
            format: s.format,
            status: match (s.path, s.error) {
                (Some(p), _) => JobStatus::Done(p),
                (None, e) => JobStatus::Failed(e.unwrap_or_default()),
            },
            progress: 1.0,
            detail: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
    }
}

// ── FFmpeg (joins separate video and audio streams) ──

static FFMPEG_FETCHING: AtomicBool = AtomicBool::new(false);
static PENDING: Mutex<Vec<(VideoItem, Option<String>)>> = Mutex::new(Vec::new());

/// yt-dlp's FFmpeg builds: (repository, release, file). The Linux one is static,
/// so it runs on any distribution.
const FFMPEG_ASSET: Option<(&str, &str, &str)> = if cfg!(target_os = "macos") {
    None
} else {
    Some((
        "yt-dlp/FFmpeg-Builds",
        "tags/latest",
        if cfg!(windows) { "ffmpeg-master-latest-win64-gpl-shared.zip" } else { "ffmpeg-master-latest-linux64-gpl.tar.xz" },
    ))
};

pub const GET_FFMPEG: Option<&str> = if cfg!(target_os = "macos") {
    None
} else if cfg!(windows) {
    Some("Get FFmpeg (91 MB)")
} else {
    Some("Get FFmpeg (158 MB)")
};
pub const GET_FFMPEG_HELP: &str = if cfg!(target_os = "macos") {
    "Videos above 360p need FFmpeg. Run brew install ffmpeg, then restart Stream Tube."
} else {
    "Videos above 360p need FFmpeg to join video and audio"
};
pub const GET_YTDLP: &str = if cfg!(target_os = "macos") {
    "Get yt-dlp"
} else if cfg!(windows) {
    "Get yt-dlp (24 MB)"
} else {
    "Get yt-dlp (41 MB)"
};

/// Downloads a file from a GitHub release into `dest`, and keeps it only if it
/// matches the SHA-256 GitHub lists for it. `release` is "latest" or "tags/<tag>".
/// How far the FFmpeg / yt-dlp download has got: idle, a percentage, or unpacking.
static FFMPEG_PCT: AtomicU32 = AtomicU32::new(IDLE);
static YTDLP_PCT: AtomicU32 = AtomicU32::new(IDLE);
const IDLE: u32 = u32::MAX;
const UNPACKING: u32 = 1000;
/// bumped each time a tool download finishes, so pages showing whether it's
/// installed look again without being reopened
static TOOLS_GEN: AtomicU64 = AtomicU64::new(0);

pub enum ToolFetch {
    Downloading(u32),
    Unpacking,
}

fn fetch_state(a: &AtomicU32) -> Option<ToolFetch> {
    match a.load(Ordering::SeqCst) {
        IDLE => None,
        UNPACKING => Some(ToolFetch::Unpacking),
        n => Some(ToolFetch::Downloading(n.min(100))),
    }
}

pub fn ffmpeg_fetch() -> Option<ToolFetch> {
    fetch_state(&FFMPEG_PCT)
}

pub fn ytdlp_fetch() -> Option<ToolFetch> {
    fetch_state(&YTDLP_PCT)
}

pub fn tools_generation() -> u64 {
    TOOLS_GEN.load(Ordering::SeqCst)
}

/// Repaints while a tool downloads, so its progress moves on screen.
fn repaint_while(flag: &'static AtomicBool, cx: &mut App) {
    cx.spawn(async move |cx| {
        while flag.load(Ordering::SeqCst) {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let _ = cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

fn download_github_asset((repo, release, name): (&str, &str, &str), dest: &std::path::Path) -> anyhow::Result<()> {
    download_github_asset_progress((repo, release, name), dest, None)
}

fn download_github_asset_progress(
    (repo, release, name): (&str, &str, &str),
    dest: &std::path::Path,
    pct: Option<&AtomicU32>,
) -> anyhow::Result<()> {
    let rel = crate::api::http::get_json(&format!("https://api.github.com/repos/{repo}/releases/{release}"))?;
    let asset = rel
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(name)))
        .ok_or_else(|| anyhow::anyhow!("{name} is not in the latest release"))?;
    let url = asset.get("browser_download_url").and_then(|u| u.as_str()).filter(|u| u.starts_with(&format!("https://github.com/{repo}/releases/download/")));
    let sha = asset.get("digest").and_then(|d| d.as_str()).and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
    let (Some(url), Some(sha)) = (url, sha) else { anyhow::bail!("GitHub gave no checksum for {name}") };
    {
        let mut f = std::fs::File::create(dest)?;
        let size = asset.get("size").and_then(|s| s.as_u64());
        crate::api::http::download_to(url, &mut f, |done, total| {
            if let (Some(p), Some(t)) = (pct, total.or(size).filter(|t| *t > 0)) {
                p.store(((done * 100) / t).min(100) as u32, Ordering::SeqCst);
            }
            true
        })?;
    }
    if crate::updater::file_sha256(dest)? != sha {
        let _ = std::fs::remove_file(dest);
        anyhow::bail!("the downloaded {name} doesn't match its checksum");
    }
    Ok(())
}

fn ffmpeg_dir() -> std::path::PathBuf {
    store::data_dir().join("ffmpeg")
}

/// FFmpeg next to the app, in the app's data folder, or on PATH.
pub fn ffmpeg_path() -> Option<String> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut cands = vec![ffmpeg_dir().join(exe)];
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        cands.push(d.join(exe));
        cands.push(d.join("ffmpeg").join(exe));
    }
    if let Some(p) = cands.into_iter().find(|p| p.is_file()) {
        return Some(p.to_string_lossy().into_owned());
    }
    static ON_PATH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on_path = *ON_PATH.get_or_init(|| {
        let mut c = Command::new("ffmpeg");
        c.arg("-version").stdout(Stdio::null()).stderr(Stdio::null());
        hide_window(&mut c);
        c.status().map(|s| s.success()).unwrap_or(false)
    });
    on_path.then(|| "ffmpeg".to_string())
}

/// Downloads FFmpeg into the data folder, then starts the downloads waiting for it.
pub fn fetch_ffmpeg(cx: &mut App) {
    let Some(asset) = FFMPEG_ASSET else { return };
    if FFMPEG_FETCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    FFMPEG_PCT.store(0, Ordering::SeqCst);
    toast(cx, translate("Downloading FFmpeg…"));
    repaint_while(&FFMPEG_FETCHING, cx);
    cx.spawn(async move |cx| {
        let res = crate::ui::blocking(move || -> anyhow::Result<()> {
            let dir = ffmpeg_dir();
            let tmp = store::data_dir().join("ffmpeg-download");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            let archive = tmp.join(asset.2);
            download_github_asset_progress(asset, &archive, Some(&FFMPEG_PCT))?;
            FFMPEG_PCT.store(UNPACKING, Ordering::SeqCst);
            // Windows 10+ ships bsdtar, which reads zip files; GNU tar unpacks .tar.xz
            let mut c = Command::new("tar");
            c.arg(if cfg!(windows) { "-xf" } else { "-xJf" }).arg(&archive).arg("-C").arg(&tmp);
            hide_window(&mut c);
            if !c.status()?.success() {
                anyhow::bail!("could not unpack FFmpeg");
            }
            let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
            let bin = std::fs::read_dir(&tmp)?
                .filter_map(Result::ok)
                .map(|e| e.path().join("bin"))
                .find(|p| p.join(exe).is_file())
                .ok_or_else(|| anyhow::anyhow!("{exe} not found in the archive"))?;
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            for e in std::fs::read_dir(&bin)?.filter_map(Result::ok) {
                // ffmpeg and ffprobe (and on Windows their DLLs); ffplay isn't needed
                if e.file_name().to_string_lossy().starts_with("ffplay") {
                    continue;
                }
                let to = dir.join(e.file_name());
                std::fs::copy(e.path(), &to)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
                }
            }
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(())
        })
        .await;
        FFMPEG_FETCHING.store(false, Ordering::SeqCst);
        FFMPEG_PCT.store(IDLE, Ordering::SeqCst);
        TOOLS_GEN.fetch_add(1, Ordering::SeqCst);
        let _ = cx.update(|cx| {
            cx.refresh_windows();
            let pending: Vec<_> = std::mem::take(&mut *PENDING.lock());
            match res {
                Ok(()) => {
                    toast(cx, translate("FFmpeg is ready"));
                    for (v, f) in pending {
                        start(v, f, cx);
                    }
                }
                Err(e) => toast(cx, tf!("FFmpeg download failed: {0}", e)),
            }
        });
    })
    .detach();
}

// ── yt-dlp (the standalone build, kept in the data folder) ──

static YTDLP_FETCHING: AtomicBool = AtomicBool::new(false);

const YTDLP_ASSET: (&str, &str, &str) = (
    "yt-dlp/yt-dlp",
    "latest",
    if cfg!(windows) { "yt-dlp.exe" } else if cfg!(target_os = "macos") { "yt-dlp_macos" } else { "yt-dlp_linux" },
);

/// Downloads yt-dlp into the data folder, then starts the downloads waiting for it.
pub fn fetch_ytdlp(cx: &mut App) {
    if YTDLP_FETCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    YTDLP_PCT.store(0, Ordering::SeqCst);
    toast(cx, translate("Downloading yt-dlp…"));
    repaint_while(&YTDLP_FETCHING, cx);
    cx.spawn(async move |cx| {
        let res = crate::ui::blocking(|| -> anyhow::Result<()> {
            let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
            let dest = store::data_dir().join(name);
            let tmp = store::data_dir().join(format!("{name}.part"));
            download_github_asset_progress(YTDLP_ASSET, &tmp, Some(&YTDLP_PCT))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            let _ = std::fs::remove_file(&dest);
            std::fs::rename(&tmp, &dest)?;
            streams::reset_ytdlp_cache();
            if streams::ytdlp_path().is_none() {
                anyhow::bail!("the downloaded yt-dlp does not run");
            }
            Ok(())
        })
        .await;
        YTDLP_FETCHING.store(false, Ordering::SeqCst);
        YTDLP_PCT.store(IDLE, Ordering::SeqCst);
        TOOLS_GEN.fetch_add(1, Ordering::SeqCst);
        let _ = cx.update(|cx| {
            cx.refresh_windows();
            let pending: Vec<_> = std::mem::take(&mut *PENDING.lock());
            match res {
                Ok(()) => {
                    toast(cx, translate("yt-dlp is ready"));
                    for (v, f) in pending {
                        start(v, f, cx);
                    }
                }
                Err(e) => toast(cx, tf!("yt-dlp download failed: {0}", e)),
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod fetch_tests {
    /// network: cargo test --release verified_download -- --ignored
    #[::core::prelude::v1::test]
    #[ignore]
    fn verified_download() {
        let p = std::env::temp_dir().join("st-ytdlp-test");
        super::download_github_asset(super::YTDLP_ASSET, &p).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() > 1_000_000);
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod kids_download_tests {
    use std::sync::atomic::AtomicBool;
    /// A video made for kids downloads from the iOS HLS manifest when yt-dlp gets no formats.
    /// network + yt-dlp: cargo test kids_download -- --ignored
    #[::core::prelude::v1::test]
    #[ignore]
    fn kids_download() {
        let bin = crate::api::streams::ytdlp_path().expect("yt-dlp");
        let dir = std::env::temp_dir().join("st-kids-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = crate::store::settings_snapshot();
        let cancel = AtomicBool::new(false);
        let url = "https://www.youtube.com/watch?v=e_04ZrNroTo";
        let fmt = "bv*[height<=360]+ba/b[height<=360]/b";
        // the retry clients (iOS, and Android, the one still served these since late 2026)
        let path = match super::ytdlp_run(&bin, &dir, url, fmt, &s, &["--extractor-args", super::YTDLP_IOS], None, None, &cancel, &|p, d| println!("{p:.2} {d}")) {
            Ok(path) => path,
            Err(e) => {
                println!("yt-dlp iOS/Android: {e}");
                let manifest = crate::api::streams::ios_hls_manifest("e_04ZrNroTo").unwrap();
                let name = format!("{} [e_04ZrNroTo]", super::sanitize("Kids: test / video?").replace('%', "%%"));
                super::ytdlp_run(&bin, &dir, &manifest, fmt, &s, &[], Some(&name), None, &cancel, &|p, d| println!("{p:.2} {d}")).unwrap()
            }
        };
        println!("{path}");
        assert!(path.contains("e_04ZrNroTo"));
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod name_tests {
    use crate::api::types::VideoItem;

    #[test]
    fn fills_in_the_file_name_template() {
        let v = VideoItem { id: "e_04ZrNroTo".into(), title: "A/B: test?".into(), channel_name: "Chan".into(), ..Default::default() };
        assert_eq!(super::custom_name(&v, ""), None);
        assert_eq!(super::custom_name(&v, "  "), None);
        assert_eq!(super::custom_name(&v, "{channel} - {title} ({id})").as_deref(), Some("Chan - A_B_ test_ (e_04ZrNroTo)"));
        assert_eq!(super::custom_name(&v, "{title}...").as_deref(), Some("A_B_ test_"));
    }
}
