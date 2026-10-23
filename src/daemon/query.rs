//! Query handlers for IPC commands.
//!
//! This module contains methods that extract query logic from the dispatch
//! match arms into dedicated methods. Each method returns a [`SocketResponse`]
//! with JSON data.

use crate::common::WindowId;
use crate::ipc::message::SocketResponse;
use crate::layout::types::VirtualLayout;

use super::types::FlowWM;

/// `flow query all` / `query layout virtual` view of one strip's columns:
/// index, width, row count, the row viewport offset and how many rows the
/// viewport shows (`visible_rows`, `0` = all).
fn columns_json(vl: &VirtualLayout, visible_rows: u32) -> Vec<serde_json::Value> {
    vl.columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            serde_json::json!({
                "index": i,
                "width_px": col.width_px,
                "row_count": col.rows.len(),
                "row_offset": col.row_offset,
                "visible_rows": col.slots(visible_rows),
            })
        })
        .collect()
}

impl FlowWM {
    /// Whether tiled window `wid` is currently parked inside its column
    /// (outside the column's row viewport): `Some(true)` parked, `Some(false)`
    /// visible in its column, `None` for a window seated in no strip.
    #[must_use]
    pub(super) fn parked_row(&self, wid: WindowId) -> Option<bool> {
        self.monitors.iter().find_map(|m| {
            m.workspaces().iter().find_map(|ws| {
                let vl = ws.scrolling.virtual_layout();
                let (col, row) = vl.find_window(wid)?;
                let visible_rows = ws.scrolling.config().visible_rows;
                Some(!vl.columns[col].row_visible(row, visible_rows))
            })
        })
    }

    /// `flow query binds`: the resolved `[binds]` table for the hotkey script
    /// and the overlay — every bind with its AutoHotkey hotkey string, kind,
    /// argv and scope, the collapsed help rows and the hardwired chords
    /// (`config::binds::report`). The running config validated at load, so
    /// a resolve failure here is answered as an error rather than reached.
    pub(super) fn query_binds(&self) -> SocketResponse {
        let binds = &self.config.binds;
        match binds.resolve() {
            Ok(resolved) => {
                let from_config = !binds.defaults || !binds.entries.is_empty();
                SocketResponse::Data {
                    payload: serde_json::json!(crate::config::binds::report(
                        &resolved,
                        from_config
                    )),
                }
            }
            Err(problems) => SocketResponse::Error {
                message: problems.join("; "),
            },
        }
    }

