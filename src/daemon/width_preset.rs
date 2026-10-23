//! Per-monitor column width presets — `[columns]` in `flow.toml` and
//! `flow dispatch width <preset|next|prev|cycle>`.
//!
//! Every [`Monitor`](crate::workspace::Monitor) remembers which preset it is
//! on (`Monitor::width_preset`), an index into **its** list: the first
//! `[[columns.screen]]` rule matching the screen's orientation, width or
//! device name, else the top-level `[columns]` list
//! ([`ColumnsConfig::list_for`]). The preset decides the strip's **base
//! column width** (`MutationConfig::column_width`): the width every new
//! column on that monitor is created at — a window that arrives by
//! creation, `set-window tile`, `move-to-monitor`, an edge hop, a hotplug
//! migration or a loadout leftover — and the `n = 0` rung of the
//! expand/shrink ladder. A preset with two or more terms (`"1/3+2/3"`) is a
//! **pattern**: the strip pins every column to `terms[i % n]` by position
//! ([`ScrollingSpace::set_width_pattern`]), so resizing is refused on that
//! monitor. `flow dispatch width` additionally sets every existing column on
//! every workspace of the focused monitor to the new preset.
//!
//! Without a `[columns]` section nothing here applies: the base width comes
//! from `columns_per_screen` / `column_width` as before
//! ([`derive_layout_config`](super::config_derive::derive_layout_config)).
//! Whether a preset fits a screen is checked against the live screens
//! ([`check_screens`]) at startup and on reload; the pixel floor is only a
//! fallback for a display that appears later. Rationale:
//! `docs/adr/0014-width-patterns-by-column-position.md`.
//!
//! [`ScrollingSpace::set_width_pattern`]: crate::workspace::ScrollingSpace::set_width_pattern

use crate::common::Rect;
use crate::config::types::FlowConfig;
use crate::config::width_preset::{ColumnsConfig, PresetList, ScreenFacts, WidthPreset};
use crate::ipc::message::{SocketResponse, WidthPresetRequest};
use crate::layout::types::MonitorInfo;

use super::config_derive;
use super::types::{FlowWM, LayoutConfig};

/// What `[[columns.screen]]` matches on, from a monitor's work area and
/// Windows device name.
#[must_use]
pub(super) fn screen_facts(work_area: Rect, device_name: &str) -> ScreenFacts {
    ScreenFacts {
        width: work_area.width,
        height: work_area.height,
        name: device_name.to_owned(),
    }
}

/// Pixel width of every term of `preset` on a work area `work_area_width`
/// wide, each floored at `min_column_width_px` — the strip's width pattern.
#[must_use]
pub(super) fn pattern_px(
    preset: &WidthPreset,
    work_area_width: i32,
    window_gap: i32,
    min_column_width_px: u32,
) -> Vec<u32> {
    preset
        .widths_px(work_area_width, window_gap)
        .into_iter()
        .map(|px| (px.max(1) as u32).max(min_column_width_px))
        .collect()
}

/// The preset a screen described by `facts` lands on and its width pattern:
/// `previous` (the preset it was on, if any) when its list still has it,
/// else the list's default. One function for startup, reload and hotplug so
/// the three agree.
#[must_use]
pub(super) fn resolve_for_screen(
    columns: &ColumnsConfig,
    facts: &ScreenFacts,
    previous: Option<&WidthPreset>,
    window_gap: i32,
    global_floor: u32,
) -> (usize, Vec<u32>) {
    let list = columns.list_for(facts);
    let idx = list.carry_over(previous);
    let floor = list.min_column_width_px.unwrap_or(global_floor);
    let px = list
        .presets
        .get(idx)
        .map(|p| pattern_px(p, facts.width, window_gap, floor))
        .unwrap_or_default();
    (idx, px)
}

