use crate::AppError;
use std::{error::Error, fmt};

/// 同时保留原始失败（含已生成回复）与输出/停止/日志失败，避免只剩错误字符串。
pub struct AppFailure {
    pub primary: AppError,
    pub secondary: Vec<AppError>,
}
impl fmt::Debug for AppFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppFailure")
            .field("message", &self.to_string())
            .finish()
    }
}
impl fmt::Display for AppFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.primary.fmt(f)?;
        for error in &self.secondary {
            write!(f, "；收尾或输出失败：{error}")?;
        }
        Ok(())
    }
}
impl Error for AppFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.as_ref())
    }
}
