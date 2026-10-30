//! Column width presets — the `[columns]` section of `flow.toml`.
//!
//! A preset is a fraction of the monitor's work area a column should span,
//! written as `"a/b"` (`"1/3"`, `"1/2"`, `"1/1"`) or as a plain number in
//! `(0, 1]` (`0.5`), or a **pattern** of such terms joined with `+`
//! (`"1/3+2/3"`, `"1/4+1/4+1/2"`): column `i` of the strip takes term
//! `i mod n`, so the pattern repeats along the strip by position. The pixel
//! width of a `1/n` term is chosen so that `n` such columns plus their `n + 1`
//! gaps fill the work area exactly:
//!
//! ```text
//! width_px(1/n) = (work_area_width - window_gap * (n + 1)) / n
//! ```
//!
//! which generalises to `a/b` as `a * (W - gap) / b - gap` (for `a = 1` the
//! two are the same expression; `a > 1` spans `a` slots plus the `a - 1`
//! gaps between them). `"1/1"` is therefore `work_area_width - 2 * window_gap`,
//! the monocle width, and the terms of a pattern that sum to `1` fill the
//! work area with their gaps exactly once per period.
//!
//! The presets are ordered: `flow dispatch width next|prev|cycle` walks the
//! list, wrapping around. Every monitor keeps its own current preset (see
//! `Monitor::width_preset`), and `default` names the preset every monitor
//! starts on. `[[columns.screen]]` rules give a screen matched by
//! orientation, work-area width or device name its own list
//! ([`ColumnsConfig::list_for`]). Why patterns are positional and pin the
//! strip's widths: `docs/adr/0014-width-patterns-by-column-position.md`.

use std::fmt;

pub use crate::layout::types::{LayoutMode, WrapAround};
use schemars::JsonSchema;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// Denominator used when a preset is given as a decimal (`0.5`, `"0.333"`):
/// the value is rounded to this many parts and reduced.
const DECIMAL_SCALE: u64 = 1_000_000;

/// One term of a preset: a reduced fraction `numerator / denominator` of the
/// work area, in `(0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fraction {
    numerator: u64,
    denominator: u64,
}

impl Fraction {
    fn from_parts(num: u64, den: u64, label: &str) -> Result<Self, String> {
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
        })
    }

    /// Parse one term: `"a/b"` or a decimal in `(0, 1]`. `label` is the
    /// whole preset text, for error messages.
    fn parse(term: &str, label: &str) -> Result<Self, String> {
        let trimmed = term.trim();
        if trimmed.is_empty() {
            return Err(format!("column width preset {label:?} has an empty term"));
        }
        if let Some((a, b)) = trimmed.split_once('/') {
            let num: u64 = a.trim().parse().map_err(|_| {
                format!("column width preset {label:?}: {a:?} is not a whole number")
            })?;
            let den: u64 = b.trim().parse().map_err(|_| {
                format!("column width preset {label:?}: {b:?} is not a whole number")
            })?;
            return Self::from_parts(num, den, label);
        }
        let value: f64 = trimmed.parse().map_err(|_| {
            format!("column width preset {label:?} is neither \"a/b\" nor a number")
        })?;
        Self::from_ratio(value, label)
    }

    fn from_ratio(value: f64, label: &str) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 || value > 1.0 {
            return Err(format!(
                "column width preset {label} must be a number in (0, 1] (or \"a/b\")"
            ));
        }
        let num = (value * DECIMAL_SCALE as f64).round() as u64;
        Self::from_parts(num, DECIMAL_SCALE, label)
    }

    /// The reduced `(numerator, denominator)`.
    #[must_use]
    pub fn parts(self) -> (u64, u64) {
        (self.numerator, self.denominator)
    }

    /// Pixel width of a column at this share on a work area
    /// `work_area_width` wide with `window_gap` between elements:
    /// `a * (W - gap) / b - gap`. Never below 1.
    #[must_use]
    pub fn width_px(self, work_area_width: i32, window_gap: i32) -> i32 {
        let inner = i64::from(work_area_width) - i64::from(window_gap);
        let scaled = inner * self.numerator as i64 / self.denominator as i64;
        let px = scaled - i64::from(window_gap);
        px.clamp(1, i64::from(i32::MAX)) as i32
    }
}

impl fmt::Display for Fraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// One column width preset: a single fraction of the work area, or a
/// pattern of fractions column `i` cycles through (`terms[i % n]`), plus the
/// text it was written as (for `flow query all`, the CLI's reply and error
/// messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidthPreset {
    terms: Vec<Fraction>,
    label: String,
}

