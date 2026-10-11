// SPDX-License-Identifier: AGPL-3.0-or-later
//! Interface language selection. Translations live in `locales/app.yml`.

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

/// Applies the system language to both the application's strings and the
/// component library's built-in strings.
pub fn follow_system() {
    let locale = supported_locale(sys_locale::get_locale().as_deref());
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
}
