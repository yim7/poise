use std::collections::HashMap;

use anyhow::{Result, anyhow};
use poise_engine::track::TrackId;

use crate::config::TrackFileDefinition;

pub(crate) const DEFAULT_TRACK_LEVERAGE: u32 = 10;
pub(crate) type TrackLeverageIndex = HashMap<TrackId, u32>;

pub(crate) fn build_track_leverage_index(tracks: &[TrackFileDefinition]) -> Result<TrackLeverageIndex> {
    let mut index = HashMap::with_capacity(tracks.len());
    for track in tracks {
        let leverage = track.leverage.unwrap_or(DEFAULT_TRACK_LEVERAGE);
        if leverage == 0 {
            return Err(anyhow!(
                "invalid track `{}`: leverage must be positive",
                track.track_id
            ));
        }
        index.insert(track.track_id(), leverage);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use poise_engine::track::TrackId;

    use super::build_track_leverage_index;
    use crate::config::TrackDefinition;

    #[test]
    fn track_leverage_index_defaults_to_ten() {
        let index = build_track_leverage_index(&[track_definition(None)]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&10));
    }

    #[test]
    fn track_leverage_index_preserves_explicit_leverage() {
        let index = build_track_leverage_index(&[track_definition(Some(25))]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&25));
    }

    #[test]
    fn track_leverage_index_stores_only_startup_fields() {
        let index = build_track_leverage_index(&[track_definition(Some(12))]).unwrap();
        let expected = HashMap::from([(TrackId::new("btc-core"), 12)]);

        assert_eq!(index, expected);
    }

    fn track_definition(leverage: Option<u32>) -> TrackDefinition {
        TrackDefinition {
            track_id: "btc-core".into(),
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: None,
            shape_family: None,
            out_of_band_policy: None,
            max_notional: None,
            leverage,
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
        }
    }
}