impl WidthPreset {
    /// Parse `"a/b"`, a decimal such as `"0.5"`, or a `+`-joined pattern of
    /// those (`"1/3+2/3"`).
    ///
    /// # Errors
    ///
    /// A term that is not a fraction or a number, a zero or negative part, a
    /// value outside `(0, 1]`, or a pattern whose terms sum to more than the
    /// screen.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("column width preset is empty".into());
        }
        let terms = trimmed
            .split('+')
            .map(|t| Fraction::parse(t, trimmed))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_terms(terms, trimmed.to_owned())
    }

    /// A preset from a plain number in `(0, 1]` (a TOML float or integer).
    ///
    /// # Errors
    ///
    /// Not finite or outside `(0, 1]`.
    pub fn from_ratio(value: f64, label: String) -> Result<Self, String> {
        let term = Fraction::from_ratio(value, &label)?;
        Ok(Self {
            terms: vec![term],
            label,
        })
    }

    fn from_terms(terms: Vec<Fraction>, label: String) -> Result<Self, String> {
        // A period wider than the screen can never be shown whole, so the
        // terms of a pattern may not add up to more than 1 (compared as
        // exact fractions over a common denominator).
        if terms.len() > 1 {
            let den = terms
                .iter()
                .fold(1u128, |acc, t| lcm(acc, u128::from(t.denominator)));
            let sum: u128 = terms
                .iter()
                .map(|t| u128::from(t.numerator) * (den / u128::from(t.denominator)))
                .sum();
            if sum > den {
                return Err(format!(
                    "column width preset {label:?}: the terms of a pattern must not add up to more than the screen (1/1)"
                ));
            }
        }
        Ok(Self { terms, label })
    }

    /// The text the preset was written as (`"1/2"`, `"0.5"`, `"1/3+2/3"`).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The terms column positions cycle through; one term for a plain
    /// fraction.
    #[must_use]
    pub fn terms(&self) -> &[Fraction] {
        &self.terms
    }

    /// Whether this preset is a pattern of two or more terms, which pins
    /// every column's width to its position.
    #[must_use]
    pub fn is_pattern(&self) -> bool {
        self.terms.len() > 1
    }

    /// The reduced fraction `(numerator, denominator)` of the first term.
    #[must_use]
    pub fn fraction(&self) -> (u64, u64) {
        self.terms[0].parts()
    }

    /// Whether two presets name the same widths (`"2/4"` equals `"1/2"`,
    /// `"1/3+2/3"` equals `"2/6+4/6"`).
    #[must_use]
    pub fn same_width(&self, other: &Self) -> bool {
        self.terms == other.terms
    }

    /// Pixel width of the first term on a work area `work_area_width` wide
    /// with `window_gap` between elements (see [`Fraction::width_px`]).
    #[must_use]
    pub fn width_px(&self, work_area_width: i32, window_gap: i32) -> i32 {
        self.terms[0].width_px(work_area_width, window_gap)
    }

    /// Pixel width of the column at strip position `index` (`terms[index %
    /// n]`).
    #[must_use]
    pub fn width_px_at(&self, index: usize, work_area_width: i32, window_gap: i32) -> i32 {
        self.terms[index % self.terms.len()].width_px(work_area_width, window_gap)
    }

    /// Pixel width of every term in order, one period of the pattern.
    #[must_use]
    pub fn widths_px(&self, work_area_width: i32, window_gap: i32) -> Vec<i32> {
        self.terms
            .iter()
            .map(|t| t.width_px(work_area_width, window_gap))
            .collect()
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

fn lcm(a: u128, b: u128) -> u128 {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        let t = x % y;
        x = y;
        y = t;
    }
    a / x * b
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

/// What a preset may be written as in TOML: a string (`"1/2"`, `"0.5"`,
/// `"1/3+2/3"`) or a bare number (`0.5`, `1`).
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

/// Which way a screen is turned, by its work area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// Wider than tall (or square).
    Landscape,
    /// Taller than wide.
    Portrait,
}

/// What a `[[columns.screen]]` rule knows about a screen: its work area and
/// its device name (`DISPLAY1`, with or without the `\\.\` prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenFacts {
    /// Work-area width in pixels.
    pub width: i32,
    /// Work-area height in pixels.
    pub height: i32,
    /// Device name as Windows reports it; empty when unknown (tests).
    pub name: String,
}

impl ScreenFacts {
    /// Portrait when the work area is taller than wide.
    #[must_use]
    pub fn orientation(&self) -> Orientation {
        if self.height > self.width {
            Orientation::Portrait
        } else {
            Orientation::Landscape
        }
    }

    /// The device name without Windows' `\\.\` prefix (`DISPLAY2`).
    #[must_use]
    pub fn short_name(&self) -> &str {
        strip_device_prefix(&self.name)
    }

