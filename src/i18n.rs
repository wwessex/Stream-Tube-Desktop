//! Interface language, independent of the language requested from YouTube.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum AppLanguage {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ru")]
    Russian,
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "az")]
    Azerbaijani,
    #[serde(rename = "uk")]
    Ukrainian,
    #[serde(rename = "kk")]
    Kazakh,
    #[serde(rename = "de")]
    German,
    #[serde(rename = "tr")]
    Turkish,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "zh-CN")]
    ChineseSimplified,
    #[serde(rename = "zh-TW")]
    ChineseTraditional,
}

impl AppLanguage {
    pub const OPTIONS: &[(Self, &str)] = &[
        (Self::English, "English"),
        (Self::Azerbaijani, "Azərbaycanca"),
        (Self::German, "Deutsch"),
        (Self::French, "Français"),
        (Self::Turkish, "Türkçe"),
        (Self::Russian, "Русский"),
        (Self::Ukrainian, "Українська"),
        (Self::Kazakh, "Қазақша"),
        (Self::Japanese, "日本語"),
        (Self::ChineseSimplified, "简体中文"),
        (Self::ChineseTraditional, "繁體中文"),
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Russian => "ru",
            Self::French => "fr",
            Self::Azerbaijani => "az",
            Self::Ukrainian => "uk",
            Self::Kazakh => "kk",
            Self::German => "de",
            Self::Turkish => "tr",
            Self::Japanese => "ja",
            Self::ChineseSimplified => "zh-CN",
            Self::ChineseTraditional => "zh-TW",
        }
    }

    fn catalog(self) -> Option<&'static Catalog> {
        static RU: Catalog = Catalog::new(include_str!("i18n/locales/ru.json"));
        static FR: Catalog = Catalog::new(include_str!("i18n/locales/fr.json"));
        static AZ: Catalog = Catalog::new(include_str!("i18n/locales/az.json"));
        static UK: Catalog = Catalog::new(include_str!("i18n/locales/uk.json"));
        static KK: Catalog = Catalog::new(include_str!("i18n/locales/kk.json"));
        static DE: Catalog = Catalog::new(include_str!("i18n/locales/de.json"));
        static TR: Catalog = Catalog::new(include_str!("i18n/locales/tr.json"));
        static JA: Catalog = Catalog::new(include_str!("i18n/locales/ja.json"));
        static ZH_CN: Catalog = Catalog::new(include_str!("i18n/locales/zh-CN.json"));
        static ZH_TW: Catalog = Catalog::new(include_str!("i18n/locales/zh-TW.json"));

        Some(match self {
            Self::English => return None,
            Self::Russian => &RU,
            Self::French => &FR,
            Self::Azerbaijani => &AZ,
            Self::Ukrainian => &UK,
            Self::Kazakh => &KK,
            Self::German => &DE,
            Self::Turkish => &TR,
            Self::Japanese => &JA,
            Self::ChineseSimplified => &ZH_CN,
            Self::ChineseTraditional => &ZH_TW,
        })
    }

    fn translate(self, source: &str) -> &str {
        self.catalog()
            .and_then(|catalog| catalog.messages().get(source))
            .map(String::as_str)
            .unwrap_or(source)
    }
}

struct Catalog {
    source: &'static str,
    messages: OnceLock<HashMap<String, String>>,
}

impl Catalog {
    const fn new(source: &'static str) -> Self {
        Self {
            source,
            messages: OnceLock::new(),
        }
    }

    fn messages(&self) -> &HashMap<String, String> {
        self.messages.get_or_init(|| {
            serde_json::from_str(self.source).expect("valid embedded interface catalog")
        })
    }
}

pub fn language() -> AppLanguage {
    crate::store::settings_snapshot().app_language
}

pub fn t(source: &str) -> &str {
    language().translate(source)
}

pub fn apply_component_locale() {
    gpui_kit::component::set_locale(language().code());
}

pub fn format(source: &str, args: &[&dyn Display]) -> String {
    format_template(t(source), args)
}

fn format_template(template: &str, args: &[&dyn Display]) -> String {
    let mut message = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((prefix, field_and_tail)) = rest.split_once('{') {
        let Some((field, tail)) = field_and_tail.split_once('}') else {
            break;
        };
        message.push_str(prefix);
        if let Some(value) = field
            .parse::<usize>()
            .ok()
            .and_then(|index| args.get(index))
        {
            write!(message, "{value}").expect("writing to a string");
        } else {
            message.push('{');
            message.push_str(field);
            message.push('}');
        }
        rest = tail;
    }
    message.push_str(rest);
    message
}

// Catalogs can reorder arguments. Apply Rust's numeric formats at the call site.
macro_rules! tf {
    ($source:expr $(, $arg:expr)* $(,)?) => {
        $crate::i18n::format($source, &[$(&($arg) as &dyn std::fmt::Display),*])
    };
}

pub(crate) use tf;

#[cfg(test)]
mod tests {
    use super::{AppLanguage, format_template};
    use crate::store::Settings;

    #[test]
    fn old_settings_keep_the_interface_in_english() {
        let settings: Settings = serde_json::from_str(r#"{"language":"ru"}"#).unwrap();
        assert_eq!(settings.app_language, AppLanguage::English);
        assert_eq!(settings.language, "ru");
    }

    #[test]
    fn russian_interface_round_trips_independently() {
        let settings: Settings =
            serde_json::from_str(r#"{"app_language":"ru","language":"en"}"#).unwrap();
        assert_eq!(settings.app_language, AppLanguage::Russian);
        assert_eq!(settings.language, "en");
        assert_eq!(
            serde_json::to_value(settings).unwrap()["app_language"],
            "ru"
        );
    }

    #[test]
    fn falls_back_to_the_source_text() {
        assert_eq!(AppLanguage::English.translate("Settings"), "Settings");
        assert_eq!(AppLanguage::Russian.translate("Settings"), "Настройки");
        assert_eq!(
            AppLanguage::Russian.translate("An untranslated label"),
            "An untranslated label"
        );
    }

    #[test]
    fn every_language_has_the_same_messages_and_arguments() {
        let reference = AppLanguage::Russian.catalog().unwrap().messages();
        let placeholders = |text: &str| {
            let mut slots: Vec<_> = text
                .split('{')
                .skip(1)
                .filter_map(|part| part.split_once('}'))
                .filter_map(|(field, _)| field.parse::<usize>().ok())
                .collect();
            slots.sort_unstable();
            slots
        };
        for &(language, _) in AppLanguage::OPTIONS {
            let saved = serde_json::to_value(language).unwrap();
            assert_eq!(saved, language.code());
            assert_eq!(
                serde_json::from_value::<AppLanguage>(saved).unwrap(),
                language
            );
            let Some(catalog) = language.catalog() else {
                continue;
            };
            let messages = catalog.messages();
            assert_eq!(messages.len(), reference.len(), "{}", language.code());
            for source in reference.keys() {
                let translated = &messages[source];
                assert!(
                    !translated.trim().is_empty(),
                    "{}: {source}",
                    language.code()
                );
                assert_eq!(
                    placeholders(translated),
                    placeholders(source),
                    "{}: {source}",
                    language.code()
                );
            }
        }
    }

    #[test]
    fn formatting_can_reorder_arguments_without_interpreting_inserted_text() {
        assert_eq!(
            format_template("{1} / {0}", &[&"title {1}", &42]),
            "42 / title {1}"
        );
        assert_eq!(
            format_template("{0}%", &[&format_args!("{:.1}", 1.24)]),
            "1.2%"
        );
    }
}
