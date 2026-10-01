use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use tracing::{info, warn};

#[link(name = "user32")]
extern "system" {
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardSequenceNumber() -> u32;
}

/// 缓存的图片数据
struct ImageCache {
    /// 剪贴板序列号
    seq: u32,
    /// PNG 数据
    png_data: Vec<u8>,
    /// Windows 路径
    win_path: PathBuf,
    /// WSL 路径
    wsl_path: String,
    /// 本图最近一次由程序写入的完整路径文本；None 表示仍是原始图片
    written_text: Option<String>,
}

impl ImageCache {
    fn matches(&self, seq: u32, text: Option<&str>) -> bool {
        seq != 0 && self.seq == seq && self.written_text.as_deref() == text
    }

    fn record_write(&mut self, source_seq: u32, written_seq: u32, text: &str) {
        if source_seq != 0 && self.seq == source_seq {
            self.seq = written_seq;
            self.written_text = Some(text.to_owned());
        }
    }

    fn image(&self) -> PasteImage {
        PasteImage {
            source_seq: self.seq,
            win_path: self.win_path.clone(),
            wsl_path: self.wsl_path.clone(),
            png_data: self.png_data.clone(),
        }
    }
}

/// 一次图片粘贴的数据及其剪贴板来源，用于上传后的条件写入
pub struct PasteImage {
    pub source_seq: u32,
    pub win_path: PathBuf,
    pub wsl_path: String,
    pub png_data: Vec<u8>,
}

#[cfg(test)]
mod repeat_image_tests {
    use super::*;

    fn image() -> ImageCache {
        ImageCache {
            seq: 10,
            png_data: vec![1, 2, 3],
            win_path: PathBuf::from(r"C:\temp\clip.png"),
            wsl_path: "/mnt/c/temp/clip.png".into(),
            written_text: None,
        }
    }

    #[test]
    fn repeated_image_survives_remote_local_remote_writes() {
        let mut cache = image();
        assert!(cache.matches(10, None));
        for (source, written, text) in [
            (10, 11, "/tmp/remote-a/clip.png"),
            (11, 12, "@\"/mnt/c/temp/clip.png\""),
            (12, 13, "/tmp/remote-b/clip.png"),
            (13, 14, "/tmp/remote-b/clip.png"),
        ] {
            cache.record_write(source, written, text);
            assert!(cache.matches(written, Some(text)));
            assert!(!cache.matches(source, Some(text)));
            assert!(!cache.matches(written, None));
            let data = cache.image();
            assert_eq!(data.source_seq, written);
            assert_eq!(data.wsl_path, "/mnt/c/temp/clip.png");
            assert_eq!(data.png_data, vec![1, 2, 3]);
        }
    }

    #[test]
    fn new_copy_even_of_identical_text_is_not_the_cached_image() {
        let mut cache = image();
        cache.record_write(10, 11, "/tmp/clip.png");
        assert!(!cache.matches(12, Some("/tmp/clip.png")));
        assert!(!cache.matches(11, Some("ordinary text")));
        assert!(!cache.matches(12, None)); // 新图片或文件列表
        assert!(!cache.matches(0, Some("/tmp/clip.png")));
        cache.record_write(10, 12, "stale operation");
        assert!(cache.matches(11, Some("/tmp/clip.png")));
    }

    #[link(name = "user32")]
    extern "system" {
        fn CreateWindowStationW(name: *const u16, flags: u32, access: u32, security: *const u8) -> isize;
        fn GetProcessWindowStation() -> isize;
        fn SetProcessWindowStation(station: isize) -> i32;
        fn CloseWindowStation(station: isize) -> i32;
    }

