//! 各层共用的类型。这里只放数据定义，不放逻辑。
//!
//! 带 `#[ts(export)]` 的类型会在 `cargo test` 时导出到
//! `web/src/api/generated/`，前端直接引用，不手写第二份。

mod api;
mod event;
mod ids;
mod model;

pub use api::*;
pub use event::*;
pub use ids::*;
pub use model::*;

/// 自 Unix 纪元起的毫秒数。
pub type Millis = i64;
