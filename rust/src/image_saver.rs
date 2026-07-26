use anyhow::Context;
use std::path::Path;
use tracing::info;

/// 确保 PNG 数据已落盘。
///
/// 必须在粘贴路径**之前**调用：CLI Agent（Claude Code / Codex 等）收到粘贴文本的
/// 瞬间会检查该路径文件是否存在，存在才会把它渲染成 `[Image #n]` 附件；
/// 旧版“先粘路径、后台再写文件”的顺序在系统卡顿时会让文件晚于检查落盘，
/// 表现为输入框里留下原始 `/mnt/...` 路径（issue #4）。
///
/// 剪贴板未变化的重复粘贴（缓存命中）文件已存在，直接跳过重写。
pub async fn ensure_saved(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("创建临时目录失败")?;
    }

    tokio::fs::write(path, data)
        .await
        .context("写入图片文件失败")?;

    info!("图片已保存: {}", path.display());
    Ok(())
}
