//! 文件元数据读取与格式化工具

use chrono::{DateTime, Datelike, Local};
use std::path::Path;
use std::time::SystemTime;

/// 单个目录条目的原始信息（与 Slint FileEntry 对应）
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_ts: i64,
    pub kind: String,
    pub icon_label: String,
    pub icon_class: String,
}

/// 将字节数格式化为友好显示
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// 将 SystemTime 转为友好的中文修改时间显示
pub fn human_time(time: SystemTime) -> String {
    let dt: DateTime<Local> = time.into();
    let now = Local::now();
    let today = now.date_naive();
    let date = dt.date_naive();
    let diff = (today - date).num_days();

    let hm = dt.format("%H:%M").to_string();
    if diff == 0 {
        format!("今天 {}", hm)
    } else if diff == 1 {
        format!("昨天 {}", hm)
    } else if diff > 1 && diff < 7 {
        let weekdays = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
        let wd = date.weekday().num_days_from_sunday() as usize;
        format!("{} {}", weekdays[wd], hm)
    } else {
        dt.format("%Y/%m/%d %H:%M").to_string()
    }
}

/// 精确日期时间（属性对话框用）
pub fn full_time(time: SystemTime) -> String {
    let dt: DateTime<Local> = time.into();
    dt.format("%Y/%m/%d %H:%M:%S").to_string()
}

/// 取 Unix 时间戳秒
pub fn unix_ts(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 从时间戳秒还原为友好显示（列表/网格用）。
/// 0 表示时间未知（如便携设备条目未提供修改日期），显示为空而非 1970/01/01。
pub fn fmt_ts_label(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(ts as u64);
    human_time(time)
}

/// 从时间戳秒还原为精确显示（详情/属性用）。
/// 0 表示时间未知（与 fmt_ts_label 口径一致），显示「—」而非 1970/01/01。
pub fn fmt_ts_full(ts: i64) -> String {
    if ts <= 0 {
        return "—".to_string();
    }
    let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(ts as u64);
    full_time(time)
}

/// 根据扩展名/是否目录推断图标类别与标签、类型描述
pub fn classify(path: &Path, is_dir: bool) -> (String, String, String) {
    // 返回 (icon_class, icon_label, kind)
    if is_dir {
        return ("folder".into(), "F".into(), "文件夹".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let (class, kind): (&str, String) = match ext.as_str() {
        "png" => ("image", "PNG 图片".into()),
        "jpg" | "jpeg" => ("image", "JPEG 图片".into()),
        "gif" => ("image", "GIF 图片".into()),
        "bmp" => ("image", "BMP 图片".into()),
        "webp" => ("image", "WebP 图片".into()),
        "svg" => ("image", "SVG 图像".into()),
        "ico" => ("image", "图标文件".into()),
        "rs" => ("code", "Rust 源文件".into()),
        "slint" => ("code", "Slint UI 文件".into()),
        "js" | "ts" | "jsx" | "tsx" => ("code", "脚本文件".into()),
        "py" => ("code", "Python 源文件".into()),
        "c" | "cpp" | "h" | "hpp" => ("code", "C/C++ 源文件".into()),
        "java" | "kt" => ("code", "源代码文件".into()),
        "go" => ("code", "Go 源文件".into()),
        "html" | "css" => ("code", "Web 文件".into()),
        "json" | "toml" | "yaml" | "yml" | "xml" => ("code", "配置文件".into()),
        "zip" => ("archive", "ZIP 压缩包".into()),
        "7z" => ("archive", "7z 压缩包".into()),
        "rar" => ("archive", "RAR 压缩包".into()),
        "tar" | "gz" | "bz2" | "xz" => ("archive", "压缩归档".into()),
        "cab" => ("archive", "CAB 安装包".into()),
        "msix" => ("archive", "MSIX 安装包".into()),
        "msixbundle" => ("archive", "MSIX Bundle 安装包".into()),
        "appx" => ("archive", "APPX 安装包".into()),
        "appxbundle" => ("archive", "APPX Bundle 安装包".into()),
        "appinstaller" => ("archive", "App Installer 清单".into()),
        "apk" => ("archive", "APK 应用包".into()),
        "aab" => ("archive", "AAB 应用包".into()),
        "ipa" => ("archive", "IPA 应用包".into()),
        "iso" => ("archive", "ISO 镜像".into()),
        "vhd" | "vhdx" => ("archive", "虚拟磁盘镜像".into()),
        "pdf" => ("default", "PDF 文件".into()),
        "txt" => ("default", "文本文档".into()),
        "md" => ("default", "Markdown 文档".into()),
        "doc" | "docx" => ("default", "Word 文档".into()),
        "xls" | "xlsx" => ("default", "Excel 工作簿".into()),
        "ppt" | "pptx" => ("default", "PowerPoint 演示文稿".into()),
        "mp4" | "mov" | "avi" | "mkv" => ("default", "视频文件".into()),
        "mp3" | "wav" | "flac" => ("default", "音频文件".into()),
        "exe" | "msi" => ("default", "应用程序".into()),
        "" => ("default", "文件".into()),
        other => ("default", format!("{} 文件", other.to_uppercase())),
    };

    // 图标标签：取扩展名大写（最多 3 字符），无扩展名用 "FILE"
    let label = if ext.is_empty() {
        "•".to_string()
    } else {
        // 按字符截取：扩展名可含多字节字符（如 "é"），字节切片会切在
        // UTF-8 字符中间导致 panic（目录列表热路径，单个文件名即崩溃）
        ext.to_uppercase().chars().take(3).collect()
    };

    (class.to_string(), label, kind)
}
