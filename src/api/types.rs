use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoItem {
    pub id: String,
    pub title: String,
    pub channel_name: String,
    pub channel_id: Option<String>,
    pub channel_avatar: Option<String>,
    pub thumbnail: String,
    pub compact_thumbnail: Option<String>,
    pub duration_text: Option<String>,
    pub duration_secs: Option<u64>,
    pub views_text: Option<String>,
    pub published_text: Option<String>,
    /// Approximate unix timestamp derived from published_text (or exact from RSS).
    pub published_ts: Option<i64>,
    pub description_snippet: Option<String>,
    pub is_live: bool,
    pub is_short: bool,
    pub is_upcoming: bool,
    /// Set when the card stands for a show (podcast, series): the playlist of its
    /// episodes, which is what opening the card should show.
    pub playlist_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ChannelItem {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
    pub handle: Option<String>,
    pub subscribers_text: Option<String>,
    pub video_count_text: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PlaylistItem {
    pub id: String,
    pub title: String,
    pub thumbnail: Option<String>,
    pub channel_name: Option<String>,
    pub channel_id: Option<String>,
    pub video_count_text: Option<String>,
    pub first_video_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Item {
    Video(VideoItem),
    Channel(ChannelItem),
    Playlist(PlaylistItem),
}

impl Item {
    pub fn as_video(&self) -> Option<&VideoItem> {
        match self {
            Item::Video(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub continuation: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Chapter {
    pub title: String,
    pub start: f64,
    pub thumbnail: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct WatchInfo {
    pub id: String,
    pub title: String,
    pub views_text: String,
    pub date_text: String,
    pub likes_text: Option<String>,
    pub description: String,
    pub description_links: Vec<DescriptionLink>,
    pub channel: ChannelItem,
    pub chapters: Vec<Chapter>,
    /// "Most replayed" graph: (start s, duration s, intensity 0..1)
    pub heatmap: Vec<(f64, f64, f32)>,
    pub related: Vec<Item>,
    pub related_continuation: Option<String>,
    pub comments_token: Option<String>,
    /// "Top" / "Newest" comment sort tokens (from the comments panel header)
    pub comments_sort: (Option<String>, Option<String>),
    pub comment_count_text: Option<String>,
    pub is_live: bool,
    /// Live chat continuation tokens for a running stream: (top chat, all messages).
    pub live_chat: Option<(String, String)>,
    /// When watching in a playlist context.
    pub playlist: Option<WatchPlaylist>,
}

#[derive(Clone, Debug)]
pub struct DescriptionLink {
    pub range: std::ops::Range<usize>,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct WatchPlaylist {
    pub id: String,
    pub title: String,
    pub items: Vec<VideoItem>,
}

#[derive(Clone, Debug, Default)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub author_id: Option<String>,
    pub author_avatar: Option<String>,
    pub text: String,
    pub published: String,
    pub likes: String,
    pub reply_count: String,
    pub replies_token: Option<String>,
    pub is_creator: bool,
    pub is_verified: bool,
    pub is_pinned: bool,
    pub hearted: bool,
    /// Channel emojis in the text: (shortcut as written, e.g. ":_wave:", image URL)
    pub emojis: Vec<(String, String)>,
    /// GIF or sticker posted with the comment: (URL, width / height)
    pub image: Option<(String, f32)>,
    pub(crate) actions: CommentActions,
    pub(crate) account: Option<std::sync::Arc<()>>,
}

impl Comment {
    pub(crate) fn set_reaction(&mut self, reaction: Option<CommentReaction>) {
        self.actions.reaction = reaction;
        self.likes = self.actions.like_count().to_owned();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommentReaction {
    Like,
    Dislike,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CommentActions {
    pub like: Vec<String>,
    pub unlike: Vec<String>,
    pub dislike: Vec<String>,
    pub undislike: Vec<String>,
    pub reply: Option<String>,
    pub reaction: Option<CommentReaction>,
    pub unliked_count: String,
    pub liked_count: String,
}

impl CommentActions {
    pub fn toggle(&self, reaction: CommentReaction) -> (Option<CommentReaction>, &[String]) {
        use CommentReaction::{Dislike, Like};

        let undo = self.reaction == Some(reaction);
        let actions = match (reaction, undo) {
            (Like, false) => &self.like,
            (Like, true) => &self.unlike,
            (Dislike, false) => &self.dislike,
            (Dislike, true) => &self.undislike,
        };
        ((!undo).then_some(reaction), actions)
    }

    pub fn like_count(&self) -> &str {
        if self.reaction == Some(CommentReaction::Like) { &self.liked_count } else { &self.unliked_count }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommentPage {
    pub comments: Vec<Comment>,
    pub continuation: Option<String>,
    pub count_text: Option<String>,
    pub sort_top_token: Option<String>,
    pub sort_new_token: Option<String>,
    pub(crate) create_params: Option<String>,
    pub(crate) account: Option<std::sync::Arc<()>>,
}

#[derive(Clone, Debug, Default)]
pub struct ChannelTab {
    pub title: String,
    /// what the tab is, whatever language its title is in: the last part of its
    /// URL ("featured", "videos", "shorts", "streams", "playlists", …)
    pub kind: String,
    pub params: Option<String>,
    pub selected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ChannelPage {
    pub info: ChannelItem,
    pub banner: Option<String>,
    pub tabs: Vec<ChannelTab>,
    pub page: Page,
    /// the tab's sort chips (Latest, Popular, Oldest), each with the
    /// continuation that loads the tab sorted that way
    pub sort_chips: Vec<SortChip>,
}

#[derive(Clone, Debug, Default)]
pub struct SortChip {
    pub title: String,
    pub token: String,
    pub selected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PlaylistPage {
    pub info: PlaylistItem,
    pub description: Option<String>,
    pub videos: Vec<VideoItem>,
    pub continuation: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamFormat {
    pub itag: u32,
    pub url: String,
    pub mime: String,
    pub codecs: String,
    pub bitrate: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality_label: String,
    pub audio_language: Option<String>,
    pub audio_default: bool,
    pub content_length: Option<u64>,
    pub is_audio: bool,
    pub is_muxed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CaptionTrack {
    pub json3_url: Option<String>,
    pub lang: String,
    pub name: String,
    pub url: String,
    pub auto: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Storyboard {
    pub url_template: String,
    pub width: u32,
    pub height: u32,
    pub count: u32,
    pub cols: u32,
    pub rows: u32,
    pub interval_ms: u32,
}

impl Storyboard {
    /// Returns (sheet url, column, row) for a time.
    pub fn frame_at(&self, secs: f64, duration: f64) -> Option<(String, u32, u32)> {
        if self.count == 0 || self.cols == 0 || self.rows == 0 {
            return None;
        }
        let idx = if self.interval_ms > 0 {
            ((secs * 1000.0) / self.interval_ms as f64) as u32
        } else if duration > 0.0 {
            ((secs / duration) * self.count as f64) as u32
        } else {
            0
        };
        let idx = idx.min(self.count - 1);
        let per = self.cols * self.rows;
        let sheet = idx / per;
        let within = idx % per;
        Some((self.url_template.replace("$M", &sheet.to_string()), within % self.cols, within / self.cols))
    }
}

#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    pub id: String,
    pub title: String,
    pub author: String,
    pub channel_id: String,
    pub length_secs: u64,
    pub view_count: Option<u64>,
    pub description: String,
    pub is_live: bool,
    pub hls_url: Option<String>,
    pub video: Vec<StreamFormat>,
    pub audio: Vec<StreamFormat>,
    pub muxed: Vec<StreamFormat>,
    pub captions: Vec<CaptionTrack>,
    /// Original audio language code detected from stream/HLS/yt-dlp metadata, if available.
    pub original_language: Option<String>,
    /// Default caption track index for the original audio track, if reported by YouTube.
    pub default_caption_index: Option<usize>,
    pub storyboard: Option<Storyboard>,
    pub source: &'static str,
    /// Play `hls_url` (through the proxy); `video` then only lists the HLS
    /// variants so the quality menu has something to offer.
    pub hls_only: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ChatRole {
    #[default]
    Viewer,
    Member,
    Moderator,
    Owner,
}

#[derive(Clone, Debug, Default)]
pub struct ChatMessage {
    pub id: String,
    pub author: String,
    pub author_photo: Option<String>,
    pub text: String,
    pub role: ChatRole,
    pub verified: bool,
    /// Super Chat amount ("₹100.00")
    pub paid: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ChatBatch {
    pub messages: Vec<ChatMessage>,
    pub next: Option<String>,
    /// how long to wait before asking for more
    pub timeout_ms: u64,
}