    /// `DISPLAY2 (1080x1920 work area)` for error messages and logs; the
    /// size alone when the name is unknown.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.name.is_empty() {
            format!("a {}x{} work area", self.width, self.height)
        } else {
            format!(
                "{} ({}x{} work area)",
                self.short_name(),
                self.width,
                self.height
            )
        }
    }
}

fn strip_device_prefix(name: &str) -> &str {
    name.strip_prefix(r"\\.\").unwrap_or(name)
}

/// The `match` table of a `[[columns.screen]]` rule; every field given must
/// hold, and at least one must be given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenMatch {
    /// `"portrait"` or `"landscape"`, by the work area.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    /// The work area must be at least this wide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width_px: Option<u32>,
    /// The work area must be at most this wide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width_px: Option<u32>,
    /// The device name (`"DISPLAY2"`; the `\\.\` prefix is optional,
    /// case-insensitive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ScreenMatch {
    /// Whether no field is set (such a rule would match every screen and is
    /// refused by [`ColumnsConfig::validate`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.orientation.is_none()
            && self.min_width_px.is_none()
            && self.max_width_px.is_none()
            && self.name.is_none()
    }

    /// Whether `facts` satisfies every field that is set.
    #[must_use]
    pub fn matches(&self, facts: &ScreenFacts) -> bool {
        if self
            .orientation
            .is_some_and(|wanted| wanted != facts.orientation())
        {
            return false;
        }
        if self
            .min_width_px
            .is_some_and(|min| i64::from(facts.width) < i64::from(min))
        {
            return false;
        }
        if self
            .max_width_px
            .is_some_and(|max| i64::from(facts.width) > i64::from(max))
        {
            return false;
        }
        if let Some(name) = &self.name
            && !strip_device_prefix(name).eq_ignore_ascii_case(facts.short_name())
        {
            return false;
        }
        true
    }
}

/// One `[[columns.screen]]` rule: the presets a matching screen uses
/// instead of the top-level list, and/or its own layout mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenPresetRule {
    /// Which screens the rule is for.
    #[serde(rename = "match")]
    pub match_: ScreenMatch,
    /// Ordered presets for those screens; `flow dispatch width` walks this
    /// list on them. May be left out when the rule only sets `layout`: the
    /// screen then uses the top-level list.
    #[schemars(with = "Vec<String>")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<WidthPreset>,
    /// `"strip"` or `"book"` for these screens in place of the top-level
    /// `layout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutMode>,
    /// The preset such a screen starts on. Must be one of `presets`; when
    /// omitted the first preset is the default.
    #[schemars(with = "Option<String>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<WidthPreset>,
    /// A floor for this screen in place of the top-level
    /// `min_column_width_px`, which is screen-blind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_column_width_px: Option<u32>,
}

/// The preset list one screen uses — the rule that matched it, or the
/// top-level `[columns]` list — resolved by [`ColumnsConfig::list_for`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresetList<'a> {
    /// Ordered presets.
    pub presets: &'a [WidthPreset],
    /// The `default` entry of the list when set.
    pub default: Option<&'a WidthPreset>,
    /// The rule's own floor, when it set one.
    pub min_column_width_px: Option<u32>,
    /// Index of the `[[columns.screen]]` rule this list came from; `None`
    /// for the top-level list.
    pub rule: Option<usize>,
}

impl ScreenPresetRule {
    /// Whether the rule brings its own preset list (a rule may set only
    /// `layout`).
    #[must_use]
    pub fn has_presets(&self) -> bool {
        !self.presets.is_empty()
    }
}

impl PresetList<'_> {
    /// Index into `presets` of the preset a screen starts on: the `default`
    /// entry when set, else the first preset.
    #[must_use]
    pub fn default_index(&self) -> usize {
        self.default
            .and_then(|d| self.presets.iter().position(|p| p.same_width(d)))
            .unwrap_or(0)
    }

    /// Index of the preset with the same widths as `wanted`, if any.
    #[must_use]
    pub fn index_of(&self, wanted: &WidthPreset) -> Option<usize> {
        self.presets.iter().position(|p| p.same_width(wanted))
    }

    /// The index a screen that was on `previous` lands on after its list
    /// changed (a reload, a rule now matching, a display re-plugged): the
    /// same preset when the new list has it, else the list's default.
    #[must_use]
    pub fn carry_over(&self, previous: Option<&WidthPreset>) -> usize {
        previous
            .and_then(|p| self.index_of(p))
            .unwrap_or_else(|| self.default_index())
    }

    /// The presets' labels, in order.
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        self.presets.iter().map(|p| p.label().to_owned()).collect()
    }

    /// `columns.screen[1]` or `columns` — where the list is configured, for
    /// messages.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.rule {
            Some(i) => format!("columns.screen[{i}]"),
            None => "columns".to_owned(),
        }
    }
}

