//! 第一版默认后端；宿主可按能力单独替换。

mod events;
mod file_state;
mod logging;
mod permissions;
mod services;
mod state;
mod tasks;

pub use events::SyncEventBus;
pub use file_state::FileStateStore;
pub use logging::{MemoryLogSnapshot, MemoryLogger, StderrLogger, WriterLogger};
pub use permissions::DeclaredPermissionChecker;
pub use services::MemoryServiceRegistry;
pub use state::MemoryStateStore;
pub use tasks::TokioTaskManager;