/// Every screen's preset list checked against that screen
/// ([`ColumnsConfig::check_screen`]); `Ok` without `[columns]`.
///
/// # Errors
///
/// The first screen whose list has a term under its floor.
pub(super) fn check_screens(config: &FlowConfig, screens: &[ScreenFacts]) -> Result<(), String> {
    let Some(columns) = &config.columns else {
        return Ok(());
    };
    for facts in screens {
        columns.check_screen(facts, config.padding.window_gap, config.min_column_width_px)?;
    }
    Ok(())
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

    /// What `[[columns.screen]]` sees of monitor `idx`.
    #[must_use]
    pub(super) fn monitor_facts(&self, idx: usize) -> ScreenFacts {
        let m = &self.monitors[idx];
        screen_facts(m.work_area(), m.device_name())
    }

    /// The preset list monitor `idx` uses; `None` without presets or for an
    /// unknown monitor.
    #[must_use]
    pub(super) fn monitor_preset_list(&self, idx: usize) -> Option<PresetList<'_>> {
        let columns = self.columns_config()?;
        if idx >= self.monitors.len() {
            return None;
        }
        Some(columns.list_for(&self.monitor_facts(idx)))
    }

    /// The preset monitor `idx` is on, clamped into its list; `None` without
    /// presets.
    #[must_use]
    pub(super) fn monitor_preset(&self, idx: usize) -> Option<&WidthPreset> {
        let list = self.monitor_preset_list(idx)?;
        let i = self
            .monitors
            .get(idx)?
            .width_preset()
            .min(list.presets.len().checked_sub(1)?);
        list.presets.get(i)
    }

    /// The label (`"1/2"`, `"1/3+2/3"`) of the preset monitor `idx` is on,
    /// for `flow query all` and the loadout; `None` without presets.
    #[must_use]
    pub(super) fn monitor_preset_label(&self, idx: usize) -> Option<String> {
        self.monitor_preset(idx).map(|p| p.label().to_owned())
    }

    /// The width pattern (pixels per term, floored) of preset `preset_idx`
    /// of monitor `idx`'s list on its own work area; `None` without presets.
    #[must_use]
    pub(super) fn monitor_pattern_px(&self, idx: usize, preset_idx: usize) -> Option<Vec<u32>> {
        let list = self.monitor_preset_list(idx)?;
        let preset = list
            .presets
            .get(preset_idx.min(list.presets.len().checked_sub(1)?))?;
        let floor = list
            .min_column_width_px
            .unwrap_or(self.config.min_column_width_px);
        Some(pattern_px(
            preset,
            self.monitors[idx].work_area().width,
            self.config.padding.window_gap,
            floor,
        ))
    }

    /// Pixel width of the preset monitor `idx` is on (its first term), on
    /// its own work area; `None` without presets. This is the width every
    /// column arriving on that monitor is created at.
    #[must_use]
    pub(super) fn monitor_preset_px(&self, idx: usize) -> Option<u32> {
        let m = self.monitors.get(idx)?;
        self.monitor_pattern_px(idx, m.width_preset())?
            .first()
            .copied()
    }

    /// Layout parameters for a monitor with geometry `info` described by
    /// `facts` that is on preset `preset_idx` of its list:
    /// [`derive_layout_config`] with the base column width replaced by the
    /// preset's first term and the width pattern filled in, when presets
    /// are configured.
    ///
    /// [`derive_layout_config`]: config_derive::derive_layout_config
    #[must_use]
    pub(super) fn layout_config_for(
        &self,
        info: &MonitorInfo,
        facts: &ScreenFacts,
        preset_idx: usize,
    ) -> LayoutConfig {
        let mut lc = config_derive::derive_layout_config(&self.config, info);
        if let Some(columns) = self.columns_config() {
            let list = columns.list_for(facts);
            if let Some(preset) = list
                .presets
                .get(preset_idx.min(list.presets.len().saturating_sub(1)))
            {
                let floor = list
                    .min_column_width_px
                    .unwrap_or(self.config.min_column_width_px);
                let px = pattern_px(
                    preset,
                    info.work_area.width,
                    self.config.padding.window_gap,
                    floor,
                );
                if let Some(&first) = px.first() {
                    lc.column_width = first;
                }
                lc.width_pattern_px = px;
            }
        }
        lc
    }

    /// Why a resize command is refused on the active monitor: its preset is
    /// a pattern, which pins every column's width to its position. `None`
    /// when columns may be resized.
    #[must_use]
    pub(super) fn pinned_widths_refusal(&self) -> Option<String> {
        if !self.active_scrolling().widths_pinned() {
            return None;
        }
        let label = self
            .monitor_preset_label(self.active_monitor)
            .unwrap_or_default();
        Some(format!(
            "column widths follow the pattern \"{label}\" on this screen (every column takes its position's share); switch to a single-fraction preset with `flow dispatch width` to resize columns"
        ))
    }

    /// `flow dispatch width <preset|next|prev|cycle>`: move the focused
    /// monitor to another preset of its list and resize every column on
    /// every one of its workspaces to it. Answers with `{"width_preset":
    /// "<label>"}`.
    pub(super) fn dispatch_width_preset(&mut self, request: &WidthPresetRequest) -> SocketResponse {
        let idx = self.active_monitor;
        let Some(list) = self.monitor_preset_list(idx) else {
            return SocketResponse::Error {
                message: "no column width presets configured (add a [columns] section with `presets` to flow.toml)".into(),
            };
        };
        let current = self.monitors[idx].width_preset();
        let target = match resolve_preset_index(current, list.presets.len(), request, |p| {
            list.index_of(p)
        }) {
            Ok(t) => t,
            Err(message) => {
                return SocketResponse::Error {
                    message: format!(
                        "{message} ({}: {})",
                        list.describe(),
                        list.labels().join(", ")
                    ),
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

    /// Put monitor `idx` on preset `preset_idx` of its list and set every
    /// column on every one of its workspaces to it (a pattern pins them by
    /// position). The active workspace animates; parked workspaces are
    /// re-projected in place and move on their next switch. No-op without
    /// presets.
    pub(super) fn apply_width_preset(&mut self, idx: usize, preset_idx: usize) {
        let Some(list) = self.monitor_preset_list(idx) else {
            return;
        };
        if list.presets.is_empty() {
            return;
        }
        let preset_idx = preset_idx.min(list.presets.len() - 1);
        self.monitors[idx].set_width_preset(preset_idx);
        let Some(px) = self.monitor_pattern_px(idx, preset_idx) else {
            return;
        };
        log::info!(
            "width: monitor {idx} -> preset {} ({}px)",
            self.monitor_preset_label(idx).unwrap_or_default(),
            px.iter().map(u32::to_string).collect::<Vec<_>>().join("+")
        );
        let active_id = self.monitors[idx].active_workspace_id();
        let mut active_applied = None;
        for ws in self.monitors[idx].workspaces_mut() {
            let applied = ws.scrolling.apply_width_pattern(&px);
            if ws.id == active_id {
                active_applied = Some(applied);
            }
        }
        if let Some(applied) = active_applied {
            self.animate_layout_on(idx, &applied);
        }
    }

    /// The preset every monitor is on, before a reload replaces the config
    /// (so [`reapply_presets_after_reload`](Self::reapply_presets_after_reload)
    /// can carry it over by width, not by index).
    #[must_use]
    pub(super) fn current_presets(&self) -> Vec<Option<WidthPreset>> {
        (0..self.monitors.len())
            .map(|mi| self.monitor_preset(mi).cloned())
            .collect()
    }

    /// Re-derive every monitor's preset and base column width from its own
    /// geometry after a config reload: a monitor stays on the preset it was
    /// on (`previous`, from [`current_presets`](Self::current_presets)) when
    /// its list still has it, else lands on that list's default — so a
    /// monitor that had no presets before, or whose list changed under it,
    /// starts on the default. Existing column widths are kept for a plain
    /// fraction (the reload policy) — only the base width and the ladder
    /// change; a pattern pins them by position.
    pub(super) fn reapply_presets_after_reload(&mut self, previous: &[Option<WidthPreset>]) {
        let columns_per_screen = self.config.columns_per_screen;
        for mi in 0..self.monitors.len() {
            let facts = self.monitor_facts(mi);
            if let Some(columns) = self.columns_config() {
                let (idx, _) = resolve_for_screen(
                    columns,
                    &facts,
                    previous.get(mi).and_then(Option::as_ref),
                    self.config.padding.window_gap,
                    self.config.min_column_width_px,
                );
                self.monitors[mi].set_width_preset(idx);
            }
            let info = *self.monitors[mi].active_scrolling().monitor();
            let lc = self.layout_config_for(&info, &facts, self.monitors[mi].width_preset());
            for ws in self.monitors[mi].workspaces_mut() {
                let _ = ws.scrolling.reconfigure(
                    lc.column_width,
                    lc.min_column_width_px,
                    lc.min_window_height_px,
                    lc.min_row_height_px,
                    lc.padding,
                    columns_per_screen,
                );
                let _ = ws.scrolling.set_row_limits(lc.visible_rows, lc.max_rows);
                let _ = ws.scrolling.set_width_pattern(&lc.width_pattern_px);
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
            is_iconic: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Active { col: 0, row: 0 });
        }
    }

    /// A monitor `screen` with workspaces 1 and 2, base width `column_width`
    /// and device name `name`.
    fn monitor_rect(screen: Rect, column_width: u32, name: &str) -> Monitor {
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
        Monitor::new(screen, screen, vec![ws(1), ws(2)], 0).with_device_name(name)
    }

    /// A 1920×1080 monitor at `x` with workspaces 1 and 2, base width
    /// `column_width`.
    fn monitor_at(x: i32, column_width: u32) -> Monitor {
        let name = if x == 0 {
            r"\\.\DISPLAY1"
        } else {
            r"\\.\DISPLAY2"
        };
        monitor_rect(r(x, 0, 1920, 1080), column_width, name)
    }

    fn presets(toml_text: &str) -> ColumnsConfig {
        toml::from_str(toml_text).expect("columns config")
    }

    /// Two monitors on preset "1/2" (936 px): monitor 0 tiles A on
    /// workspace 1 and B on workspace 2, monitor 1 tiles C and D.
    fn fixture_with(m0: Monitor, m1: Monitor) -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C, D] {
            fake_window(&mut registry, hwnd);
        }
        let mut m0 = m0.with_width_preset(1);
        let mut m1 = m1.with_width_preset(1);
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

    fn fixture() -> FlowWM {
        fixture_with(monitor_at(0, 936), monitor_at(-1920, 936))
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
                                title: None,
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
        let SocketResponse::Error { message } = resp else {
            panic!("expected Error, got {resp:?}");
        };
        assert!(message.contains("columns: 1/3, 1/2, 1/1"), "{message}");
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
    fn query_all_reports_each_monitors_preset_and_device_name() {
        let mut flow = fixture();
        let _ = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/1".into()),
        });
        let SocketResponse::Data { payload } = flow.query_windows_all() else {
            panic!("expected Data");
        };
        assert_eq!(payload["monitors"][0]["width_preset"], "1/1");
        assert_eq!(payload["monitors"][0]["column_width"], 1888);
        assert_eq!(payload["monitors"][0]["device_name"], r"\\.\DISPLAY1");
        assert_eq!(payload["monitors"][1]["width_preset"], "1/2");
        assert_eq!(payload["monitors"][1]["device_name"], r"\\.\DISPLAY2");
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
    fn reload_carries_the_preset_over_by_width_and_lands_on_the_default_otherwise() {
        let mut flow = fixture();
        flow.monitors[0].set_width_preset(2); // "1/1"
        let before = flow.current_presets();
        assert_eq!(before[0].as_ref().map(|p| p.label()), Some("1/1"));
        // "1/1" moves to index 0 of the new list: the monitor follows it.
        flow.config.columns = Some(presets("presets = [\"1/1\", \"1/3\", \"1/2\"]"));
        flow.reapply_presets_after_reload(&before);
        assert_eq!(flow.monitors[0].width_preset(), 0);
        assert_eq!(base_width(&flow, 0, 1), 1888);
        // Monitor 1 was on "1/2", now index 2.
        assert_eq!(flow.monitors[1].width_preset(), 2);
        // Existing column widths are preserved by a reload.
        assert_eq!(widths(&flow, 0, 1), vec![936]);

        // The list no longer has the monitor's preset: its default ("1/2").
        let before = flow.current_presets();
        flow.config.columns = Some(presets("presets = [\"1/3\", \"1/2\"]\ndefault = \"1/2\""));
        flow.reapply_presets_after_reload(&before);
        assert_eq!(flow.monitors[0].width_preset(), 1);
        assert_eq!(base_width(&flow, 0, 1), 936);

        // Presets appear for the first time: every monitor starts on the default.
        flow.config.columns = None;
        let before = flow.current_presets();
        assert_eq!(before, vec![None, None]);
        flow.monitors[0].set_width_preset(0);
        flow.config.columns = Some(presets(
            "presets = [\"1/3\", \"1/2\", \"1/1\"]\ndefault = \"1/1\"",
        ));
        flow.reapply_presets_after_reload(&before);
        assert_eq!(flow.monitors[0].width_preset(), 2);
        assert_eq!(flow.monitors[1].width_preset(), 2);
        assert_eq!(base_width(&flow, 1, 2), 1888);

        // Presets removed: back to columns_per_screen on each monitor's own
        // work area ((1920 - 5 * 16) / 4 = 460).
        let before = flow.current_presets();
        flow.config.columns = None;
        flow.reapply_presets_after_reload(&before);
        assert_eq!(base_width(&flow, 0, 1), 460);
        assert_eq!(base_width(&flow, 1, 1), 460);
    }

    #[test]
    fn pattern_px_floors_every_term_at_min_column_width() {
        let third = WidthPreset::parse("1/3").expect("1/3");
        // (1920 - 64) / 3 = 618, below the 640 default floor.
        assert_eq!(pattern_px(&third, 1920, 16, 640), vec![640]);
        assert_eq!(pattern_px(&third, 1920, 16, 320), vec![618]);
        let full = WidthPreset::parse("1/1").expect("1/1");
        assert_eq!(pattern_px(&full, 1920, 16, 640), vec![1888]);
        // Every term of a pattern is floored on its own.
        let pattern = WidthPreset::parse("1/4+3/4").expect("pattern");
        assert_eq!(pattern_px(&pattern, 1920, 16, 320), vec![460, 1412]);
        assert_eq!(pattern_px(&pattern, 1920, 16, 600), vec![600, 1412]);
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

    // ── Patterns ────────────────────────────────────────────────────────

    const PATTERNS: &str = "presets = [\"1/3\", \"1/3+2/3\", \"1/2\", \"1/1\"]\ndefault = \"1/2\"";

    /// Monitor 1 (C, D) on the pattern "1/3+2/3", borders off.
    fn pattern_fixture() -> FlowWM {
        let mut flow = fixture();
        flow.config.borders.enabled = false;
        flow.config.columns = Some(presets(PATTERNS));
        // The fixture's monitors sit on index 1, which is now the pattern
        // in this list; re-apply so the strips learn it.
        flow.apply_width_preset(1, 1);
        flow
    }

    #[test]
    fn a_pattern_pins_widths_by_position_and_reflows_on_insert_and_remove() {
        let mut flow = pattern_fixture();
        assert_eq!(flow.monitor_preset_label(1).as_deref(), Some("1/3+2/3"));
        // C at position 0 takes 1/3 (618), D at position 1 takes 2/3 (1253).
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253]);
        assert_eq!(base_width(&flow, 1, 1), 618);
        assert!(flow.monitors[1].active_scrolling().widths_pinned());
        // The empty workspace 2 learned the pattern too.
        assert!(
            flow.monitors[1]
                .workspace(WorkspaceId(2))
                .expect("ws 2")
                .scrolling
                .widths_pinned()
        );
        // A third column repeats the period: 1/3 again.
        flow.monitors[1]
            .active_scrolling_mut()
            .add_window(WindowId(0x99));
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253, 618]);
        // Inserting at the front re-flows everything after it.
        flow.monitors[1]
            .active_scrolling_mut()
            .prepend_window(WindowId(0x98));
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253, 618, 1253]);
        // Removing the first column re-flows the rest.
        flow.monitors[1]
            .active_scrolling_mut()
            .remove_window(WindowId(0x98));
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253, 618]);
        // Swapping two columns moves the windows, not the widths.
        let strip = flow.monitors[1].active_scrolling().virtual_layout();
        let first = strip.columns[0].rows[0].window_id;
        let second = strip.columns[1].rows[0].window_id;
        flow.monitors[1]
            .active_scrolling_mut()
            .swap_column(first, crate::common::Direction::Right);
        let strip = flow.monitors[1].active_scrolling().virtual_layout();
        assert_eq!(strip.columns[0].rows[0].window_id, second);
        assert_eq!(strip.columns[1].rows[0].window_id, first);
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253, 618]);
        // Back to a single fraction: columns are free again.
        flow.apply_width_preset(1, 2);
        assert_eq!(widths(&flow, 1, 1), vec![936, 936, 936]);
        assert!(!flow.monitors[1].active_scrolling().widths_pinned());
    }

    #[test]
    fn resize_commands_are_refused_on_a_pattern_and_a_slipped_resize_is_undone() {
        let mut flow = pattern_fixture();
        flow.active_monitor = 1;
        flow.registry.set_focused(C);
        for (msg, name) in [
            (SocketMessage::ExpandColumn, "expand"),
            (SocketMessage::ShrinkColumn, "shrink"),
            (SocketMessage::SetColumnWidth { width_px: 800 }, "set-width"),
            (SocketMessage::ToggleMonocle, "monocle"),
        ] {
            let resp = flow.dispatch(&msg);
            let SocketResponse::Error { message } = resp else {
                panic!("{name}: expected Error, got {resp:?}");
            };
            assert!(message.contains("1/3+2/3"), "{name}: {message}");
            assert_eq!(widths(&flow, 1, 1), vec![618, 1253], "{name}");
        }
        // A width written straight into the layout (a mouse resize commit)
        // is undone on the next mutation.
        let mut vl = flow.monitors[1].active_scrolling().virtual_layout().clone();
        vl.columns[0].width_px = 900;
        let applied = flow.monitors[1].active_scrolling_mut().commit_layout(vl);
        assert_eq!(applied.virtual_layout.columns[0].width_px, 618);
        // Monitor 0 (on "1/3+2/3" as well by index, but re-applied? no: it
        // kept its old single-width strip) is not pinned.
        assert!(!flow.monitors[0].active_scrolling().widths_pinned());
        flow.active_monitor = 0;
        assert!(flow.pinned_widths_refusal().is_none());
    }

    #[test]
    fn reload_onto_a_pattern_pins_and_off_a_pattern_frees() {
        let mut flow = fixture();
        let before = flow.current_presets(); // both on "1/2"
        flow.config.columns = Some(presets(
            "presets = [\"1/3+2/3\", \"1/2\"]\ndefault = \"1/3+2/3\"",
        ));
        flow.reapply_presets_after_reload(&before);
        // "1/2" is still in the list: both monitors stay on it, unpinned.
        assert_eq!(flow.monitors[0].width_preset(), 1);
        assert_eq!(widths(&flow, 1, 1), vec![936, 936]);
        assert!(!flow.monitors[1].active_scrolling().widths_pinned());
        // A list without "1/2": the default pattern pins the columns.
        let before = flow.current_presets();
        flow.config.columns = Some(presets("presets = [\"1/3+2/3\", \"1/1\"]"));
        flow.reapply_presets_after_reload(&before);
        assert_eq!(flow.monitors[1].width_preset(), 0);
        assert_eq!(widths(&flow, 1, 1), vec![618, 1253]);
        assert!(flow.monitors[1].active_scrolling().widths_pinned());
    }

    // ── Per-screen rules ───────────────────────────────────────────────

    const RULES: &str = r#"
