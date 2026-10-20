//! Per-monitor column width presets — `[columns]` in `flow.toml` and
//! `flow dispatch width <preset|next|prev|cycle>`.
//!
//! Every [`Monitor`](crate::workspace::Monitor) remembers which preset it is
//! on (`Monitor::width_preset`). The preset decides the strip's **base
//! column width** (`MutationConfig::column_width`): the width every new
//! column on that monitor is created at — a window that arrives by
//! creation, `set-window tile`, `move-to-monitor`, an edge hop, a hotplug
//! migration or a loadout leftover — and the `n = 0` rung of the
//! expand/shrink ladder. `flow dispatch width` additionally sets every
//! existing column on every workspace of the focused monitor to the new
//! width.
//!
//! Without a `[columns]` section nothing here applies: the base width comes
//! from `columns_per_screen` / `column_width` as before
//! ([`derive_layout_config`](super::config_derive::derive_layout_config)).

use crate::config::width_preset::{ColumnsConfig, WidthPreset};
use crate::ipc::message::{SocketResponse, WidthPresetRequest};
use crate::layout::types::MonitorInfo;

use super::config_derive;
use super::types::{FlowWM, LayoutConfig};

/// Pixel width of `preset` on a work area `work_area_width` wide, floored
/// at `min_column_width_px` (a narrow preset on a small display never
/// drops a column below the configured minimum).
#[must_use]
pub(super) fn preset_width_px(
    preset: &WidthPreset,
    work_area_width: i32,
    window_gap: i32,
    min_column_width_px: u32,
) -> u32 {
    let px = preset.width_px(work_area_width, window_gap).max(1) as u32;
    px.max(min_column_width_px)
}

/// The preset index `request` lands on from `current` in a list of `len`
/// presets: `next` / `cycle` step forward and `prev` backward, both
/// wrapping; a named preset resolves through `index_of`. `None` when the
/// list is empty or the named preset is not configured.
pub(super) fn resolve_preset_index(
    current: usize,
    len: usize,
    request: &WidthPresetRequest,
    index_of: impl Fn(&WidthPreset) -> Option<usize>,
) -> Result<usize, String> {
    if len == 0 {
        return Err("no column width presets configured".into());
    }
    let current = current.min(len - 1);
    match request {
        WidthPresetRequest::Next | WidthPresetRequest::Cycle => Ok((current + 1) % len),
        WidthPresetRequest::Prev => Ok((current + len - 1) % len),
        WidthPresetRequest::Preset(text) => {
            let wanted = WidthPreset::parse(text)?;
            index_of(&wanted).ok_or_else(|| format!("{text} is not a configured preset"))
        }
    }
}

impl FlowWM {
    /// The `[columns]` section, when configured.
    #[must_use]
    pub(super) fn columns_config(&self) -> Option<&ColumnsConfig> {
        self.config.columns.as_ref()
    }

    /// The preset monitor `idx` is on, clamped into the configured list;
    /// `None` without presets.
    #[must_use]
    pub(super) fn monitor_preset(&self, idx: usize) -> Option<&WidthPreset> {
        let columns = self.columns_config()?;
        let i = self
            .monitors
            .get(idx)?
            .width_preset()
            .min(columns.presets.len().checked_sub(1)?);
        columns.presets.get(i)
    }

    /// The label (`"1/2"`) of the preset monitor `idx` is on, for `flow query
    /// all` and the loadout; `None` without presets.
    #[must_use]
    pub(super) fn monitor_preset_label(&self, idx: usize) -> Option<String> {
        self.monitor_preset(idx).map(|p| p.label().to_owned())
    }

    /// Pixel width of preset `preset_idx` on a monitor whose work area is
    /// `info`'s, floored at `min_column_width_px`; `None` without presets.
    #[must_use]
    pub(super) fn preset_px_for(&self, info: &MonitorInfo, preset_idx: usize) -> Option<u32> {
        let columns = self.columns_config()?;
        let preset = columns
            .presets
            .get(preset_idx.min(columns.presets.len().checked_sub(1)?))?;
        Some(preset_width_px(
            preset,
            info.work_area.width,
            self.config.padding.window_gap,
            self.config.min_column_width_px,
        ))
    }

