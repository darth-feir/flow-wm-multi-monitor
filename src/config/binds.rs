//! `[binds]` — key bindings as configuration, in niri's spelling.
//!
//! The daemon owns the table: it parses the chords (`"Mod+Shift+Left"`) and
//! the actions (`"move-column-left"`, `{ spawn = ["alacritty"] }`), lays the
//! user's entries over the built-in defaults ([`DEFAULT_BINDS_TOML`], the
//! keymap's `binds.toml`), and hands the resolved list to the hotkey script
//! through `flow query binds`, which registers the chords at runtime. Nothing
//! here touches Win32; the script's hotkey syntax is produced as text.
//!
//! (`docs/src/dev-guide/keybinds.md`, `docs/adr/0015-binds-are-config.md`)

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The keymap's default table (`keymaps/win-niri/binds.toml`), compiled in so
/// a `flow.toml` without `[binds]` still drives the win-niri keymap.
pub const DEFAULT_BINDS_TOML: &str = include_str!("../../keymaps/win-niri/binds.toml");

/// The key the table reserves for "start from an empty table": not a chord.
const DEFAULTS_KEY: &str = "defaults";

/// The action text that unbinds a chord.
const NONE_ACTION: &str = "none";

/// Field separator of the `--plain` listing: a tab.
const PLAIN_SEP: char = '\t';

/// Separator between argv items inside one `--plain` field (ASCII unit
/// separator, which no argument contains in practice).
pub const PLAIN_ARGV_SEP: char = '\u{1f}';

// ── Chords ────────────────────────────────────────────────────────────────

/// A chord modifier, in the canonical order the chord text uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    /// The Windows key (`Mod`, `Super` and `Win` all spell it).
    Mod,
    /// `Ctrl` / `Control`.
    Ctrl,
    /// `Alt`.
    Alt,
    /// `Shift`.
    Shift,
}

impl Modifier {
    fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "mod" | "super" | "win" => Some(Self::Mod),
            "ctrl" | "control" => Some(Self::Ctrl),
            "alt" => Some(Self::Alt),
            "shift" => Some(Self::Shift),
            _ => None,
        }
    }

    /// The name the canonical chord text uses: the Windows key prints as
    /// `Win`, the key it is, though a table may spell it `Mod` (ADR-0031).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mod => "Win",
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Shift => "Shift",
        }
    }

    /// AutoHotkey's prefix symbol.
    #[must_use]
    pub const fn ahk(self) -> char {
        match self {
            Self::Mod => '#',
            Self::Ctrl => '^',
            Self::Alt => '!',
            Self::Shift => '+',
        }
    }
}

/// Named keys: niri / XKB spelling on the left, AutoHotkey's on the right.
/// Letters, digits, `F1`..`F24` and raw `vkXX` / `scXXX` codes are handled
/// in [`Key::parse`]. `Slash` is bound by virtual-key code because `#+/`
/// never fires in AutoHotkey v2 (the keymap's README, *AutoHotkey lessons*).
const NAMED_KEYS: &[(&str, &str)] = &[
    ("Left", "Left"),
    ("Right", "Right"),
    ("Up", "Up"),
    ("Down", "Down"),
    ("Home", "Home"),
    ("End", "End"),
    ("Page_Up", "PgUp"),
    ("Page_Down", "PgDn"),
    ("Insert", "Insert"),
    ("Delete", "Delete"),
    ("BackSpace", "Backspace"),
    ("Return", "Enter"),
    ("Enter", "Enter"),
    ("Tab", "Tab"),
    ("Escape", "Escape"),
    ("Space", "Space"),
    ("Print", "PrintScreen"),
    ("Pause", "Pause"),
    ("Minus", "-"),
    ("Equal", "="),
    ("BracketLeft", "["),
    ("BracketRight", "]"),
    ("Semicolon", ";"),
    ("Apostrophe", "'"),
    ("Grave", "`"),
    ("Backslash", "\\"),
    ("Comma", ","),
    ("Period", "."),
    ("Slash", "vkBF"),
    ("WheelUp", "WheelUp"),
    ("WheelDown", "WheelDown"),
    ("WheelLeft", "WheelLeft"),
    ("WheelRight", "WheelRight"),
];

/// Other spellings of a named key: the alias on the left, the canonical
/// name in [`NAMED_KEYS`] on the right. niri spells the wheel
/// `WheelScrollDown`; both parse to one chord.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("WheelScrollUp", "WheelUp"),
    ("WheelScrollDown", "WheelDown"),
    ("WheelScrollLeft", "WheelLeft"),
    ("WheelScrollRight", "WheelRight"),
];

/// The mouse-wheel keys (canonical names). The keymap gives their chords a
/// per-chord cooldown so one flick is one step (ADR-0029).
const WHEEL_KEYS: &[&str] = &["WheelUp", "WheelDown", "WheelLeft", "WheelRight"];

/// Keys that may be bound without a modifier (a bare letter or digit would
/// swallow typing).
const BARE_OK: &[&str] = &["Print", "Pause"];

/// The key of a chord: its canonical name and its AutoHotkey spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    name: String,
    ahk: String,
}

impl Key {
    /// Parse one key name. Case-insensitive for named keys and letters.
    fn parse(word: &str) -> Result<Self, String> {
        if word.is_empty() {
            return Err("missing key after the last '+'".into());
        }
        let lower = word.to_ascii_lowercase();
        if lower.starts_with("mouse") || lower.ends_with("button") {
            return Err(format!(
                "\"{word}\": mouse buttons are not configurable (Win + left button is the drag)"
            ));
        }
        if word.len() == 1 {
            let c = word.chars().next().expect("one char");
            if c.is_ascii_alphabetic() {
                return Ok(Self {
                    name: c.to_ascii_uppercase().to_string(),
                    ahk: c.to_ascii_lowercase().to_string(),
                });
            }
            if c.is_ascii_digit() {
                return Ok(Self {
                    name: c.to_string(),
                    ahk: c.to_string(),
                });
            }
        }
        let word = KEY_ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(word))
            .map_or(word, |(_, name)| *name);
        if let Some((name, ahk)) = NAMED_KEYS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(word))
        {
            return Ok(Self {
                name: (*name).to_owned(),
                ahk: (*ahk).to_owned(),
            });
        }
        if let Some(n) = lower.strip_prefix('f')
            && let Ok(n) = n.parse::<u8>()
            && (1..=24).contains(&n)
        {
            return Ok(Self {
                name: format!("F{n}"),
                ahk: format!("F{n}"),
            });
        }
        let is_hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
        if let Some(hex) = lower.strip_prefix("vk")
            && hex.len() == 2
            && is_hex(hex)
        {
            let code = format!("vk{}", hex.to_ascii_uppercase());
            return Ok(Self {
                name: code.clone(),
                ahk: code,
            });
        }
        if let Some(hex) = lower.strip_prefix("sc")
            && hex.len() == 3
            && is_hex(hex)
        {
            let code = format!("sc{}", hex.to_ascii_uppercase());
            return Ok(Self {
                name: code.clone(),
                ahk: code,
            });
        }
        Err(format!(
            "unknown key \"{word}\" (a letter, a digit, F1..F24, one of {}, or vkXX / scXXX)",
            NAMED_KEYS
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// The canonical name (`Left`, `H`, `BracketLeft`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is a mouse-wheel key (`WheelUp` .. `WheelRight`).
    #[must_use]
    pub fn is_wheel(&self) -> bool {
        WHEEL_KEYS.contains(&self.name.as_str())
    }

    /// The overlay groups keys of one family on a row: arrows, wheel, the rest.
    fn family(&self) -> u8 {
        match self.name.as_str() {
            "Left" | "Right" | "Up" | "Down" => 0,
            _ if self.is_wheel() => 1,
            _ => 2,
        }
    }

    /// Whether this is a digit key.
    #[must_use]
    pub fn digit(&self) -> Option<u8> {
        let mut chars = self.name.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_digit() => Some(c as u8 - b'0'),
            _ => None,
        }
    }

    /// Listing order: arrows (niri's Left, Down, Up, Right), Space, digits
    /// with `0` as ten, letters, the other named keys, F-keys, raw codes.
    fn rank(&self) -> u32 {
        match self.name.as_str() {
            "Left" => 0,
            "Down" => 1,
            "Up" => 2,
            "Right" => 3,
            "Space" => 4,
            _ => {
                if let Some(d) = self.digit() {
                    return 10 + if d == 0 { 10 } else { u32::from(d) };
                }
                let mut chars = self.name.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    return 100 + u32::from(c as u8);
                }
                if let Some(i) = NAMED_KEYS.iter().position(|(n, _)| *n == self.name) {
                    return 200 + u32::try_from(i).unwrap_or(0);
                }
                if let Some(n) = self
                    .name
                    .strip_prefix('F')
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    return 300 + n;
                }
                400
            }
        }
    }
}