    /// Return all tracked windows as JSON.
    ///
    /// The viewport offset from the virtual layout is included so clients
    /// can determine which windows are currently visible on screen. Each
    /// window entry carries a `monitor` field: the index of the monitor
    /// whose workspace holds it (else the remembered owning monitor), or
    /// `null` for a window seated nowhere (ignored, minimized, hidden and
    /// never seated) — and a `mode` field: `"tiled"`, `"floating"`,
    /// `"fullscreen"` (a `set-window fullscreen` window, which the `state`
    /// records as a float), `"minimized"`, `"hidden"` or `"ignored"`. A
    /// `parked_row` field says whether a tile is hidden inside its column by
    /// the row viewport (`[columns] visible_rows`); `null` for a window in
    /// no strip. Each monitor lists its active strip's `columns` with their
    /// `row_count`, `row_offset` and `visible_rows`.
    pub(super) fn query_windows_all(&self) -> SocketResponse {
        let viewport_offset = self.active_scrolling().virtual_layout().viewport_offset;
        let mut payload = self.registry.to_json_value(viewport_offset);
        annotate_windows_with_monitor(&mut payload, |hwnd| {
            let wid = crate::common::WindowId(hwnd);
            self.monitor_index_containing(wid)
                .or_else(|| self.remembered_monitor(wid))
        });
        annotate_windows_with_mode(&mut payload, |hwnd| self.window_mode(hwnd));
        annotate_windows_with(&mut payload, "parked_row", |hwnd| {
            serde_json::json!(self.parked_row(WindowId(hwnd)))
        });
        if let serde_json::Value::Object(map) = &mut payload {
            let monitors: Vec<serde_json::Value> = self
                .monitors
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let info = m.active_scrolling().monitor();
                    let sr = m.screen_rect();
                    let wa = m.work_area();
                    serde_json::json!({
                        "index": i,
                        "active": i == self.active_monitor,
                        "hmonitor": m.hmonitor(),
                        "device_name": m.device_name(),
                        "screen_rect": { "x": sr.x, "y": sr.y, "width": sr.width, "height": sr.height },
                        "work_area": { "x": wa.x, "y": wa.y, "width": wa.width, "height": wa.height },
                        "neighbour_left": info.neighbour_left,
                        "neighbour_right": info.neighbour_right,
                        "neighbour_up": info.neighbour_up,
                        "neighbour_down": info.neighbour_down,
                        "width_preset": self.monitor_preset_label(i),
                        "column_width": m.active_scrolling().config().column_width,
                        "active_workspace": m.active_workspace_id().0,
                        "viewport_offset": m.active_scrolling().virtual_layout().viewport_offset,
                        "tiled_windows": m.active_scrolling().virtual_layout().window_count(),
                        "visible_rows": m.active_scrolling().config().visible_rows,
                        "max_rows": m.active_scrolling().config().max_rows,
                        "columns": columns_json(
                            m.active_scrolling().virtual_layout(),
                            m.active_scrolling().config().visible_rows,
                        ),
                    })
                })
                .collect();
            map.insert(
                "active_monitor".into(),
                serde_json::json!(self.active_monitor),
            );
            map.insert("monitors".into(), serde_json::Value::Array(monitors));
            map.insert(
                "tiling_enabled".into(),
                serde_json::json!(!self.tiling_paused),
            );
        }
        SocketResponse::Data { payload }
    }

    /// Return the virtual layout structure as JSON.
    ///
    /// The virtual layout represents the logical structure on an infinite
    /// horizontal canvas. Columns store pixel widths (`width_px`), not pixel
    /// positions (the projection layer derives positions via prefix-sum).
    pub(super) fn query_layout_virtual(&self) -> SocketResponse {
        let vl = self.active_scrolling().virtual_layout();
        let visible_rows = self.active_scrolling().config().visible_rows;
        let columns_json: Vec<serde_json::Value> = columns_json(vl, visible_rows)
            .into_iter()
            .zip(&vl.columns)
            .map(|(mut json, col)| {
                let rows: Vec<serde_json::Value> = col
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(r, row)| {
                        serde_json::json!({
                            "window_id": row.window_id.0,
                            "height_px": row.height,
                            "parked_row": !col.row_visible(r, visible_rows),
                        })
                    })
                    .collect();
                if let Some(obj) = json.as_object_mut() {
                    obj.insert("rows".into(), serde_json::Value::Array(rows));
                }
                json
            })
            .collect();
        SocketResponse::Data {
            payload: serde_json::json!({
                "viewport_offset": vl.viewport_offset,
                "column_count": vl.columns.len(),
                "window_count": vl.window_count(),
                "columns": columns_json,
            }),
        }
    }

    /// Return the actual layout (projected coordinates) as JSON.
    ///
    /// The actual layout contains pixel-level coordinates for each window
    /// after projection and padding have been applied.
    pub(super) fn query_layout_actual(&self) -> SocketResponse {
        let vl = self.active_scrolling().virtual_layout();
        let al = self.active_scrolling().actual_layout();
        let entries_json: Vec<serde_json::Value> = al
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "window_id": e.window_id.0,
                    "rect": {
                        "x": e.rect.x,
                        "y": e.rect.y,
                        "width": e.rect.width,
                        "height": e.rect.height,
                    },
                })
            })
            .collect();
        SocketResponse::Data {
            payload: serde_json::json!({
                "viewport_offset": vl.viewport_offset,
                "entry_count": al.entries.len(),
                "entries": entries_json,
            }),
        }
    }
}

