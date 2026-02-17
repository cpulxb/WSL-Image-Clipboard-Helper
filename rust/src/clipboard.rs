use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5};

use tracing::info;

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

    /// 获取当前剪贴板序列号
    fn get_sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// 读取图片并准备粘贴数据（含缓存）
    /// 返回 (win_path, wsl_path, png_data)
    pub fn read_image_for_paste(&self) -> Option<(PathBuf, String, Vec<u8>)> {
        let seq = self.get_sequence();

        // 检查缓存
        if let Ok(cache) = self.cache.lock() {
            if let Some(ref cached) = *cache {
                if cached.seq == seq && seq != 0 {
                    info!("使用缓存的图片数据 (seq={})", seq);
                    return Some((
                        cached.win_path.clone(),
                        cached.wsl_path.clone(),
                        cached.png_data.clone(),
                    ));
                }
            }
        }

        // 读取新数据
        let png_data = self.get_image_data()?;

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
            });
        }

        Some((win_path, wsl_path, png_data))
    }

    /// 获取图片数据并转换为 PNG
    fn get_image_data(&self) -> Option<Vec<u8>> {
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

            CloseClipboard().ok();

            if let Some(dib) = dib_data {
                if dib.is_empty() {
                    return None;
                }
                return Self::convert_dib_to_png(&dib);
            }

            None
        }
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
                        img_data.push(pixel_data[pixel_start + 3]);
                    }
                }
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
