mod adapters;
mod apply;
mod cursor_history;
mod discover;
mod fsx;
mod manifest;
mod mcp;
mod pack;

pub use adapters::{AgentKind, AgentPaths};
pub use apply::{
    apply_pack, diff_pack, format_diff, verify_pack, ApplyOptions, ApplyReport, Change,
    ChangeAction, VerifyReport,
};
pub use cursor_history::{
    export_cursor_history, export_cursor_history_from_stdin, install_cursor_history_hook,
    CursorHistoryInstallReport,
};
pub use discover::{discover, AgentInventory, Inventory};
pub use pack::{export_pack, init_pack, ExportOptions, ExportReport, InitReport, SourceSelection};