/// A chord: a set of modifiers and one key. Two spellings of the same chord
/// (`Shift+Mod+Left`, `mod+shift+left`, `Win+Shift+Left`) parse equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    mods: Vec<Modifier>,
    key: Key,
}

impl Chord {
    /// Parse `"Mod+Shift+Left"`. The last `+`-separated word is the key.
    ///
    /// # Errors
    ///
    /// An unknown modifier or key, a repeated modifier, a bare key that is
    /// not `Print` / `Pause`, or a mouse button.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (key_word, mod_words) = parts.split_last().ok_or("empty chord")?;
        let mut mods: Vec<Modifier> = Vec::new();
        for word in mod_words {
            let m = Modifier::parse(word).ok_or_else(|| {
                format!("unknown modifier \"{word}\" (Mod / Super / Win, Ctrl, Alt, Shift)")
            })?;
            if mods.contains(&m) {
                return Err(if m == Modifier::Mod {
                    format!("modifier \"{word}\" given twice (Mod, Win and Super are one key)")
                } else {
                    format!("modifier \"{word}\" given twice")
                });
            }
            mods.push(m);
        }
        mods.sort();
        let key = Key::parse(key_word)?;
        if mods.is_empty() && !BARE_OK.contains(&key.name.as_str()) {
            return Err(format!(
                "\"{text}\" has no modifier; a bare key would shadow typing (Print and Pause are the exceptions)"
            ));
        }
        Ok(Self { mods, key })
    }

    /// The canonical text, modifiers in [`Modifier`] order: `Win+Ctrl+Alt+Shift+Key`.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        for m in &self.mods {
            out.push_str(m.name());
            out.push('+');
        }
        out.push_str(&self.key.name);
        out
    }

    /// AutoHotkey's hotkey string (`#+Left`).
    #[must_use]
    pub fn ahk(&self) -> String {
        let mut out: String = self.mods.iter().map(|m| m.ahk()).collect();
        out.push_str(&self.key.ahk);
        out
    }

    /// The modifier prefix of the canonical text (`Win+Shift+`), for help rows.
    #[must_use]
    pub fn mod_prefix(&self) -> String {
        self.mods.iter().map(|m| format!("{}+", m.name())).collect()
    }

    /// The key.
    #[must_use]
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// Whether the chord is one the keymap hardwires (the FlowWM on/off
    /// switch and the tiling pause): not available to the table.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        self.key.name == "T"
            && self.mods.contains(&Modifier::Mod)
            && self.mods.contains(&Modifier::Ctrl)
            && !self.mods.contains(&Modifier::Alt)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

// ── Actions ───────────────────────────────────────────────────────────────

/// What the keymap does when a bound chord fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    /// `flow dispatch <argv>`.
    Dispatch,
    /// Run a program (`argv`, optionally elevated).
    Spawn,
    /// The keymap's application launcher.
    Launcher,
    /// The key-binding overlay.
    Help,
    /// `flow stop` and the script's own exit.
    Quit,
    /// `flow dispatch tiling toggle` with the keymap's own state flip and message.
    ToggleTiling,
}

/// When a bind is live: window-management chords only while tiling is on,
/// the rest whenever the daemon answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Bound while the daemon runs and tiling is on.
    Tiling,
    /// Bound while the daemon runs.
    Always,
}

/// A resolved action: what to run, when, and how the overlay describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Canonical action text (`focus-workspace 3`, `spawn alacritty`).
    pub text: String,
    /// What the keymap does.
    pub kind: ActionKind,
    /// `flow dispatch` arguments or the program and its arguments.
    pub argv: Vec<String>,
    /// Spawn elevated (`admin = true`).
    pub admin: bool,
    /// When the bind is live.
    pub scope: Scope,
    /// One line for the overlay.
    pub help: String,
    /// Binds with the same group collapse into one overlay row.
    pub group: String,
}

