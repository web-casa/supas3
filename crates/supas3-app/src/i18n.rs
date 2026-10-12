// SPDX-License-Identifier: AGPL-3.0-or-later
//! Interface language selection. Translations live in `locales/app.yml`.

use crate::settings::Language;

pub const ENGLISH: &str = "en";
pub const SIMPLIFIED_CHINESE: &str = "zh-CN";

/// Maps a system locale tag to a language the interface ships.
///
/// Any Chinese locale gets Simplified Chinese, the only Chinese translation
/// available; everything else gets English.
pub fn supported_locale(system_locale: Option<&str>) -> &'static str {
    match system_locale {
        Some(tag) if tag.to_ascii_lowercase().starts_with("zh") => SIMPLIFIED_CHINESE,
        _ => ENGLISH,
    }
}

/// The locale a language preference resolves to.
pub fn locale_for(language: Language, system_locale: Option<&str>) -> &'static str {
    match language {
        Language::System => supported_locale(system_locale),
        Language::English => ENGLISH,
        Language::SimplifiedChinese => SIMPLIFIED_CHINESE,
    }
}

/// Applies a language preference to the application's strings and to the
/// component library's built-in strings.
pub fn apply(language: Language) {
    let locale = locale_for(language, sys_locale::get_locale().as_deref());
    rust_i18n::set_locale(locale);
    gpui_kit::component::set_locale(locale);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_locales_use_simplified_chinese() {
        for tag in ["zh-CN", "zh_CN.UTF-8", "zh", "zh-Hans-CN", "zh-TW", "ZH-cn"] {
            assert_eq!(supported_locale(Some(tag)), SIMPLIFIED_CHINESE, "{tag}");
        }
    }

    #[test]
    fn other_or_unknown_locales_use_english() {
        for tag in ["en-US", "ja-JP", "de", "C", ""] {
            assert_eq!(supported_locale(Some(tag)), ENGLISH, "{tag}");
        }
        assert_eq!(supported_locale(None), ENGLISH);
    }

    #[test]
    fn a_chosen_language_overrides_the_system() {
        assert_eq!(locale_for(Language::English, Some("zh-CN")), ENGLISH);
        assert_eq!(
            locale_for(Language::SimplifiedChinese, Some("en-US")),
            SIMPLIFIED_CHINESE
        );
        assert_eq!(
            locale_for(Language::System, Some("zh-CN")),
            SIMPLIFIED_CHINESE
        );
        assert_eq!(locale_for(Language::System, None), ENGLISH);
    }
}