/// The `[columns]` section: an ordered list of width presets and the one
/// every monitor starts on, plus optional per-screen rules. Absent from
/// `flow.toml` (`FlowConfig::columns` is `None`) the daemon sizes columns
/// from `columns_per_screen` / `column_width` exactly as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ColumnsConfig {
    /// Ordered presets; `flow dispatch width next|prev|cycle` walks this
    /// list and wraps around. A preset is a fraction (`"1/2"`) or a pattern
    /// (`"1/3+2/3"`).
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
    /// Where columns out of view go: `"strip"` (the default) parks them
    /// off-screen; `"book"` piles them behind the leftmost and rightmost
    /// visible columns, so flipping through the strip is a one-column
    /// scroll and every window stays on screen. A `[[columns.screen]]` rule
    /// can set its own.
    pub layout: LayoutMode,
    /// Whether `focus-column-left/right` wraps from one end of the strip to
    /// the other: `"off"` (the default; at the end focus hops to the next
    /// screen or stops), `"on"` (always wraps, strip and book mode) or
    /// `"book"` (wraps only on a screen in book mode). Moving a column never
    /// wraps.
    pub wrap_around: WrapAround,
    /// `[[columns.screen]]`: screens matched by a rule use its list instead;
    /// the first matching rule wins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub screen: Vec<ScreenPresetRule>,
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
            layout: LayoutMode::Strip,
            wrap_around: WrapAround::Off,
            screen: Vec::new(),
        }
    }
}

impl ColumnsConfig {
    /// Validate the section: at least one preset, `default` (when set)
    /// names one of them, `max_rows` (when set) is not below `visible_rows`
    /// (when set), and every `[[columns.screen]]` rule matches on something,
    /// has a sane width range, a non-empty list, a `default` from that list
    /// and a positive floor. Whether a preset fits a screen is checked
    /// against the live screens by [`check_screen`](Self::check_screen).
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
        validate_list("columns", &self.presets, self.default.as_ref())?;
        for (i, rule) in self.screen.iter().enumerate() {
            let where_ = format!("columns.screen[{i}]");
            if rule.match_.is_empty() {
                return Err(format!(
                    "{where_}.match must set at least one of orientation, min_width_px, max_width_px, name"
                ));
            }
            if let (Some(min), Some(max)) = (rule.match_.min_width_px, rule.match_.max_width_px)
                && min > max
            {
                return Err(format!(
                    "{where_}.match: min_width_px ({min}) must not exceed max_width_px ({max})"
                ));
            }
            if rule
                .match_
                .name
                .as_deref()
                .is_some_and(|n| n.trim().is_empty())
            {
                return Err(format!("{where_}.match.name must not be empty"));
            }
            if rule.min_column_width_px == Some(0) {
                return Err(format!(
                    "{where_}.min_column_width_px must be positive, got 0"
                ));
            }
            if !rule.has_presets() {
                if rule.layout.is_none() {
                    return Err(format!(
                        "{where_}.presets: give at least one preset, or set layout"
                    ));
                }
                if rule.default.is_some() || rule.min_column_width_px.is_some() {
                    return Err(format!(
                        "{where_}: default and min_column_width_px need the rule's own presets"
                    ));
                }
                continue;
            }
            validate_list(&where_, &rule.presets, rule.default.as_ref())?;
        }
        Ok(())
    }

    /// Index of the first `[[columns.screen]]` rule matching `facts`.
    #[must_use]
    pub fn rule_for(&self, facts: &ScreenFacts) -> Option<usize> {
        self.screen.iter().position(|r| r.match_.matches(facts))
    }

    /// The preset list a screen described by `facts` uses: the first
    /// matching rule's, else the top-level list.
    #[must_use]
    pub fn list_for(&self, facts: &ScreenFacts) -> PresetList<'_> {
        match self.rule_for(facts) {
            Some(i) if self.screen[i].has_presets() => {
                let rule = &self.screen[i];
                PresetList {
                    presets: &rule.presets,
                    default: rule.default.as_ref(),
                    min_column_width_px: rule.min_column_width_px,
                    rule: Some(i),
                }
            }
            _ => self.top_level(),
        }
    }

    /// The layout mode a screen described by `facts` uses: the first
    /// matching rule's `layout` when it sets one, else the top-level
    /// `layout`.
    #[must_use]
    pub fn layout_for(&self, facts: &ScreenFacts) -> LayoutMode {
        self.rule_for(facts)
            .and_then(|i| self.screen[i].layout)
            .unwrap_or(self.layout)
    }

    /// The top-level `[columns]` list (what every screen used before
    /// `[[columns.screen]]` existed).
    #[must_use]
    pub fn top_level(&self) -> PresetList<'_> {
        PresetList {
            presets: &self.presets,
            default: self.default.as_ref(),
            min_column_width_px: None,
            rule: None,
        }
    }

    /// Check that every preset of the list `facts` resolves to has every
    /// term at least `min_column_width_px` wide on that screen (the rule's
    /// own floor when it set one, else `global_floor`) with `window_gap`.
    ///
    /// # Errors
    ///
    /// A message naming the list, the preset, the term, its pixel width,
    /// the screen and the floor it fell under.
    pub fn check_screen(
        &self,
        facts: &ScreenFacts,
        window_gap: i32,
        global_floor: u32,
    ) -> Result<(), String> {
        let list = self.list_for(facts);
        let floor = list.min_column_width_px.unwrap_or(global_floor);
        for preset in list.presets {
            for term in preset.terms() {
                let px = term.width_px(facts.width, window_gap);
                if px < floor as i32 {
                    return Err(format!(
                        "{}.presets: {} is {px} px wide on {}, below min_column_width_px {floor}{}",
                        list.describe(),
                        if preset.is_pattern() {
                            format!("term {term} of \"{}\"", preset.label())
                        } else {
                            format!("\"{}\"", preset.label())
                        },
                        facts.describe(),
                        if list.rule.is_none() {
                            " (give that screen its own [[columns.screen]] rule, or lower the floor)"
                        } else {
                            ""
                        }
                    ));
                }
            }
        }
        Ok(())
    }

    /// Index into the top-level `presets` of the preset every monitor
    /// starts on: the `default` entry when set, else the first preset.
    #[must_use]
    pub fn default_index(&self) -> usize {
        self.top_level().default_index()
    }

    /// Index of the top-level preset with the same widths as `wanted`.
    #[must_use]
    pub fn index_of(&self, wanted: &WidthPreset) -> Option<usize> {
        self.top_level().index_of(wanted)
    }

    /// The top-level presets' labels, in order.
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        self.top_level().labels()
    }
}