/// Fixed-name actions: niri name, dispatch argv, help, overlay group.
const FIXED_ACTIONS: &[(&str, &[&str], &str, &str)] = &[
    (
        "focus-column-left",
        &["focus", "left"],
        "focus the next window on this screen",
        "focus",
    ),
    (
        "focus-column-right",
        &["focus", "right"],
        "focus the next window on this screen",
        "focus",
    ),
    (
        "focus-window-up",
        &["focus", "up"],
        "focus the next window on this screen",
        "focus",
    ),
    (
        "focus-window-down",
        &["focus", "down"],
        "focus the next window on this screen",
        "focus",
    ),
    (
        "move-column-left",
        &["move-window", "left"],
        "move the window along the strip (up/down inside a stacked column)",
        "move",
    ),
    (
        "move-column-right",
        &["move-window", "right"],
        "move the window along the strip (up/down inside a stacked column)",
        "move",
    ),
    (
        "move-window-up",
        &["move-window", "up"],
        "move the window along the strip (up/down inside a stacked column)",
        "move",
    ),
    (
        "move-window-down",
        &["move-window", "down"],
        "move the window along the strip (up/down inside a stacked column)",
        "move",
    ),
    (
        "focus-monitor-left",
        &["focus-monitor", "left"],
        "focus the other screen",
        "focus-monitor",
    ),
    (
        "focus-monitor-right",
        &["focus-monitor", "right"],
        "focus the other screen",
        "focus-monitor",
    ),
    (
        "focus-monitor-up",
        &["focus-monitor", "up"],
        "focus the other screen",
        "focus-monitor",
    ),
    (
        "focus-monitor-down",
        &["focus-monitor", "down"],
        "focus the other screen",
        "focus-monitor",
    ),
    (
        "move-column-to-monitor-left",
        &["move-to-monitor", "left"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-column-to-monitor-right",
        &["move-to-monitor", "right"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-column-to-monitor-up",
        &["move-to-monitor", "up"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-column-to-monitor-down",
        &["move-to-monitor", "down"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-window-to-monitor-left",
        &["move-to-monitor", "left"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-window-to-monitor-right",
        &["move-to-monitor", "right"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-window-to-monitor-up",
        &["move-to-monitor", "up"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "move-window-to-monitor-down",
        &["move-to-monitor", "down"],
        "move the window to the other screen",
        "move-to-monitor",
    ),
    (
        "close-window",
        &["close-window"],
        "close window",
        "close-window",
    ),
    (
        "minimize-window",
        &["minimize-window"],
        "minimize window",
        "minimize-window",
    ),
    (
        "center-column",
        &["center"],
        "center the focused column",
        "center-column",
    ),
    (
        "toggle-window-floating",
        &["set-window", "cycle"],
        "toggle floating",
        "toggle-window-floating",
    ),
    (
        "move-window-to-floating",
        &["set-window", "float"],
        "float the window",
        "move-window-to-floating",
    ),
    (
        "move-window-to-tiling",
        &["set-window", "tile"],
        "tile the window",
        "move-window-to-tiling",
    ),
    (
        "switch-focus-between-floating-and-tiling",
        &["switch-focus-between-floating-and-tiling"],
        "switch focus between the floating and the tiled windows",
        "switch-focus-between-floating-and-tiling",
    ),
    (
        "focus-floating",
        &["focus-floating"],
        "focus the floating windows (the one used last)",
        "focus-floating",
    ),
    (
        "focus-tiling",
        &["focus-tiling"],
        "focus the tiled windows (the one used last)",
        "focus-tiling",
    ),
    (
        "switch-preset-column-width",
        &["width", "next"],
        "cycle this screen's column widths (flow.toml [columns] presets)",
        "switch-preset-column-width",
    ),
    (
        "switch-preset-column-width-back",
        &["width", "prev"],
        "cycle this screen's column widths backwards",
        "switch-preset-column-width-back",
    ),
    (
        "toggle-layout",
        &["set-layout", "toggle"],
        "book / strip layout on this screen (columns out of view piled or parked)",
        "toggle-layout",
    ),
    (
        "fullscreen-window",
        &["set-window", "fullscreen"],
        "fullscreen toggle",
        "fullscreen-window",
    ),
    (
        "consume-or-expel-window-left",
        &["consume-or-expel", "left"],
        "alone: merge into the column on that side; stacked: pop out to that side",
        "consume-or-expel",
    ),
    (
        "consume-or-expel-window-right",
        &["consume-or-expel", "right"],
        "alone: merge into the column on that side; stacked: pop out to that side",
        "consume-or-expel",
    ),
    (
        "consume-window-into-column",
        &["merge-column", "right"],
        "join the column to the right (FlowWM's nearest to niri's consume)",
        "consume-window-into-column",
    ),
    (
        "expel-window-from-column",
        &["promote", "right"],
        "pop the window out into its own column to the right",
        "expel-window-from-column",
    ),
];

/// The names [`resolve_action`] accepts, for error messages and docs.
fn action_names() -> String {
    let mut names: Vec<&str> = FIXED_ACTIONS.iter().map(|(n, ..)| *n).collect();
    names.extend([
        "focus-workspace N",
        "move-column-to-workspace N",
        "set-column-width +N% / -N%",
        "dispatch <flow dispatch arguments>",
        "show-hotkey-overlay",
        "launcher",
        "toggle-tiling",
        "quit",
        "none",
    ]);
    names.join(", ")
}

/// Resolve an action line (`"focus-workspace 3"`) into what the keymap runs.
///
/// # Errors
///
/// An unknown action name or a bad argument, naming the accepted forms.
pub fn resolve_action(line: &str) -> Result<Action, String> {
    let line = line.trim();
    let (name, arg) = match line.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (line, ""),
    };
    let name_lc = name.to_ascii_lowercase();
    let dispatch = |text: String, argv: Vec<String>, help: String, group: &str| Action {
        text,
        kind: ActionKind::Dispatch,
        argv,
        admin: false,
        scope: Scope::Tiling,
        help,
        group: group.to_owned(),
    };
    let builtin = |kind: ActionKind, help: &str| Action {
        text: name_lc.clone(),
        kind,
        argv: Vec::new(),
        admin: false,
        scope: Scope::Always,
        help: help.to_owned(),
        group: name_lc.clone(),
    };
    if let Some((n, argv, help, group)) = FIXED_ACTIONS.iter().find(|(n, ..)| *n == name_lc) {
        if !arg.is_empty() {
            return Err(format!("{n} takes no argument (got \"{arg}\")"));
        }
        return Ok(dispatch(
            (*n).to_owned(),
            argv.iter().map(|s| (*s).to_owned()).collect(),
            (*help).to_owned(),
            group,
        ));
    }
    match name_lc.as_str() {
        "focus-workspace" => {
            let n = workspace_arg(arg, name)?;
            Ok(dispatch(
                format!("focus-workspace {n}"),
                vec!["switch-workspace".into(), n.to_string()],
                format!("switch to workspace {n}"),
                "focus-workspace",
            ))
        }
        "move-column-to-workspace" | "move-window-to-workspace" => {
            let n = workspace_arg(arg, name)?;
            Ok(dispatch(
                format!("move-column-to-workspace {n}"),
                vec!["move-to-workspace".into(), n.to_string()],
                format!("move the window to workspace {n}"),
                "move-to-workspace",
            ))
        }
        "set-column-width" => {
            let a = arg.trim_matches('"');
            if a.starts_with('+') && a.len() > 1 {
                Ok(dispatch(
                    format!("set-column-width {a}"),
                    vec!["expand-column".into()],
                    "widen the focused column one step".into(),
                    "expand-column",
                ))
            } else if a.starts_with('-') && a.len() > 1 {
                Ok(dispatch(
                    format!("set-column-width {a}"),
                    vec!["shrink-column".into()],
                    "narrow the focused column one step".into(),
                    "shrink-column",
                ))
            } else {
                Err(format!(
                    "set-column-width takes \"+N%\" (widen one step) or \"-N%\" (narrow one step), got \"{arg}\""
                ))
            }
        }
        "dispatch" => {
            let argv: Vec<String> = arg.split_whitespace().map(str::to_owned).collect();
            if argv.is_empty() {
                return Err(
                    "dispatch needs the flow dispatch arguments, e.g. \"dispatch width 1/2\""
                        .into(),
                );
            }
            Ok(dispatch(
                format!("dispatch {}", argv.join(" ")),
                argv.clone(),
                format!("flow dispatch {}", argv.join(" ")),
                &format!("dispatch {}", argv.join(" ")),
            ))
        }
        "show-hotkey-overlay" => Ok(builtin(ActionKind::Help, "this help (Esc hides it)")),
        "launcher" => Ok(builtin(
            ActionKind::Launcher,
            "application launcher (apps; window - x and file - x search those instead)",
        )),
        "toggle-tiling" => Ok(builtin(
            ActionKind::ToggleTiling,
            "pause toggle (off: Windows manages windows, chords fall through)",
        )),
        "quit" => Ok(builtin(ActionKind::Quit, "stop FlowWM")),
        _ => Err(format!(
            "unknown action \"{name}\" (one of: {})",
            action_names()
        )),
    }
}

fn workspace_arg(arg: &str, name: &str) -> Result<u32, String> {
    match arg.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "{name} needs a workspace number from 1, got \"{arg}\""
        )),
    }
}

/// The action behind a `{ spawn = [...] }` value.
fn spawn_action(argv: &[String], admin: bool, help: Option<&str>) -> Result<Action, String> {
    let Some(program) = argv.first() else {
        return Err("spawn needs at least the program name".into());
    };
    if program.trim().is_empty() {
        return Err("spawn: the program name is empty".into());
    }
    let joined = argv.join(" ");
    let text = if admin {
        format!("spawn {joined} (admin)")
    } else {
        format!("spawn {joined}")
    };
    let help = help.map_or_else(
        || {
            if admin {
                format!("run {joined} as admin")
            } else {
                format!("run {joined}")
            }
        },
        str::to_owned,
    );
    Ok(Action {
        text: text.clone(),
        kind: ActionKind::Spawn,
        argv: argv.to_vec(),
        admin,
        scope: Scope::Always,
        help,
        group: text,
    })
}

// ── The table ─────────────────────────────────────────────────────────────

/// One value of the `[binds]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindValue {
    /// `"focus-column-left"`, `"focus-workspace 3"`, `"none"`.
    Action(String),
    /// `{ spawn = ["alacritty"], admin = true, help = "..." }`.
    Spawn {
        /// The program and its arguments.
        spawn: Vec<String>,
        /// Run elevated.
        admin: bool,
        /// Overlay text instead of `run <argv>`.
        help: Option<String>,
    },
}

impl<'de> Deserialize<'de> for BindValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BindValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an action string or a { spawn = [...] } table")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<BindValue, E> {
                Ok(BindValue::Action(s.to_owned()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<BindValue, A::Error> {
                let mut spawn: Option<Vec<String>> = None;
                let mut admin = false;
                let mut help: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "spawn" => spawn = Some(map.next_value()?),
                        "admin" => admin = map.next_value()?,
                        "help" => help = Some(map.next_value()?),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["spawn", "admin", "help"],
                            ));
                        }
                    }
                }
                let spawn = spawn.ok_or_else(|| de::Error::missing_field("spawn"))?;
                Ok(BindValue::Spawn { spawn, admin, help })
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for BindValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Action(a) => s.serialize_str(a),
            Self::Spawn { spawn, admin, help } => {
                let mut m = s.serialize_map(None)?;
                m.serialize_entry("spawn", spawn)?;
                if *admin {
                    m.serialize_entry("admin", admin)?;
                }
                if let Some(h) = help {
                    m.serialize_entry("help", h)?;
                }
                m.end()
            }
        }
    }
}

/// The `[binds]` table as written: entries in document order, plus the
/// `defaults` switch. Empty with `defaults = true` when the section is
/// absent, which resolves to the keymap's built-in table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindsConfig {
    /// Start from the built-in defaults (`true`) or from nothing.
    pub defaults: bool,
    /// `(chord text, value)` in the order written.
    pub entries: Vec<(String, BindValue)>,
}

impl Default for BindsConfig {
    fn default() -> Self {
        Self {
            defaults: true,
            entries: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for BindsConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BindsConfig;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a table of chord = action entries")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<BindsConfig, A::Error> {
                let mut out = BindsConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    if key == DEFAULTS_KEY {
                        out.defaults = map.next_value()?;
                    } else {
                        let value: BindValue = map.next_value()?;
                        out.entries.push((key, value));
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_map(V)
    }
}

impl Serialize for BindsConfig {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        if !self.defaults {
            m.serialize_entry(DEFAULTS_KEY, &false)?;
        }
        for (chord, value) in &self.entries {
            m.serialize_entry(chord, value)?;
        }
        m.end()
    }
}

impl JsonSchema for BindsConfig {
    fn schema_name() -> Cow<'static, str> {
        "BindsConfig".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::BindsConfig").into()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "description": "Key bindings in niri's spelling: \"Mod+Shift+Left\" = \"move-column-left\" (Mod, Win and Super all name the Windows key; the overlay prints Win). Entries are laid over the keymap's built-in defaults; \"none\" unbinds a chord.",
            "properties": {
                "defaults": {
                    "type": "boolean",
                    "default": true,
                    "description": "Start from the keymap's built-in table (true) or from an empty one (false)."
                }
            },
            "additionalProperties": {
                "oneOf": [
                    { "type": "string", "description": "An action, e.g. \"focus-column-left\", \"focus-workspace 3\", \"none\"." },
                    {
                        "type": "object",
                        "properties": {
                            "spawn": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "The program and its arguments." },
                            "admin": { "type": "boolean", "default": false, "description": "Run elevated." },
                            "help": { "type": "string", "description": "Overlay text instead of \"run <program>\"." }
                        },
                        "required": ["spawn"],
                        "additionalProperties": false
                    }
                ]
            }
        })
    }
}

