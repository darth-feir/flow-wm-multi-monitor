//! Column width presets — the `[columns]` section of `flow.toml`.
//!
//! A preset is a fraction of the monitor's work area a column should span,
//! written as `"a/b"` (`"1/3"`, `"1/2"`, `"1/1"`) or as a plain number in
//! `(0, 1]` (`0.5`). The pixel width of a `1/n` preset is chosen so that `n`
//! such columns plus their `n + 1` gaps fill the work area exactly:
//!
//! ```text
//! width_px(1/n) = (work_area_width - window_gap * (n + 1)) / n
//! ```
//!
//! which generalises to `a/b` as `a * (W - gap) / b - gap` (for `a = 1` the
//! two are the same expression; `a > 1` spans `a` slots plus the `a - 1`
//! gaps between them). `"1/1"` is therefore `work_area_width - 2 * window_gap`,
//! the monocle width.
//!
//! The presets are ordered: `flow dispatch width next|prev|cycle` walks the
//! list, wrapping around. Every monitor keeps its own current preset (see
//! `Monitor::width_preset`), and `default` names the preset every monitor
//! starts on.

use std::fmt;

use schemars::JsonSchema;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// Denominator used when a preset is given as a decimal (`0.5`, `"0.333"`):
/// the value is rounded to this many parts and reduced.
const DECIMAL_SCALE: u64 = 1_000_000;

/// One column width preset: a fraction `numerator / denominator` of the
/// work area, in `(0, 1]`, plus the text it was written as (for `flow query
/// all`, the CLI's reply and error messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidthPreset {
    numerator: u64,
    denominator: u64,
    label: String,
}

impl WidthPreset {
    /// Parse `"a/b"` or a decimal such as `"0.5"`.
    ///
    /// # Errors
    ///
    /// Not a fraction or a number, a zero or negative part, or a value
    /// outside `(0, 1]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("column width preset is empty".into());
        }
        if let Some((a, b)) = trimmed.split_once('/') {
            let num: u64 = a.trim().parse().map_err(|_| {
                format!("column width preset {trimmed:?}: {a:?} is not a whole number")
            })?;
            let den: u64 = b.trim().parse().map_err(|_| {
                format!("column width preset {trimmed:?}: {b:?} is not a whole number")
            })?;
            return Self::from_parts(num, den, trimmed.to_owned());
        }
        let value: f64 = trimmed.parse().map_err(|_| {
            format!("column width preset {trimmed:?} is neither \"a/b\" nor a number")
        })?;
        Self::from_ratio(value, trimmed.to_owned())
    }

    /// A preset from a plain number in `(0, 1]` (a TOML float or integer).
    ///
    /// # Errors
    ///
    /// Not finite or outside `(0, 1]`.
    pub fn from_ratio(value: f64, label: String) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 || value > 1.0 {
            return Err(format!(
                "column width preset {label} must be a number in (0, 1] (or \"a/b\")"
            ));
        }
        let num = (value * DECIMAL_SCALE as f64).round() as u64;
        Self::from_parts(num, DECIMAL_SCALE, label)
    }

    fn from_parts(num: u64, den: u64, label: String) -> Result<Self, String> {
        if num == 0 || den == 0 {
            return Err(format!(
                "column width preset {label}: both parts must be positive"
            ));
        }
        if num > den {
            return Err(format!(
                "column width preset {label}: a column cannot be wider than the screen (a/b must be <= 1)"
            ));
        }
        let g = gcd(num, den);
        Ok(Self {
            numerator: num / g,
            denominator: den / g,
            label,
        })
    }

    /// The text the preset was written as (`"1/2"`, `"0.5"`).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The reduced fraction `(numerator, denominator)`.
    #[must_use]
    pub fn fraction(&self) -> (u64, u64) {
        (self.numerator, self.denominator)
    }

    /// Whether two presets name the same width (`"2/4"` equals `"1/2"`).
    #[must_use]
    pub fn same_width(&self, other: &Self) -> bool {
        self.numerator == other.numerator && self.denominator == other.denominator
    }

    /// Pixel width of a column at this preset on a work area
    /// `work_area_width` wide with `window_gap` between elements:
    /// `a * (W - gap) / b - gap` — for `1/n`, `(W - gap * (n + 1)) / n`, so
    /// `n` columns and their gaps fill the work area exactly. Never below 1.
    #[must_use]
    pub fn width_px(&self, work_area_width: i32, window_gap: i32) -> i32 {
        let inner = i64::from(work_area_width) - i64::from(window_gap);
        let scaled = inner * self.numerator as i64 / self.denominator as i64;
        let px = scaled - i64::from(window_gap);
        px.clamp(1, i64::from(i32::MAX)) as i32
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

impl fmt::Display for WidthPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl Serialize for WidthPreset {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label)
    }
}

