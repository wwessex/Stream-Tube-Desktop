use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::LazyLock};

type Catalog = HashMap<String, HashMap<String, String>>;

static MESSAGES: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("locales.json")).expect("valid Android interface catalog")
});

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    System,
    English,
    Azerbaijani,
    German,
    French,
    Turkish,
    Russian,
    Ukrainian,
    Kazakh,
    Japanese,
    ChineseSimplified,
    ChineseTraditional,
}

impl Language {
    pub const OPTIONS: &[Self] = &[
        Self::System,
        Self::English,
        Self::Azerbaijani,
        Self::German,
        Self::French,
        Self::Turkish,
        Self::Russian,
        Self::Ukrainian,
        Self::Kazakh,
        Self::Japanese,
        Self::ChineseSimplified,
        Self::ChineseTraditional,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::Azerbaijani => "az",
            Self::German => "de",
            Self::French => "fr",
            Self::Turkish => "tr",
            Self::Russian => "ru",
            Self::Ukrainian => "uk",
            Self::Kazakh => "kk",
            Self::Japanese => "ja",
            Self::ChineseSimplified => "zh-CN",
            Self::ChineseTraditional => "zh-TW",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::English => "English",
            Self::Azerbaijani => "Azərbaycanca",
            Self::German => "Deutsch",
            Self::French => "Français",
            Self::Turkish => "Türkçe",
            Self::Russian => "Русский",
            Self::Ukrainian => "Українська",
            Self::Kazakh => "Қазақша",
            Self::Japanese => "日本語",
            Self::ChineseSimplified => "简体中文",
            Self::ChineseTraditional => "繁體中文",
        }
    }

    pub fn credit(self) -> &'static str {
        match self {
            Self::Azerbaijani => "POMI (MrPOMIDOR4EK)",
            Self::Russian => "Kushida",
            _ => "",
        }
    }

    pub fn from_locale(tag: &str) -> Self {
        let tag = tag.to_ascii_lowercase();
        let mut parts = tag.split(['-', '_']);
        let language = parts.next().unwrap_or_default();
        if language == "zh" {
            return if parts.any(|part| matches!(part, "hant" | "tw" | "hk" | "mo")) {
                Self::ChineseTraditional
            } else {
                Self::ChineseSimplified
            };
        }
        Self::OPTIONS
            .iter()
            .copied()
            .find(|option| *option != Self::System && option.code() == language)
            .unwrap_or(Self::English)
    }

    pub fn effective(self, device: Self) -> Self {
        if self == Self::System { device } else { self }
    }
}

pub fn translate<'a>(language: &str, message: &'a str) -> &'a str {
    MESSAGES
        .get(language)
        .and_then(|labels| labels.get(message))
        .map(String::as_str)
        .unwrap_or(message)
}

#[cfg(target_os = "android")]
pub fn player_labels(language: Language) -> String {
    MESSAGES
        .get(language.code())
        .map(|labels| serde_json::to_string(labels).expect("translations contain only strings"))
        .unwrap_or_else(|| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_device_language_and_chinese_script() {
        assert_eq!(Language::from_locale("az-Latn-AZ"), Language::Azerbaijani);
        assert_eq!(
            Language::from_locale("zh_Hant_HK"),
            Language::ChineseTraditional
        );
        assert_eq!(Language::from_locale("zh_CN"), Language::ChineseSimplified);
        assert_eq!(Language::from_locale("es-ES"), Language::English);
        assert_eq!(
            Language::System.effective(Language::Kazakh),
            Language::Kazakh
        );
    }
}
