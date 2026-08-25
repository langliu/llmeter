use rust_i18n::t;

use llmeter_collector::fx::ExchangeRates;

pub const CURRENCY_SETTING: &str = "currency";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayCurrency {
    #[default]
    Usd,
    Eur,
    Gbp,
    Cny,
    Jpy,
    Hkd,
}

impl DisplayCurrency {
    pub const ALL: [Self; 6] = [
        Self::Usd,
        Self::Eur,
        Self::Gbp,
        Self::Cny,
        Self::Jpy,
        Self::Hkd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usd => "USD",
            Self::Eur => "EUR",
            Self::Gbp => "GBP",
            Self::Cny => "CNY",
            Self::Jpy => "JPY",
            Self::Hkd => "HKD",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Usd => "$",
            Self::Eur => "€",
            Self::Gbp => "£",
            Self::Cny => "¥",
            Self::Jpy => "JPY",
            Self::Hkd => "HK$",
        }
    }

    pub fn from_setting(value: Option<String>) -> Self {
        match value
            .as_deref()
            .map(|value| value.trim().to_ascii_uppercase())
            .as_deref()
        {
            Some("EUR") => Self::Eur,
            Some("GBP") => Self::Gbp,
            Some("CNY") => Self::Cny,
            Some("JPY") => Self::Jpy,
            Some("HKD") => Self::Hkd,
            _ => Self::Usd,
        }
    }
}

pub fn convert_usd(usd: f64, currency: DisplayCurrency, rates: &ExchangeRates) -> f64 {
    usd * rates.usd_to(currency.as_str())
}

pub fn format_amount(usd: f64, currency: DisplayCurrency, rates: &ExchangeRates) -> String {
    let amount = convert_usd(usd, currency, rates);
    match currency {
        DisplayCurrency::Jpy => format!("{} {}", currency.symbol(), amount.round() as i64),
        _ => format!("{} {:.2}", currency.symbol(), amount),
    }
}

pub fn format_cost(
    usd: Option<f64>,
    currency: DisplayCurrency,
    rates: &ExchangeRates,
) -> String {
    match usd {
        Some(value) => format_amount(value, currency, rates),
        None => t!("overview.unpriced").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llmeter_collector::fx::{ExchangeRates, FxSource};

    fn rates() -> ExchangeRates {
        ExchangeRates {
            eur: 0.92,
            gbp: 0.79,
            cny: 7.2,
            jpy: 155.0,
            hkd: 7.8,
            source: FxSource::Default,
        }
    }

    #[test]
    fn usd_keeps_dollar_format() {
        assert_eq!(format_amount(1.23, DisplayCurrency::Usd, &rates()), "$ 1.23");
    }

    #[test]
    fn cny_uses_offline_default_rate() {
        assert_eq!(format_amount(1.0, DisplayCurrency::Cny, &rates()), "¥ 7.20");
    }

    #[test]
    fn jpy_rounds_to_integer() {
        assert_eq!(
            format_amount(1.0, DisplayCurrency::Jpy, &rates()),
            "JPY 155"
        );
    }

    #[test]
    fn unknown_setting_falls_back_to_usd() {
        assert_eq!(
            DisplayCurrency::from_setting(Some("BTC".into())),
            DisplayCurrency::Usd
        );
        assert_eq!(DisplayCurrency::from_setting(None), DisplayCurrency::Usd);
        assert_eq!(
            DisplayCurrency::from_setting(Some("cny".into())),
            DisplayCurrency::Cny
        );
    }
}