/// What a preset may be written as in TOML: a string (`"1/2"`, `"0.5"`) or
/// a bare number (`0.5`, `1`).
#[derive(Deserialize)]
#[serde(untagged)]
enum PresetRepr {
    Text(String),
    Float(f64),
    Int(u64),
}

impl<'de> Deserialize<'de> for WidthPreset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parsed = match PresetRepr::deserialize(deserializer)? {
            PresetRepr::Text(s) => Self::parse(&s),
            PresetRepr::Float(v) => Self::from_ratio(v, v.to_string()),
            PresetRepr::Int(v) => Self::from_ratio(v as f64, v.to_string()),
        };
        parsed.map_err(de::Error::custom)
    }
}

/// The `[columns]` section: an ordered list of width presets and the one
/// every monitor starts on. Absent from `flow.toml` (`FlowConfig::columns`
/// is `None`) the daemon sizes columns from `columns_per_screen` /
/// `column_width` exactly as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ColumnsConfig {
    /// Ordered presets; `flow dispatch width next|prev|cycle` walks this
    /// list and wraps around.
    #[schemars(with = "Vec<String>")]
    pub presets: Vec<WidthPreset>,
    /// The preset every monitor starts on. Must be one of `presets`; when
    /// omitted the first preset is the default.
    #[schemars(with = "Option<String>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<WidthPreset>,
    /// How many rows of a stacked column are on screen at once. `0` (the
    /// default) shows every row sharing the height, as before. With
    /// `visible_rows = 2` a column of four windows shows two, and
    /// `focus up/down` scrolls through the stack.
    pub visible_rows: u32,
    /// The most rows a column may hold; `0` (the default) is unlimited.
    /// Merging into a full column moves the window past it instead. Must be
    /// at least `visible_rows` when both are set.
    pub max_rows: u32,
}

impl Default for ColumnsConfig {
    fn default() -> Self {
        Self {
            presets: ["1/3", "1/2", "1/1"]
                .iter()
                .filter_map(|p| WidthPreset::parse(p).ok())
                .collect(),
            default: None,
            visible_rows: 0,
            max_rows: 0,
        }
    }
}