    #[test]
    #[ignore = "仅显式单线程运行：在独立 window station 中测试真实剪贴板，不发送按键"]
    fn isolated_clipboard_remote_then_local() {
        use crate::paste::write_text;
        use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
        use windows::Win32::System::Memory::{GlobalAlloc, GMEM_MOVEABLE};

        struct Station { original: isize, isolated: isize }
        impl Drop for Station {
            fn drop(&mut self) {
                unsafe {
                    assert_ne!(SetProcessWindowStation(self.original), 0);
                    assert_ne!(CloseWindowStation(self.isolated), 0);
                }
            }
        }
        unsafe {
            let name: Vec<u16> = format!("WslClipboardTest-{}-{}", std::process::id(),
                chrono::Utc::now().timestamp_millis()).encode_utf16().chain(Some(0)).collect();
            let original = GetProcessWindowStation();
            assert_ne!(original, 0);
            let isolated = CreateWindowStationW(name.as_ptr(), 0, 0x037f, std::ptr::null());
            assert_ne!(isolated, 0, "无法创建隔离 window station，禁止回退到用户剪贴板: {}", windows::core::Error::from_win32());
            if SetProcessWindowStation(isolated) == 0 {
                CloseWindowStation(isolated);
                panic!("无法切换到隔离 window station，禁止操作用户剪贴板");
            }
            let _station = Station { original, isolated };
            assert_eq!(GetProcessWindowStation(), isolated);

            // 1x1、24 位的 DIB：40 字节头 + 4 字节对齐像素
            let mut dib = vec![0u8; 44];
            dib[0..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&1i32.to_le_bytes());
            dib[8..12].copy_from_slice(&1i32.to_le_bytes());
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&24u16.to_le_bytes());
            dib[20..24].copy_from_slice(&4u32.to_le_bytes());
            dib[40..43].copy_from_slice(&[0, 0, 255]);
            OpenClipboard(None).unwrap();
            EmptyClipboard().unwrap();
            let mem = GlobalAlloc(GMEM_MOVEABLE, dib.len()).unwrap();
            let ptr = GlobalLock(mem) as *mut u8;
            assert!(!ptr.is_null());
            std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr, dib.len());
            let _ = GlobalUnlock(mem);
            SetClipboardData(CF_DIB.0 as u32, HANDLE(mem.0 as isize)).unwrap();
            CloseClipboard().unwrap();

            let manager = ClipboardManager::new(PathBuf::from(r"C:\temp"));
            assert!(manager.has_image());
            let first = manager.read_image_for_paste().unwrap();
            assert!(!first.png_data.is_empty());
            let mut source = first.source_seq;
            for text in ["/tmp/remote/clip.png", first.wsl_path.as_str(), "/tmp/remote/clip.png"] {
                let written = write_text(text, Some(source)).unwrap().unwrap();
                manager.record_image_text(source, written, text);
                assert!(!manager.has_image());
                let cached = manager.read_own_image_text().unwrap().expect("自身路径应仍可识别为原图");
                assert_eq!(cached.png_data, first.png_data);
                assert_eq!(cached.win_path, first.win_path);
                assert_eq!(cached.source_seq, written, "锁内取得的序号必须在关闭剪贴板后仍有效");
                source = written;
            }
            // 模拟用户复制完全相同的文本，不能复用，也不能被旧上传结果覆盖
            write_text("/tmp/remote/clip.png", None).unwrap();
            assert!(manager.read_own_image_text().unwrap().is_none());
            assert!(write_text("stale upload", Some(source)).unwrap().is_none());
            OpenClipboard(None).unwrap();
            assert_eq!(ClipboardManager::read_unicode_text().as_deref(), Some("/tmp/remote/clip.png"));
            CloseClipboard().unwrap();
        }
    }

    #[test]
    fn successful_write_remains_retryable_without_successful_input() {
        let mut cache = image();
        // 写入后立即登记；按键发送是否成功不改变这个状态
        cache.record_write(10, 11, "/tmp/clip.png");
        assert!(cache.matches(11, Some("/tmp/clip.png")));
        // 无法取得写入序号时，不允许复用之前的凭证
        cache.record_write(11, 0, "/mnt/c/temp/clip.png");
        assert!(!cache.matches(0, Some("/mnt/c/temp/clip.png")));
        assert!(!cache.matches(11, Some("/tmp/clip.png")));
    }
}

/// 剪贴板管理器
pub struct ClipboardManager {
    temp_dir: PathBuf,
    wsl_temp_dir: String,
    cache: Mutex<Option<ImageCache>>,
}

