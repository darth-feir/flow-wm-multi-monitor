//! Integration tests for `flow dispatch tiling on|off|toggle`.
//!
//! A real `flowd` on an isolated test desktop: tiling is switched off and
//! on again through the CLI, and the state is read back through `flow
//! ping`, `flow query all` and the refusal every layout command gets while
//! tiling is off. The pause/resume mechanics themselves (which windows move
//! where, how the layout is rebuilt) are unit-tested in
//! `src/daemon/tiling.rs` against a mock desktop; this file proves the
//! wiring — IPC message, dispatcher gate, CLI output.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use predicates::prelude::*;

use super::common::{flow, unique_pipe_name};
use super::test_desktop::{DaemonGuard, TestDesktop, query_windows, start_test_daemon};

/// Turning tiling off and on again round-trips through the CLI, is
/// reported by `ping` and `query all`, and gates layout commands.
#[test]
fn tiling_toggle_round_trip() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    // Fresh daemon: tiling is on.
    flow(&pipe)
        .arg("ping")
        .assert()
        .stdout(predicate::str::contains("tiling on"))
        .success();

    // Off: reported by the command, the ping and the query.
    flow(&pipe)
        .args(["dispatch", "tiling", "off"])
        .assert()
        .stdout(predicate::str::contains("flow: tiling off"))
        .success();
    flow(&pipe)
        .arg("ping")
        .assert()
        .stdout(predicate::str::contains("tiling off"))
        .success();
    let all = query_windows(&pipe).expect("query all while paused");
    assert_eq!(all["tiling_enabled"], serde_json::json!(false));

    // Every layout command is refused with a pointer to the way back.
    for args in [
        &["dispatch", "focus", "left"][..],
        &["dispatch", "switch-workspace", "2"],
        &["dispatch", "set-window", "cycle"],
        &["dispatch", "center"],
    ] {
        flow(&pipe)
            .args(args)
            .assert()
            .stderr(predicate::str::contains(
                "tiling is off (flow dispatch tiling on)",
            ))
            .failure();
    }
    // Off again is a no-op that still reports the state.
    flow(&pipe)
        .args(["dispatch", "tiling", "off"])
        .assert()
        .stdout(predicate::str::contains("flow: tiling off"))
        .success();

    // Toggle brings it back on; `on` again is idempotent.
    flow(&pipe)
        .args(["dispatch", "tiling", "toggle"])
        .assert()
        .stdout(predicate::str::contains("flow: tiling on"))
        .success();
    flow(&pipe)
        .arg("ping")
        .assert()
        .stdout(predicate::str::contains("tiling on"))
        .success();
    let all = query_windows(&pipe).expect("query all after resume");
    assert_eq!(all["tiling_enabled"], serde_json::json!(true));
    flow(&pipe)
        .args(["dispatch", "tiling", "on"])
        .assert()
        .stdout(predicate::str::contains("flow: tiling on"))
        .success();
    // Layout commands are accepted again (no focused window on the empty
    // desktop is a silent Ok, not a refusal).
    flow(&pipe)
        .args(["dispatch", "focus", "left"])
        .assert()
        .stderr(predicate::str::contains("tiling is off").not())
        .success();
    // And toggle from on lands on off.
    flow(&pipe)
        .args(["dispatch", "tiling", "toggle"])
        .assert()
        .stdout(predicate::str::contains("flow: tiling off"))
        .success();
}
