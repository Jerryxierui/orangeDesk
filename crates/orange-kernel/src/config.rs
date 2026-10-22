//! 配置持久化（§2：内核负责配置的加载 / 迁移 / 默认值填充）。
//!
//! P2「默认即好看」：所有配置都有内置默认值，全新安装零配置可运行；
//! 模块私有配置放在 `modules.<module_id>` 段，由模块自行解释（缺失即用模块默认值）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 当前配置 schema 版本（每次破坏性结构变更 +1，旧配置走迁移）
pub const SCHEMA_VERSION: u32 = 1;

/// 应用根配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub schema_version: u32,
    /// 各模块配置段，键为模块 id
    pub modules: BTreeMap<String, serde_json::Value>,
    pub logging: LoggingSettings,
    /// 当前激活皮肤文件路径（§13.2 L2；启动时加载并覆盖对应模块段）
    pub active_skin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingSettings {
    /// log level filter: "error" | "warn" | "info" | "debug" | "trace"
    pub level: String,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            modules: BTreeMap::new(),
            logging: LoggingSettings::default(),
            active_skin: None,
        }
    }
}

/// 配置目录（Windows: %APPDATA%\orangeDesk；Unix: $XDG_CONFIG_HOME/orange-desk 或 ~/.config/orange-desk）
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        // APPDATA 由系统保证存在；防御性回退到用户目录
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home_dir_fallback)
            .join("orangeDesk")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir_fallback().join(".config"))
            .join("orange-desk")
    }
}

fn home_dir_fallback() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        // 极端情况（无 HOME / APPDATA）：当前目录
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// 性能仪表盘快照文件（supervisor 周期写入，`--status` / 卫星进程读取）
pub fn status_path() -> PathBuf {
    config_dir().join("status.json")
}

/// 加载配置；文件不存在时写入默认配置（首启动体验：零配置直接可跑）。
///
/// 返回 (config, 实际路径)。读取失败时给出明确错误（P3：绝不静默吞配置损坏）。
pub fn load_or_create() -> std::io::Result<(AppConfig, PathBuf)> {
    let path = config_path();
    if !path.exists() {
        let default = AppConfig::default();
        save(&default, &path)?;
        log::info!("created default config at {}", path.display());
        return Ok((default, path));
    }
    let raw = std::fs::read_to_string(&path)?;
    let mut config: AppConfig = serde_json::from_str(&raw).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config parse error at {}: {err}", path.display()),
        )
    })?;
    migrate(&mut config, &path);
    Ok((config, path))
}

/// schema 迁移钩子（v1.0 前只有占位：旧版本无损升级）
fn migrate(config: &mut AppConfig, path: &Path) {
    if config.schema_version == SCHEMA_VERSION {
        return;
    }
    log::info!(
        "migrating config {} from schema v{} to v{}",
        path.display(),
        config.schema_version,
        SCHEMA_VERSION
    );
    // v0.x：尚无破坏性变更；迁移后统一写回当前版本号
    config.schema_version = SCHEMA_VERSION;
    let _ = save(config, path);
}

pub fn save(config: &AppConfig, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(config)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    // 先写临时文件再原子重命名：断电 / 崩溃不产生半截配置
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_complete() {
        let config = AppConfig::default();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        assert_eq!(config.logging.level, "info");
        // 默认配置可序列化往返（持久化无损）
        let text = serde_json::to_string(&config).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 旧配置缺字段 → serde(default) 兜底，而不是解析失败
        let partial = r#"{"schema_version": 1, "modules": {"wallpaper": {"mode": "solid"}}}"#;
        let config: AppConfig = serde_json::from_str(partial).unwrap();
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.modules.get("wallpaper").unwrap()["mode"], "solid");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("orangedesk-test-{}", std::process::id()));
        let path = dir.join("nested/config.json");
        let mut config = AppConfig::default();
        config.modules.insert(
            "wallpaper".to_string(),
            serde_json::json!({"solid_color": "#F76B1C"}),
        );
        save(&config, &path).unwrap();
        let (loaded, _) = load_from(&path);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn load_from(path: &Path) -> (AppConfig, PathBuf) {
        let raw = std::fs::read_to_string(path).unwrap();
        let config: AppConfig = serde_json::from_str(&raw).unwrap();
        (config, path.to_path_buf())
    }
}