impl ClipboardManager {
    pub fn new(temp_dir: PathBuf) -> Self {
        // 预计算 WSL 路径（匹配 AHK 的 gWslTempDir 优化）
        let wsl_temp_dir = convert_path_to_wsl(&temp_dir.to_string_lossy());

        info!("WSL 临时目录: {}", wsl_temp_dir);

        Self {
            temp_dir,
            wsl_temp_dir,
            cache: Mutex::new(None),
        }
    }

    /// 检查剪贴板是否有图片（不需要打开剪贴板，更快）
    pub fn has_image(&self) -> bool {
        unsafe {
            IsClipboardFormatAvailable(CF_BITMAP.0 as u32) != 0
                || IsClipboardFormatAvailable(CF_DIB.0 as u32) != 0
                || IsClipboardFormatAvailable(CF_DIBV5.0 as u32) != 0
        }
    }

    pub fn has_file_list(&self) -> bool {
        unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) != 0 }
    }

    pub fn read_file_list_for_paste(&self) -> Option<Vec<String>> {
        let paths = self.get_file_paths()?;
        let wsl_paths: Vec<String> = paths
            .iter()
            .map(|path| convert_path_to_wsl(path))
            .filter(|path| !path.is_empty())
            .collect();

        if wsl_paths.is_empty() {
            None
        } else {
            Some(wsl_paths)
        }
    }

    /// 读取 Explorer 复制的原始 Windows 路径（远程模式下需要按本地路径打开文件上传）
    pub fn read_file_list_raw(&self) -> Option<Vec<String>> {
        let paths: Vec<String> = self
            .get_file_paths()?
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }

    /// 获取当前剪贴板序列号
    fn get_sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// 只认本程序为缓存图片写入的文本，不把用户复制的路径当作图片。
    pub fn read_own_image_text(&self) -> anyhow::Result<Option<PasteImage>> {
        // 普通文本不必打开剪贴板；若可能是自身文本但打不开，应报错而不是粘旧路径。
        let seq = self.get_sequence();
        let candidate = self.cache.lock().ok().is_some_and(|cache| {
            info!(current_seq = seq, cached_seq = ?cache.as_ref().map(|c| c.seq),
                has_written_text = cache.as_ref().is_some_and(|c| c.written_text.is_some()),
                "检查自身图片路径缓存");
            cache.as_ref().is_some_and(|cached| {
                seq != 0 && cached.seq == seq && cached.written_text.is_some()
            })
        });
        if !candidate {
            return Ok(None);
        }
        unsafe {
            OpenClipboard(None).map_err(|e| anyhow::anyhow!("无法读取缓存图片对应的剪贴板文本: {}", e))?;
            let result = (|| {
                let seq = self.get_sequence();
                let cache = self.cache.lock().ok()?;
                let cached = cache.as_ref()?;
                if seq == 0 || cached.seq != seq || cached.written_text.is_none() {
                    return None;
                }
                let text = Self::read_unicode_text()?;
                if cached.matches(seq, Some(&text)) && self.get_sequence() == seq {
                    Some(cached.image())
                } else {
                    None
                }
            })();
            CloseClipboard().ok();
            info!(cache_hit = result.is_some(), "自身图片路径校验完成");
            Ok(result)
        }
    }

    pub(crate) unsafe fn read_unicode_text() -> Option<String> {
        let data = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let mem = HGLOBAL(data.0 as *mut _);
        let len = GlobalSize(mem) / std::mem::size_of::<u16>();
        let ptr = GlobalLock(mem) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(ptr, len);
        let text = units.iter().position(|&u| u == 0)
            .and_then(|end| String::from_utf16(&units[..end]).ok());
        let _ = GlobalUnlock(mem);
        text
    }

    pub fn record_image_text(&self, source_seq: u32, written_seq: u32, text: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(cached) = cache.as_mut() {
                cached.record_write(source_seq, written_seq, text);
                info!(source_seq, written_seq, cached_seq = cached.seq, "登记图片路径写入凭证");
            }
        }
    }

    /// 读取真实图片并准备粘贴数据（含缓存及来源序号）
    pub fn read_image_for_paste(&self) -> Option<PasteImage> {
        let seq = self.get_sequence();
        if let Ok(cache) = self.cache.lock() {
            if let Some(ref cached) = *cache {
                if cached.matches(seq, None) {
                    info!("使用缓存的图片数据 (seq={})", seq);
                    return Some(cached.image());
                }
            }
        }

        // 数据与 sequence 在同一次打开剪贴板期间取得
        let (seq, png_data) = self.get_image_data()?;

        // 生成文件名和路径
        let now = chrono::Local::now();
        let timestamp = now.format("%Y%m%d_%H%M%S_%3f").to_string();
        let filename = format!("clip_{}.png", timestamp);

        let win_path = self.temp_dir.join(&filename);
        let wsl_path = if !self.wsl_temp_dir.is_empty() {
            format!("{}/{}", self.wsl_temp_dir, filename)
        } else {
            convert_path_to_wsl(&win_path.to_string_lossy())
        };

        // 更新缓存
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(ImageCache {
                seq,
                png_data: png_data.clone(),
                win_path: win_path.clone(),
                wsl_path: wsl_path.clone(),
                written_text: None,
            });
        }

        Some(PasteImage { source_seq: seq, win_path, wsl_path, png_data })
    }

    fn get_file_paths(&self) -> Option<Vec<String>> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }

            let paths = if let Ok(h_data) = GetClipboardData(CF_HDROP.0 as u32) {
                Self::read_hdrop_paths(HDROP(h_data.0 as isize))
            } else {
                Vec::new()
            };

            CloseClipboard().ok();

            if paths.is_empty() {
                None
            } else {
                Some(paths)
            }
        }
    }

    /// 获取图片数据并转换为 PNG
    fn get_image_data(&self) -> Option<(u32, Vec<u8>)> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }

            // 尝试获取 DIB 数据
            let dib_data = if let Ok(h_data) = GetClipboardData(CF_DIB.0 as u32) {
                Some(Self::read_dib_data(h_data))
            } else if let Ok(h_data) = GetClipboardData(CF_DIBV5.0 as u32) {
                Some(Self::read_dib_data(h_data))
            } else {
                CloseClipboard().ok();
                return None;
            };

            // GetClipboardData 可触发延迟渲染；在数据读取后、释放锁前绑定序号
            let seq = self.get_sequence();
            CloseClipboard().ok();

            if let Some(dib) = dib_data {
                if dib.is_empty() || seq == 0 {
                    return None;
                }
                return Self::convert_dib_to_png(&dib).map(|png| (seq, png));
            }

            None
        }
    }

    unsafe fn read_hdrop_paths(hdrop: HDROP) -> Vec<String> {
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::new();

        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, None);
            if len == 0 {
                continue;
            }

            let mut buffer = vec![0u16; len as usize + 1];
            let copied = DragQueryFileW(hdrop, index, Some(&mut buffer));
            if copied == 0 {
                warn!("读取剪贴板文件路径失败: index={}", index);
                continue;
            }

            paths.push(String::from_utf16_lossy(&buffer[..copied as usize]));
        }

        paths
    }

    /// 从剪贴板读取 DIB 数据
    unsafe fn read_dib_data(h_data: HANDLE) -> Vec<u8> {
        const MAX_DIB_SIZE: usize = 100 * 1024 * 1024;

        let h_global = HGLOBAL(h_data.0 as *mut std::ffi::c_void);
        let ptr = GlobalLock(h_global);

        if ptr.is_null() {
            return Vec::new();
        }

        let data = (|| {
            let global_size = GlobalSize(h_global);
            if global_size < BITMAPINFOHEADER_SIZE {
                return Vec::new();
            }

            let header = std::slice::from_raw_parts(ptr as *const u8, BITMAPINFOHEADER_SIZE);
            let info = read_bitmap_info(header);

            // 从 packed struct 复制字段到本地变量（避免对齐问题）
            let bi_width = info.bi_width.unsigned_abs() as usize;
            let bi_height = info.bi_height.unsigned_abs() as usize;
            let bi_bit_count = info.bi_bit_count;
            let bi_size = info.bi_size;
            let bi_compression = info.bi_compression;
            let bi_clr_used = info.bi_clr_used;

            let expected_size = calculate_dib_copy_size(
                bi_size,
                bi_bit_count,
                bi_compression,
                bi_clr_used,
                bi_width,
                bi_height,
            )
            .unwrap_or(global_size);

            // 读取大小同时受预估大小、实际分配大小和上限保护
            let read_size = expected_size.min(global_size).min(MAX_DIB_SIZE);
            if read_size == 0 {
                return Vec::new();
            }

            let mut data = vec![0u8; read_size];
            std::ptr::copy_nonoverlapping(ptr as *const u8, data.as_mut_ptr(), read_size);
            data
        })();

        let _ = GlobalUnlock(h_global);

        data
    }

    /// 将 DIB 数据转换为 PNG
    fn convert_dib_to_png(dib_data: &[u8]) -> Option<Vec<u8>> {
        if dib_data.len() < std::mem::size_of::<BITMAPINFOHEADER>() {
            return None;
        }

        let info = read_bitmap_info(dib_data);

        // 从 packed struct 复制字段到本地变量（避免对齐问题）
        let bi_width = info.bi_width;
        let bi_height = info.bi_height;
        let bi_bit_count = info.bi_bit_count;
        let bi_size = info.bi_size;
        let bi_compression = info.bi_compression;
        let bi_clr_used = info.bi_clr_used;

        // 只支持 24位和 32位 DIB
        if bi_bit_count != 24 && bi_bit_count != 32 {
            tracing::warn!("不支持的位深度: {} 位", bi_bit_count);
            return None;
        }

        if bi_width == 0 || bi_height == 0 {
            return None;
        }

        let width = bi_width.unsigned_abs() as usize;
        let height = bi_height.unsigned_abs() as usize;
        let bottom_up = bi_height > 0;

        let pixel_offset =
            calculate_dib_pixel_offset(bi_size, bi_bit_count, bi_compression, bi_clr_used)?;

        let channels = if bi_bit_count == 32 { 4 } else { 3 };
        let row_size = calculate_row_size(width, bi_bit_count)?;
        let image_size = row_size.checked_mul(height)?;
        let pixel_end = pixel_offset.checked_add(image_size)?;

        if dib_data.len() < pixel_end {
            return None;
        }

        let pixel_data = &dib_data[pixel_offset..pixel_end];
        let has_alpha = bi_bit_count == 32;

        let capacity = width.checked_mul(height)?.checked_mul(channels)?;
        let mut img_data = Vec::with_capacity(capacity);
        let mut alpha_any_nonzero = false;

        // DIB bottom-up 时行从下到上存储
        for y in 0..height {
            let src_y = if bottom_up { height - 1 - y } else { y };
            let row_start = src_y * row_size;
            for x in 0..width {
                let pixel_start = row_start + x * channels;

                if pixel_start + channels <= pixel_data.len() {
                    // BGR(A) -> RGB(A)
                    let b = pixel_data[pixel_start];
                    let g = pixel_data[pixel_start + 1];
                    let r = pixel_data[pixel_start + 2];

                    img_data.push(r);
                    img_data.push(g);
                    img_data.push(b);

                    if has_alpha {
                        let a = pixel_data[pixel_start + 3];
                        if a != 0 {
                            alpha_any_nonzero = true;
                        }
                        img_data.push(a);
                    }
                }
            }
        }

        // 32 位 BI_RGB DIB 的第 4 字节在很多来源里是保留填充（恒为 0）而非真实 alpha；
        // 若整张图 alpha 全 0，按不透明处理（与浏览器处理剪贴板 DIB 的启发式一致），
        // 否则导出的 PNG 全透明，AI 工具读到的是一张"空白图"
        if has_alpha && !alpha_any_nonzero {
            for a in img_data.iter_mut().skip(3).step_by(4) {
                *a = 255;
            }
        }

        // 使用 image crate 创建并编码 PNG
        #[cfg(feature = "image-support")]
        {
            use image::{DynamicImage, ImageBuffer};

            let img: DynamicImage = if has_alpha {
                DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(width as u32, height as u32, img_data)?,
                )
            } else {
                DynamicImage::ImageRgb8(
                    ImageBuffer::from_raw(width as u32, height as u32, img_data)?,
                )
            };

            let mut buffer = Cursor::new(Vec::new());
            if img.write_to(&mut buffer, image::ImageFormat::Png).is_ok() {
                return Some(buffer.into_inner());
            }
        }

        #[cfg(not(feature = "image-support"))]
        {
            tracing::warn!("image-support feature 未启用");
        }

        None
    }
}

