use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub fn temp_dir_from_current_exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()
        .context("获取可执行文件路径失败")?
        .parent()
        .map(|p| p.join("temp"))
        .unwrap_or_else(|| PathBuf::from("./temp")))
}

/// 退出时清理 temp 目录下的所有 PNG 文件。
pub fn cleanup_temp_png(temp_dir: &Path) -> Result<()> {
    if !temp_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(temp_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !is_png_file(&path) {
            continue;
        }

        if let Err(e) = fs::remove_file(&path) {
            warn!("删除临时文件失败 {}: {}", path.display(), e);
        } else {
            info!("退出清理: {}", path.display());
        }
    }

    Ok(())
}

/// 清理超过 2 小时的临时 PNG 文件。
pub fn cleanup_old_files(temp_dir: &Path) -> Result<()> {
    let now = Local::now();

    if !temp_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(temp_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !is_png_file(&path) {
            continue;
        }

        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                let modified_time = DateTime::<Local>::from(modified);
                let hours_old = now.signed_duration_since(modified_time).num_hours();

                if hours_old > 2 {
                    if let Err(e) = fs::remove_file(&path) {
                        warn!("删除过期文件失败 {}: {}", path.display(), e);
                    } else {
                        info!("删除过期文件: {}", path.display());
                    }
                }
            }
        }
    }

    Ok(())
}

fn is_png_file(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().eq_ignore_ascii_case("png"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::cleanup_temp_png;

    #[test]
    fn cleanup_temp_png_deletes_png_files_only() {
        let temp_root = std::env::temp_dir().join(format!(
            "wsl_clipboard_cleanup_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_root);
        std::fs::create_dir_all(&temp_root).unwrap();

        let png_path = temp_root.join("clip_test.png");
        let upper_png_path = temp_root.join("clip_upper.PNG");
        let txt_path = temp_root.join("keep.txt");
        std::fs::write(&png_path, b"png").unwrap();
        std::fs::write(&upper_png_path, b"png").unwrap();
        std::fs::write(&txt_path, b"text").unwrap();

        cleanup_temp_png(&temp_root).unwrap();

        assert!(!png_path.exists());
        assert!(!upper_png_path.exists());
        assert!(txt_path.exists());

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