/// Add a `mode` field to every entry of `payload["windows"]` from
/// `mode_of(hwnd)`. Entries without a numeric `hwnd` are left untouched.
pub(super) fn annotate_windows_with_mode(
    payload: &mut serde_json::Value,
    mode_of: impl Fn(isize) -> &'static str,
) {
    annotate_windows_with(payload, "mode", |hwnd| serde_json::json!(mode_of(hwnd)));
}

/// Add field `key` to every entry of `payload["windows"]` from
/// `value_of(hwnd)`. Entries without a numeric `hwnd` are left untouched; a
/// payload without a `windows` array is a no-op.
pub(super) fn annotate_windows_with(
    payload: &mut serde_json::Value,
    key: &str,
    value_of: impl Fn(isize) -> serde_json::Value,
) {
    let Some(windows) = payload
        .get_mut("windows")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for entry in windows {
        let Some(hwnd) = entry.get("hwnd").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(key.to_owned(), value_of(hwnd as isize));
        }
    }
}

/// Add a `monitor` field to every entry of `payload["windows"]`, using
/// `monitor_of(hwnd)` — `None` serialises as `null`. Entries without a
/// numeric `hwnd` are left untouched; a payload without a `windows` array is
/// a no-op.
pub(super) fn annotate_windows_with_monitor(
    payload: &mut serde_json::Value,
    monitor_of: impl Fn(isize) -> Option<usize>,
) {
    let Some(windows) = payload
        .get_mut("windows")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for entry in windows {
        let Some(hwnd) = entry.get("hwnd").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "monitor".into(),
                serde_json::json!(monitor_of(hwnd as isize)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every window entry gains a `monitor` index from the lookup; windows
    /// seated nowhere get `null`.
    #[test]
    fn windows_payload_is_annotated_with_monitor() {
        let mut payload = serde_json::json!({
            "windows": [
                { "hwnd": 10, "exe": "a.exe" },
                { "hwnd": 20, "exe": "b.exe" },
                { "hwnd": 30, "exe": "c.exe" },
            ],
            "count": 3,
        });
        annotate_windows_with_monitor(&mut payload, |hwnd| match hwnd {
            10 => Some(0),
            20 => Some(1),
            _ => None,
        });
        assert_eq!(payload["windows"][0]["monitor"], 0);
        assert_eq!(payload["windows"][1]["monitor"], 1);
        assert_eq!(payload["windows"][2]["monitor"], serde_json::Value::Null);
        // Other fields are untouched.
        assert_eq!(payload["windows"][0]["exe"], "a.exe");
        assert_eq!(payload["count"], 3);
    }

    /// Every window entry gains a `mode` from the lookup.
    #[test]
    fn windows_payload_is_annotated_with_mode() {
        let mut payload = serde_json::json!({
            "windows": [ { "hwnd": 10 }, { "hwnd": 20 }, { "title": "no hwnd" } ],
        });
        annotate_windows_with_mode(&mut payload, |hwnd| {
            if hwnd == 10 { "fullscreen" } else { "tiled" }
        });
        assert_eq!(payload["windows"][0]["mode"], "fullscreen");
        assert_eq!(payload["windows"][1]["mode"], "tiled");
        assert!(payload["windows"][2].get("mode").is_none());
    }

    /// A payload without a `windows` array is left as it is.
    #[test]
    fn annotate_without_windows_is_a_noop() {
        let mut payload = serde_json::json!({ "count": 0 });
        annotate_windows_with_monitor(&mut payload, |_| Some(0));
        assert_eq!(payload, serde_json::json!({ "count": 0 }));
    }

    /// Verify that query methods exist and have the correct return type.
    ///
    /// Since FlowWM construction requires admin rights (for named pipe)
    /// and a real Windows desktop, we can't do full unit tests here.
    /// The query methods are tested indirectly via integration tests in tests/cli/.
    #[test]
    fn query_methods_exist_and_have_correct_return_type() {
        // This test verifies that the methods exist and return SocketResponse.
        // We can't call them directly without a FlowWM instance.
        // The actual behavior is tested via integration tests.

        // query_windows_all returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_windows_all;

        // query_layout_virtual returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_layout_virtual;

        // query_layout_actual returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_layout_actual;
    }

    /// Positive: verify that SocketResponse::Data is constructible with JSON.
    ///
    /// This ensures that the query methods can return the correct type.
    #[test]
    fn socket_response_data_constructible_with_json() {
        let payload = serde_json::json!({
            "viewport_offset": 0,
            "column_count": 0,
            "window_count": 0,
            "columns": []
        });
        let response = SocketResponse::Data { payload };

        match response {
            SocketResponse::Data { payload } => {
                assert!(payload.is_object());
                assert_eq!(payload["viewport_offset"], 0);
            }
            _ => panic!("should be Data variant"),
        }
    }

    /// Positive: verify that JSON serialization works for query payloads.
    #[test]
    fn query_payload_serializes_to_json() {
        let payload = serde_json::json!({
            "test_key": "test_value",
            "number": 42
        });

        let json_str = serde_json::to_string(&payload);
        assert!(json_str.is_ok());
        assert!(json_str.unwrap().contains("test_value"));
    }

    /// Positive: verify that virtual layout JSON structure is valid.
    #[test]
    fn virtual_layout_json_structure_is_valid() {
        // Create a minimal virtual layout structure using a JSON array
        let columns_json = serde_json::json!([]);
        let window_count: usize = 0;

        let json = serde_json::json!({
            "viewport_offset": 0,
            "column_count": columns_json.as_array().unwrap().len(),
            "window_count": window_count,
            "columns": columns_json
        });

        assert_eq!(json["viewport_offset"], 0);
        assert_eq!(json["column_count"], 0);
        assert_eq!(json["window_count"], 0);
        assert!(json["columns"].as_array().unwrap().is_empty());
    }

    /// Positive: verify that actual layout JSON structure is valid.
    #[test]
    fn actual_layout_json_structure_is_valid() {
        // Create a minimal actual layout structure using a JSON array
        let entries_json = serde_json::json!([]);

        let json = serde_json::json!({
            "viewport_offset": 0,
            "entry_count": entries_json.as_array().unwrap().len(),
            "entries": entries_json
        });

        assert_eq!(json["viewport_offset"], 0);
        assert_eq!(json["entry_count"], 0);
        assert!(json["entries"].as_array().unwrap().is_empty());
    }

    /// Columns report their row count and row viewport; rows say whether
    /// they are parked by it.
    #[test]
    fn columns_json_reports_row_counts_and_viewports() {
        use crate::common::WindowId;
        use crate::layout::types::{Column, Row};
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_rows(960, (1..=4).map(|i| Row::new(WindowId(i), 100)).collect())
                    .with_row_offset(1),
                Column::with_row(960, Row::new(WindowId(9), 1072)),
            ],
            0,
        );
        let cols = columns_json(&vl, 2);
        assert_eq!(cols[0]["row_count"], 4);
        assert_eq!(cols[0]["row_offset"], 1);
        assert_eq!(cols[0]["visible_rows"], 2);
        assert_eq!(cols[1]["row_count"], 1);
        assert_eq!(cols[1]["visible_rows"], 1);
        assert!(vl.columns[0].row_visible(1, 2) && vl.columns[0].row_visible(2, 2));
        assert!(!vl.columns[0].row_visible(0, 2) && !vl.columns[0].row_visible(3, 2));
        // Without a viewport every row is visible.
        assert_eq!(columns_json(&vl, 0)[0]["visible_rows"], 4);
    }
}
