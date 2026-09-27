//! 第一版默认后端；宿主可按能力单独替换。

mod events;
mod permissions;
mod services;
mod state;

pub use events::SyncEventBus;
pub use permissions::DeclaredPermissionChecker;
pub use services::MemoryServiceRegistry;
pub use state::MemoryStateStore;
