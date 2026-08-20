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

    /// 输入法保护策略（仅在 runtime_mode = "safe" 时生效）
    /// 旧版配置文件没有该字段，缺省按 imm 处理
    #[serde(default)]
    pub ime_protection: ImeProtection,
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

/// 输入法保护策略（issue #10）
///
/// 背景：早期版本在**启动时**就无条件调用 `LoadKeyboardLayoutW("00000409", KLF_ACTIVATE)`
/// 预加载英文布局。`LoadKeyboardLayoutW` 会把该布局登记进**系统**的输入法列表，
/// 于是只要本工具一启动，`Win + Space` 里就会凭空多出
/// `ENG / English (United States)`，任务栏输入法图标也会重新出现——
/// 即使用户的语言列表里根本没有添加过英语。
///
/// 现在改为按策略惰性处理，从"零副作用"到"完全兼容旧行为"依次递进：
/// - `Off`：完全不碰输入法与键盘布局
/// - `Imm`：只关闭前台窗口的 IME 输入状态（直接输入），不触碰键盘布局 —— 默认
/// - `Layout`：切换到英文键盘布局，但**只复用系统里已加载的**英文布局，绝不新增
/// - `LayoutForce`：找不到英文布局时才惰性加载（不带 KLF_ACTIVATE），并在退出时卸载
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImeProtection {
    /// 完全关闭输入法保护
    Off,
    /// 仅关闭前台窗口 IME 输入状态（零副作用，不会新增键盘布局）
    #[default]
    Imm,
    /// 切换到英文键盘布局，但只复用已加载的布局
    Layout,
    /// 切换到英文键盘布局，必要时惰性加载（退出时卸载）
    LayoutForce,
}

impl ImeProtection {
    pub fn display_name(&self) -> &'static str {
        match self {
            ImeProtection::Off => "关闭（不干预输入法）",
            ImeProtection::Imm => "关闭 IME 输入状态（推荐，无副作用）",
            ImeProtection::Layout => "切英文布局（仅复用已装布局）",
            ImeProtection::LayoutForce => "切英文布局（必要时加载 ENG）",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ImeProtection::Off => "off",
            ImeProtection::Imm => "imm",
            ImeProtection::Layout => "layout",
            ImeProtection::LayoutForce => "layout-force",
        }
    }

    /// 该策略是否允许通过 LoadKeyboardLayoutW 向系统新增英文布局
    pub fn allows_loading_layout(&self) -> bool {
        matches!(self, ImeProtection::LayoutForce)
    }

    pub fn all() -> &'static [ImeProtection] {
        &[
            ImeProtection::Off,
            ImeProtection::Imm,
            ImeProtection::Layout,
            ImeProtection::LayoutForce,
        ]
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "!v".to_string(),
            runtime_mode: RuntimeMode::Fast,
            paste_format: PasteFormat::Plain,
            path_style: PathStyle::default(),
            ime_protection: ImeProtection::default(),
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
