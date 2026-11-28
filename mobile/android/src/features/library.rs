use crate::{
    entities::{Channel, Video},
    features::services::Extras,
    shared::i18n::Language,
};
use serde::{Deserialize, Serialize};

const STORAGE_KEY: &str = "stream-tube-mobile";
const LIMIT: usize = 150;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub saved: Vec<Video>,
    pub history: Vec<Video>,
    pub recent_searches: Vec<String>,
    pub following: Vec<Channel>,
    pub language: Language,
    pub thumbnails: bool,
    pub extras: Extras,
    pub connected_account: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            saved: Vec::new(),
            history: Vec::new(),
            recent_searches: Vec::new(),
            following: Vec::new(),
            language: Language::System,
            thumbnails: true,
            extras: Extras::default(),
            connected_account: false,
        }
    }
}

impl Library {
    pub fn load() -> rustferry::Result<Self> {
        Ok(rustferry::storage::get(STORAGE_KEY)?.unwrap_or_default())
    }

    pub fn write(&self) -> rustferry::Result<()> {
        rustferry::storage::set(STORAGE_KEY, self)
    }

    pub fn watched(&mut self, video: Video) {
        self.history.retain(|item| item.id != video.id);
        self.history.insert(0, video);
        self.history.truncate(LIMIT);
    }

    pub fn searched(&mut self, query: String) {
        self.recent_searches.retain(|saved| saved != &query);
        self.recent_searches.insert(0, query);
        self.recent_searches.truncate(12);
    }

    pub fn toggle_saved(&mut self, video: Video) {
        if self.is_saved(&video.id) {
            self.saved.retain(|item| item.id != video.id);
        } else {
            self.saved.insert(0, video);
            self.saved.truncate(LIMIT);
        }
    }

    pub fn is_saved(&self, id: &str) -> bool {
        self.saved.iter().any(|video| video.id == id)
    }

    pub fn toggle_following(&mut self, channel: Channel) {
        if self.is_following(&channel.id) {
            self.following.retain(|item| item.id != channel.id);
        } else {
            self.following.insert(0, channel);
            self.following.truncate(LIMIT);
        }
    }

    pub fn is_following(&self, id: &str) -> bool {
        self.following.iter().any(|channel| channel.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_library_deduplicates_history_and_toggles_saved_videos() {
        let video = Video {
            id: "M7lc1UVf-VE".into(),
            ..Video::default()
        };
        let mut library = Library::default();
        library.watched(video.clone());
        library.watched(video.clone());
        assert_eq!(library.history.len(), 1);
        library.toggle_saved(video.clone());
        assert!(library.is_saved(&video.id));
        library.toggle_saved(video);
        assert!(library.saved.is_empty());
    }
}
