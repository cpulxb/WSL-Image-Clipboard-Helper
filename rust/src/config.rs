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

    /// 路径粘贴风格: "plain" (纯路径), "at" (@ 前缀), "quoted" (引号包裹)
    /// 旧版配置文件没有该字段，缺省按 plain 处理
    #[serde(default)]
    pub path_style: PathStyle,
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

/// 路径粘贴风格
/// - Plain: `/mnt/c/...`，Claude Code / Codex / OpenCode 可直接识别为图片
/// - At: `@/mnt/c/... `，适配 Kimi Code CLI / Gemini CLI / Qwen Code 等 @ 文件引用语法
/// - Quoted: `"/mnt/c/..."`，用于含空格路径或直接喂给 shell 命令
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathStyle {
    #[default]
    Plain,
    At,
    Quoted,
}

impl PathStyle {
    /// 将一组 WSL 路径格式化为最终粘贴文本
    pub fn format_paths(&self, paths: &[String]) -> String {
        match self {
            PathStyle::Plain => paths.join("\n"),
            PathStyle::Quoted => paths
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join("\n"),
            // @ 引用需要空格断词：多个路径用空格分隔，并保留尾随空格，
            // 这样 CLI 能立刻结束文件引用解析，用户也可以直接继续输入
            PathStyle::At => {
                let joined = paths
                    .iter()
                    .map(|p| format!("@{}", p))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{} ", joined)
            }
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            PathStyle::Plain => "纯路径（Claude/Codex/OpenCode）",
            PathStyle::At => "@ 路径（Kimi/Gemini/Qwen）",
            PathStyle::Quoted => "引号路径（含空格场景）",
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "!v".to_string(),
            runtime_mode: RuntimeMode::Fast,
            paste_format: PasteFormat::Plain,
            path_style: PathStyle::default(),
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
