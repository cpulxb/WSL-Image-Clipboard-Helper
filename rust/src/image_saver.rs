use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub fn start_saver() -> mpsc::Sender<(PathBuf, Vec<u8>)> {
    let (tx, mut rx) = mpsc::channel::<(PathBuf, Vec<u8>)>(64);

    tokio::spawn(async move {
        while let Some((path, data)) = rx.recv().await {
            if let Err(e) = save_image(&path, &data).await {
                warn!("保存图片失败 {}: {}", path.display(), e);
            } else {
                info!("图片已保存: {}", path.display());
            }
        }
    });

    tx
}

async fn save_image(path: &PathBuf, data: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(path, data)
        .await
        .context("写入图片文件失败")?;

    Ok(())
}