    /// Pixel width of the preset monitor `idx` is on, on its own work area
    /// (`min_column_width_px` floored); `None` without presets. This is the
    /// width every column arriving on that monitor is created at.
    #[must_use]
    pub(super) fn monitor_preset_px(&self, idx: usize) -> Option<u32> {
        let m = self.monitors.get(idx)?;
        let info = *m.active_scrolling().monitor();
        self.preset_px_for(&info, m.width_preset())
    }

    /// Layout parameters for a monitor with geometry `info` that is on
    /// preset `preset_idx`: [`derive_layout_config`] with the base column
    /// width replaced by the preset's pixel width when presets are
    /// configured.
    ///
    /// [`derive_layout_config`]: config_derive::derive_layout_config
    #[must_use]
    pub(super) fn layout_config_for(&self, info: &MonitorInfo, preset_idx: usize) -> LayoutConfig {
        let mut lc = config_derive::derive_layout_config(&self.config, info);
        if let Some(px) = self.preset_px_for(info, preset_idx) {
            lc.column_width = px;
        }
        lc
    }

    /// `flow dispatch width <preset|next|prev|cycle>`: move the focused
    /// monitor to another preset and resize every column on every one of
    /// its workspaces to it. Answers with `{"width_preset": "<label>"}`.
    pub(super) fn dispatch_width_preset(&mut self, request: &WidthPresetRequest) -> SocketResponse {
        let Some(columns) = self.columns_config() else {
            return SocketResponse::Error {
                message: "no column width presets configured (add a [columns] section with `presets` to flow.toml)".into(),
            };
        };
        let idx = self.active_monitor;
        let current = self.monitors[idx].width_preset();
        let target = match resolve_preset_index(current, columns.presets.len(), request, |p| {
            columns.index_of(p)
        }) {
            Ok(t) => t,
            Err(message) => {
                return SocketResponse::Error {
                    message: format!("{message} (presets: {})", columns.labels().join(", ")),
                };
            }
        };
        self.apply_width_preset(idx, target);
        SocketResponse::Data {
            payload: serde_json::json!({
                "width_preset": self.monitor_preset_label(idx),
                "monitor": idx,
            }),
        }
    }

    /// Put monitor `idx` on preset `preset_idx` and set every column on
    /// every one of its workspaces to that width. The active workspace
    /// animates; parked workspaces are re-projected in place and move on
    /// their next switch. No-op without presets.
    pub(super) fn apply_width_preset(&mut self, idx: usize, preset_idx: usize) {
        let Some(columns) = self.columns_config() else {
            return;
        };
        if idx >= self.monitors.len() || columns.presets.is_empty() {
            return;
        }
        let preset_idx = preset_idx.min(columns.presets.len() - 1);
        self.monitors[idx].set_width_preset(preset_idx);
        let info = *self.monitors[idx].active_scrolling().monitor();
        let Some(px) = self.preset_px_for(&info, preset_idx) else {
            return;
        };
        log::info!(
            "width: monitor {idx} -> preset {} ({px}px)",
            self.monitor_preset_label(idx).unwrap_or_default()
        );
        let active_id = self.monitors[idx].active_workspace_id();
        let mut active_applied = None;
        for ws in self.monitors[idx].workspaces_mut() {
            let applied = ws.scrolling.apply_column_width_to_all(px);
            if ws.id == active_id {
                active_applied = Some(applied);
            }
        }
        if let Some(applied) = active_applied {
            self.animate_layout_on(idx, &applied);
        }
    }