presets = ["1/3", "1/3+2/3", "1/2", "1/1"]
default = "1/2"

[[screen]]
match = { orientation = "portrait" }
presets = ["1/2", "1/1"]
default = "1/1"
min_column_width_px = 500

[[screen]]
match = { name = "DISPLAY2" }
presets = ["1/2", "1/1"]
default = "1/2"
"#;

    #[test]
    fn a_screen_rule_gives_a_monitor_its_own_list_and_floor() {
        // Monitor 1 is a portrait 1080x1920 display.
        let mut flow = fixture_with(
            monitor_at(0, 936),
            monitor_rect(r(-1080, 0, 1080, 1920), 936, r"\\.\DISPLAY2"),
        );
        flow.config.borders.enabled = false;
        let before = flow.current_presets(); // both on "1/2"
        flow.config.columns = Some(presets(RULES));
        flow.reapply_presets_after_reload(&before);
        // Monitor 0 falls through to the top-level list and stays on "1/2".
        let list0 = flow.monitor_preset_list(0).expect("list");
        assert_eq!(list0.rule, None);
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/2"));
        // Monitor 1 matches the portrait rule (first match wins over the
        // name rule) and stays on "1/2", which that list has, at the rule's
        // own floor: (1080 - 48) / 2 = 516 >= 500.
        let list1 = flow.monitor_preset_list(1).expect("list");
        assert_eq!(list1.rule, Some(0));
        assert_eq!(list1.labels(), vec!["1/2", "1/1"]);
        assert_eq!(flow.monitors[1].width_preset(), 0);
        assert_eq!(base_width(&flow, 1, 1), 516);
        // Cycling on monitor 1 walks its own list.
        flow.active_monitor = 1;
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Next,
        });
        let SocketResponse::Data { payload } = resp else {
            panic!("expected Data, got {resp:?}");
        };
        assert_eq!(payload["width_preset"], "1/1");
        assert_eq!(widths(&flow, 1, 1), vec![1048, 1048]);
        // A preset from the top-level list is not on this screen.
        let resp = flow.dispatch(&SocketMessage::SetWidthPreset {
            preset: WidthPresetRequest::Preset("1/3+2/3".into()),
        });
        let SocketResponse::Error { message } = resp else {
            panic!("expected Error, got {resp:?}");
        };
        assert!(message.contains("columns.screen[0]: 1/2, 1/1"), "{message}");
        // The pattern on monitor 0 is unaffected by monitor 1's rule.
        flow.active_monitor = 0;
        flow.apply_width_preset(0, 1);
        assert_eq!(widths(&flow, 0, 1), vec![618]);
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/3+2/3"));
    }

    #[test]
    fn check_screens_names_the_screen_and_passes_with_a_matching_rule() {
        let mut config = FlowConfig::default();
        config.padding.window_gap = GAP;
        config.min_column_width_px = 600;
        config.columns = Some(presets("presets = [\"1/3\", \"1/2\", \"1/1\"]"));
        let wide = screen_facts(r(0, 0, 1920, 1080), r"\\.\DISPLAY1");
        let tall = screen_facts(r(-1080, 0, 1080, 1920), r"\\.\DISPLAY2");
        assert!(check_screens(&config, std::slice::from_ref(&wide)).is_ok());
        let err = check_screens(&config, &[wide.clone(), tall.clone()]).expect_err("portrait");
        assert!(err.contains("\"1/3\" is 338 px wide on DISPLAY2 (1080x1920 work area), below min_column_width_px 600"), "{err}");
        // A portrait rule with its own floor makes the same desktop valid.
        config.columns = Some(presets(RULES));
        assert!(check_screens(&config, &[wide, tall]).is_ok());
        // Without [columns] there is nothing to check.
        config.columns = None;
        assert!(check_screens(&config, &[screen_facts(r(0, 0, 100, 100), "")]).is_ok());
    }

    #[test]
    fn reload_refuses_a_list_that_does_not_fit_a_screen_and_keeps_the_old_config() {
        let mut flow = fixture();
        flow.config.borders.enabled = false;
        let dir = std::env::temp_dir().join(format!(
            "flowwm-width-reload-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("config dir");
        flow.config_dir = dir.clone();
        let toml_path = dir.join("flow.toml");
        // 1/4 of 1920 is 460 px, under the 600 floor on both screens.
        std::fs::write(
            &toml_path,
            "min_column_width_px = 600\n[borders]\nenabled = false\n[columns]\npresets = [\"1/4\", \"1/2\"]\n",
        )
        .expect("write");
        let resp = flow.dispatch(&SocketMessage::ReloadConfig);
        let SocketResponse::Error { message } = resp else {
            panic!("expected Error, got {resp:?}");
        };
        assert!(
            message.contains("\"1/4\" is 460 px wide on DISPLAY1 (1920x1080 work area), below min_column_width_px 600"),
            "{message}"
        );
        // Nothing was applied.
        assert_eq!(flow.config.min_column_width_px, 320);
        assert_eq!(
            flow.columns_config().expect("columns").labels(),
            vec!["1/3", "1/2", "1/1"]
        );
        assert_eq!(widths(&flow, 0, 1), vec![936]);

        // A rule for those screens with its own floor makes the same list
        // valid; the monitors carry "1/2" over.
        std::fs::write(
            &toml_path,
            "min_column_width_px = 600\n[borders]\nenabled = false\n[columns]\npresets = [\"1/4\", \"1/2\"]\n\
             [[columns.screen]]\nmatch = { orientation = \"landscape\" }\npresets = [\"1/4\", \"1/2\"]\nmin_column_width_px = 400\n",
        )
        .expect("write");
        assert_eq!(
            flow.dispatch(&SocketMessage::ReloadConfig),
            SocketResponse::Ok
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(flow.config.min_column_width_px, 600);
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/2"));
        assert_eq!(flow.monitors[0].width_preset(), 1);
        assert_eq!(flow.monitor_preset_list(0).expect("list").rule, Some(0));
        // "1/4" is now reachable on that screen at the rule's floor.
        flow.apply_width_preset(0, 0);
        assert_eq!(widths(&flow, 0, 1), vec![460]);
    }

    #[test]
    fn resolve_for_screen_carries_over_or_defaults_with_the_rules_floor() {
        let columns = presets(RULES);
        let tall = screen_facts(r(-1080, 0, 1080, 1920), r"\\.\DISPLAY2");
        let half = WidthPreset::parse("1/2").expect("1/2");
        let pattern = WidthPreset::parse("1/3+2/3").expect("pattern");
        // Rotated from "1/2": stays on it (index 0 of the portrait list).
        assert_eq!(
            resolve_for_screen(&columns, &tall, Some(&half), GAP, 600),
            (0, vec![516])
        );
        // Rotated from the pattern: the portrait default "1/1".
        assert_eq!(
            resolve_for_screen(&columns, &tall, Some(&pattern), GAP, 600),
            (1, vec![1048])
        );
        // A new screen: the default.
        let wide = screen_facts(r(0, 0, 1920, 1080), r"\\.\DISPLAY1");
        assert_eq!(
            resolve_for_screen(&columns, &wide, None, GAP, 320),
            (2, vec![936])
        );
        // The name rule applies to a landscape DISPLAY2.
        let second = screen_facts(r(-1920, 0, 1920, 1080), r"\\.\DISPLAY2");
        assert_eq!(
            resolve_for_screen(&columns, &second, Some(&pattern), GAP, 320),
            (0, vec![936])
        );
        // Without a matching rule the global floor applies to each term.
        let (i, px) = resolve_for_screen(&columns, &wide, Some(&pattern), GAP, 700);
        assert_eq!((i, px), (1, vec![700, 1253]));
    }
}
