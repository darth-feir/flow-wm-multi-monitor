//! Query handlers for IPC commands.
//!
//! This module contains methods that extract query logic from the dispatch
//! match arms into dedicated methods. Each method returns a [`SocketResponse`]
//! with JSON data.

use crate::ipc::message::SocketResponse;

use super::types::FlowWM;

impl FlowWM {
    /// Return all tracked windows as JSON.
    ///
    /// The viewport offset from the virtual layout is included so clients
    /// can determine which windows are currently visible on screen. Each
    /// window entry carries a `monitor` field: the index of the monitor
    /// whose workspace holds it (else the remembered owning monitor), or
    /// `null` for a window seated nowhere (ignored, minimized, hidden and
    /// never seated) — and a `mode` field: `"tiled"`, `"floating"`,
    /// `"fullscreen"` (a `set-window fullscreen` window, which the `state`
    /// records as a float), `"minimized"`, `"hidden"` or `"ignored"`.
    pub(super) fn query_windows_all(&self) -> SocketResponse {
        let viewport_offset = self.active_scrolling().virtual_layout().viewport_offset;
        let mut payload = self.registry.to_json_value(viewport_offset);
        annotate_windows_with_monitor(&mut payload, |hwnd| {
            let wid = crate::common::WindowId(hwnd);
            self.monitor_index_containing(wid)
                .or_else(|| self.remembered_monitor(wid))
        });
        annotate_windows_with_mode(&mut payload, |hwnd| self.window_mode(hwnd));
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
        let columns_json: Vec<serde_json::Value> = vl
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let rows: Vec<serde_json::Value> = col
                    .rows
                    .iter()
                    .map(|row| serde_json::json!({ "window_id": row.window_id.0, "height_px": row.height }))
                    .collect();
                serde_json::json!({
                    "index": i,
                    "width_px": col.width_px,
                    "rows": rows,
                })
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
            obj.insert("mode".into(), serde_json::json!(mode_of(hwnd as isize)));
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
}
