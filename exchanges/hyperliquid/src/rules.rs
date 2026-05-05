use poise_core::types::{PricePrecision, PriceRounding};

const PERP_MAX_DECIMALS: u32 = 6;
const PRICE_SIGNIFICANT_FIGURES: u32 = 5;

pub(crate) fn perp_price_precision(sz_decimals: u32) -> PricePrecision {
    PricePrecision::significant_figures(
        PERP_MAX_DECIMALS.saturating_sub(sz_decimals),
        PRICE_SIGNIFICANT_FIGURES,
    )
}

pub(crate) fn representative_perp_price_tick(sz_decimals: u32) -> f64 {
    let price_decimals = (PERP_MAX_DECIMALS as i32 - sz_decimals as i32)
        .min(PRICE_SIGNIFICANT_FIGURES as i32 - sz_decimals as i32)
        .max(0);
    decimal_step(price_decimals)
}

pub(crate) fn normalize_perp_price(value: f64, sz_decimals: u32, rounding: PriceRounding) -> f64 {
    perp_price_precision(sz_decimals).round(value, rounding, 0.0)
}

fn decimal_step(decimals: i32) -> f64 {
    10_f64.powi(-decimals)
}

#[cfg(test)]
mod tests {
    use poise_core::types::PriceRounding;

    use super::{normalize_perp_price, representative_perp_price_tick};

    #[test]
    fn normalizes_perp_price_by_significant_figures_and_size_decimals() {
        assert!((normalize_perp_price(1234.56, 4, PriceRounding::Down) - 1234.5).abs() < 1e-9);
        assert!((normalize_perp_price(1234.56, 4, PriceRounding::Up) - 1234.6).abs() < 1e-9);
        assert!(
            (normalize_perp_price(123456.0, 4, PriceRounding::Nearest) - 123460.0).abs() < 1e-9
        );
    }

    #[test]
    fn keeps_representative_tick_for_legacy_rule_display() {
        assert_eq!(representative_perp_price_tick(5), 1.0);
        assert_eq!(representative_perp_price_tick(4), 0.1);
    }
}