/// 将 Windows 路径转换为 WSL 路径
pub fn convert_path_to_wsl(path_str: &str) -> String {
    let path_str = path_str.trim_matches('"');

    // 处理驱动器路径 "C:\path\to\file"
    if path_str.len() >= 3 && path_str.as_bytes()[1] == b':' && path_str.as_bytes()[2] == b'\\' {
        let drive = &path_str[0..1];
        let rest = path_str[3..].replace('\\', "/");
        let rest = rest.trim_start_matches('/');
        return format!("/mnt/{}/{}", drive.to_lowercase(), rest);
    }

    // 处理没有反斜杠的路径（如 D:\temp 变为 D:/temp）
    if path_str.len() >= 3 && path_str.as_bytes()[1] == b':' {
        let drive = &path_str[0..1];
        let rest = path_str[2..].replace('\\', "/");
        let rest = rest.trim_start_matches('/');
        return format!("/mnt/{}/{}", drive.to_lowercase(), rest);
    }

    String::new()
}

/// BITMAPINFOHEADER 结构（部分字段）
#[repr(C, packed)]
struct BITMAPINFOHEADER {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

const BITMAPINFOHEADER_SIZE: usize = std::mem::size_of::<BITMAPINFOHEADER>();
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;
const RGBQUAD_SIZE: usize = 4;

fn read_bitmap_info(data: &[u8]) -> BITMAPINFOHEADER {
    let header_size = BITMAPINFOHEADER_SIZE;
    let mut header = BITMAPINFOHEADER {
        bi_size: 0,
        bi_width: 0,
        bi_height: 0,
        bi_planes: 0,
        bi_bit_count: 0,
        bi_compression: 0,
        bi_size_image: 0,
        bi_x_pels_per_meter: 0,
        bi_y_pels_per_meter: 0,
        bi_clr_used: 0,
        bi_clr_important: 0,
    };

    if data.len() >= header_size {
        let bytes = &data[0..header_size.min(40)];
        let ptr = bytes.as_ptr() as *const BITMAPINFOHEADER;
        unsafe { std::ptr::copy_nonoverlapping(ptr, &mut header, 1) };
    }

    header
}

fn calculate_row_size(width: usize, bit_count: u16) -> Option<usize> {
    if width == 0 || bit_count == 0 {
        return None;
    }

    let row_bits = width.checked_mul(usize::from(bit_count))?;
    row_bits.checked_add(31)?.checked_div(32)?.checked_mul(4)
}

fn calculate_palette_size(bit_count: u16, clr_used: u32) -> Option<usize> {
    if bit_count > 8 {
        return Some(0);
    }

    let entries = if clr_used > 0 {
        usize::try_from(clr_used).ok()?
    } else {
        1usize.checked_shl(u32::from(bit_count))?
    };

    entries.checked_mul(RGBQUAD_SIZE)
}

fn calculate_dib_pixel_offset(
    bi_size: u32,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_clr_used: u32,
) -> Option<usize> {
    let header_size = usize::try_from(bi_size).ok()?;
    if header_size < BITMAPINFOHEADER_SIZE {
        return None;
    }

    // BITMAPINFOHEADER + BI_BITFIELDS 时，掩码位于 header 与像素之间。
    let mask_size = if header_size == BITMAPINFOHEADER_SIZE
        && (bi_compression == BI_BITFIELDS || bi_compression == BI_ALPHABITFIELDS)
    {
        if bi_compression == BI_ALPHABITFIELDS {
            16
        } else {
            12
        }
    } else {
        0
    };

    let palette_size = calculate_palette_size(bi_bit_count, bi_clr_used)?;
    header_size.checked_add(mask_size)?.checked_add(palette_size)
}

fn calculate_dib_copy_size(
    bi_size: u32,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_clr_used: u32,
    width: usize,
    height: usize,
) -> Option<usize> {
    let pixel_offset = calculate_dib_pixel_offset(bi_size, bi_bit_count, bi_compression, bi_clr_used)?;
    let row_size = calculate_row_size(width, bi_bit_count)?;
    let image_size = row_size.checked_mul(height)?;
    pixel_offset.checked_add(image_size)
}
