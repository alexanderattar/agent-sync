mod adapters;
mod apply;
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
pub use discover::{discover, AgentInventory, Inventory};
pub use pack::{export_pack, init_pack, ExportOptions, ExportReport, InitReport, SourceSelection};