    /// Re-derive every monitor's base column width from its own geometry
    /// and preset after a config reload: a monitor that had no presets
    /// before lands on the new default, one whose preset index is now out
    /// of range is clamped. Existing column widths are kept (the reload
    /// policy) — only the base width and the ladder change.
    pub(super) fn reapply_base_widths_after_reload(&mut self, had_presets: bool) {
        let columns_per_screen = self.config.columns_per_screen;
        for mi in 0..self.monitors.len() {
            if let Some(columns) = self.columns_config() {
                let idx = if had_presets {
                    self.monitors[mi]
                        .width_preset()
                        .min(columns.presets.len().saturating_sub(1))
                } else {
                    columns.default_index()
                };
                self.monitors[mi].set_width_preset(idx);
            }
            let info = *self.monitors[mi].active_scrolling().monitor();
            let lc = self.layout_config_for(&info, self.monitors[mi].width_preset());
            for ws in self.monitors[mi].workspaces_mut() {
                let _ = ws.scrolling.reconfigure(
                    lc.column_width,
                    lc.min_column_width_px,
                    lc.min_window_height_px,
                    lc.min_row_height_px,
                    lc.padding,
                    columns_per_screen,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Rect, WindowId};
    use crate::config::types::WindowRulesConfig;
    use crate::ipc::message::SocketMessage;
    use crate::layout::types::Padding;
    use crate::registry::WindowRegistry;
    use crate::registry::types::{TilingState, WindowState};
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};
    use windows::Win32::Foundation::HWND;

    const GAP: i32 = 16;
    const A: isize = 0xA;
    const B: isize = 0xB;
    const C: isize = 0xC;
    const D: isize = 0xD;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn fake_window(registry: &mut WindowRegistry, hwnd: isize) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: r(0, 0, 800, 600),
            exe: "app.exe".into(),
            process_path: "C:\\app.exe".into(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Active { col: 0, row: 0 });
        }
    }

