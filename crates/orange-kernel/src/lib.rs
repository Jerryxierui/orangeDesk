//! orangeDesk 微内核 —— 只做四件事：总线、预算、注册、守护（§2 内核纪律）。
//!
//! 内核不包含任何具体渲染逻辑，不直接持有 DirectX 设备。
//! 新增能力 = 注册一个实现 [`orange_module_api::Module`] 的新模块。

pub mod budget;
pub mod bus;
pub mod config;
pub mod platform;
pub mod registry;
pub mod sampling;
pub mod single_instance;
pub mod supervisor;

/// 内核级错误
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("another orangeDesk instance is already running")]
    AlreadyRunning,
    #[error("configuration error: {0}")]
    Config(String),
    #[error("module registry error: {0}")]
    Registry(#[from] registry::RegistryError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
