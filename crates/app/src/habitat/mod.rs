//! 像素栖息地：三位住户的实时状态面板。
//!
//! `state` 是纯状态机，`view` 是 gpui 视图。

mod state;
mod view;

// 数据层接线（C / D 块）用的类型，bin 里暂时没有调用方。
#[allow(unused_imports)]
pub use state::{
    ActivityKind, HabitatEvent, HabitatState, Resident, ResidentId, ResidentState, TimeOfDay,
};
pub use view::{HabitatSize, HabitatView};
