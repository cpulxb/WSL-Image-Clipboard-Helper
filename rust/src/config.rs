use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 热键组合: "!v", "^!v", "!Enter"
    pub hotkey: String,

    /// 运行模式: "safe" (输入法保护), "fast" (极速)
    pub runtime_mode: RuntimeMode,

    /// 粘贴格式: "plain" (路径), "attachment" (附件)
    pub paste_format: PasteFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Safe,
    Fast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasteFormat {
    Plain,
    Attachment,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "!v".to_string(),
            runtime_mode: RuntimeMode::Fast,
            paste_format: PasteFormat::Plain,
        }
    }
}

impl AppConfig {
    pub fn load() -> anyhow::Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            // 创建默认配置
            let default = Self::default();
            default.save()?;
            return Ok(default);
        }

        let content = std::fs::read_to_string(&config_path)
            .context("读取配置文件失败")?;

        let config: Self = toml::from_str(&content)
            .context("解析配置文件失败")?;

        Ok(config)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        let content = toml::to_string_pretty(self)
            .context("序列化配置失败")?;

        std::fs::write(&path, content)
            .context("写入配置文件失败")?;

        Ok(())
    }

    fn config_path() -> anyhow::Result<PathBuf> {
        let exe_dir = std::env::current_exe()
            .context("获取可执行文件路径失败")?
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(exe_dir.join("wsl_clipboard.toml"))
    }
}
