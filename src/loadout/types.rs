//! Serde data model for the versioned loadout file.
//!
//! These types describe the JSON schema that `flowd` reads and writes when
//! saving/restoring a workspace arrangement across restarts. The daemon
//! owns the actual file I/O; this module is a pure data layer.

use serde::{Deserialize, Serialize};

/// Top-level loadout file envelope.
///
/// Serialized as a JSON object with a `version` field for forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadoutFile {
    /// File-format version number. Must equal [`LoadoutFile::CURRENT_VERSION`];
    /// any other value is rejected at load time rather than migrated.
    pub version: u32,
    /// Per-workspace snapshots in the order they were saved.
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// Per-monitor state (the column width preset). Absent in files written
    /// before presets existed, which maps to an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<MonitorSnapshot>,
}

impl LoadoutFile {
    /// Current loadout file-format version.
    ///
    /// The writer always emits this value; the loader rejects any file whose
    /// `version` differs (a legacy pre-`HWND` file cannot be migrated because
    /// `HWND` cannot be synthesized, so it is skipped with a logged reason
    /// rather than silently misread). Bump this on any breaking schema change.
    pub const CURRENT_VERSION: u32 = 3;
}

/// Per-monitor state that is not tied to a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    /// Index of the monitor (primary = 0).
    pub index: u32,
    /// Label of the column width preset the monitor was on (`"1/2"`), or
    /// `None` when no presets were configured at save time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_preset: Option<String>,
}

/// Snapshot of a single workspace's tiling and floating state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// Index of the monitor this workspace belongs to (primary = 0).
    /// Absent in files written by single-monitor builds, which maps to 0.
    #[serde(default)]
    pub monitor: u32,
    /// 0-based workspace index.
    pub workspace_id: u32,
    /// Whether this workspace was active (visible) when saved.
    pub active: bool,
    /// Tiling area snapshot (scrolling columns + rows).
    pub scrolling: ScrollingSnapshot,
    /// Floating window entries that were on this workspace.
    pub floating: Vec<FloatingEntry>,
}

/// Snapshot of the tiling canvas within a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollingSnapshot {
    /// Horizontal pixel offset the viewport was scrolled to.
    pub viewport_offset: i32,
    /// The window that had keyboard focus, if any.
    pub focus: Option<WindowRef>,
    /// Ordered list of tile columns.
    pub columns: Vec<ColumnSnapshot>,
}

/// Snapshot of a single tile column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSnapshot {
    /// Pixel width of this column.
    pub width_px: u32,
    /// Ordered list of tile rows within this column.
    pub rows: Vec<RowSnapshot>,
}

/// Snapshot of a single tiled window row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowSnapshot {
    /// Window identity for match-based restoration.
    pub window: WindowRef,
    /// Pixel height allocated to this row.
    pub height_px: i32,
}

/// A floating window entry with its position and size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingEntry {
    /// Window identity for match-based restoration.
    pub window: WindowRef,
    /// Screen rectangle where the window was placed.
    pub rect: RectJson,
}

