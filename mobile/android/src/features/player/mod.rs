use crate::{entities::Video, features::youtube};
use serde::Serialize;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub use android::{append, describe, listen, open, set_extras, update_saved};

#[derive(Serialize)]
struct Track<'a> {
    id: &'a str,
    title: &'a str,
    channel: &'a str,
    channel_link: bool,
    saved: bool,
}

pub fn tracks(videos: &[Video], is_saved: impl Fn(&str) -> bool) -> String {
    let tracks: Vec<_> = videos
        .iter()
        .map(|video| Track {
            id: &video.id,
            title: &video.title,
            channel: &video.channel.name,
            channel_link: youtube::is_channel_id(&video.channel.id),
            saved: is_saved(&video.id),
        })
        .collect();
    serde_json::to_string(&tracks).expect("track metadata contains only strings and flags")
}

#[cfg(not(target_os = "android"))]
pub fn open(
    _: i32,
    _: &str,
    _: usize,
    _: crate::shared::i18n::Language,
    _: bool,
    _: bool,
) -> Result<(), String> {
    Err("The embedded player needs Android".to_owned())
}

#[cfg(not(target_os = "android"))]
pub fn listen(_: impl Fn(i32, i32, String) + Send + Sync + 'static) {}

#[cfg(not(target_os = "android"))]
pub fn append(_: i32, _: &str, _: bool, _: bool) {}

#[cfg(not(target_os = "android"))]
pub fn update_saved(_: i32, _: &str, _: bool) {}

#[cfg(not(target_os = "android"))]
pub fn describe(_: i32, _: &str, _: &str) {}

#[cfg(not(target_os = "android"))]
pub fn set_extras(_: i32, _: &str, _: &str) {}
