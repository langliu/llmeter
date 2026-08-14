use rust_i18n::t;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LocalePreference {
    Zh,
    En,
    #[default]
    System,
}

impl LocalePreference {
    pub(crate) const ALL: [Self; 3] = [Self::Zh, Self::En, Self::System];

    pub(crate) fn label(self) -> String {
        match self {
            Self::Zh => t!("settings.locale_zh").to_string(),
            Self::En => t!("settings.locale_en").to_string(),
            Self::System => t!("settings.system").to_string(),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
            Self::System => "system",
        }
    }

    pub(crate) fn from_setting(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("zh") => Self::Zh,
            Some("en") => Self::En,
            _ => Self::System,
        }
    }

    pub(crate) fn resolve(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
            Self::System => system_locale(),
        }
    }
}

fn system_locale() -> &'static str {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh",
        _ => "en",
    }
}

pub(crate) fn apply(preference: LocalePreference) {
    rust_i18n::set_locale(preference.resolve());
}
