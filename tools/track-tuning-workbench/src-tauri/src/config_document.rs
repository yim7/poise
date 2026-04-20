use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

const DEFAULT_MIN_REBALANCE_UNITS: f64 = 0.5;
const DEFAULT_LEVERAGE: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackShapeFamily {
    Linear,
    Inertial,
    Responsive,
}

impl TrackShapeFamily {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "linear" => Ok(Self::Linear),
            "inertial" => Ok(Self::Inertial),
            "responsive" => Ok(Self::Responsive),
            other => bail!("unsupported shape_family `{other}`"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Inertial => "inertial",
            Self::Responsive => "responsive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackBandProtectionKind {
    Freeze,
    Hold,
    Flatten,
    Terminate,
}

impl TrackBandProtectionKind {
    fn parse_legacy(value: &str) -> Result<Self> {
        match value {
            "freeze" => Ok(Self::Freeze),
            "hold" => Ok(Self::Hold),
            "flatten" => Ok(Self::Flatten),
            "terminate" => Ok(Self::Terminate),
            other => bail!("unsupported out_of_band_policy `{other}`"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Freeze => "freeze",
            Self::Hold => "hold",
            Self::Flatten => "flatten",
            Self::Terminate => "terminate",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditableTrackFields {
    pub track_id: String,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub max_notional: f64,
    pub min_rebalance_units: f64,
    pub leverage: u32,
    pub out_of_band_policy: TrackBandProtectionKind,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
    pub shape_family: TrackShapeFamily,
}

impl Default for EditableTrackFields {
    fn default() -> Self {
        Self {
            track_id: String::new(),
            symbol: String::new(),
            lower_price: 0.0,
            upper_price: 0.0,
            long_exposure_units: 0.0,
            short_exposure_units: 0.0,
            notional_per_unit: 0.0,
            max_notional: 0.0,
            min_rebalance_units: DEFAULT_MIN_REBALANCE_UNITS,
            leverage: DEFAULT_LEVERAGE,
            out_of_band_policy: TrackBandProtectionKind::Freeze,
            daily_loss_limit: 0.0,
            total_loss_limit: 0.0,
            shape_family: TrackShapeFamily::Linear,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackDraft {
    pub draft_id: String,
    pub fields: EditableTrackFields,
    pub load_issues: Vec<TrackLoadIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackLoadIssue {
    pub field_key: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackDocument {
    drafts: Vec<TrackDraft>,
}

impl TrackDocument {
    pub fn drafts(&self) -> &[TrackDraft] {
        &self.drafts
    }

    pub fn remove_track(&mut self, draft_id: &str) -> Result<TrackDraft> {
        let Some(index) = self
            .drafts
            .iter()
            .position(|draft| draft.draft_id == draft_id)
        else {
            return Err(anyhow!("draft `{draft_id}` not found"));
        };
        Ok(self.drafts.remove(index))
    }

    pub fn duplicate_track(&mut self, draft_id: &str) -> Result<&TrackDraft> {
        let Some(index) = self
            .drafts
            .iter()
            .position(|draft| draft.draft_id == draft_id)
        else {
            return Err(anyhow!("draft `{draft_id}` not found"));
        };
        let mut fields = self.drafts[index].fields.clone();
        fields.track_id = self.allocate_duplicate_track_id(&fields.track_id);
        let duplicate = TrackDraft {
            draft_id: self.allocate_draft_id(&fields),
            fields,
            load_issues: Vec::new(),
        };
        let insert_at = index + 1;
        self.drafts.insert(insert_at, duplicate);
        Ok(&self.drafts[insert_at])
    }

    pub fn append_blank_track(&mut self) -> &TrackDraft {
        let fields = EditableTrackFields::default();
        let draft = TrackDraft {
            draft_id: self.allocate_draft_id(&fields),
            fields,
            load_issues: Vec::new(),
        };
        self.drafts.push(draft);
        self.drafts.last().expect("blank track was just pushed")
    }

    fn allocate_draft_id(&self, fields: &EditableTrackFields) -> String {
        let base = stable_draft_id(fields);
        disambiguate_identifier(
            &base,
            self.drafts
                .iter()
                .map(|draft| draft.draft_id.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
        )
    }

    fn allocate_duplicate_track_id(&self, source_track_id: &str) -> String {
        let source_track_id = if source_track_id.is_empty() {
            "track"
        } else {
            source_track_id
        };
        let base = format!("{source_track_id}-copy");
        disambiguate_identifier(
            &base,
            self.drafts
                .iter()
                .map(|draft| draft.fields.track_id.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
        )
    }
}

pub fn load_track_document(path: impl AsRef<Path>) -> Result<TrackDocument> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file `{}`", path.display()))?;
    parse_track_document(&raw)
}

pub fn parse_track_document(input: &str) -> Result<TrackDocument> {
    let document = input
        .parse::<DocumentMut>()
        .context("failed to parse TOML config")?;
    let mut drafts: Vec<TrackDraft> = Vec::new();
    let track_tables = read_track_tables(&document)?;

    for (index, table) in track_tables.iter().enumerate() {
        let (fields, load_issues) = project_track_fields_lossy(table);
        drafts.push(TrackDraft {
            draft_id: disambiguate_identifier(
                &stable_draft_id(&fields),
                &drafts
                    .iter()
                    .map(|draft| draft.draft_id.as_str())
                    .collect::<Vec<_>>(),
            ),
            fields,
            load_issues: load_issues
                .into_iter()
                .map(|issue| TrackLoadIssue {
                    field_key: issue.field_key,
                    message: format!("track #{}: {}", index + 1, issue.message),
                })
                .collect(),
        });
    }

    Ok(TrackDocument { drafts })
}

fn read_track_tables(document: &DocumentMut) -> Result<&ArrayOfTables> {
    let Some(item) = document.get("tracks") else {
        return Ok(empty_array_of_tables());
    };
    item.as_array_of_tables()
        .ok_or_else(|| anyhow!("`tracks` must be an array of tables"))
}

fn empty_array_of_tables() -> &'static ArrayOfTables {
    use std::sync::OnceLock;

    static EMPTY: OnceLock<ArrayOfTables> = OnceLock::new();
    EMPTY.get_or_init(ArrayOfTables::new)
}

fn project_track_fields_lossy(table: &Table) -> (EditableTrackFields, Vec<TrackLoadIssue>) {
    let mut issues = Vec::new();
    let mut fields = EditableTrackFields::default();

    fields.track_id = required_string_lossy(table, "track_id", &mut issues);
    fields.symbol = required_string_lossy(table, "symbol", &mut issues);

    let lower_price = required_f64_lossy(table, "lower_price", &mut issues);
    let upper_price = required_f64_lossy(table, "upper_price", &mut issues);
    fields.lower_price = lower_price.unwrap_or(0.0);
    fields.upper_price = upper_price.unwrap_or(0.0);

    let long_exposure_units = required_f64_lossy(table, "long_exposure_units", &mut issues);
    let short_exposure_units = required_f64_lossy(table, "short_exposure_units", &mut issues);
    fields.long_exposure_units = long_exposure_units.unwrap_or(0.0);
    fields.short_exposure_units = short_exposure_units.unwrap_or(0.0);

    fields.notional_per_unit =
        required_f64_lossy(table, "notional_per_unit", &mut issues).unwrap_or(0.0);
    let implied_max_notional =
        fields.long_exposure_units.max(fields.short_exposure_units) * fields.notional_per_unit;
    fields.max_notional =
        optional_f64_lossy(table, "max_notional", &mut issues).unwrap_or(implied_max_notional);
    fields.min_rebalance_units = optional_f64_lossy(table, "min_rebalance_units", &mut issues)
        .unwrap_or(DEFAULT_MIN_REBALANCE_UNITS);
    fields.leverage =
        optional_u32_lossy(table, "leverage", &mut issues).unwrap_or(DEFAULT_LEVERAGE);
    fields.out_of_band_policy =
        optional_band_protection_kind_lossy(table, "out_of_band_policy", &mut issues)
            .unwrap_or(TrackBandProtectionKind::Freeze);
    fields.daily_loss_limit =
        required_f64_lossy(table, "daily_loss_limit", &mut issues).unwrap_or(0.0);
    fields.total_loss_limit =
        required_f64_lossy(table, "total_loss_limit", &mut issues).unwrap_or(0.0);
    fields.shape_family = optional_string_lossy(table, "shape_family", &mut issues)
        .and_then(|value| match TrackShapeFamily::parse(&value) {
            Ok(shape_family) => Some(shape_family),
            Err(_) => {
                issues.push(TrackLoadIssue {
                    field_key: "shape_family".to_string(),
                    message: format!("field `shape_family` has unsupported value `{value}`"),
                });
                None
            }
        })
        .unwrap_or(TrackShapeFamily::Linear);

    (fields, issues)
}

fn optional_string(table: &Table, key: &str) -> Result<Option<String>> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    item.as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| anyhow!("field `{key}` must be a string"))
}

fn optional_f64(table: &Table, key: &str) -> Result<Option<f64>> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    if let Some(value) = item.as_float() {
        return Ok(Some(value));
    }
    if let Some(value) = item.as_integer() {
        return Ok(Some(value as f64));
    }
    Err(anyhow!("field `{key}` must be numeric"))
}

fn optional_u32(table: &Table, key: &str) -> Result<Option<u32>> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    let Some(value) = item.as_integer() else {
        return Err(anyhow!("field `{key}` must be an integer"));
    };
    let value = u32::try_from(value).map_err(|_| anyhow!("field `{key}` must be >= 0"))?;
    Ok(Some(value))
}

fn required_string_lossy(table: &Table, key: &str, issues: &mut Vec<TrackLoadIssue>) -> String {
    match optional_string(table, key) {
        Ok(Some(value)) => value,
        Ok(None) => {
            issues.push(load_issue(key, format!("missing string field `{key}`")));
            String::new()
        }
        Err(_) => {
            issues.push(load_issue(key, format!("field `{key}` must be a string")));
            String::new()
        }
    }
}

fn optional_string_lossy(
    table: &Table,
    key: &str,
    issues: &mut Vec<TrackLoadIssue>,
) -> Option<String> {
    match optional_string(table, key) {
        Ok(value) => value,
        Err(_) => {
            issues.push(load_issue(key, format!("field `{key}` must be a string")));
            None
        }
    }
}

fn optional_band_protection_kind_lossy(
    table: &Table,
    key: &str,
    issues: &mut Vec<TrackLoadIssue>,
) -> Option<TrackBandProtectionKind> {
    let Some(item) = table.get(key) else {
        return None;
    };

    match parse_band_protection_kind(item, key) {
        Ok(value) => Some(value),
        Err(error) => {
            issues.push(load_issue(key, error.to_string()));
            None
        }
    }
}

fn parse_band_protection_kind(item: &Item, key: &str) -> Result<TrackBandProtectionKind> {
    if let Some(value) = item.as_str() {
        return TrackBandProtectionKind::parse_legacy(value);
    }

    let Some(policy) = item.as_inline_table() else {
        return Err(anyhow!("field `{key}` must be a string or inline table"));
    };

    let kinds = [
        ("freeze", TrackBandProtectionKind::Freeze),
        ("hold", TrackBandProtectionKind::Hold),
        ("flatten", TrackBandProtectionKind::Flatten),
        ("terminate", TrackBandProtectionKind::Terminate),
    ];

    let mut matched = None;
    for (name, kind) in kinds {
        if policy.contains_key(name) {
            if matched.is_some() {
                return Err(anyhow!(
                    "field `{key}` must contain exactly one policy variant"
                ));
            }
            matched = Some(kind);
        }
    }

    matched.ok_or_else(|| {
        anyhow!("field `{key}` must contain one of: freeze, hold, flatten, terminate")
    })
}

fn required_f64_lossy(table: &Table, key: &str, issues: &mut Vec<TrackLoadIssue>) -> Option<f64> {
    match optional_f64(table, key) {
        Ok(Some(value)) => Some(value),
        Ok(None) => {
            issues.push(load_issue(key, format!("missing numeric field `{key}`")));
            None
        }
        Err(_) => {
            issues.push(load_issue(key, format!("field `{key}` must be numeric")));
            None
        }
    }
}

fn optional_f64_lossy(table: &Table, key: &str, issues: &mut Vec<TrackLoadIssue>) -> Option<f64> {
    match optional_f64(table, key) {
        Ok(value) => value,
        Err(_) => {
            issues.push(load_issue(key, format!("field `{key}` must be numeric")));
            None
        }
    }
}

fn optional_u32_lossy(table: &Table, key: &str, issues: &mut Vec<TrackLoadIssue>) -> Option<u32> {
    match optional_u32(table, key) {
        Ok(value) => value,
        Err(_) => {
            issues.push(load_issue(key, format!("field `{key}` must be an integer")));
            None
        }
    }
}

fn load_issue(key: &str, message: String) -> TrackLoadIssue {
    TrackLoadIssue {
        field_key: key.to_string(),
        message,
    }
}

fn stable_draft_id(fields: &EditableTrackFields) -> String {
    let mut hasher = StableHasher::default();
    hasher.write_str(&fields.track_id);
    hasher.write_str(&fields.symbol);
    hasher.write_u64(fields.lower_price.to_bits());
    hasher.write_u64(fields.upper_price.to_bits());
    hasher.write_u64(fields.long_exposure_units.to_bits());
    hasher.write_u64(fields.short_exposure_units.to_bits());
    hasher.write_u64(fields.notional_per_unit.to_bits());
    hasher.write_u64(fields.max_notional.to_bits());
    hasher.write_u64(fields.min_rebalance_units.to_bits());
    hasher.write_u32(fields.leverage);
    hasher.write_str(fields.out_of_band_policy.as_str());
    hasher.write_u64(fields.daily_loss_limit.to_bits());
    hasher.write_u64(fields.total_loss_limit.to_bits());
    hasher.write_str(fields.shape_family.as_str());
    format!("draft-{:016x}", hasher.finish())
}

fn disambiguate_identifier(base: &str, existing: &[&str]) -> String {
    if !existing.contains(&base) {
        return base.to_string();
    }

    let mut suffix = 2_u32;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !existing.contains(&candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
    }
}

#[derive(Debug, Clone, Copy)]
struct StableHasher {
    state: u64,
}

impl Default for StableHasher {
    fn default() -> Self {
        Self {
            state: 0xcbf29ce484222325,
        }
    }
}

impl StableHasher {
    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x100000001b3);
        }
    }

    fn write_str(&mut self, value: &str) {
        self.write_u64(value.len() as u64);
        self.write_bytes(value.as_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LEVERAGE, DEFAULT_MIN_REBALANCE_UNITS, parse_track_document};

    #[test]
    fn export_only_contains_tracks_without_top_level_exchange() {
        let document = parse_track_document(
            r#"
[exchange]
venue = "binance"
api_key = "demo"
api_secret = "secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 3000.0
min_rebalance_units = 0.5
leverage = 10
out_of_band_policy = { freeze = { recover = "back_in_band" } }
daily_loss_limit = 375.0
total_loss_limit = 750.0
shape_family = "linear"
"#,
        )
        .unwrap();

        let exported = crate::config_projection::export_all_tracks(document.drafts());

        assert!(exported.contains("[[tracks]]"));
        assert!(!exported.contains("[exchange]"));
        assert!(!exported.contains("venue ="));
    }

    #[test]
    fn export_omits_unsupported_track_fields() {
        let document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 3000.0
min_rebalance_units = 0.5
leverage = 10
out_of_band_policy = { freeze = { recover = "back_in_band" } }
daily_loss_limit = 375.0
total_loss_limit = 750.0
shape_family = "linear"
tick_timeout_secs = 30
"#,
        )
        .unwrap();

        let exported = crate::config_projection::export_all_tracks(document.drafts());

        assert!(!exported.contains("tick_timeout_secs"));
    }

    #[test]
    fn deleting_track_keeps_remaining_export_order_and_draft_ids() {
        let mut document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "alpha"
symbol = "BTCUSDT"
lower_price = 100.0
upper_price = 120.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
daily_loss_limit = 100.0
total_loss_limit = 200.0

[[tracks]]
track_id = "beta"
symbol = "ETHUSDT"
lower_price = 200.0
upper_price = 240.0
long_exposure_units = 5.0
short_exposure_units = 5.0
notional_per_unit = 50.0
daily_loss_limit = 80.0
total_loss_limit = 160.0

[[tracks]]
track_id = "gamma"
symbol = "SOLUSDT"
lower_price = 20.0
upper_price = 28.0
long_exposure_units = 4.0
short_exposure_units = 4.0
notional_per_unit = 25.0
daily_loss_limit = 30.0
total_loss_limit = 60.0
"#,
        )
        .unwrap();
        let remaining_draft_id = document.drafts()[2].draft_id.clone();
        let deleted_draft_id = document.drafts()[1].draft_id.clone();

        let deleted = document.remove_track(&deleted_draft_id).unwrap();
        let exported = crate::config_projection::export_all_tracks(document.drafts());

        assert_eq!(deleted.fields.track_id, "beta");
        assert_eq!(document.drafts()[1].draft_id, remaining_draft_id);
        assert!(
            exported.find("track_id = \"alpha\"").unwrap()
                < exported.find("track_id = \"gamma\"").unwrap()
        );
    }

    #[test]
    fn duplicating_track_only_copies_supported_fields() {
        let mut document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 3000.0
min_rebalance_units = 0.5
leverage = 10
out_of_band_policy = { freeze = { recover = "back_in_band" } }
daily_loss_limit = 375.0
total_loss_limit = 750.0
shape_family = "linear"
tick_timeout_secs = 30
"#,
        )
        .unwrap();
        let source_draft_id = document.drafts()[0].draft_id.clone();

        let duplicate = document.duplicate_track(&source_draft_id).unwrap().clone();
        let exported = crate::config_projection::export_all_tracks(document.drafts());

        assert_ne!(duplicate.draft_id, source_draft_id);
        assert_ne!(duplicate.fields.track_id, "btc-core");
        assert_eq!(exported.matches("track_id = \"btc-core\"").count(), 1);
        assert_eq!(
            exported
                .matches(&format!("track_id = \"{}\"", duplicate.fields.track_id))
                .count(),
            1
        );
        assert!(!exported.contains("tick_timeout_secs"));
        assert!(exported.contains("shape_family = \"linear\""));
    }

