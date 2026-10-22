//! Loadout save/restore data model.
//!
//! Pure serde types for serializing workspace snapshots to JSON. The daemon
//! reads/writes the loadout file — this module has no I/O or Win32 dependencies.

mod path;
mod types;

pub use path::{validate_default_path, validate_loadout_path};
pub use types::{
    ColumnSnapshot, FloatingEntry, LoadoutFile, MonitorSnapshot, RectJson, RowSnapshot,
    ScrollingSnapshot, WindowRef, WorkspaceSnapshot,
};