/// One resolved bind: a chord and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// The chord.
    pub chord: Chord,
    /// The action.
    pub action: Action,
}

/// Wrapper for parsing a TOML document that holds only `[binds]`
/// (the built-in `binds.toml`).
#[derive(Debug, Deserialize)]
struct BindsDocument {
    binds: BindsConfig,
}

impl BindsConfig {
    /// Parse the built-in table. The defaults are compiled in and tested, so
    /// a failure here is a build defect; it is reported as an error rather
    /// than a panic so a daemon still starts (with no table).
    fn builtin() -> Result<Self, String> {
        toml::from_str::<BindsDocument>(DEFAULT_BINDS_TOML)
            .map(|d| d.binds)
            .map_err(|e| format!("built-in binds.toml does not parse: {e}"))
    }

    /// Resolve the table: the built-in defaults (unless `defaults = false`)
    /// with this table's entries laid over them, in order. A chord given
    /// twice keeps the later value; `"none"` removes the chord.
    ///
    /// # Errors
    ///
    /// Every problem, one line each, named by its key: `binds."Mod+Bogus":
    /// unknown key ...`.
    pub fn resolve(&self) -> Result<Vec<Bind>, Vec<String>> {
        let mut problems: Vec<String> = Vec::new();
        let mut binds: Vec<Bind> = Vec::new();
        if self.defaults {
            match Self::builtin() {
                Ok(builtin) => match builtin.resolve_entries_onto(Vec::new()) {
                    Ok(b) => binds = b,
                    Err(mut p) => problems.append(&mut p),
                },
                Err(e) => problems.push(e),
            }
        }
        match self.resolve_entries_onto(binds) {
            Ok(mut b) if problems.is_empty() => {
                b.sort_by_key(canonical_rank);
                Ok(b)
            }
            Ok(_) => Err(problems),
            Err(mut p) => {
                problems.append(&mut p);
                Err(problems)
            }
        }
    }

    /// Lay this table's entries over `binds`.
    fn resolve_entries_onto(&self, mut binds: Vec<Bind>) -> Result<Vec<Bind>, Vec<String>> {
        let mut problems: Vec<String> = Vec::new();
        let mut seen: Vec<(Chord, &str)> = Vec::new();
        for (text, value) in &self.entries {
            let chord = match Chord::parse(text) {
                Ok(c) => c,
                Err(e) => {
                    problems.push(format!("binds.\"{text}\": {e}"));
                    continue;
                }
            };
            if let Some((_, first)) = seen.iter().find(|(c, _)| *c == chord) {
                problems.push(format!(
                    "binds.\"{text}\": the same chord as \"{first}\" ({chord}); keep one"
                ));
                continue;
            }
            seen.push((chord.clone(), text.as_str()));
            if chord.is_reserved() {
                problems.push(format!(
                    "binds.\"{text}\": {chord} is reserved by the keymap (Win+Ctrl+T is FlowWM off/on, Win+Ctrl+Shift+T the tiling pause)"
                ));
                continue;
            }
            let action = match value {
                BindValue::Action(a) if a.trim().eq_ignore_ascii_case(NONE_ACTION) => {
                    binds.retain(|b| b.chord != chord);
                    continue;
                }
                BindValue::Action(a) => resolve_action(a),
                BindValue::Spawn { spawn, admin, help } => {
                    spawn_action(spawn, *admin, help.as_deref())
                }
            };
            match action {
                Ok(action) => {
                    binds.retain(|b| b.chord != chord);
                    binds.push(Bind { chord, action });
                }
                Err(e) => problems.push(format!("binds.\"{text}\": {e}")),
            }
        }
        if problems.is_empty() {
            Ok(binds)
        } else {
            Err(problems)
        }
    }

    /// Validate for `FlowConfig::validate`: every problem on one line.
    ///
    /// # Errors
    ///
    /// The joined problem list.
    pub fn validate(&self) -> Result<(), String> {
        self.resolve().map(|_| ()).map_err(|p| p.join("; "))
    }
}

/// Listing order of the groups: the fixed actions in table order, then
/// raw dispatches, programs and the keymap's own actions.
const GROUP_ORDER: &[&str] = &[
    "focus",
    "move",
    "focus-monitor",
    "move-to-monitor",
    "focus-workspace",
    "move-to-workspace",
    "close-window",
    "minimize-window",
    "center-column",
    "toggle-window-floating",
    "move-window-to-floating",
    "move-window-to-tiling",
    "switch-focus-between-floating-and-tiling",
    "focus-floating",
    "focus-tiling",
    "switch-preset-column-width",
    "switch-preset-column-width-back",
    "expand-column",
    "shrink-column",
    "fullscreen-window",
    "consume-or-expel",
    "consume-window-into-column",
    "expel-window-from-column",
];

/// The canonical position of a bind: its group, then its modifiers, then
/// its key. The table is a set (TOML tables are unordered and the parser
/// sorts them), so the listing and the overlay use this order instead.
fn canonical_rank(bind: &Bind) -> (usize, String, Vec<Modifier>, u32) {
    let group = bind.action.group.as_str();
    let (rank, tail) = match GROUP_ORDER.iter().position(|g| *g == group) {
        Some(i) => (i, String::new()),
        None => {
            let class = match bind.action.kind {
                ActionKind::Dispatch => 0,
                ActionKind::Launcher => 1,
                ActionKind::Spawn => 2,
                ActionKind::Help => 3,
                ActionKind::ToggleTiling => 4,
                ActionKind::Quit => 5,
            };
            (GROUP_ORDER.len() + class, group.to_owned())
        }
    };
    (rank, tail, bind.chord.mods.clone(), bind.chord.key.rank())
}

/// The built-in table resolved: what a `flow.toml` without `[binds]` gets.
#[must_use]
pub fn default_binds() -> Vec<Bind> {
    BindsConfig::default()
        .resolve()
        .expect("the built-in binds.toml resolves (tested)")
}

// ── The report the keymap consumes ────────────────────────────────────────

/// One line of the key-binding overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpRow {
    /// The chord or chords (`Win+Left/Down/Up/Right`, `Win+1..9, Win+0`).
    pub chords: String,
    /// What they do.
    pub help: String,
}

/// One bind as `flow query binds` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindReport {
    /// Canonical chord text.
    pub chord: String,
    /// AutoHotkey hotkey string.
    pub hotkey: String,
    /// Canonical action text.
    pub action: String,
    /// What the keymap does.
    pub kind: ActionKind,
    /// Dispatch arguments or the program and its arguments.
    pub argv: Vec<String>,
    /// Spawn elevated.
    pub admin: bool,
    /// When the bind is live.
    pub scope: Scope,
    /// One line for the overlay.
    pub help: String,
}

/// The whole answer to `flow query binds`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindsReport {
    /// `"defaults"` when `flow.toml` has no `[binds]`, else `"flow.toml"`.
    pub source: String,
    /// Every bind, table order.
    pub binds: Vec<BindReport>,
    /// The overlay rows for the table (collapsed).
    pub rows: Vec<HelpRow>,
    /// The chords the keymap hardwires, for the overlay.
    pub hardwired: Vec<HelpRow>,
    /// `[keymap] selection_buffer`: the keymap runs its selection buffer
    /// (`selection.ahk`). Absent from an older daemon's answer: off.
    #[serde(default)]
    pub selection_buffer: bool,
}

impl BindsReport {
    /// Carry `[keymap] selection_buffer`; when on, the overlay also lists
    /// the middle click.
    #[must_use]
    pub fn with_selection_buffer(mut self, on: bool) -> Self {
        self.selection_buffer = on;
        if on {
            self.hardwired.push(HelpRow {
                chords: "Middle click".into(),
                help: "paste the text last highlighted with the mouse (selection buffer; the clipboard is kept)".into(),
            });
        }
        self
    }
}

/// The chords the keymap hardwires (see `flow.ahk`); listed so the overlay
/// shows them next to the table.
#[must_use]
pub fn hardwired_rows() -> Vec<HelpRow> {
    vec![
        HelpRow {
            chords: "Win + left-button drag".into(),
            help: "move a window grabbed anywhere (a tile drops into the zone under the cursor, a float moves; a click without moving only activates it)".into(),
        },
        HelpRow {
            chords: "Win+Ctrl+T".into(),
            help: "FlowWM off (and on again, via flow-resident.ahk)".into(),
        },
        HelpRow {
            chords: "Win+Ctrl+Shift+T".into(),
            help: "pause toggle (off: Windows manages windows, chords fall through)".into(),
        },
    ]
}