    #[test]
    fn blank_track_export_only_contains_supported_fields() {
        let mut document = parse_track_document(
            r#"
[exchange]
venue = "binance"
"#,
        )
        .unwrap();

        let draft = document.append_blank_track().clone();
        let exported = crate::config_projection::export_current_track(&draft);

        for field in [
            "track_id =",
            "symbol =",
            "lower_price =",
            "upper_price =",
            "long_exposure_units =",
            "short_exposure_units =",
            "notional_per_unit =",
            "max_notional =",
            "min_rebalance_units =",
            "leverage =",
            "out_of_band_policy =",
            "daily_loss_limit =",
            "total_loss_limit =",
            "shape_family =",
        ] {
            assert!(exported.contains(field), "missing field: {field}");
        }
        assert!(!exported.contains("tick_timeout_secs"));
        assert!(!exported.contains("[exchange]"));
    }

    #[test]
    fn export_explicitly_writes_supported_defaults() {
        let document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 375.0
total_loss_limit = 750.0
"#,
        )
        .unwrap();

        let exported = crate::config_projection::export_current_track(&document.drafts()[0]);

        assert!(exported.contains(&format!(
            "min_rebalance_units = {DEFAULT_MIN_REBALANCE_UNITS}"
        )));
        assert!(exported.contains("max_notional = 3000.0"));
        assert!(exported.contains(&format!("leverage = {DEFAULT_LEVERAGE}")));
        assert!(
            exported.contains("out_of_band_policy = { freeze = { recover = \"back_in_band\" } }")
        );
        assert!(exported.contains("shape_family = \"linear\""));
    }

