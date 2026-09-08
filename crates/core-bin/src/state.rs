//! ServerState 聚合定义（避免循环依赖的薄模块）。

use std::sync::Arc;

/// IPC 层使用的聚合句柄。
pub struct ServerState {
    pub controller: Arc<crate::controller::Controller>,
}

impl ServerState {
    pub fn new(controller: crate::controller::Controller) -> Self {
        Self {
            controller: Arc::new(controller),
        }
    }
}