/// Build the report for `binds`.
#[must_use]
pub fn report(binds: &[Bind], from_config: bool) -> BindsReport {
    BindsReport {
        source: if from_config { "flow.toml" } else { "defaults" }.to_owned(),
        binds: binds
            .iter()
            .map(|b| BindReport {
                chord: b.chord.canonical(),
                hotkey: b.chord.ahk(),
                action: b.action.text.clone(),
                kind: b.action.kind,
                argv: b.action.argv.clone(),
                admin: b.action.admin,
                scope: b.action.scope,
                help: b.action.help.clone(),
            })
            .collect(),
        rows: help_rows(binds),
        hardwired: hardwired_rows(),
        selection_buffer: false,
    }
}

/// Collapse the binds into overlay rows: consecutive binds of the same
/// group with the same modifiers and the same key family (arrows, wheel,
/// other) share a row (`Win+Left/Down/Up/Right`,
/// `Win+WheelUp/WheelDown/WheelLeft/WheelRight`, `Win+1..9, Win+0`,
/// `Alt+H/J/K/L`); everything else is one row each.
#[must_use]
pub fn help_rows(binds: &[Bind]) -> Vec<HelpRow> {
    let mut rows: Vec<HelpRow> = Vec::new();
    let mut i = 0;
    while i < binds.len() {
        let first = &binds[i];
        let prefix = first.chord.mod_prefix();
        let mut keys: Vec<&Key> = vec![first.chord.key()];
        let mut j = i + 1;
        while j < binds.len()
            && binds[j].action.group == first.action.group
            && binds[j].chord.mod_prefix() == prefix
            && binds[j].chord.key().family() == first.chord.key().family()
        {
            keys.push(binds[j].chord.key());
            j += 1;
        }
        let help = if keys.len() > 1 {
            group_help(&first.action)
        } else {
            first.action.help.clone()
        };
        rows.push(HelpRow {
            chords: collapse_keys(&prefix, &keys),
            help,
        });
        i = j;
    }
    rows
}

/// The help line for a collapsed row: the group's generic text where a
/// single bind's text would name one direction or workspace.
fn group_help(action: &Action) -> String {
    match action.group.as_str() {
        "focus-workspace" => "switch to that workspace".into(),
        "move-to-workspace" => "move the window to that workspace".into(),
        _ => action.help.clone(),
    }
}

/// `prefix` + the keys: `/`-joined for arrows, wheel keys and letters, `a..b` runs for
/// consecutive digits, otherwise each chord in full, comma-separated.
fn collapse_keys(prefix: &str, keys: &[&Key]) -> String {
    if keys.len() == 1 {
        return format!("{prefix}{}", keys[0].name());
    }
    let simple = keys.iter().all(|k| {
        matches!(k.name(), "Left" | "Right" | "Up" | "Down")
            || k.is_wheel()
            || k.name().len() == 1 && k.digit().is_none()
    });
    if simple {
        let names: Vec<&str> = keys.iter().map(|k| k.name()).collect();
        return format!("{prefix}{}", names.join("/"));
    }
    // Digit runs: 1..9 collapses, anything else stands alone.
    let mut pieces: Vec<String> = Vec::new();
    let mut k = 0;
    while k < keys.len() {
        let start = keys[k];
        let mut end = k;
        if let Some(mut d) = start.digit() {
            while end + 1 < keys.len() && keys[end + 1].digit() == Some(d + 1) {
                d += 1;
                end += 1;
            }
        }
        if end - k >= 2 {
            pieces.push(format!("{prefix}{}..{}", start.name(), keys[end].name()));
        } else {
            for key in &keys[k..=end] {
                pieces.push(format!("{prefix}{}", key.name()));
            }
        }
        k = end + 1;
    }
    pieces.join(", ")
}