/// `presets` non-empty and `default` (when set) one of them, for the list
/// configured at `where_`.
fn validate_list(
    where_: &str,
    presets: &[WidthPreset],
    default: Option<&WidthPreset>,
) -> Result<(), String> {
    if presets.is_empty() {
        return Err(format!(
            "{where_}.presets must list at least one preset (e.g. [\"1/3\", \"1/2\", \"1/1\"])"
        ));
    }
    if let Some(default) = default
        && !presets.iter().any(|p| p.same_width(default))
    {
        return Err(format!(
            "{where_}.default ({default}) is not one of {where_}.presets ({})",
            presets
                .iter()
                .map(|p| p.label().to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(width: i32, height: i32, name: &str) -> ScreenFacts {
        ScreenFacts {
            width,
            height,
            name: name.to_owned(),
        }
    }

    #[test]
    fn parses_fractions_and_reduces_them() {
        let half = WidthPreset::parse("1/2").expect("1/2");
        assert_eq!(half.fraction(), (1, 2));
        assert_eq!(half.label(), "1/2");
        assert!(!half.is_pattern());
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
            "", "0/2", "1/0", "3/2", "2", "0", "-0.5", "abc", "1/x", "1.5", "+", "1/3+", "+1/3",
            "1/3++2/3",
        ] {
            assert!(WidthPreset::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn parses_patterns_by_term_and_compares_them_by_width() {
        let p = WidthPreset::parse("1/3+2/3").expect("pattern");
        assert!(p.is_pattern());
        assert_eq!(p.label(), "1/3+2/3");
        assert_eq!(
            p.terms().iter().map(|t| t.parts()).collect::<Vec<_>>(),
            vec![(1, 3), (2, 3)]
        );
        assert_eq!(p.fraction(), (1, 3));
        assert!(p.same_width(&WidthPreset::parse("2/6 + 4/6").expect("spaced")));
        assert!(!p.same_width(&WidthPreset::parse("2/3+1/3").expect("reversed")));
        let q = WidthPreset::parse("1/4+1/4+1/2").expect("three terms");
        assert_eq!(q.terms().len(), 3);
        let d = WidthPreset::parse("0.25+0.75").expect("decimals");
        assert!(d.same_width(&WidthPreset::parse("1/4+3/4").expect("fractions")));
        // A pattern narrower than the screen is allowed; one wider is not.
        assert!(WidthPreset::parse("1/4+1/4").is_ok());
        let err = WidthPreset::parse("1/2+2/3").expect_err("too wide");
        assert!(err.contains("more than the screen"), "{err}");
        assert!(WidthPreset::parse("1/3+1/3+1/3").is_ok());
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
    fn pattern_widths_cycle_by_position_and_fill_one_period() {
        let gap = 16;
        let p = WidthPreset::parse("1/3+2/3").expect("pattern");
        assert_eq!(p.widths_px(1920, gap), vec![618, 1253]);
        for (i, want) in [618, 1253, 618, 1253, 618].into_iter().enumerate() {
            assert_eq!(p.width_px_at(i, 1920, gap), want, "column {i}");
        }
        // One period plus its gaps is the work area (within rounding).
        let total: i32 = p.widths_px(1920, gap).iter().sum::<i32>() + gap * 3;
        assert!((total - 1920).abs() <= 1, "{total}");
        let q = WidthPreset::parse("1/4+1/4+1/2").expect("quarters");
        let w = q.widths_px(2560, gap);
        assert_eq!(w, vec![620, 620, 1256]);
        let total: i32 = w.iter().sum::<i32>() + gap * 4;
        assert!((total - 2560).abs() <= 1, "{total}");
        // A single fraction is the n = 1 pattern.
        let half = WidthPreset::parse("1/2").expect("1/2");
        assert_eq!(half.width_px_at(7, 1920, gap), half.width_px(1920, gap));
    }

    #[test]
    fn serde_accepts_strings_and_numbers_and_writes_labels() {
        #[derive(Deserialize)]
        struct T {
            presets: Vec<WidthPreset>,
        }
        let t: T = toml::from_str("presets = [\"1/3\", 0.5, 1, \"1/3+2/3\"]").expect("parse");
        assert_eq!(t.presets[0].fraction(), (1, 3));
        assert_eq!(t.presets[1].fraction(), (1, 2));
        assert_eq!(t.presets[2].fraction(), (1, 1));
        assert!(t.presets[3].is_pattern());
        assert_eq!(
            serde_json::to_string(&t.presets[0]).expect("json"),
            "\"1/3\""
        );
        assert_eq!(
            serde_json::to_string(&t.presets[3]).expect("json"),
            "\"1/3+2/3\""
        );
        assert!(toml::from_str::<T>("presets = [\"3/2\"]").is_err());
        assert!(toml::from_str::<T>("presets = [\"1/2+2/3\"]").is_err());
    }

    #[test]
    fn columns_config_defaults_validate_and_default_index() {
        let cfg = ColumnsConfig::default();
        assert_eq!(cfg.labels(), vec!["1/3", "1/2", "1/1"]);
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.default_index(), 0);
        assert!(cfg.screen.is_empty());

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

        // A pattern default is fine when it is in the list.
        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/3\", \"1/3+2/3\", \"1/1\"]\ndefault = \"1/3+2/3\"")
                .expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.default_index(), 1);
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

    const TWO_RULES: &str = r#"
presets = ["1/3", "1/3+2/3", "1/2", "1/1"]
default = "1/2"

[[screen]]
match = { orientation = "portrait" }
presets = ["1/2", "1/1"]
default = "1/1"
min_column_width_px = 500

[[screen]]
match = { min_width_px = 2560 }
presets = ["1/4", "1/4+1/4+1/2", "1/3", "1/1"]
default = "1/3"
"#;

    #[test]
    fn screen_rules_parse_and_the_first_match_wins() {
        let cfg: ColumnsConfig = toml::from_str(TWO_RULES).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.screen.len(), 2);

        // A landscape 1920 screen falls through to the top-level list.
        let wide = facts(1872, 1080, r"\\.\DISPLAY1");
        assert_eq!(cfg.rule_for(&wide), None);
        let list = cfg.list_for(&wide);
        assert_eq!(list.labels(), vec!["1/3", "1/3+2/3", "1/2", "1/1"]);
        assert_eq!(list.default_index(), 2);
        assert_eq!(list.describe(), "columns");
        assert_eq!(list.min_column_width_px, None);

        // Portrait takes the first rule with its own floor.
        let tall = facts(1080, 1872, r"\\.\DISPLAY2");
        assert_eq!(tall.orientation(), Orientation::Portrait);
        let list = cfg.list_for(&tall);
        assert_eq!(list.rule, Some(0));
        assert_eq!(list.labels(), vec!["1/2", "1/1"]);
        assert_eq!(list.default_index(), 1);
        assert_eq!(list.min_column_width_px, Some(500));
        assert_eq!(list.describe(), "columns.screen[0]");

        // A wide landscape screen takes the second rule; a wide portrait one
        // still takes the first (first match wins).
        let ultra = facts(3440, 1440, "DISPLAY3");
        assert_eq!(cfg.list_for(&ultra).rule, Some(1));
        assert_eq!(cfg.list_for(&ultra).default_index(), 2);
        let tall_wide = facts(2560, 4000, "DISPLAY4");
        assert_eq!(cfg.list_for(&tall_wide).rule, Some(0));
    }

    #[test]
    fn screen_match_fields_each_constrain() {
        let m = |t: &str| -> ScreenMatch { toml::from_str(t).expect("match") };
        let f = facts(1872, 1080, r"\\.\DISPLAY2");
        assert!(m("orientation = \"landscape\"").matches(&f));
        assert!(!m("orientation = \"portrait\"").matches(&f));
        assert!(m("min_width_px = 1872").matches(&f));
        assert!(!m("min_width_px = 1873").matches(&f));
        assert!(m("max_width_px = 1872").matches(&f));
        assert!(!m("max_width_px = 1871").matches(&f));
        assert!(m("min_width_px = 1000\nmax_width_px = 2000").matches(&f));
        // The name matches with or without the prefix, case-insensitively.
        assert!(m("name = \"DISPLAY2\"").matches(&f));
        assert!(m("name = \"display2\"").matches(&f));
        assert!(m("name = '\\\\.\\DISPLAY2'").matches(&f));
        assert!(!m("name = \"DISPLAY1\"").matches(&f));
        // Every field given must hold.
        assert!(!m("name = \"DISPLAY2\"\norientation = \"portrait\"").matches(&f));
        assert!(ScreenMatch::default().is_empty());
        // A square work area is landscape.
        assert_eq!(facts(1000, 1000, "").orientation(), Orientation::Landscape);
        assert_eq!(facts(1000, 1000, "").describe(), "a 1000x1000 work area");
        assert_eq!(f.describe(), "DISPLAY2 (1872x1080 work area)");
    }

    #[test]
    fn screen_rules_validate_their_fields() {
        let bad = |body: &str| -> String {
            let cfg: ColumnsConfig =
                toml::from_str(&format!("presets = [\"1/2\"]\n{body}")).expect("parse");
            cfg.validate().expect_err("must be rejected")
        };
        let err = bad("[[screen]]\nmatch = {}\npresets = [\"1/2\"]");
        assert!(err.contains("columns.screen[0].match must set"), "{err}");
        let err = bad("[[screen]]\nmatch = { name = \"DISPLAY2\" }\npresets = []");
        assert!(err.contains("columns.screen[0].presets"), "{err}");
        let err = bad(
            "[[screen]]\nmatch = { name = \"DISPLAY2\" }\npresets = [\"1/2\"]\ndefault = \"1/3\"",
        );
        assert!(err.contains("columns.screen[0].default"), "{err}");
        let err = bad(
            "[[screen]]\nmatch = { min_width_px = 3000, max_width_px = 2000 }\npresets = [\"1/2\"]",
        );
        assert!(err.contains("min_width_px (3000) must not exceed"), "{err}");
        let err = bad(
            "[[screen]]\nmatch = { name = \"DISPLAY2\" }\npresets = [\"1/2\"]\nmin_column_width_px = 0",
        );
        assert!(
            err.contains("columns.screen[0].min_column_width_px"),
            "{err}"
        );
        let err = bad("[[screen]]\nmatch = { name = \"  \" }\npresets = [\"1/2\"]");
        assert!(err.contains("name must not be empty"), "{err}");
        // The second rule is named by its index.
        let err = bad(
            "[[screen]]\nmatch = { name = \"A\" }\npresets = [\"1/2\"]\n[[screen]]\nmatch = {}\npresets = [\"1/2\"]",
        );
        assert!(err.contains("columns.screen[1].match"), "{err}");
        // Unknown match keys are refused (a typo would silently match everything).
        assert!(
            toml::from_str::<ColumnsConfig>(
                "presets = [\"1/2\"]\n[[screen]]\nmatch = { nmae = \"A\" }\npresets = [\"1/2\"]"
            )
            .is_err()
        );
    }

    #[test]
    fn check_screen_names_the_term_the_screen_and_the_floor() {
        let cfg: ColumnsConfig = toml::from_str(TWO_RULES).expect("parse");
        let gap = 16;
        // 1/3 of 1872 is (1872 - 16) / 3 - 16 = 602 px: fine at a 600
        // floor, not at 640.
        let wide = facts(1872, 1080, r"\\.\DISPLAY1");
        assert!(cfg.check_screen(&wide, gap, 600).is_ok());
        let err = cfg
            .check_screen(&wide, gap, 640)
            .expect_err("1/3 under 640");
        assert!(err.starts_with("columns.presets: \"1/3\" is 602 px wide on DISPLAY1 (1872x1080 work area), below min_column_width_px 640"), "{err}");
        assert!(err.contains("[[columns.screen]]"), "{err}");
        // A pattern term is named as such.
        let cfg2: ColumnsConfig =
            toml::from_str("presets = [\"1/2\", \"1/4+3/4\"]").expect("parse");
        let err = cfg2
            .check_screen(&wide, gap, 600)
            .expect_err("1/4 under 600");
        assert!(
            err.contains("term 1/4 of \"1/4+3/4\" is 448 px wide"),
            "{err}"
        );
        // The portrait rule's own floor (500) applies: 1/2 of 1080 is 516.
        let tall = facts(1080, 1872, r"\\.\DISPLAY2");
        assert!(cfg.check_screen(&tall, gap, 640).is_ok());
        let cfg3: ColumnsConfig = toml::from_str(
            "presets = [\"1/2\"]\n[[screen]]\nmatch = { orientation = \"portrait\" }\npresets = [\"1/2\"]\nmin_column_width_px = 600",
        )
        .expect("parse");
        let err = cfg3.check_screen(&tall, gap, 300).expect_err("rule floor");
        assert!(err.starts_with("columns.screen[0].presets: \"1/2\" is 516 px wide on DISPLAY2 (1080x1872 work area), below min_column_width_px 600"), "{err}");
        assert!(!err.contains("[[columns.screen]]"), "{err}");
        // The wide rule: 1/4 of 2560 is 620, fine at 600.
        let ultra = facts(2560, 1440, "DISPLAY3");
        assert!(cfg.check_screen(&ultra, gap, 600).is_ok());
    }

    #[test]
    fn carry_over_keeps_the_same_preset_or_falls_back_to_the_default() {
        let cfg: ColumnsConfig = toml::from_str(TWO_RULES).expect("parse");
        let wide = cfg.list_for(&facts(1872, 1080, "DISPLAY1"));
        let tall = cfg.list_for(&facts(1080, 1872, "DISPLAY2"));
        let half = WidthPreset::parse("1/2").expect("1/2");
        let pattern = WidthPreset::parse("1/3+2/3").expect("pattern");
        // 1/2 exists in both lists: the screen stays on it when rotated.
        assert_eq!(wide.carry_over(Some(&half)), 2);
        assert_eq!(tall.carry_over(Some(&half)), 0);
        // The pattern is not in the portrait list: its default (1/1).
        assert_eq!(tall.carry_over(Some(&pattern)), 1);
        assert_eq!(wide.carry_over(Some(&pattern)), 1);
        // No previous preset: the default.
        assert_eq!(wide.carry_over(None), 2);
        assert_eq!(tall.carry_over(None), 1);
    }

    #[test]
    fn layout_and_wrap_around_parse_with_a_per_screen_layout() {
        let cfg: ColumnsConfig = toml::from_str(
            "presets = [\"1/2\"]\nlayout = \"book\"\nwrap_around = \"book\"\n\
             [[screen]]\nmatch = { name = \"DISPLAY2\" }\nlayout = \"strip\"\n\
             [[screen]]\nmatch = { orientation = \"portrait\" }\npresets = [\"1/1\"]",
        )
        .expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.layout, LayoutMode::Book);
        assert_eq!(cfg.wrap_around, WrapAround::Book);
        // A layout-only rule changes the mode, not the list.
        let d2 = facts(1920, 1080, "DISPLAY2");
        assert_eq!(cfg.layout_for(&d2), LayoutMode::Strip);
        assert_eq!(cfg.list_for(&d2).rule, None);
        assert_eq!(cfg.list_for(&d2).labels(), vec!["1/2".to_owned()]);
        // A preset rule without layout keeps the top-level mode.
        let tall = facts(1080, 1920, "DISPLAY3");
        assert_eq!(cfg.layout_for(&tall), LayoutMode::Book);
        assert_eq!(cfg.list_for(&tall).rule, Some(1));
        // Defaults: strip, no wrap.
        let plain: ColumnsConfig = toml::from_str("presets = [\"1/2\"]").expect("parse");
        assert_eq!(
            (plain.layout, plain.wrap_around),
            (LayoutMode::Strip, WrapAround::Off)
        );
        // Unknown values are refused.
        assert!(toml::from_str::<ColumnsConfig>("layout = \"pages\"").is_err());
        assert!(toml::from_str::<ColumnsConfig>("wrap_around = \"yes\"").is_err());
        // A rule with neither presets nor layout, or a default without its
        // own presets, is refused.
        let bad = |body: &str| {
            toml::from_str::<ColumnsConfig>(&format!("presets = [\"1/2\"]\n{body}"))
                .expect("parse")
                .validate()
                .expect_err("refused")
        };
        assert!(bad("[[screen]]\nmatch = { name = \"D\" }").contains("or set layout"));
        assert!(
            bad("[[screen]]\nmatch = { name = \"D\" }\nlayout = \"book\"\ndefault = \"1/2\"")
                .contains("need the rule's own presets")
        );
    }
}