    /// A 1920×1080 monitor at `x` with workspaces 1 and 2, base width
    /// `column_width`.
    fn monitor_at(x: i32, column_width: u32) -> Monitor {
        let screen = r(x, 0, 1920, 1080);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    MonitorInfo::single(screen),
                    column_width,
                    320,
                    100,
                    100,
                    Padding {
                        window_gap: GAP,
                        up: 0,
                        down: 0,
                    },
                    4,
                ),
            )
        };
        Monitor::new(screen, screen, vec![ws(1), ws(2)], 0)
    }

    fn presets(toml_text: &str) -> ColumnsConfig {
        toml::from_str(toml_text).expect("columns config")
    }

    /// Two monitors on preset "1/2" (936 px): monitor 0 tiles A on
    /// workspace 1 and B on workspace 2, monitor 1 tiles C and D.
    fn fixture() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C, D] {
            fake_window(&mut registry, hwnd);
        }
        let mut m0 = monitor_at(0, 936).with_width_preset(1);
        let mut m1 = monitor_at(-1920, 936).with_width_preset(1);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m0.workspace_mut(WorkspaceId(2))
            .expect("ws 2")
            .scrolling
            .add_window(WindowId(B));
        m1.active_scrolling_mut().add_window(WindowId(C));
        m1.active_scrolling_mut().add_window(WindowId(D));
        registry.set_focused(A);
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.config.min_column_width_px = 320;
        flow.config.padding.window_gap = GAP;
        flow.config.columns = Some(presets(
            "presets = [\"1/3\", \"1/2\", \"1/1\"]\ndefault = \"1/2\"",
        ));
        for hwnd in [A, B] {
            flow.remember_monitor(WindowId(hwnd), 0);
        }
        for hwnd in [C, D] {
            flow.remember_monitor(WindowId(hwnd), 1);
        }
        flow
    }

    fn widths(flow: &FlowWM, mi: usize, ws: u32) -> Vec<i32> {
        flow.monitors[mi]
            .workspace(WorkspaceId(ws))
            .expect("workspace")
            .scrolling
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.width_px)
            .collect()
    }

    fn base_width(flow: &FlowWM, mi: usize, ws: u32) -> u32 {
        flow.monitors[mi]
            .workspace(WorkspaceId(ws))
            .expect("workspace")
            .scrolling
            .config()
            .column_width
    }

    /// Monitor 0 on "1/2" (936 px), monitor 1 on "1/1" (1888 px), borders
    /// off so no overlay window is created by the focus bookkeeping.
    fn two_preset_fixture() -> FlowWM {
        let mut flow = fixture();
        flow.config.borders.enabled = false;
        flow.apply_width_preset(1, 2);
        assert_eq!(base_width(&flow, 1, 1), 1888);
        assert_eq!(widths(&flow, 1, 1), vec![1888, 1888]);
        flow
    }

    #[test]
    fn move_to_monitor_gives_the_window_the_target_monitors_preset_width() {
        let mut flow = two_preset_fixture();
        // A (936 wide on monitor 0) moves left onto monitor 1 (1/1).
        flow.registry.set_focused(A);
        assert_eq!(
            flow.dispatch(&SocketMessage::MoveWindowToMonitor {
                direction: crate::common::Direction::Left,
            }),
            SocketResponse::Ok
        );
        assert_eq!(widths(&flow, 0, 1), Vec::<i32>::new());
        let strip = flow.monitors[1].active_scrolling().virtual_layout();
        let (col, _) = strip.find_window(WindowId(A)).expect("A on monitor 1");
        assert_eq!(strip.columns[col].width_px, 1888);
        assert_eq!(flow.active_monitor, 1);

        // And back: A (now 1888 wide, the right-most column of monitor 1)
        // hops right by move-window at the strip edge and lands on monitor
        // 0 at 936.
        flow.registry.set_focused(A);
        assert_eq!(
            flow.dispatch(&SocketMessage::MoveWindow {
                direction: crate::common::Direction::Right,
            }),
            SocketResponse::Ok
        );
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        let (col, _) = strip.find_window(WindowId(A)).expect("A back on monitor 0");
        assert_eq!(strip.columns[col].width_px, 936);
        assert_eq!(widths(&flow, 1, 1), vec![1888, 1888]);
    }

    #[test]
    fn a_column_merged_from_a_detached_monitor_takes_the_receivers_preset_width() {
        let mut flow = two_preset_fixture();
        use crate::loadout::{
            ColumnSnapshot, LoadoutFile, RowSnapshot, ScrollingSnapshot, WindowRef,
            WorkspaceSnapshot,
        };
        // A snapshot from monitor 7 (not attached) carrying C at 1200 px.
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 7,
                workspace_id: 1,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: None,
                    columns: vec![ColumnSnapshot {
                        width_px: 1200,
                        rows: vec![RowSnapshot {
                            window: WindowRef {
                                hwnd: C,
                                exe: "app.exe".into(),
                                title: "winC".into(),
                            },
                            height_px: 1000,
                        }],
                    }],
                },
                floating: vec![],
            }],
        };
        let path = std::env::temp_dir().join(format!(
            "flowwm-width-merge-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, serde_json::to_string(&file).expect("json")).expect("write");
        assert_eq!(
            flow.dispatch_loadout_load(Some(path.clone())),
            SocketResponse::Ok
        );
        let _ = std::fs::remove_file(&path);
        // Merged into monitor 0 (the first with workspace 1): at 936, not 1200.
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        let (col, _) = strip
            .find_window(WindowId(C))
            .expect("C merged onto monitor 0");
        assert_eq!(strip.columns[col].width_px, 936);
    }

    #[test]
    fn cycle_resizes_every_column_on_every_workspace_of_the_focused_monitor_only() {
        let mut flow = fixture();
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Cycle,
        });
        let SocketResponse::Data { payload } = resp else {
            panic!("expected Data, got {resp:?}");
        };
        assert_eq!(payload["width_preset"], "1/1");
        assert_eq!(payload["monitor"], 0);
        assert_eq!(flow.monitors[0].width_preset(), 2);
        // 1/1 = 1920 - 2 * 16 on both workspaces of monitor 0.
        assert_eq!(widths(&flow, 0, 1), vec![1888]);
        assert_eq!(widths(&flow, 0, 2), vec![1888]);
        assert_eq!(base_width(&flow, 0, 1), 1888);
        assert_eq!(base_width(&flow, 0, 2), 1888);
        // Monitor 1 is untouched.
        assert_eq!(flow.monitors[1].width_preset(), 1);
        assert_eq!(widths(&flow, 1, 1), vec![936, 936]);
        assert_eq!(base_width(&flow, 1, 1), 936);

        // Wraps: 1/1 -> 1/3, floored at min_column_width_px (320 here, so 618).
        let _ = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Next,
        });
        assert_eq!(flow.monitors[0].width_preset(), 0);
        assert_eq!(widths(&flow, 0, 1), vec![618]);
        // prev wraps the other way.
        let _ = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Prev,
        });
        assert_eq!(flow.monitors[0].width_preset(), 2);
    }

    #[test]
    fn a_named_preset_is_applied_and_the_min_width_floor_is_respected() {
        let mut flow = fixture();
        flow.config.min_column_width_px = 640;
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/3".into()),
        });
        assert!(matches!(resp, SocketResponse::Data { .. }), "{resp:?}");
        // (1920 - 64) / 3 = 618 < 640 floor.
        assert_eq!(widths(&flow, 0, 1), vec![640]);
        assert_eq!(base_width(&flow, 0, 2), 640);
        // A new column on that monitor is created at the preset width.
        let applied = flow.monitors[0]
            .active_scrolling_mut()
            .add_window(WindowId(0x99));
        assert_eq!(applied.virtual_layout.columns[1].width_px, 640);
        // The ladder still works from the preset: expand lands on the next rung.
        flow.registry.set_focused(A);
        assert_eq!(
            flow.dispatch(&SocketMessage::ExpandColumn),
            SocketResponse::Ok
        );
        assert_eq!(widths(&flow, 0, 1)[0], 640 + 640 + GAP);
    }

    #[test]
    fn unknown_or_unconfigured_presets_are_errors() {
        let mut flow = fixture();
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/4".into()),
        });
        assert!(matches!(resp, SocketResponse::Error { .. }), "{resp:?}");
        assert_eq!(widths(&flow, 0, 1), vec![936]);

        flow.config.columns = None;
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Cycle,
        });
        let SocketResponse::Error { message } = resp else {
            panic!("expected Error, got {resp:?}");
        };
        assert!(message.contains("[columns]"), "{message}");
    }

    #[test]
    fn query_all_reports_each_monitors_preset() {
        let mut flow = fixture();
        let _ = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/1".into()),
        });
        let SocketResponse::Data { payload } = flow.query_windows_all() else {
            panic!("expected Data");
        };
        assert_eq!(payload["monitors"][0]["width_preset"], "1/1");
        assert_eq!(payload["monitors"][0]["column_width"], 1888);
        assert_eq!(payload["monitors"][1]["width_preset"], "1/2");
        flow.config.columns = None;
        let SocketResponse::Data { payload } = flow.query_windows_all() else {
            panic!("expected Data");
        };
        assert_eq!(
            payload["monitors"][0]["width_preset"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn loadout_round_trips_the_preset_per_monitor() {
        let mut flow = fixture();
        let _ = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/1".into()),
        });
        let path = std::env::temp_dir().join(format!(
            "flowwm-width-preset-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        assert_eq!(
            flow.dispatch_loadout_save(Some(path.clone())),
            SocketResponse::Ok
        );
        let raw = std::fs::read_to_string(&path).expect("read loadout");
        assert!(raw.contains("\"width_preset\": \"1/1\""), "{raw}");

        // Back on 1/2, then restore: monitor 0 returns to 1/1, monitor 1 stays.
        flow.apply_width_preset(0, 1);
        assert_eq!(base_width(&flow, 0, 1), 936);
        assert_eq!(
            flow.dispatch_loadout_load(Some(path.clone())),
            SocketResponse::Ok
        );
        let _ = std::fs::remove_file(&path);
        assert_eq!(flow.monitors[0].width_preset(), 2);
        assert_eq!(base_width(&flow, 0, 1), 1888);
        assert_eq!(base_width(&flow, 0, 2), 1888);
        assert_eq!(flow.monitors[1].width_preset(), 1);
        assert_eq!(base_width(&flow, 1, 1), 936);
        // The file's column widths were restored as saved (1888).
        assert_eq!(widths(&flow, 0, 1), vec![1888]);
    }

    #[test]
    fn reload_lands_on_the_default_when_presets_appear_and_clamps_when_they_shrink() {
        let mut flow = fixture();
        flow.monitors[0].set_width_preset(2);
        // Presets shrink to two: index 2 is clamped to 1 ("1/2").
        flow.config.columns = Some(presets("presets = [\"1/3\", \"1/2\"]"));
        flow.reapply_base_widths_after_reload(true);
        assert_eq!(flow.monitors[0].width_preset(), 1);
        assert_eq!(base_width(&flow, 0, 1), 936);
        // Existing column widths are preserved by a reload.
        assert_eq!(widths(&flow, 0, 1), vec![936]);

        // Presets appear for the first time: every monitor starts on the default.
        flow.monitors[0].set_width_preset(0);
        flow.config.columns = Some(presets(
            "presets = [\"1/3\", \"1/2\", \"1/1\"]\ndefault = \"1/1\"",
        ));
        flow.reapply_base_widths_after_reload(false);
        assert_eq!(flow.monitors[0].width_preset(), 2);
        assert_eq!(flow.monitors[1].width_preset(), 2);
        assert_eq!(base_width(&flow, 1, 2), 1888);

        // Presets removed: back to columns_per_screen on each monitor's own
        // work area ((1920 - 5 * 16) / 4 = 460).
        flow.config.columns = None;
        flow.reapply_base_widths_after_reload(true);
        assert_eq!(base_width(&flow, 0, 1), 460);
        assert_eq!(base_width(&flow, 1, 1), 460);
    }

    #[test]
    fn preset_px_floors_at_min_column_width() {
        let third = WidthPreset::parse("1/3").expect("1/3");
        // (1920 - 64) / 3 = 618, below the 640 default floor.
        assert_eq!(preset_width_px(&third, 1920, 16, 640), 640);
        assert_eq!(preset_width_px(&third, 1920, 16, 320), 618);
        let full = WidthPreset::parse("1/1").expect("1/1");
        assert_eq!(preset_width_px(&full, 1920, 16, 640), 1888);
    }

    #[test]
    fn resolve_steps_and_wraps() {
        let cfg: ColumnsConfig =
            toml::from_str("presets = [\"1/3\", \"1/2\", \"1/1\"]").expect("cfg");
        let by = |p: &WidthPreset| cfg.index_of(p);
        assert_eq!(
            resolve_preset_index(0, 3, &WidthPresetRequest::Next, by),
            Ok(1)
        );
        assert_eq!(
            resolve_preset_index(2, 3, &WidthPresetRequest::Next, by),
            Ok(0)
        );
        assert_eq!(
            resolve_preset_index(2, 3, &WidthPresetRequest::Cycle, by),
            Ok(0)
        );
        assert_eq!(
            resolve_preset_index(0, 3, &WidthPresetRequest::Prev, by),
            Ok(2)
        );
        assert_eq!(
            resolve_preset_index(0, 3, &WidthPresetRequest::Preset("2/4".into()), by),
            Ok(1)
        );
        assert!(resolve_preset_index(0, 3, &WidthPresetRequest::Preset("1/4".into()), by).is_err());
        assert!(resolve_preset_index(0, 3, &WidthPresetRequest::Preset("x".into()), by).is_err());
        assert!(resolve_preset_index(0, 0, &WidthPresetRequest::Next, by).is_err());
        // An out-of-range current index is clamped before stepping.
        assert_eq!(
            resolve_preset_index(9, 3, &WidthPresetRequest::Next, by),
            Ok(0)
        );
    }
}