    #[test]
    fn projections_assign_unique_internal_draft_ids() {
        let document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "alpha"
symbol = "BTCUSDT"
lower_price = 100.0
upper_price = 120.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
daily_loss_limit = 100.0
total_loss_limit = 200.0

[[tracks]]
track_id = "beta"
symbol = "ETHUSDT"
lower_price = 200.0
upper_price = 240.0
long_exposure_units = 5.0
short_exposure_units = 5.0
notional_per_unit = 50.0
daily_loss_limit = 80.0
total_loss_limit = 160.0
"#,
        )
        .unwrap();

        assert_eq!(document.drafts().len(), 2);
        assert_ne!(document.drafts()[0].draft_id, document.drafts()[1].draft_id);
    }

    #[test]
    fn round_trip_keeps_unmodified_track_draft_ids_stable_after_copy_inserts_new_track() {
        let mut document = parse_track_document(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "alpha"
symbol = "BTCUSDT"
lower_price = 100.0
upper_price = 120.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
daily_loss_limit = 100.0
total_loss_limit = 200.0

[[tracks]]
track_id = "beta"
symbol = "ETHUSDT"
lower_price = 200.0
upper_price = 240.0
long_exposure_units = 5.0
short_exposure_units = 5.0
notional_per_unit = 50.0
daily_loss_limit = 80.0
total_loss_limit = 160.0
"#,
        )
        .unwrap();
        let alpha_draft_id = document.drafts()[0].draft_id.clone();
        let beta_draft_id = document.drafts()[1].draft_id.clone();
        let alpha_source_draft_id = document.drafts()[0].draft_id.clone();

        let duplicate = document
            .duplicate_track(&alpha_source_draft_id)
            .unwrap()
            .clone();
        let reloaded = parse_track_document(&crate::config_projection::export_all_tracks(
            document.drafts(),
        ))
        .unwrap();

        assert_eq!(
            find_draft_id_by_track_id(&reloaded, "alpha").unwrap(),
            alpha_draft_id
        );
        assert_eq!(
            find_draft_id_by_track_id(&reloaded, "beta").unwrap(),
            beta_draft_id
        );
        assert_eq!(
            find_draft_id_by_track_id(&reloaded, &duplicate.fields.track_id).unwrap(),
            duplicate.draft_id
        );
    }

    fn find_draft_id_by_track_id(
        document: &super::TrackDocument,
        track_id: &str,
    ) -> Option<String> {
        document
            .drafts()
            .iter()
            .find(|draft| draft.fields.track_id == track_id)
            .map(|draft| draft.draft_id.clone())
    }
}