impl ColumnsConfig {
    /// Validate the section: at least one preset, `default` (when set)
    /// names one of them, and `max_rows` (when set) is not below
    /// `visible_rows` (when set).
    ///
    /// # Errors
    ///
    /// A message naming the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.visible_rows > 0 && self.max_rows > 0 && self.max_rows < self.visible_rows {
            return Err(format!(
                "columns.max_rows ({}) must be at least columns.visible_rows ({})",
                self.max_rows, self.visible_rows
            ));
        }
        if self.presets.is_empty() {
            return Err(
                "columns.presets must list at least one preset (e.g. [\"1/3\", \"1/2\", \"1/1\"])"
                    .into(),
            );
        }
        if let Some(default) = &self.default
            && !self.presets.iter().any(|p| p.same_width(default))
        {
            return Err(format!(
                "columns.default ({default}) is not one of columns.presets ({})",
                self.labels().join(", ")
            ));
        }
        Ok(())
    }

    /// Index into `presets` of the preset every monitor starts on: the
    /// `default` entry when set, else the first preset.
    #[must_use]
    pub fn default_index(&self) -> usize {
        self.default
            .as_ref()
            .and_then(|d| self.presets.iter().position(|p| p.same_width(d)))
            .unwrap_or(0)
    }

    /// Index of the preset with the same width as `wanted`, if any.
    #[must_use]
    pub fn index_of(&self, wanted: &WidthPreset) -> Option<usize> {
        self.presets.iter().position(|p| p.same_width(wanted))
    }

    /// The presets' labels, in order.
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        self.presets.iter().map(|p| p.label().to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fractions_and_reduces_them() {
        let half = WidthPreset::parse("1/2").expect("1/2");
        assert_eq!(half.fraction(), (1, 2));
        assert_eq!(half.label(), "1/2");
        let two_quarters = WidthPreset::parse(" 2 / 4 ").expect("2/4");
        assert_eq!(two_quarters.fraction(), (1, 2));
        assert_eq!(two_quarters.label(), "2 / 4");
        assert!(half.same_width(&two_quarters));
        assert_eq!(WidthPreset::parse("1/1").expect("1/1").fraction(), (1, 1));
    }

    #[test]
    fn parses_decimals() {
        let half = WidthPreset::parse("0.5").expect("0.5");
        assert_eq!(half.fraction(), (1, 2));
        assert_eq!(half.label(), "0.5");
        let one = WidthPreset::from_ratio(1.0, "1".into()).expect("1");
        assert_eq!(one.fraction(), (1, 1));
        let third = WidthPreset::parse("0.25").expect("0.25");
        assert_eq!(third.fraction(), (1, 4));
    }

    #[test]
    fn rejects_bad_presets() {
        for bad in [
            "", "0/2", "1/0", "3/2", "2", "0", "-0.5", "abc", "1/x", "1.5",
        ] {
            assert!(WidthPreset::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn width_px_fills_the_work_area_with_n_columns_and_their_gaps() {
        let gap = 16;
        for n in 1..=4 {
            let preset = WidthPreset::parse(&format!("1/{n}")).expect("preset");
            let px = preset.width_px(1920, gap);
            assert_eq!(px, (1920 - gap * (n + 1)) / n, "1/{n}");
        }
        assert_eq!(
            WidthPreset::parse("1/1").expect("1/1").width_px(1920, gap),
            1920 - 2 * gap
        );
        // 2/3 spans two 1/3 slots plus the gap between them.
        let third = WidthPreset::parse("1/3").expect("1/3").width_px(1920, gap);
        let two_thirds = WidthPreset::parse("2/3").expect("2/3").width_px(1920, gap);
        assert_eq!(third, 618);
        assert_eq!(two_thirds, 1253); // 2 * (1920 - 16) / 3 - 16; within a pixel of 2 * 618 + 16
        assert!(WidthPreset::parse("1/1").expect("1/1").width_px(10, 16) >= 1);
    }

    #[test]
    fn serde_accepts_strings_and_numbers_and_writes_labels() {
        #[derive(Deserialize)]
        struct T {
            presets: Vec<WidthPreset>,
        }
        let t: T = toml::from_str("presets = [\"1/3\", 0.5, 1]").expect("parse");
        assert_eq!(t.presets[0].fraction(), (1, 3));
        assert_eq!(t.presets[1].fraction(), (1, 2));
        assert_eq!(t.presets[2].fraction(), (1, 1));
        assert_eq!(
            serde_json::to_string(&t.presets[0]).expect("json"),
            "\"1/3\""
        );
        assert!(toml::from_str::<T>("presets = [\"3/2\"]").is_err());
    }

    #[test]
    fn columns_config_defaults_validate_and_default_index() {
        let cfg = ColumnsConfig::default();
        assert_eq!(cfg.labels(), vec!["1/3", "1/2", "1/1"]);
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.default_index(), 0);

        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/3\", \"1/2\", \"1/1\"]\ndefault = \"1/2\"")
                .expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.default_index(), 1);
        assert_eq!(
            cfg.index_of(&WidthPreset::parse("2/2").expect("2/2")),
            Some(2)
        );

        let bad: ColumnsConfig =
            toml::from_str("presets = [\"1/3\"]\ndefault = \"1/2\"").expect("parse");
        assert!(bad.validate().is_err());
        let empty: ColumnsConfig = toml::from_str("presets = []").expect("parse");
        assert!(empty.validate().is_err());
    }

    #[test]
    fn row_limits_default_to_zero_and_validate_against_each_other() {
        let cfg: ColumnsConfig = toml::from_str("presets = [\"1/2\"]").expect("parse");
        assert_eq!((cfg.visible_rows, cfg.max_rows), (0, 0));
        assert!(cfg.validate().is_ok());

        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/2\"]\nvisible_rows = 2\nmax_rows = 4").expect("parse");
        assert_eq!((cfg.visible_rows, cfg.max_rows), (2, 4));
        assert!(cfg.validate().is_ok());

        // Equal is fine; only one of them set is fine.
        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/2\"]\nvisible_rows = 3\nmax_rows = 3").expect("parse");
        assert!(cfg.validate().is_ok());
        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/2\"]\nmax_rows = 1").expect("parse");
        assert!(cfg.validate().is_ok());
        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/2\"]\nvisible_rows = 5").expect("parse");
        assert!(cfg.validate().is_ok());

        // A cap below the viewport is rejected.
        let bad: ColumnsConfig =
            toml::from_str("presets = [\"1/2\"]\nvisible_rows = 3\nmax_rows = 2").expect("parse");
        let err = bad.validate().expect_err("max_rows < visible_rows");
        assert!(err.contains("max_rows"), "{err}");
    }
}