/// The `--plain` listing the hotkey script parses: one record per line,
/// tab-separated. `bind` records carry the hotkey, scope, kind, admin flag,
/// chord, help and the argv (items joined by [`PLAIN_ARGV_SEP`]); `row` and
/// `hardwired` records carry the overlay text; `option` records carry the
/// keymap options (`option <name> 0|1`); `source` closes the list.
#[must_use]
pub fn plain_text(report: &BindsReport) -> String {
    let mut out = String::new();
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    for b in &report.binds {
        let kind = serde_json::to_value(b.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let scope = serde_json::to_value(b.scope)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let argv: Vec<String> = b.argv.iter().map(|a| clean(a)).collect();
        out.push_str(
            &[
                "bind",
                b.hotkey.as_str(),
                scope.as_str(),
                kind.as_str(),
                if b.admin { "1" } else { "0" },
                b.chord.as_str(),
                &clean(&b.help),
                &argv.join(&PLAIN_ARGV_SEP.to_string()),
            ]
            .join(&PLAIN_SEP.to_string()),
        );
        out.push('\n');
    }
    for (tag, rows) in [("row", &report.rows), ("hardwired", &report.hardwired)] {
        for r in rows {
            out.push_str(&[tag, &clean(&r.chords), &clean(&r.help)].join(&PLAIN_SEP.to_string()));
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "option{PLAIN_SEP}selection_buffer{PLAIN_SEP}{}\n",
        u8::from(report.selection_buffer)
    ));
    out.push_str(&format!("source{PLAIN_SEP}{}\n", report.source));
    out
}

// ── CLI aliases ───────────────────────────────────────────────────────────

/// `flow dispatch <niri action> [arg]` → `flow dispatch <our subcommand ...>`.
///
/// `args` is the full process argv. `None` when the line is not a
/// `dispatch` with a niri action name (so clap parses it as written).
#[must_use]
pub fn niri_alias(args: &[String]) -> Option<Vec<String>> {
    if args.len() < 3 || args[1] != "dispatch" {
        return None;
    }
    let line = args[2..].join(" ");
    if args[2].eq_ignore_ascii_case("dispatch") {
        return None;
    }
    let action = resolve_action(&line).ok()?;
    if action.kind != ActionKind::Dispatch {
        return None;
    }
    // Our own names resolve to themselves (close-window); leave those alone.
    if action.argv.first().is_some_and(|a| *a == args[2]) {
        return None;
    }
    let mut out = vec![args[0].clone(), "dispatch".to_owned()];
    out.extend(action.argv);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_binds(toml: &str) -> BindsConfig {
        toml::from_str::<BindsDocument>(toml).expect("parses").binds
    }

    // ── chords ──

    #[test]
    fn chord_spellings_normalise_to_one_canonical_form() {
        for text in [
            "Mod+Shift+Left",
            "shift+mod+left",
            "Super+Shift+Left",
            " Win + Shift + Left ",
        ] {
            let c = Chord::parse(text).expect(text);
            assert_eq!(c.canonical(), "Win+Shift+Left", "{text}");
            assert_eq!(c.ahk(), "#+Left", "{text}");
        }
        assert_eq!(Chord::parse("Mod+Ctrl+Alt+Shift+H").unwrap().ahk(), "#^!+h");
        assert_eq!(Chord::parse("Alt+h").unwrap().canonical(), "Alt+H");
        assert_eq!(Chord::parse("Mod+1").unwrap().ahk(), "#1");
        assert_eq!(Chord::parse("Mod+Shift+Slash").unwrap().ahk(), "#+vkBF");
        assert_eq!(Chord::parse("Mod+BracketLeft").unwrap().ahk(), "#[");
        assert_eq!(
            Chord::parse("Mod+WheelScrollDown").unwrap().ahk(),
            "#WheelDown"
        );
        assert_eq!(Chord::parse("Mod+Page_Up").unwrap().ahk(), "#PgUp");
        assert_eq!(Chord::parse("Win+F12").unwrap().canonical(), "Win+F12");
        assert_eq!(Chord::parse("Mod+vkbf").unwrap().ahk(), "#vkBF");
        assert_eq!(Chord::parse("Win+sc01a").unwrap().canonical(), "Win+sc01A");
        assert_eq!(Chord::parse("Print").unwrap().ahk(), "PrintScreen");
    }

    #[test]
    fn chord_errors_name_the_problem() {
        let err = |t: &str| Chord::parse(t).expect_err(t);
        assert!(err("Mod+Bogus").contains("unknown key \"Bogus\""));
        assert!(err("Hyper+Left").contains("unknown modifier \"Hyper\""));
        assert!(err("Mod+Mod+Left").contains("given twice"));
        assert!(err("Mod+").contains("missing key"));
        assert!(err("H").contains("no modifier"));
        assert!(err("Mod+F25").contains("unknown key"));
        assert!(err("Mod+MouseLeft").contains("mouse buttons"));
        assert!(err("Mod+LButton").contains("mouse buttons"));
    }

    #[test]
    fn wheel_keys_parse_in_both_spellings_and_never_bare() {
        for (text, canonical, ahk) in [
            ("Mod+WheelUp", "Win+WheelUp", "#WheelUp"),
            ("mod+wheeldown", "Win+WheelDown", "#WheelDown"),
            ("Shift+Mod+WheelLeft", "Win+Shift+WheelLeft", "#+WheelLeft"),
            ("Alt+Win+WheelRight", "Win+Alt+WheelRight", "#!WheelRight"),
            ("Mod+Ctrl+WheelScrollUp", "Win+Ctrl+WheelUp", "#^WheelUp"),
            ("Mod+WheelScrollRight", "Win+WheelRight", "#WheelRight"),
        ] {
            let c = Chord::parse(text).expect(text);
            assert_eq!(c.canonical(), canonical, "{text}");
            assert_eq!(c.ahk(), ahk, "{text}");
            assert!(c.key().is_wheel(), "{text}");
        }
        assert!(!Chord::parse("Mod+Left").unwrap().key().is_wheel());
        // niri's spelling and ours are one chord.
        assert_eq!(
            Chord::parse("Mod+WheelScrollDown").unwrap(),
            Chord::parse("Mod+WheelDown").unwrap()
        );
        // The plain wheel is never bound: apps keep scrolling.
        for bare in [
            "WheelUp",
            "WheelDown",
            "WheelLeft",
            "WheelRight",
            "WheelScrollUp",
        ] {
            assert!(
                Chord::parse(bare).expect_err(bare).contains("no modifier"),
                "{bare}"
            );
        }
        let err = Chord::parse("Mod+WheelSideways").expect_err("bogus wheel");
        assert!(err.contains("unknown key \"WheelSideways\""), "{err}");
        assert!(
            err.contains("WheelUp, WheelDown, WheelLeft, WheelRight"),
            "{err}"
        );
    }

    #[test]
    fn two_spellings_of_one_wheel_chord_are_refused() {
        let p = parse_binds(
            "[binds]
\"Mod+WheelUp\" = \"focus-column-left\"
\"Mod+WheelScrollUp\" = \"focus-column-right\"
",
        )
        .resolve()
        .expect_err("same chord twice");
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("the same chord as"), "{p:?}");
    }

    #[test]
    fn a_user_table_remaps_and_clears_wheel_chords() {
        let binds = parse_binds(
            "[binds]
\"Mod+WheelDown\" = \"focus-monitor-right\"
\"Mod+WheelUp\" = \"none\"
",
        )
        .resolve()
        .unwrap();
        let find = |c: &str| binds.iter().find(|b| b.chord == Chord::parse(c).unwrap());
        assert_eq!(
            find("Mod+WheelDown").unwrap().action.argv,
            vec!["focus-monitor", "right"]
        );
        assert!(find("Mod+WheelUp").is_none());
        assert_eq!(
            find("Mod+WheelLeft").unwrap().action.argv,
            vec!["focus", "left"]
        );
        assert_eq!(binds.len(), default_binds().len() - 1);
    }

    #[test]
    fn the_on_off_and_pause_chords_are_reserved() {
        assert!(Chord::parse("Mod+Ctrl+T").unwrap().is_reserved());
        assert!(Chord::parse("Ctrl+Shift+Mod+t").unwrap().is_reserved());
        assert!(!Chord::parse("Mod+T").unwrap().is_reserved());
        assert!(!Chord::parse("Mod+Alt+Ctrl+T").unwrap().is_reserved());
        assert!(!Chord::parse("Ctrl+Shift+T").unwrap().is_reserved());
    }

    // ── actions ──

    #[test]
    fn fixed_actions_map_onto_dispatch_argv() {
        let a = resolve_action("focus-column-left").unwrap();
        assert_eq!(a.argv, vec!["focus", "left"]);
        assert_eq!(a.kind, ActionKind::Dispatch);
        assert_eq!(a.scope, Scope::Tiling);
        assert_eq!(a.group, "focus");
        assert_eq!(
            resolve_action("Move-Column-Right").unwrap().argv,
            vec!["move-window", "right"]
        );
        assert_eq!(
            resolve_action("toggle-window-floating").unwrap().argv,
            vec!["set-window", "cycle"]
        );
        assert_eq!(
            resolve_action("switch-preset-column-width-back")
                .unwrap()
                .argv,
            vec!["width", "prev"]
        );
        assert_eq!(
            resolve_action("fullscreen-window").unwrap().argv,
            vec!["set-window", "fullscreen"]
        );
        assert_eq!(
            resolve_action("consume-or-expel-window-left").unwrap().argv,
            vec!["consume-or-expel", "left"]
        );
        assert_eq!(
            resolve_action("move-window-to-monitor-up").unwrap().argv,
            vec!["move-to-monitor", "up"]
        );
    }

    #[test]
    fn parametric_actions_take_their_argument() {
        let ws = resolve_action("focus-workspace 3").unwrap();
        assert_eq!(ws.argv, vec!["switch-workspace", "3"]);
        assert_eq!(ws.text, "focus-workspace 3");
        assert_eq!(ws.help, "switch to workspace 3");
        let mv = resolve_action("move-window-to-workspace 10").unwrap();
        assert_eq!(mv.argv, vec!["move-to-workspace", "10"]);
        assert_eq!(mv.text, "move-column-to-workspace 10");
        assert_eq!(
            resolve_action("set-column-width \"+10%\"").unwrap().argv,
            vec!["expand-column"]
        );
        assert_eq!(
            resolve_action("set-column-width -10%").unwrap().argv,
            vec!["shrink-column"]
        );
        let raw = resolve_action("dispatch width 1/2").unwrap();
        assert_eq!(raw.argv, vec!["width", "1/2"]);
        assert_eq!(raw.help, "flow dispatch width 1/2");
    }

    #[test]
    fn builtin_actions_are_always_scoped() {
        for (name, kind) in [
            ("launcher", ActionKind::Launcher),
            ("show-hotkey-overlay", ActionKind::Help),
            ("quit", ActionKind::Quit),
            ("toggle-tiling", ActionKind::ToggleTiling),
        ] {
            let a = resolve_action(name).unwrap();
            assert_eq!(a.kind, kind, "{name}");
            assert_eq!(a.scope, Scope::Always, "{name}");
            assert!(a.argv.is_empty());
        }
    }

    #[test]
    fn action_errors_name_the_accepted_forms() {
        let err = |t: &str| resolve_action(t).expect_err(t);
        assert!(err("fly-away").contains("unknown action \"fly-away\""));
        assert!(err("fly-away").contains("focus-column-left"));
        assert!(err("focus-workspace").contains("needs a workspace number"));
        assert!(err("focus-workspace 0").contains("needs a workspace number"));
        assert!(err("focus-workspace x").contains("needs a workspace number"));
        assert!(err("close-window now").contains("takes no argument"));
        assert!(err("set-column-width 10").contains("+N%"));
        assert!(err("dispatch").contains("needs the flow dispatch arguments"));
    }

    #[test]
    fn spawn_values_parse_and_describe_themselves() {
        let cfg = parse_binds(
            "[binds]\n\"Mod+T\" = { spawn = [\"alacritty\"] }\n\"Mod+Shift+T\" = { spawn = [\"wt\", \"-p\", \"PowerShell\"], admin = true, help = \"admin shell\" }\n",
        );
        let cfg = BindsConfig {
            defaults: false,
            ..cfg
        };
        let binds = cfg.resolve().unwrap();
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].action.kind, ActionKind::Spawn);
        assert_eq!(binds[0].action.argv, vec!["alacritty"]);
        assert!(!binds[0].action.admin);
        assert_eq!(binds[0].action.help, "run alacritty");
        assert_eq!(binds[0].action.scope, Scope::Always);
        assert!(binds[1].action.admin);
        assert_eq!(binds[1].action.help, "admin shell");
        assert_eq!(binds[1].action.text, "spawn wt -p PowerShell (admin)");
    }

    #[test]
    fn spawn_table_rejects_unknown_fields_and_an_empty_program() {
        let err = toml::from_str::<BindsDocument>("[binds]\n\"Mod+T\" = { run = [\"x\"] }\n")
            .expect_err("unknown field");
        assert!(err.to_string().contains("unknown field `run`"), "{err}");
        let err = toml::from_str::<BindsDocument>("[binds]\n\"Mod+T\" = { admin = true }\n")
            .expect_err("missing spawn");
        assert!(err.to_string().contains("missing field `spawn`"), "{err}");
        let cfg = BindsConfig {
            defaults: false,
            entries: vec![(
                "Mod+T".into(),
                BindValue::Spawn {
                    spawn: vec![],
                    admin: false,
                    help: None,
                },
            )],
        };
        let p = cfg.resolve().expect_err("empty spawn");
        assert!(
            p[0].contains("binds.\"Mod+T\": spawn needs at least the program name"),
            "{p:?}"
        );
    }

    // ── the table ──

    #[test]
    fn the_builtin_table_resolves_to_todays_chords() {
        let binds = default_binds();
        let find = |chord: &str| {
            binds
                .iter()
                .find(|b| b.chord == Chord::parse(chord).unwrap())
                .unwrap_or_else(|| panic!("{chord} bound"))
        };
        assert_eq!(find("Mod+Left").action.argv, vec!["focus", "left"]);
        assert_eq!(
            find("Mod+Shift+Right").action.argv,
            vec!["move-window", "right"]
        );
        assert_eq!(find("Alt+Shift+J").action.argv, vec!["move-window", "down"]);
        assert_eq!(find("Mod+Ctrl+Up").action.argv, vec!["focus-monitor", "up"]);
        assert_eq!(
            find("Mod+Alt+Right").action.argv,
            vec!["move-to-monitor", "right"]
        );
        assert_eq!(find("Mod+0").action.argv, vec!["switch-workspace", "10"]);
        assert_eq!(
            find("Mod+Ctrl+9").action.argv,
            vec!["move-to-workspace", "9"]
        );
        assert_eq!(find("Mod+Q").action.argv, vec!["close-window"]);
        assert_eq!(find("Mod+M").action.argv, vec!["minimize-window"]);
        assert_eq!(find("Mod+C").action.argv, vec!["center"]);
        assert_eq!(find("Mod+V").action.argv, vec!["set-window", "cycle"]);
        assert_eq!(
            find("Mod+Shift+V").action.argv,
            vec!["switch-focus-between-floating-and-tiling"]
        );
        assert_eq!(find("Mod+R").action.argv, vec!["width", "next"]);
        assert_eq!(
            find("Mod+Shift+B").action.argv,
            vec!["set-layout", "toggle"]
        );
        assert_eq!(find("Mod+F").action.argv, vec!["set-window", "fullscreen"]);
        assert_eq!(
            find("Mod+BracketRight").action.argv,
            vec!["consume-or-expel", "right"]
        );
        assert_eq!(find("Mod+Space").action.kind, ActionKind::Launcher);
        assert_eq!(find("Mod+D").action.kind, ActionKind::Launcher);
        assert_eq!(find("Mod+T").action.argv, vec!["alacritty"]);
        assert!(find("Mod+Shift+T").action.admin);
        assert_eq!(find("Mod+Alt+T").action.argv, vec!["wt"]);
        assert!(find("Mod+Alt+Shift+T").action.admin);
        assert_eq!(find("Mod+Shift+Slash").action.kind, ActionKind::Help);
        assert_eq!(find("Mod+Shift+Slash").chord.ahk(), "#+vkBF");
        assert_eq!(find("Mod+Shift+E").action.kind, ActionKind::Quit);
        // The wheel mirrors the Left / Right arrows (tilt = the same as up / down).
        for (mods, left, right) in [
            ("Mod+", vec!["focus", "left"], vec!["focus", "right"]),
            (
                "Mod+Shift+",
                vec!["move-window", "left"],
                vec!["move-window", "right"],
            ),
            (
                "Mod+Ctrl+",
                vec!["focus-monitor", "left"],
                vec!["focus-monitor", "right"],
            ),
            (
                "Mod+Alt+",
                vec!["move-to-monitor", "left"],
                vec!["move-to-monitor", "right"],
            ),
        ] {
            for key in ["WheelUp", "WheelLeft"] {
                let b = find(&format!("{mods}{key}"));
                assert_eq!(b.action.argv, left, "{mods}{key}");
                assert_eq!(b.action.argv, find(&format!("{mods}Left")).action.argv);
            }
            for key in ["WheelDown", "WheelRight"] {
                let b = find(&format!("{mods}{key}"));
                assert_eq!(b.action.argv, right, "{mods}{key}");
                assert_eq!(b.action.argv, find(&format!("{mods}Right")).action.argv);
            }
        }
        // Mod+Minus / Mod+Equal left the defaults in stage 20.
        assert!(
            binds
                .iter()
                .all(|b| !matches!(b.chord.canonical().as_str(), "Mod+Minus" | "Mod+Equal"))
        );
        // focus 8, move 8, monitors 8, wheel 16, workspaces 20, q alt+m c v shift+v r f shift+b [ ] = 10, programs 6, help + quit.
        assert_eq!(binds.len(), 8 + 8 + 8 + 16 + 20 + 10 + 6 + 2);
    }

    #[test]
    fn entries_layer_over_the_defaults_in_canonical_order() {
        let cfg = parse_binds(
            "[binds]\n\"Mod+Shift+R\" = \"switch-preset-column-width-back\"\n\"Mod+Q\" = \"none\"\n\"shift+mod+left\" = \"focus-column-right\"\n\"Mod+Z\" = \"close-window\"\n",
        );
        assert_eq!(cfg.entries.len(), 4);
        let binds = cfg.resolve().unwrap();
        let defaults = default_binds();
        // One unbound, two added, one replaced.
        assert_eq!(binds.len(), defaults.len() + 1);
        assert!(binds.iter().all(|b| b.chord.canonical() != "Win+Q"));
        let moved = binds
            .iter()
            .find(|b| b.chord.canonical() == "Win+Shift+Left")
            .unwrap();
        assert_eq!(moved.action.argv, vec!["focus", "right"]);
        // The listing is canonical whatever the table's order: the
        // replaced chord sits with its new group (focus, after the arrows),
        // the width chords are adjacent, Mod+Z is with close-window.
        let chords: Vec<String> = binds.iter().map(|b| b.chord.canonical()).collect();
        let at = |c: &str| {
            chords
                .iter()
                .position(|x| x == c)
                .unwrap_or_else(|| panic!("{c}"))
        };
        assert_eq!(at("Win+Left"), 0);
        assert!(at("Win+Shift+Left") < at("Alt+H"), "{chords:?}");
        assert_eq!(at("Win+Shift+R"), at("Win+R") + 1, "{chords:?}");
        assert!(at("Win+Z") < at("Win+C"), "{chords:?}");
    }

    #[test]
    fn two_spellings_of_one_chord_are_refused() {
        let cfg = parse_binds(
            "[binds]\n\"Mod+Shift+Left\" = \"focus-column-left\"\n\"shift+mod+left\" = \"focus-column-right\"\n",
        );
        let p = cfg.resolve().expect_err("duplicate");
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].contains("the same chord as \"Mod+Shift+Left\" (Win+Shift+Left)"),
            "{}",
            p[0]
        );
    }

    #[test]
    fn mod_win_and_super_are_one_modifier() {
        let cfg = parse_binds(
            "[binds]\ndefaults = false\n\"Win+Q\" = \"close-window\"\n\"Super+Shift+Left\" = \"move-column-left\"\n\"mod+c\" = \"center-column\"\n",
        );
        let binds = cfg.resolve().unwrap();
        let hotkeys: Vec<String> = binds.iter().map(|b| b.chord.ahk()).collect();
        assert!(hotkeys.contains(&"#q".to_owned()), "{hotkeys:?}");
        assert!(hotkeys.contains(&"#+Left".to_owned()), "{hotkeys:?}");
        assert!(hotkeys.contains(&"#c".to_owned()), "{hotkeys:?}");
        // A Win spelling replaces the default bound under the Mod spelling.
        let over = parse_binds("[binds]\n\"Win+Left\" = \"focus-column-right\"\n");
        let binds = over.resolve().unwrap();
        assert_eq!(binds.len(), default_binds().len());
        let left: Vec<&Bind> = binds.iter().filter(|b| b.chord.ahk() == "#Left").collect();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].action.argv, vec!["focus", "right"]);
    }

    #[test]
    fn mod_and_win_spellings_in_one_table_are_a_duplicate() {
        for (a, b) in [("Mod+Left", "Win+Left"), ("Super+Left", "Mod+Left")] {
            let cfg = parse_binds(&format!(
                "[binds]\n\"{a}\" = \"focus-column-left\"\n\"{b}\" = \"focus-column-right\"\n"
            ));
            let p = cfg.resolve().expect_err("duplicate");
            assert_eq!(p.len(), 1, "{p:?}");
            // Both of the user's spellings are quoted as written.
            assert!(p[0].contains("the same chord as"), "{}", p[0]);
            assert!(
                p[0].contains(&format!("\"{a}\"")) && p[0].contains(&format!("\"{b}\"")),
                "{}",
                p[0]
            );
        }
        let err = Chord::parse("Mod+Win+Left").expect_err("twice");
        assert!(err.contains("\"Win\" given twice"), "{err}");
    }

    #[test]
    fn overlay_and_hardwired_rows_name_the_windows_key() {
        let r = report(&default_binds(), false);
        assert!(
            r.rows
                .iter()
                .chain(&r.hardwired)
                .all(|row| !row.chords.contains("Mod")),
            "{:?}",
            r.rows
        );
        let hard: Vec<&str> = r.hardwired.iter().map(|h| h.chords.as_str()).collect();
        assert_eq!(
            hard,
            ["Win + left-button drag", "Win+Ctrl+T", "Win+Ctrl+Shift+T"]
        );
        assert!(r.binds.iter().all(|b| !b.chord.contains("Mod")));
        // The hotkey strings the keymap registers do not change.
        assert_eq!(r.binds[0].hotkey, "#Left");
        let problem = parse_binds("[binds]\n\"Mod+Ctrl+T\" = \"close-window\"\n")
            .resolve()
            .expect_err("reserved");
        assert!(
            problem[0].starts_with("binds.\"Mod+Ctrl+T\": Win+Ctrl+T is reserved"),
            "{}",
            problem[0]
        );
    }

    #[test]
    fn defaults_false_starts_from_nothing() {
        let cfg = parse_binds("[binds]\ndefaults = false\n\"Mod+Q\" = \"close-window\"\n");
        assert!(!cfg.defaults);
        let binds = cfg.resolve().unwrap();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].chord.canonical(), "Win+Q");
        // And an absent section is the defaults.
        assert_eq!(
            BindsConfig::default().resolve().unwrap().len(),
            default_binds().len()
        );
    }

    #[test]
    fn every_problem_is_reported_and_named_by_its_key() {
        let cfg = parse_binds(
            "[binds]\n\"Mod+Bogus\" = \"close-window\"\n\"Mod+Ctrl+T\" = \"close-window\"\n\"Mod+Q\" = \"fly\"\n\"Mod+W\" = \"close-window\"\n",
        );
        let problems = cfg.resolve().expect_err("three problems");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].starts_with("binds.\"Mod+Bogus\": unknown key"),
            "{}",
            problems[0]
        );
        assert!(
            problems[1].contains("reserved by the keymap"),
            "{}",
            problems[1]
        );
        assert!(
            problems[2].starts_with("binds.\"Mod+Q\": unknown action \"fly\""),
            "{}",
            problems[2]
        );
        let joined = cfg.validate().expect_err("joined");
        assert!(joined.contains("; "));
    }

    #[test]
    fn serialize_round_trips_through_toml() {
        let cfg = parse_binds(
            "[binds]\ndefaults = false\n\"Mod+Q\" = \"close-window\"\n\"Mod+T\" = { spawn = [\"wt\"], admin = true }\n",
        );
        let text = toml::to_string(&BindsDocument2 { binds: cfg.clone() }).unwrap();
        let back = parse_binds(&text);
        assert_eq!(back, cfg);
        #[derive(Serialize)]
        struct BindsDocument2 {
            binds: BindsConfig,
        }
    }

    // ── report ──

    #[test]
    fn help_rows_collapse_arrows_letters_and_digit_runs() {
        let rows = help_rows(&default_binds());
        let chords: Vec<&str> = rows.iter().map(|r| r.chords.as_str()).collect();
        assert_eq!(
            &chords[..14],
            &[
                "Win+Left/Down/Up/Right",
                "Win+WheelUp/WheelDown/WheelLeft/WheelRight",
                "Alt+H/J/K/L",
                "Win+Shift+Left/Down/Up/Right",
                "Win+Shift+WheelUp/WheelDown/WheelLeft/WheelRight",
                "Alt+Shift+H/J/K/L",
                "Win+Ctrl+Left/Down/Up/Right",
                "Win+Ctrl+WheelUp/WheelDown/WheelLeft/WheelRight",
                "Win+Alt+Left/Down/Up/Right",
                "Win+Alt+WheelUp/WheelDown/WheelLeft/WheelRight",
                "Win+1..9, Win+0",
                "Win+Ctrl+1..9, Win+Ctrl+0",
                "Win+Q",
                "Win+M",
            ]
        );
        assert_eq!(rows[0].help, "focus the next window on this screen");
        assert_eq!(rows[1].help, "focus the next window on this screen");
        assert_eq!(rows[9].help, "move the window to the other screen");
        assert_eq!(rows[10].help, "switch to that workspace");
        assert!(
            chords.contains(&"Win+BracketLeft, Win+BracketRight"),
            "{chords:?}"
        );
        assert!(chords.contains(&"Win+Space, Win+D"), "{chords:?}");
        assert!(chords.contains(&"Win+Shift+Slash"));
        assert_eq!(*chords.last().unwrap(), "Win+Shift+E");
        // 2 focus + 2 move + monitors 2 + wheel 4 + workspaces 2 + q alt+m c v shift+v r f shift+b + consume + launcher + 4 spawn + help + quit
        assert_eq!(rows.len(), 2 + 2 + 2 + 4 + 2 + 8 + 1 + 1 + 4 + 1 + 1);
    }

    #[test]
    fn report_and_plain_listing_carry_every_field() {
        let binds = default_binds();
        let r = report(&binds, false);
        assert_eq!(r.source, "defaults");
        assert_eq!(r.binds.len(), binds.len());
        assert_eq!(r.hardwired.len(), 3);
        let first = &r.binds[0];
        assert_eq!(first.chord, "Win+Left");
        assert_eq!(first.hotkey, "#Left");
        assert_eq!(first.kind, ActionKind::Dispatch);
        assert_eq!(first.scope, Scope::Tiling);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["binds"][0]["kind"], "dispatch");
        assert_eq!(json["binds"][0]["scope"], "tiling");
        let back: BindsReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
        let plain = plain_text(&r);
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(
            lines[0],
            "bind\t#Left\ttiling\tdispatch\t0\tWin+Left\tfocus the next window on this screen\tfocus\u{1f}left"
        );
        assert!(lines.contains(
            &"bind\t#+t\talways\tspawn\t1\tWin+Shift+T\trun alacritty as admin\talacritty"
        ));
        assert!(lines.contains(&"bind\t#Space\talways\tlauncher\t0\tWin+Space\tapplication launcher (apps; window - x and file - x search those instead)\t"));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("row\tWin+1..9, Win+0\t"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("hardwired\tWin+Ctrl+T\t"))
        );
        assert!(lines.contains(&"option\tselection_buffer\t0"));
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("hardwired\tMiddle click\t"))
        );
        assert_eq!(*lines.last().unwrap(), "source\tdefaults");
        assert_eq!(report(&binds, true).source, "flow.toml");

        // [keymap] selection_buffer on: the option flips and the overlay
        // lists the middle click; an older daemon's JSON (no field) is off.
        let on = report(&binds, false).with_selection_buffer(true);
        assert!(on.selection_buffer);
        assert_eq!(on.hardwired.len(), 4);
        let plain_on = plain_text(&on);
        assert!(plain_on.lines().any(|l| l == "option\tselection_buffer\t1"));
        assert!(
            plain_on
                .lines()
                .any(|l| l.starts_with("hardwired\tMiddle click\t"))
        );
        let mut old = serde_json::to_value(&r).unwrap();
        old.as_object_mut().unwrap().remove("selection_buffer");
        let old: BindsReport = serde_json::from_value(old).unwrap();
        assert!(!old.selection_buffer);
    }

    // ── the keymap's baked fallback ──

    /// `flow.ahk` carries the default listing for a daemon that cannot
    /// answer `query binds`; it must be the listing the daemon would give.
    #[test]
    fn the_keymaps_baked_fallback_matches_the_defaults() {
        let ahk = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/keymaps/win-niri/flow.ahk"
        ))
        .expect("flow.ahk")
        .replace("\r\n", "\n");
        let start = ahk.find("; BEGIN DEFAULT BINDS").expect("begin marker");
        let end = ahk.find("; END DEFAULT BINDS").expect("end marker");
        let block = &ahk[start..end];
        let open = block.find("\n(\n").expect("continuation opens") + 3;
        let close = block.rfind("\n)\"").expect("continuation closes");
        let baked = block[open..close].replace("<US>", "\u{1f}");
        let expected = plain_text(&report(&default_binds(), false));
        assert_eq!(
            baked.trim_end(),
            expected.trim_end(),
            "the baked block in keymaps/win-niri/flow.ahk is out of date; paste this between the markers:\n(\n{}\n)\"",
            expected.trim_end().replace('\u{1f}', "<US>")
        );
    }

    // ── CLI aliases ──

    #[test]
    fn niri_names_alias_our_dispatch_subcommands() {
        let args = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            niri_alias(&args("flow dispatch focus-column-left")),
            Some(args("flow dispatch focus left"))
        );
        assert_eq!(
            niri_alias(&args("flow dispatch focus-workspace 3")),
            Some(args("flow dispatch switch-workspace 3"))
        );
        assert_eq!(
            niri_alias(&args("flow dispatch switch-preset-column-width")),
            Some(args("flow dispatch width next"))
        );
        // Our own names, unknown words and non-dispatch actions pass through.
        assert_eq!(niri_alias(&args("flow dispatch close-window")), None);
        assert_eq!(niri_alias(&args("flow dispatch focus left")), None);
        assert_eq!(niri_alias(&args("flow dispatch launcher")), None);
        assert_eq!(niri_alias(&args("flow dispatch dispatch width 1/2")), None);
        assert_eq!(niri_alias(&args("flow ping")), None);
        assert_eq!(niri_alias(&args("flow dispatch")), None);
    }
}