/// Identity for matching a window across daemon restarts.
///
/// The matcher keys **only** on [`hwnd`](Self::hwnd): a Win32 window handle
/// is stable and unique across a daemon restart — the target applications
/// keep running independently of the daemon — so each saved slot resolves to
/// at most one live window with no scoring or tie-breaking. `exe` is
/// **diagnostic-only**: never consulted by the matcher, but it makes a
/// failed restore self-describing (a window's identity is only known at
/// save time, so it must be persisted to name the missing window at load
/// time).
///
/// `title` used to be written for the same reason. It is personal
/// (document names, chat partners, URLs) and the file sits in the config
/// directory for as long as the user keeps it, so the daemon **no longer
/// writes it**; it is still read from files that have it, and shown in the
/// abort message when present.
///
/// See the design-decision record (`docs/src/dev-guide/design-decisions.md`,
/// "Loadout Window Identity: HWND-Exact (Not Fuzzy Matching)").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRef {
    /// Underlying `HWND` handle value (`HWND as isize`), mirroring the
    /// `WindowId`/HWND-as-`isize` convention used in the registry.
    pub hwnd: isize,
    /// Executable basename (e.g. `"firefox.exe"`) — diagnostic only.
    pub exe: String,
    /// Window title at the time of snapshot — diagnostic only, read from
    /// older files, never written any more (see the type docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Plain pixel rectangle for floating window placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RectJson {
    /// Left-edge pixel coordinate.
    pub x: i32,
    /// Top-edge pixel coordinate.
    pub y: i32,
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Positive: serde round-trip ──────────────────────────────────

    /// `CURRENT_VERSION` is exactly 3 (the HWND-identity schema, no timestamp).
    //
    // Locks the version the writer emits and the loader accepts; a schema
    // change must deliberately bump this.
    #[test]
    fn current_version_is_three() {
        assert_eq!(LoadoutFile::CURRENT_VERSION, 3);
    }

    /// A `WindowRef` without a title (what the daemon writes now) omits the
    /// key; an older file that has one still parses with it.
    #[test]
    fn title_is_optional_and_not_written_when_absent() {
        let bare = WindowRef {
            hwnd: 0x10,
            exe: "code.exe".into(),
            title: None,
        };
        let json = serde_json::to_string(&bare).expect("serialize");
        assert_eq!(json, r#"{"hwnd":16,"exe":"code.exe"}"#);
        let back: WindowRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.title, None);
        let old: WindowRef =
            serde_json::from_str(r#"{"hwnd":16,"exe":"code.exe","title":"secret.docx"}"#)
                .expect("old file with a title still loads");
        assert_eq!(old.title.as_deref(), Some("secret.docx"));
    }

    /// Round-trip a complete [`LoadoutFile`] through JSON.
    // Positive: full loadout serializes and deserializes correctly with the
    // HWND-identity `{hwnd, exe[, title]}` shape (no `class`).
    #[test]
    fn round_trip_full_loadout() {
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![MonitorSnapshot {
                index: 0,
                width_preset: Some("1/2".into()),
            }],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 0,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 128,
                    focus: Some(WindowRef {
                        hwnd: 0x00_0A_0B_0C,
                        exe: "code.exe".into(),
                        title: Some("main.rs — flow-wm".into()),
                    }),
                    columns: vec![ColumnSnapshot {
                        width_px: 960,
                        rows: vec![RowSnapshot {
                            window: WindowRef {
                                hwnd: 0x00_0A_0B_0C,
                                exe: "code.exe".into(),
                                title: Some("main.rs — flow-wm".into()),
                            },
                            height_px: 600,
                        }],
                    }],
                },
                floating: vec![FloatingEntry {
                    window: WindowRef {
                        hwnd: 0x00_0D_0E_0F,
                        exe: "slack.exe".into(),
                        title: Some("Slack".into()),
                    },
                    rect: RectJson {
                        x: 100,
                        y: 100,
                        w: 400,
                        h: 300,
                    },
                }],
            }],
        };

        let json = serde_json::to_string(&file).expect("serialize");
        let restored: LoadoutFile = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.version, LoadoutFile::CURRENT_VERSION);
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.monitors.len(), 1);
        assert_eq!(restored.monitors[0].width_preset.as_deref(), Some("1/2"));
        let ws = &restored.workspaces[0];
        assert_eq!(ws.workspace_id, 0);
        assert!(ws.active);
        assert_eq!(ws.scrolling.viewport_offset, 128);
        assert!(ws.scrolling.focus.is_some());
        assert_eq!(ws.scrolling.columns.len(), 1);
        assert_eq!(ws.scrolling.columns[0].width_px, 960);
        assert_eq!(ws.floating.len(), 1);
        assert_eq!(ws.floating[0].rect.x, 100);
        // ── HWND is the round-tripped identity; `class` is absent ──
        let row_ref = &ws.scrolling.columns[0].rows[0].window;
        assert_eq!(row_ref.hwnd, 0x00_0A_0B_0C);
        assert_eq!(row_ref.exe, "code.exe");
        assert_eq!(row_ref.title.as_deref(), Some("main.rs — flow-wm"));
        // The serialized shape must carry `hwnd` and never `class`.
        assert!(json.contains("\"hwnd\""));
        assert!(!json.contains("class"));
    }

    /// A file written by a single-monitor build has no `monitor` field: it
    /// deserialises to the primary (0) instead of being rejected.
    #[test]
    fn workspace_monitor_field_defaults_to_zero_when_absent() {
        let json = r#"{
            "workspace_id": 3,
            "active": false,
            "scrolling": { "viewport_offset": 0, "focus": null, "columns": [] },
            "floating": []
        }"#;
        let ws: WorkspaceSnapshot = serde_json::from_str(json).expect("legacy snapshot loads");
        assert_eq!(ws.monitor, 0);
        assert_eq!(ws.workspace_id, 3);
    }

    /// A non-zero monitor index survives save → load unchanged.
    #[test]
    fn workspace_monitor_field_round_trips() {
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 2,
                workspace_id: 1,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: None,
                    columns: vec![],
                },
                floating: vec![],
            }],
        };
        let json = serde_json::to_string(&file).expect("serialize");
        assert!(json.contains("\"monitor\":2"), "{json}");
        let restored: LoadoutFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.workspaces[0].monitor, 2);
        assert_eq!(restored.workspaces[0].workspace_id, 1);
    }

    /// Round-trip an empty loadout (no workspaces).
    // Positive: empty loadout round-trips without loss.
    #[test]
    fn round_trip_empty_loadout() {
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![],
        };

        let json = serde_json::to_string(&file).expect("serialize");
        let restored: LoadoutFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.workspaces.len(), 0);
    }

    // ── Negative: serde malformed input ──────────────────────────────

    /// JSON missing a required field (`version`) fails to deserialize.
    ///
    /// Every field of [`LoadoutFile`] is required (no `#[serde(default)]`),
    /// so a payload omitting any one of them must be rejected at parse time
    /// rather than silently producing a half-populated struct.
    // Negative: missing required field rejects deserialization.
    #[test]
    fn missing_version_field_fails_to_deserialize() {
        let json = r#"{"workspaces":[]}"#;
        let result: Result<LoadoutFile, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "missing 'version' field must fail deserialization"
        );
    }

    /// Completely invalid JSON fails to deserialize.
    // Negative: garbage input rejects deserialization.
    #[test]
    fn invalid_json_fails_to_deserialize() {
        let result: Result<LoadoutFile, _> = serde_json::from_str("not json {{{");
        assert!(result.is_err(), "garbage input must fail deserialization");
    }

    // ── Negative: legacy (pre-`HWND`) schema rejection ───────────────

    /// A legacy `WindowRef` payload (with `class`, without the now-required
    /// `hwnd`) is rejected at deserialization rather than producing a
    /// half-populated struct.
    //
    // Negative: every field of `WindowRef` is required (no `#[serde(default)]`),
    // so the missing `hwnd` must reject the legacy shape at parse time.
    #[test]
    fn legacy_windowref_without_hwnd_fails_to_deserialize() {
        let json = r#"{"exe":"code.exe","class":"Chrome_WidgetWin_1","title":"main.rs"}"#;
        let result: Result<WindowRef, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "legacy WindowRef without `hwnd` must be rejected"
        );
    }

    /// A whole version-1 (pre-`HWND`) loadout file is rejected at parse time
    /// — the new schema's required `hwnd` is absent in every slot — rather
    /// than being silently loaded or crashing the daemon.
    //
    // Negative: a legacy file must be rejected gracefully (skip with a log),
    // never parsed into a partial model.
    #[test]
    fn legacy_version1_loadout_fails_to_deserialize() {
        let json = r#"{
            "version": 1,
            "saved_at": "2026-07-24T12:00:00Z",
            "workspaces": [{
                "workspace_id": 0,
                "active": true,
                "scrolling": {
                    "viewport_offset": 0,
                    "focus": {"exe":"code.exe","class":"Cls","title":"x"},
                    "columns": [{"width_px": 800, "rows": [
                        {"window": {"exe":"code.exe","class":"Cls","title":"x"}, "height_px": 600}
                    ]}]
                },
                "floating": []
            }]
        }"#;
        let result: Result<LoadoutFile, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "legacy v1 loadout (no hwnd) must be rejected at parse, not silently loaded"
        );
    }
}
