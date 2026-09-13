//! 空格键 Quick Look 预览内容计算（类 macOS「快速查看」）
//!
//! 仅负责把"选中文件 / 文件夹"归类并产出可直接显示的文本/统计信息；
//! 图片的大图渲染复用 thumbnail 模块，由 ui_bridge 在更大尺寸下提取位图。

use std::path::Path;

/// 预览归类：决定 Quick Look 浮层用哪种方式展示
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreviewKind {
    /// 图片：渲染缩略图大图
    Image,
    /// 文本/代码：显示文件首部内容
    Text,
    /// 文件夹：统计顶层项数与大小
    Folder,
    /// 视频：内嵌播放（Media Foundation 子窗口渲染）
    Video,
    /// 音频：显示封面与播放控制（Media Foundation 音频播放）
    Audio,
    /// 归档：列出压缩包内文件（内容区复用文本面板显示清单）
    Archive,
    /// 其它：仅展示图标与基础信息
    Info,
}

impl PreviewKind {
    /// 0 信息 / 1 图片 / 2 文本 / 3 文件夹 / 4 视频 / 5 归档树 / 6 音频。
    pub fn code(self) -> i32 {
        match self {
            PreviewKind::Info => 0,
            PreviewKind::Image => 1,
            PreviewKind::Text => 2,
            PreviewKind::Folder => 3,
            PreviewKind::Video => 4,
            PreviewKind::Archive => 5,
            PreviewKind::Audio => 6,
        }
    }
}

/// 归档树的单个节点（供预览窗口渲染可展开/折叠的层级列表）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveTreeNode {
    /// 仅本级名称（不含父级路径）
    pub name: String,
    /// 归档内完整路径（以 / 分隔，目录不带尾斜杠），作为展开状态的稳定键
    pub full_path: String,
    /// 文件字节数；目录为其所有后代文件之和
    pub size: u64,
    pub is_dir: bool,
    /// 缩进层级，根级为 0
    pub level: i32,
    /// 目录是否含子项（决定是否绘制展开箭头）
    pub has_children: bool,
}

/// 可作为图片大图预览的扩展名（与缩略图提取能力一致）
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico",
];

/// 可内嵌播放的视频扩展名（Media Foundation 支持的常见容器）
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "wmv", "m4v", "webm", "mpg", "mpeg",
];

/// 可由 Media Foundation 直接播放的音频扩展名。
/// opus/aiff/aif 在 Win10+ 自带解码器；ape/mka 若系统无解码器则播放失败并提示，
/// 仍比落入十六进制文本预览更符合预期。
const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "flac", "m4a", "aac", "wma", "ogg", "opus", "aiff", "aif", "ape", "mka",
];

/// 可作为纯文本预览的扩展名（含常见源码 / 配置 / 文档）
/// 注意：kind_of 已对非图片/视频/归档/二进制文件统一兜底为文本预览，本表仅供 renderable_web
/// 之外的特殊判断参考，当前无直接引用，保留以备未来按扩展名区分高亮等用途。
#[allow(dead_code)]
const TEXT_EXTS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "log",
    "ini",
    "cfg",
    "conf",
    "toml",
    "yaml",
    "yml",
    "json",
    "xml",
    "csv",
    "rs",
    "go",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "cs",
    "java",
    "kt",
    "rb",
    "php",
    "sh",
    "bat",
    "ps1",
    "css",
    "scss",
    "less",
    "html",
    "htm",
    "slint",
    "sql",
    "lua",
    "vue",
    "svelte",
    "gradle",
    "properties",
    "env",
    "gitignore",
    "dockerfile",
    "makefile",
    "gitattributes",
    "dockerignore",
    "npmignore",
    "editorconfig",
    "license",
    "readme",
    "lock",
];

/// 二进制/可执行/容器类扩展名：预览时显示应用/文件基本信息（含版本资源）。
/// 注意：bin/dat 等无明确类型归属的不在其内——它们走文本通道的十六进制预览。
/// Office 新旧格式（doc/docx/xls/xlsx/ppt/pptx）一律走 Office 高保真预览，
/// 不在此列；PDF 走 Edge 原生渲染，同样不在此列。
const BINARY_EXTS: &[&str] = &[
    // 可执行/系统二进制
    "exe", "msi", "dll", "sys", "com", "scr",
    // 磁盘映像/非 ZIP 容器：无解包支持（cab 为 MSCF、iso 为 ISO9660、vhd 为
    // 虚拟磁盘、rar 为专有格式），强行按 ZIP/7z 解析必然报错，按信息展示
    "iso", "img", "vhd", "vhdx", "cab", "rar",
    "msu", "dmp", "pdb", "deb", "rpm", "appimage", "dmg", "pkg",
];

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 是否支持「渲染视图」（WebView2 显示高保真效果，与源码视图可切换）：
/// Markdown 转 HTML 渲染；HTML/HTM 直接渲染；PHP 渲染其中的静态 HTML 部分；
/// DOCX/XLSX/PPTX/PDF 走 Office/Edge 高保真渲染；旧版 DOC/XLS/PPT 同样经由
/// 本机 Office 转 PDF 后渲染（未安装 Office 时回退文本抽取）。
pub fn renderable_web(path: &Path) -> bool {
    matches!(
        ext_of(path).as_str(),
        "md" | "markdown"
            | "html" | "htm" | "php"
            | "docx" | "doc" | "xlsx" | "xls" | "pptx" | "ppt" | "pdf"
    )
}

/// 判断给定路径的预览类型
pub fn kind_of(path: &Path, is_dir: bool) -> PreviewKind {
    if is_dir {
        return PreviewKind::Folder;
    }
    let ext = ext_of(path);
    if IMAGE_EXTS.contains(&ext.as_str()) {
        PreviewKind::Image
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        PreviewKind::Video
    } else if AUDIO_EXTS.contains(&ext.as_str()) {
        PreviewKind::Audio
    } else if is_archive_kind(&ext, path) {
        PreviewKind::Archive
    } else if is_binary_kind(&ext) {
        // 旧版 Office/未知容器显示基础信息；可解析格式已在前面进入文本预览
        PreviewKind::Info
    } else {
        // 兜底用文本预览：文本文件显示内容，二进制文件由 read_text_head 的
        // NUL 检测生成十六进制预览
        PreviewKind::Text
    }
}

/// 是否作为二进制/可执行文件（预览时显示信息而非文本）
fn is_binary_kind(ext: &str) -> bool {
    BINARY_EXTS.contains(&ext)
}

/// 是否作为归档预览（与 operations::is_archive 一致的格式集合）
/// ZIP 系容器扩展（msix/appx 等本质为 ZIP）同样进入归档树预览；
/// cab/iso/vhd/rar 无对应解析器，不走归档预览（按二进制信息展示）
fn is_archive_kind(ext: &str, _path: &Path) -> bool {
    matches!(
        ext,
        "zip"
            | "7z"
            | "tar"
            | "gz"
            | "tgz"
            | "msix"
            | "msixbundle"
            | "appx"
            | "appxbundle"
            | "apk"
            | "aab"
            | "ipa"
    )
}

/// 读取文本文件首部，最多 `max_bytes` 字节，按编码检测解码：
/// UTF-8（含 BOM）→ UTF-16（BOM）→ 系统 ANSI 码页（中文 Windows 为 GBK）。
/// 截断时在结尾追加省略提示。读取失败返回错误说明文本。
pub fn read_text_head(path: &Path, max_bytes: usize) -> String {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return format!("无法读取文件：{}", e),
    };
    let mut buf = vec![0u8; max_bytes];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(e) => return format!("读取出错：{}", e),
    };
    buf.truncate(n);
    let truncated = matches!(std::fs::metadata(path), Ok(m) if m.len() as usize > n);
    let mut text = match decode_text(&buf) {
        Some(t) => t,
        // 二进制内容：不再只显示占位提示，改为十六进制预览（仅头部 8KB 防卡顿）
        None => {
            let head = &buf[..buf.len().min(8 * 1024)];
            let mut t = String::from("（二进制内容 · 开头十六进制预览）\n\n");
            t.push_str(&hex_dump(head));
            t
        }
    };
    // 截断可能劈裂多字节字符，解码会在末尾产出一个替换符，展示前去掉
    if truncated && text.ends_with('\u{FFFD}') {
        text.pop();
    }
    // 文件比读取窗口更大时提示已截断
    if truncated {
        text.push_str("\n\n…（仅显示开头部分）");
    }
    text
}

/// 二进制内容的十六进制预览（xxd 风格）：偏移 + 16 字节十六进制（8 字节一组）+ ASCII 侧栏。
/// 不可打印字符以 '.' 表示，与常见十六进制查看器一致。
fn hex_dump(buf: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(buf.len() / 16 * 78 + 80);
    for (i, chunk) in buf.chunks(16).enumerate() {
        let _ = write!(out, "{:08X}  ", i * 16);
        for (j, b) in chunk.iter().enumerate() {
            let _ = write!(out, "{:02X} ", b);
            if j == 7 {
                out.push(' ');
            }
        }
        // 末行不足 16 字节时补齐间距，保持侧栏对齐
        if chunk.len() < 16 {
            for _ in 0..(16 - chunk.len()) {
                out.push_str("   ");
            }
            if chunk.len() <= 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for b in chunk {
            out.push(if (0x20..0x7F).contains(b) { *b as char } else { '.' });
        }
        out.push('\n');
    }
    out
}

/// 抽取 Word 文档（.docx）正文：使用 docx-rs 解析，提取段落文本。
/// 读取失败返回 None（调用方回退其它预览方式）。
pub fn office_text(path: &Path) -> Option<String> {
    let ext = ext_of(path);
    match ext.as_str() {
        "docx" => extract_docx(path),
        "xlsx" => extract_xlsx(path),
        "pdf" => extract_pdf(path),
        _ => None,
    }
}

/// 从 .docx 文件提取文本
fn extract_docx(path: &Path) -> Option<String> {
    use docx_rs::*;
    let data = std::fs::read(path).ok()?;
    let docx = read_docx(&data).ok()?;
    let mut text = String::new();
    for child in &docx.document.children {
        if let DocumentChild::Paragraph(p) = child {
            for run in &p.children {
                if let ParagraphChild::Run(r) = run {
                    for c in &r.children {
                        if let RunChild::Text(t) = c {
                            text.push_str(&t.text);
                        }
                    }
                }
            }
            text.push('\n');
        }
    }
    Some(text)
}

/// 从 .xlsx 文件提取文本（所有工作表的单元格内容）
fn extract_xlsx(path: &Path) -> Option<String> {
    use calamine::{open_workbook, Reader, Xlsx};
    let mut workbook: Xlsx<_> = open_workbook(path).ok()?;
    let mut text = String::new();
    for sheet_name in workbook.sheet_names() {
        text.push_str(&format!("=== {} ===\n\n", sheet_name));
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            for row in range.rows() {
                let line: Vec<String> = row.iter().map(|cell| format!("{}", cell)).collect();
                text.push_str(&line.join("\t"));
                text.push('\n');
            }
        }
        text.push('\n');
    }
    Some(text)
}

/// 从 PDF 文件提取文本
fn extract_pdf(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    pdf_extract::extract_text_from_mem(&bytes).ok()
}

/// 读取 PE 版本资源中的描述/公司/版本/产品（常见代码页试探）。
/// 无版本资源或读取失败返回空 vec，调用方仅显示基础信息。
#[cfg(windows)]
pub fn exe_version_info(path: &Path) -> Vec<(String, String)> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::core::PCWSTR;
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), Some(&mut handle));
        if size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            Some(handle),
            size,
            buf.as_mut_ptr() as *mut _,
        )
        .is_err()
        {
            return Vec::new();
        }
        let query = |sub: &str| -> Option<String> {
            let subw: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            let _ = VerQueryValueW(
                buf.as_ptr() as *const _,
                PCWSTR(subw.as_ptr()),
                &mut ptr,
                &mut len,
            );
            if ptr.is_null() || len == 0 {
                return None;
            }
            let units = std::slice::from_raw_parts(ptr as *const u16, len as usize);
            let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            let s = String::from_utf16_lossy(&units[..end]);
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let mut out = Vec::new();
        for (key, label) in [
            ("FileDescription", "描述"),
            ("CompanyName", "公司"),
            ("FileVersion", "版本"),
            ("ProductName", "产品"),
        ] {
            // 常见代码页试探：英文(0409)+UTF-16(04B0)、简体中文(0804)、ANSI(04E4)
            let v = query(&format!("\\StringFileInfo\\040904B0\\{}", key))
                .or_else(|| query(&format!("\\StringFileInfo\\080404B0\\{}", key)))
                .or_else(|| query(&format!("\\StringFileInfo\\040904E4\\{}", key)));
            if let Some(v) = v {
                out.push((label.to_string(), v));
            }
        }
        out
    }
}

#[cfg(not(windows))]
pub fn exe_version_info(_path: &Path) -> Vec<(String, String)> {
    Vec::new()
}

/// 把原始字节解码为文本。返回 None 表示二进制内容（含 NUL 且非 UTF-16）。
///
/// 顺序：UTF-16 LE/BE BOM → 二进制检测 → UTF-8 BOM → 严格 UTF-8 → 系统 ANSI 码页。
/// 非 UTF-8 的 ANSI/GBK 文件若直接 lossy 会满屏 U+FFFD 替换符（乱码），
/// 改按系统码页解码后与记事本的「ANSI」行为一致。
fn decode_text(buf: &[u8]) -> Option<String> {
    // UTF-16 BOM：ASCII 字符高低字节含大量 0x00，必须先于二进制检测分支
    if let Some(rest) = buf.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    if let Some(rest) = buf.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    // 检测是否为二进制（含 NUL 字节）：避免把二进制文件当文本显示成乱码
    if buf.iter().any(|&b| b == 0) {
        return None;
    }
    // UTF-8 BOM：剥离 BOM，避免预览首行多出不可见字符
    if let Some(rest) = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(rest).into_owned());
    }
    if let Ok(s) = std::str::from_utf8(buf) {
        return Some(s.to_string());
    }
    // 非 UTF-8：按系统 ANSI 码页解码（中文 Windows = GBK/GB18030）
    let enc = system_ansi_encoding();
    Some(enc.decode(buf).0.into_owned())
}

/// 系统 ANSI 码页对应的文本编码：GetACP() 936→GBK、950→Big5、932→Shift_JIS、1252→Windows-1252…
/// WHATWG 编码标签对常见码页均接受 `windows-<cp>` 形式。
#[cfg(windows)]
fn system_ansi_encoding() -> &'static encoding_rs::Encoding {
    use windows_sys::Win32::Globalization::GetACP;
    let cp = unsafe { GetACP() };
    if cp == 65001 {
        return encoding_rs::UTF_8;
    }
    let label = format!("windows-{}", cp);
    encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::WINDOWS_1252)
}

#[cfg(not(windows))]
fn system_ansi_encoding() -> &'static encoding_rs::Encoding {
    encoding_rs::WINDOWS_1252
}

/// 按扩展名抽取可读文档内容，供源码视图和 WebView2 渲染视图共用。
/// 新旧版 Office 共用同一高保真通道：渲染视图走 Office 转 PDF；
/// 源码视图为旧版同样尝试从缓存 PDF 抽取文本（快），无缓存时走 OLE 容器
/// 轻量抽取而非占位提示，避免截图中的“渲染/源码切换提示”占位。
pub fn document_text(path: &Path) -> Result<String, String> {
    match ext_of(path).as_str() {
        "pdf" => pdf_text(path),
        "docx" | "doc" => office_text(path)
            .or_else(|| legacy_office_raw_text(path))
            .ok_or_else(|| "无法读取 Word 文档内容".to_string()),
        "xlsx" | "xls" => xlsx_text(path).or_else(|_| {
            legacy_office_raw_text(path).ok_or_else(|| "无法读取 Excel 内容".to_string())
        }),
        "pptx" | "ppt" => pptx_text(path).or_else(|_| {
            legacy_office_raw_text(path).ok_or_else(|| "无法读取演示文稿内容".to_string())
        }),
        _ => Err("不支持的文档格式".to_string()),
    }
}

/// 旧版 Office 轻量文本兜底：优先从已缓存的 PDF 抽取（与渲染一致），
/// 否则尝试 OLE 容器原始文本抽取，彻底消除占位提示。
fn legacy_office_raw_text(path: &Path) -> Option<String> {
    if let Some(pdf) = super::office_preview::cached_pdf_if_fresh(path) {
        if let Ok(t) = pdf_extract::extract_text(&pdf) {
            if !t.trim().is_empty() {
                return Some(t);
            }
        }
    }
    // OLE 旧版容器内文本碎片（尽力抽取可读片段）
    ole_text_fallback(path)
}

/// 旧版 Office 文档（.doc/.xls/.ppt 等 OLE 复合文档）的文本兜底抽取。
///
/// 只在未安装 Office、且没有缓存 PDF 时使用；装了 Office 时走转 PDF 高保真预览。
/// 旧实现把高位字节统一替换成 `·`（怕 GBK 乱码），中文文档整篇变成点阵；
/// 改为按编码检测解码：含 NUL 的可读区间先试 UTF-16LE（Word 97-2003 正文流即
/// UTF-16），其余交给通用解码（UTF-8 / BOM / 系统码页 = GBK 等），中文可读。
fn ole_text_fallback(path: &Path) -> Option<String> {
    use std::io::Read;
    // 抽取窗口 1MB：旧版文档的正文文本流集中在文件头部
    const WINDOW: usize = 1024 * 1024;
    // 输出上限：二进制噪声再多也不该撑爆预览
    const OUT_LIMIT: usize = 128 * 1024;
    let f = std::fs::File::open(path).ok()?;
    // take + read_to_end 循环读满：云盘挂载路径单次 read 允许短读，
    // 单次 read 会静默截断抽取窗口
    let mut buf = Vec::with_capacity(WINDOW);
    if f.take(WINDOW as u64).read_to_end(&mut buf).is_err() {
        return None;
    }

    // 复合文档内部由二进制结构分隔各流：以不可读字节切段，逐段解码后按行过滤。
    // 短于 24 字节的段不值得做编码检测（会产生上万次无效检测）。
    let mut out = String::new();
    let mut seg: Vec<u8> = Vec::new();
    // NUL 并入段内（UTF-16 正文里 ASCII 字符带 0x00 尾字节，切掉就废了），
    // 因此不能靠链入 NUL 哨兵触发末段解码——循环结束后必须显式 flush，
    // 否则末段（文档结尾的正文）被静默丢弃
    fn flush(out: &mut String, seg: &mut Vec<u8>) {
        if seg.len() >= 24 {
            for line in ole_decode_segment(seg).lines() {
                let t = line.trim_matches(|c: char| c.is_control() || c == ' ' || c == '\t');
                if is_meaningful_line(t) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        seg.clear();
    }
    for &b in buf.iter() {
        // 0x1A（DOS EOF 标记）并入可读字节：UTF-16 汉字低位字节可为 0x1A
        // （如「会」U+4F1A、全角冒号 U+FF1A），在此切断会把正文粉碎成
        // 不足 24 字节的碎片而被整段丢弃
        let readable = b == 0
            || b == 0x1A
            || (0x20..=0x7E).contains(&b)
            || b == b'\t'
            || b == 0x0A
            || b == 0x0D
            || b >= 0x80;
        if readable {
            seg.push(b);
            continue;
        }
        flush(&mut out, &mut seg);
        if out.len() >= OUT_LIMIT {
            break;
        }
    }
    flush(&mut out, &mut seg);
    let t = tidy_text(&out);
    // 至少要有 8 个字母/数字/汉字才认为抽到了真内容（否则全是二进制噪声）
    if t.chars().filter(|c| c.is_alphanumeric()).count() >= 8 {
        Some(t)
    } else {
        None
    }
}

/// 单个可读区间的解码：含 NUL 时在 UTF-16LE 与通用解码间按可读字符占比择优
/// （UTF-16 正文里中文无 NUL、ASCII 有 NUL，一律按 NUL 占比判断会误判）。
fn ole_decode_segment(seg: &[u8]) -> String {
    // NUL 字符是复合文档的结构性填充：字节层面必须保留（UTF-16 配对依赖），
    // 解码输出层面只会拉低可读占比导致正文整行被过滤，故剥离
    let strip_nul = |s: String| -> String { s.chars().filter(|&c| c != '\0').collect() };
    let utf16 = || -> String {
        let units: Vec<u16> = seg
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if seg.len() >= 16 && seg.iter().filter(|&&b| b == 0).count() >= 4 {
        let a = strip_nul(utf16());
        if let Some(b) = decode_text(seg) {
            return if printable_ratio(&a) > printable_ratio(&b) { a } else { strip_nul(b) };
        }
        return a;
    }
    decode_text(seg).map(strip_nul).unwrap_or_default()
}

/// 非控制字符占比：衡量一段解码结果有多"像文本"
fn printable_ratio(s: &str) -> f32 {
    let total = s.chars().count();
    if total == 0 {
        return 0.0;
    }
    let ok = s.chars().filter(|c| !c.is_control()).count();
    ok as f32 / total as f32
}

/// 是否值得作为预览行展示：长度够、可见字符占多数、且含字母/数字/汉字
fn is_meaningful_line(line: &str) -> bool {
    let total = line.chars().count();
    if total < 3 {
        return false;
    }
    let visible = line.chars().filter(|c| !c.is_control()).count();
    if visible * 10 < total * 6 {
        return false;
    }
    line.chars().filter(|c| c.is_alphanumeric()).count() >= 3
}

fn read_zip_xml(path: &Path, name: &str) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("无法打开文档：{}", e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("无法读取文档容器：{}", e))?;
    let entry = zip.by_name(name).map_err(|_| format!("文档缺少 {}", name))?;
    let mut raw = String::new();
    entry
        .take(16 * 1024 * 1024)
        .read_to_string(&mut raw)
        .map_err(|e| format!("读取文档内容失败：{}", e))?;
    Ok(raw)
}

fn xml_text(raw: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xlsx_text(path: &Path) -> Result<String, String> {
    let shared = read_zip_xml(path, "xl/sharedStrings.xml").unwrap_or_default();
    let shared_values: Vec<String> = shared
        .split("<si")
        .skip(1)
        .map(|s| xml_text(s.split("</si>").next().unwrap_or(s)))
        .collect();
    let mut out = String::new();
    let mut sheets = Vec::new();
    for i in 1..=100 {
        let name = format!("xl/worksheets/sheet{}.xml", i);
        let Ok(raw) = read_zip_xml(path, &name) else { break };
        sheets.push((i, raw));
    }
    if sheets.is_empty() {
        return Err("Excel 文档没有可读取的工作表".to_string());
    }
    for (index, raw) in sheets {
        out.push_str(&format!("工作表 {}\n", index));
        for row in raw.split("<row").skip(1) {
            let row = row.split("</row>").next().unwrap_or(row);
            let mut cells = Vec::new();
            for cell in row.split("<c").skip(1) {
                let kind_shared = cell.contains("t=\"s\"");
                let value = xml_text(cell.split("<v>").nth(1).unwrap_or("").split("</v>").next().unwrap_or(""));
                let value = if kind_shared {
                    value.parse::<usize>().ok().and_then(|i| shared_values.get(i).cloned()).unwrap_or(value)
                } else { value };
                cells.push(value);
            }
            if !cells.is_empty() { out.push_str(&format!("{}\n", cells.join("\t"))); }
        }
        out.push('\n');
    }
    Ok(out)
}

/// 抽取 .pptx 演示文稿文本（Office 高保真转换不可用时的回退内容）。
///
/// 与旧实现的差别：
/// 1. 幻灯片按 slideN.xml 的序号排序 —— ZIP 条目顺序是 slide1/slide10/slide11/…，
///    逐条遍历会把第 10 页排到第 2 页前面；
/// 2. 只取 DrawingML 正文 <a:t> 并按段落组织，不再是「剥掉尖括号就当文本」——
///    后者会把版式、主题、占位符里的内容一并混进正文；
/// 3. 附演讲者备注：经幻灯片的关系文件定位备注页（备注页编号与幻灯片编号不保证一致）。
fn pptx_text(path: &Path) -> Result<String, String> {
    use std::io::Read;
    // 单条目上限：正常幻灯片 XML 只有几十 KB，超大说明不是普通演示文稿
    const MAX_ENTRY: u64 = 8 * 1024 * 1024;
    let file = std::fs::File::open(path).map_err(|e| format!("无法打开 PPT：{}", e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("无法读取 PPT 容器：{}", e))?;

    let mut slides: Vec<(u32, String)> = Vec::new();
    let mut rels: Vec<(u32, String)> = Vec::new();
    let mut notes: Vec<(u32, String)> = Vec::new();
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        let name = entry.name().to_string();
        let target = if name.starts_with("ppt/slides/_rels/slide") {
            "rels"
        } else if name.starts_with("ppt/slides/slide") {
            "slide"
        } else if name.starts_with("ppt/notesSlides/notesSlide") {
            "notes"
        } else {
            continue;
        };
        // 页号：slideN.xml / slideN.xml.rels / notesSlideN.xml 的 N
        // （先后剥 .rels/.xml 后缀，再剥 slide/notesSlide 前缀）
        let num = name
            .rsplit(['\\', '/'])
            .next()
            .and_then(|base| {
                let b = base.strip_suffix(".rels").unwrap_or(base);
                let b = b.strip_suffix(".xml").unwrap_or(b);
                b.strip_prefix("slide").or_else(|| b.strip_prefix("notesSlide"))
            })
            .and_then(|n| n.parse::<u32>().ok());
        let Some(num) = num else { continue };
        let mut raw = String::new();
        if !entry.take(MAX_ENTRY).read_to_string(&mut raw).is_ok() {
            continue;
        }
        // 边遍历边抽取（slides 收集排序后的正文而非原始 XML）：
        // 异常构造的演示文稿单条目可达 8MB 上限，全量驻留会累积数百 MB
        match target {
            "rels" => rels.push((num, raw)),
            "slide" => slides.push((num, tidy_text(&drawing_text(&raw)))),
            _ => notes.push((num, tidy_text(&drawing_text(&raw)))),
        }
    }
    if slides.is_empty() {
        return Err("PowerPoint 文档没有可读取的幻灯片".to_string());
    }
    slides.sort_by_key(|(n, _)| *n);

    let mut out = String::new();
    for (num, body) in &slides {
        // 标签用幻灯片实际页号（slideN 的 N），而非排序后的遍历序号：
        // 否则 slide10 会显示成“幻灯片 3”
        out.push_str(&format!("幻灯片 {}\n", num));
        if body.is_empty() {
            out.push_str("（本页无文本）\n");
        } else {
            out.push_str(body);
            out.push('\n');
        }
        if let Some(notes_num) = rels
            .iter()
            .find(|(n, _)| n == num)
            .and_then(|(_, r)| notes_slide_number(r))
        {
            if let Some((_, note)) = notes.iter().find(|(n, _)| *n == notes_num) {
                if !note.is_empty() {
                    out.push_str("〔备注〕");
                    out.push_str(note);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// 幻灯片关系文件（slideN.xml.rels）中 notesSlide 的编号
fn notes_slide_number(rels: &str) -> Option<u32> {
    for item in rels.split("<Relationship").skip(1) {
        if !item.contains("notesSlide") {
            continue;
        }
        let Some(target) = item.split("Target=\"").nth(1).and_then(|s| s.split('"').next()) else {
            continue;
        };
        // Target 形如 ../notesSlides/notesSlide3.xml
        if let Some(n) = target
            .rsplit("notesSlide")
            .next()
            .and_then(|tail| tail.split('.').next())
            .and_then(|s| s.parse::<u32>().ok())
        {
            return Some(n);
        }
    }
    None
}

/// 从 DrawingML（slide / notesSlide）XML 抽取正文：
/// <a:t> 为文本、</a:p> 段落换行、<a:br> 强制换行、<a:tab> 制表符。
fn drawing_text(xml: &str) -> String {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"a:t" => in_text = true,
                b"a:br" => out.push('\n'),
                b"a:tab" => out.push('\t'),
                _ => {}
            },
            // <a:br/>、<a:tab/> 常以自闭合形式出现，不会再有对应的 End 事件
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"a:br" => out.push('\n'),
                b"a:tab" => out.push('\t'),
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"a:t" => in_text = false,
                b"a:p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_text {
                    if let Ok(s) = t.unescape() {
                        out.push_str(&s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            // 单个条目解析失败不放弃整份文档：保留已抽出的文本
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// 折叠连续空行、去掉行首尾空白：表格与多形状页会产出大片空白
fn tidy_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut blanks = 0;
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(t);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn pdf_text(path: &Path) -> Result<String, String> {
    let text = pdf_extract::extract_text(path).map_err(|e| format!("PDF 文本抽取失败：{}", e))?;
    if text.trim().is_empty() { Err("PDF 不包含可抽取的文本（可能是扫描图片）".to_string()) } else { Ok(text) }
}

/// 上限 2000 项防超大归档卡顿。复用 tasks.rs 已验证的读取路径
/// （zip::ZipArchive / sevenz_rust::SevenZReader / tar::Archive）。
pub fn read_archive_entries(path: &Path) -> Result<Vec<(String, u64, bool)>, String> {
    use std::io::Read;
    let ext = ext_of(path);
    // 用闭包包裹使 `?` 可用并保持原有各格式分支不变
    let result: Result<Vec<(String, u64, bool)>, String> = (|| {
        let mut items: Vec<(String, u64, bool)> = Vec::new();
        match ext.as_str() {
            // ZIP 系容器（msix/appx/apk 等本质为 ZIP，按 ZIP 分支列目录）
            "zip"
            | "msix"
            | "msixbundle"
            | "appx"
            | "appxbundle"
            | "apk"
            | "aab"
            | "ipa" => {
                let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let mut zip = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
                for i in 0..zip.len() {
                    if items.len() >= 2000 {
                        break;
                    }
                    if let Ok(e) = zip.by_index_raw(i) {
                        items.push((e.name().to_string(), e.size(), e.is_dir()));
                    }
                }
            }
            "7z" => {
                let sz = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty())
                    .map_err(|e| e.to_string())?;
                for e in sz.archive().files.iter() {
                    if items.len() >= 2000 {
                        break;
                    }
                    items.push((e.name().to_string(), e.size, e.is_directory()));
                }
            }
            "tar" => {
                let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let mut tar = tar::Archive::new(f);
                for entry in tar.entries().map_err(|e| e.to_string())? {
                    if items.len() >= 2000 {
                        break;
                    }
                    if let Ok(e) = entry {
                        let name = e
                            .path()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let size = e.header().size().unwrap_or(0);
                        let is_dir = e.header().entry_type().is_dir();
                        items.push((name, size, is_dir));
                    }
                }
            }
            "tgz" | "gz" => {
                // tar.gz / tgz 需 gzip 解码；纯 .gz（非 tar 流）退回单文件条目
                let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let gz = flate2::read::GzDecoder::new(f);
                let mut tar = tar::Archive::new(gz);
                let mut pulled = 0;
                for entry in tar.entries().map_err(|e| e.to_string())? {
                    if items.len() >= 2000 {
                        break;
                    }
                    if let Ok(e) = entry {
                        let name = e
                            .path()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let size = e.header().size().unwrap_or(0);
                        let is_dir = e.header().entry_type().is_dir();
                        items.push((name, size, is_dir));
                        pulled += 1;
                    }
                }
                if pulled == 0 {
                    // 普通 .gz 单文件：用解压后的原始文件名与解压大小。
                    // 流式统计解压字节数（不落内存），64MB 上限防 gzip 解压炸弹
                    // （高压缩比小文件可在预览时膨胀为数十 GB 导致 OOM）
                    let mut f2 = std::fs::File::open(path).map_err(|e| e.to_string())?;
                    let dec = flate2::read::GzDecoder::new(&mut f2);
                    let size = std::io::copy(
                        &mut dec.take(64 * 1024 * 1024),
                        &mut std::io::sink(),
                    )
                    .unwrap_or(0);
                    let stem = path
                        .file_name()
                        .map(|n| n.to_string_lossy().trim_end_matches(".gz").to_string())
                        .unwrap_or_else(|| "解压内容".into());
                    items.push((stem, size, false));
                }
            }
            _ => return Err("不支持的归档格式".into()),
        }
        Ok(items)
    })();
    result
}

/// 由归档原始条目构建树形节点列表（深度优先展开顺序）。
///
/// 归档内的条目名是扁平的相对路径（如 `a/b/c.txt`），中间目录未必有显式条目，
/// 因此这里按 `/` 拆分并补齐所有中间目录。目录大小为其后代文件之和；
/// 同层内目录在前、文件在后，各自按名称不区分大小写排序，与文件管理器一致。
pub fn build_archive_tree(items: &[(String, u64, bool)]) -> Vec<ArchiveTreeNode> {
    use std::collections::BTreeMap;

    /// 构建期的中间树：children 用 BTreeMap 保证遍历顺序稳定
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        is_dir: bool,
        size: u64,
    }

    let mut root = Node {
        is_dir: true,
        ..Default::default()
    };

    for (raw_name, size, is_dir) in items {
        // 归一化分隔符：部分归档（尤其 7z/zip 由 Windows 工具创建）使用反斜杠
        let normalized = raw_name.replace('\\', "/");
        let parts: Vec<&str> = normalized
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if parts.is_empty() {
            continue;
        }
        let last = parts.len() - 1;
        let mut cur = &mut root;
        for (i, part) in parts.iter().enumerate() {
            let entry = cur.children.entry((*part).to_string()).or_default();
            if i == last {
                // 末段：目录条目标记为目录，文件记录大小
                if *is_dir {
                    entry.is_dir = true;
                } else {
                    entry.size = *size;
                }
            } else {
                // 中间段一定是目录（即使归档未给出显式目录条目）
                entry.is_dir = true;
            }
            cur = entry;
        }
    }

    // 递归累计目录大小，并按「目录优先 + 名称序」展平为带层级的列表
    fn accumulate(node: &Node) -> u64 {
        if node.is_dir {
            node.children.values().map(accumulate).sum()
        } else {
            node.size
        }
    }

    fn flatten(
        node: &Node,
        prefix: &str,
        level: i32,
        out: &mut Vec<ArchiveTreeNode>,
    ) {
        let mut dirs: Vec<(&String, &Node)> = Vec::new();
        let mut files: Vec<(&String, &Node)> = Vec::new();
        for (name, child) in node.children.iter() {
            if child.is_dir {
                dirs.push((name, child));
            } else {
                files.push((name, child));
            }
        }
        let by_name = |a: &(&String, &Node), b: &(&String, &Node)| {
            a.0.to_lowercase().cmp(&b.0.to_lowercase())
        };
        dirs.sort_by(by_name);
        files.sort_by(by_name);

        for (name, child) in dirs.into_iter().chain(files.into_iter()) {
            let full_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            out.push(ArchiveTreeNode {
                name: name.clone(),
                full_path: full_path.clone(),
                size: accumulate(child),
                is_dir: child.is_dir,
                level,
                has_children: !child.children.is_empty(),
            });
            if child.is_dir {
                flatten(child, &full_path, level + 1, out);
            }
        }
    }

    let mut out = Vec::new();
    flatten(&root, "", 0, &mut out);
    out
}

/// 读取归档并直接构建树（失败时返回错误说明，供 UI 以信息态展示）
pub fn archive_tree(path: &Path) -> Result<Vec<ArchiveTreeNode>, String> {
    read_archive_entries(path).map(|items| build_archive_tree(&items))
}

/// 归档整体统计：(目录数, 文件数, 文件总字节)，用于预览副标题
pub fn archive_summary(nodes: &[ArchiveTreeNode]) -> (usize, usize, u64) {
    let dirs = nodes.iter().filter(|n| n.is_dir).count();
    let files = nodes.len() - dirs;
    let total = nodes
        .iter()
        .filter(|n| !n.is_dir)
        .map(|n| n.size)
        .sum::<u64>();
    (dirs, files, total)
}

/// 文件夹递归统计：返回 (子文件夹数, 文件数, 文件总字节)。
/// 使用与目录列表相同的过滤规则，且不跟随符号链接，避免循环遍历。
pub fn folder_summary(path: &Path, show_hidden: bool, show_protected: bool) -> (usize, usize, u64) {
    let mut dirs = 0usize;
    let mut files = 0usize;
    let mut size = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if super::operations::is_hidden_entry(&name, &meta, show_hidden, show_protected) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {
                    dirs += 1;
                    stack.push(entry.path());
                }
                Ok(ft) if ft.is_file() => {
                    files += 1;
                    size = size.saturating_add(meta.len());
                }
                _ => {}
            }
        }
    }
    (dirs, files, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_of() {
        assert_eq!(kind_of(Path::new("a.png"), false), PreviewKind::Image);
        assert_eq!(kind_of(Path::new("a.rs"), false), PreviewKind::Text);
        // 已知二进制/磁盘映像走文本通道生成十六进制预览，而非仅显示基础信息。
        assert_eq!(kind_of(Path::new("a.bin"), false), PreviewKind::Text);
        assert_eq!(kind_of(Path::new("a.zip"), false), PreviewKind::Archive);
        assert_eq!(kind_of(Path::new("a.mp3"), false), PreviewKind::Audio);
        assert_eq!(kind_of(Path::new("a.mp4"), false), PreviewKind::Video);
        assert_eq!(kind_of(Path::new("a.pdf"), false), PreviewKind::Text);
        assert_eq!(kind_of(Path::new("a.xlsx"), false), PreviewKind::Text);
        assert_eq!(kind_of(Path::new("a.pptx"), false), PreviewKind::Text);
        // 大小写不敏感
        assert_eq!(kind_of(Path::new("A.PNG"), false), PreviewKind::Image);
    }

    /// PPT 文本回退：幻灯片按页号排序、只取 <a:t> 正文、备注按关系文件匹配
    #[test]
    fn test_pptx_text_order_body_and_notes() {
        use std::io::Write;
        let p = std::env::temp_dir().join(format!("filefiles_pptx_{}.pptx", std::process::id()));
        let slide = |body: &str| {
            format!(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree><p:sp><p:txBody>{body}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        };
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            // 条目顺序故意打乱：slide10 → slide2 → slide1
            zip.start_file("ppt/slides/slide10.xml", opts).unwrap();
            zip.write_all(slide("<a:p><a:r><a:t>第十页</a:t></a:r></a:p>").as_bytes())
                .unwrap();
            zip.start_file("ppt/slides/slide2.xml", opts).unwrap();
            zip.write_all(slide("<a:p><a:r><a:t>第二页</a:t></a:r></a:p>").as_bytes())
                .unwrap();
            zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
            zip.write_all(slide("<a:p><a:r><a:t>第一页</a:t></a:r></a:p>").as_bytes())
                .unwrap();
            // 第 1 页的关系文件指向备注页 1
            zip.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
            zip.write_all(
                br#"<Relationships><Relationship Id="rId1" Type="http://x/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
            )
            .unwrap();
            zip.start_file("ppt/notesSlides/notesSlide1.xml", opts).unwrap();
            zip.write_all(slide("<a:p><a:r><a:t>讲解要点</a:t></a:r></a:p>").as_bytes())
                .unwrap();
            zip.finish().unwrap();
        }

        let text = pptx_text(&p).expect("应能抽取 pptx 文本");
        let i1 = text.find("幻灯片 1\n").expect("缺少第 1 页");
        let i2 = text.find("幻灯片 2\n").expect("缺少第 2 页");
        let i10 = text.find("幻灯片 10\n").expect("缺少第 10 页");
        assert!(i1 < i2 && i2 < i10, "幻灯片应按页号排序：{text}");
        assert!(text.contains("第一页") && text.contains("第二页") && text.contains("第十页"));
        assert!(text.contains("〔备注〕讲解要点"), "备注应匹配到第 1 页：{text}");
        std::fs::remove_file(&p).ok();
    }

    /// 旧版 OLE 回退：UTF-16LE 正文（Word 97-2003 的存储方式）要能解出中文
    #[test]
    fn test_ole_text_utf16_chinese() {
        let p = std::env::temp_dir().join(format!("filefiles_ole_{}.doc", std::process::id()));
        let mut bytes = vec![0u8; 64];
        for u in "会议纪要：确认预览方案".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        bytes.extend_from_slice(&[0u8; 32]);
        std::fs::write(&p, &bytes).unwrap();
        let text = ole_text_fallback(&p).expect("UTF-16 正文应可抽取");
        assert!(text.contains("会议纪要"), "中文应可读而不是点阵：{text}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_read_text_head() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_{}.txt", std::process::id()));
        std::fs::write(&p, b"hello world").unwrap();
        let t = read_text_head(&p, 1024);
        assert!(t.contains("hello world"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_read_text_head_truncate() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_big_{}.txt", std::process::id()));
        std::fs::write(&p, vec![b'x'; 5000]).unwrap();
        let t = read_text_head(&p, 100);
        assert!(t.contains("仅显示开头部分"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_read_text_head_gbk_no_replacement() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_gbk_{}.txt", std::process::id()));
        // GBK 编码的「你好」(0xC4 0xE3 0xBA 0xC3)，不是合法 UTF-8
        std::fs::write(&p, b"\xc4\xe3\xba\xc3 world").unwrap();
        let t = read_text_head(&p, 1024);
        // 按系统码页解码不应再产出 U+FFFD 替换符（乱码根源）
        assert!(!t.contains('\u{FFFD}'));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_read_text_head_utf16_le() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_u16_{}.txt", std::process::id()));
        let mut bytes = vec![0xFF, 0xFE];
        for u in "hello 预览".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&p, &bytes).unwrap();
        let t = read_text_head(&p, 1024);
        // UTF-16 含 NUL 字节但不应被判为二进制，且正确解码出中文
        assert!(t.contains("hello"));
        assert!(t.contains("预览"));
        assert!(!t.contains('\u{FFFD}'));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_read_text_head_utf8_bom_stripped() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_bom_{}.txt", std::process::id()));
        std::fs::write(&p, b"\xef\xbb\xbfhello bom").unwrap();
        let t = read_text_head(&p, 1024);
        assert!(t.starts_with("hello bom"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_build_archive_tree_nests_and_sorts() {
        // 扁平条目：中间目录 dir 无显式条目，需自动补齐
        let items = vec![
            ("b.txt".to_string(), 100, false),
            ("dir/inner.txt".to_string(), 20, false),
            ("dir/sub/deep.txt".to_string(), 5, false),
            ("a.txt".to_string(), 10, false),
            ("empty/".to_string(), 0, true),
        ];
        let tree = build_archive_tree(&items);
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        // 目录优先并按名称排序，文件其后；子项紧随父目录
        assert_eq!(
            names,
            vec!["dir", "sub", "deep.txt", "inner.txt", "empty", "a.txt", "b.txt"]
        );

        let dir = &tree[0];
        assert!(dir.is_dir);
        assert_eq!(dir.level, 0);
        assert!(dir.has_children);
        // 目录大小为后代文件之和
        assert_eq!(dir.size, 25);

        let sub = &tree[1];
        assert_eq!(sub.level, 1);
        assert_eq!(sub.full_path, "dir/sub");
        assert_eq!(tree[2].level, 2);
        assert_eq!(tree[2].full_path, "dir/sub/deep.txt");

        // 空目录不显示展开箭头
        let empty = tree.iter().find(|n| n.name == "empty").unwrap();
        assert!(empty.is_dir);
        assert!(!empty.has_children);
    }

    #[test]
    fn test_build_archive_tree_backslash_and_summary() {
        // Windows 工具生成的反斜杠路径也应正确分层
        let items = vec![
            ("top\\mid\\f.bin".to_string(), 8, false),
            ("top\\g.bin".to_string(), 2, false),
        ];
        let tree = build_archive_tree(&items);
        assert_eq!(tree[0].name, "top");
        assert_eq!(tree[0].size, 10);
        assert_eq!(tree[1].full_path, "top/mid");
        let (dirs, files, total) = archive_summary(&tree);
        assert_eq!((dirs, files), (2, 2));
        assert_eq!(total, 10);
    }

    /// 端到端：写一个真实 zip，走 read_archive_entries → build_archive_tree。
    /// 结构对齐参考截图：空文件夹若干 + 含子文件夹与文件的文件夹 + 根级文件。
    #[test]
    fn test_real_zip_end_to_end_tree() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("ff_arch_tree_{}.zip", std::process::id()));

        let f = std::fs::File::create(&p).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.add_directory("新建文件夹 (3)/", opts).unwrap();
        zip.add_directory("新建文件夹 (7)/新建文件夹/", opts).unwrap();
        zip.start_file("新建文件夹 (7)/新建 PPT 演示文稿.ppt", opts)
            .unwrap();
        zip.write_all(&vec![b'p'; 2048]).unwrap();
        zip.start_file("新建 DOC 文档.doc", opts).unwrap();
        zip.write_all(&vec![b'd'; 1024]).unwrap();
        zip.finish().unwrap();

        let items = read_archive_entries(&p).expect("读取 zip 失败");
        let tree = build_archive_tree(&items);
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        // 目录优先、同层按名称序；子项紧随父目录；根级文件最后
        assert_eq!(
            names,
            vec![
                "新建文件夹 (3)",
                "新建文件夹 (7)",
                "新建文件夹",
                "新建 PPT 演示文稿.ppt",
                "新建 DOC 文档.doc",
            ]
        );

        // 空目录无箭头；含子项的目录有箭头且大小为后代之和
        let d3 = &tree[0];
        assert!(d3.is_dir && !d3.has_children && d3.level == 0);
        let d7 = &tree[1];
        assert!(d7.is_dir && d7.has_children && d7.size == 2048);
        // 嵌套空目录层级为 1，文件层级为 1
        assert_eq!(tree[2].level, 1);
        assert!(tree[2].is_dir && !tree[2].has_children);
        assert_eq!(tree[3].level, 1);
        assert_eq!(tree[3].full_path, "新建文件夹 (7)/新建 PPT 演示文稿.ppt");
        // 根级文件
        assert!(!tree[4].is_dir && tree[4].level == 0 && tree[4].size == 1024);

        let (dirs, files, total) = archive_summary(&tree);
        assert_eq!((dirs, files), (3, 2));
        assert_eq!(total, 3072);

        std::fs::remove_file(&p).ok();
    }

    /// 端到端：写一个真实 7z（含显式空目录条目），验证 sevenz_rust 的
    /// 目录条目语义下树构建同样正确（is_directory 而非 zip 的尾斜杠约定）。
    #[test]
    fn test_real_7z_end_to_end_tree() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("ff_7z_src_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("空文件夹")).unwrap();
        std::fs::create_dir_all(dir.join("有内容/子层")).unwrap();
        std::fs::write(dir.join("有内容/子层/深层.bin"), vec![b'x'; 300]).unwrap();
        std::fs::write(dir.join("根文件.txt"), vec![b'y'; 100]).unwrap();

        let mut archive = std::env::temp_dir();
        archive.push(format!("ff_7z_{}.7z", std::process::id()));
        let _ = std::fs::remove_file(&archive);
        {
            let mut sz = sevenz_rust::SevenZWriter::create(&archive).unwrap();
            // 目录条目 + 文件条目，相对路径用 / 分隔
            for (rel, is_dir) in [
                ("空文件夹", true),
                ("有内容", true),
                ("有内容/子层", true),
                ("有内容/子层/深层.bin", false),
                ("根文件.txt", false),
            ] {
                let src = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
                let entry = sevenz_rust::SevenZWriter::<std::fs::File>::create_archive_entry(
                    &src,
                    rel.to_string(),
                );
                let reader = if is_dir {
                    None
                } else {
                    Some(std::fs::File::open(&src).unwrap())
                };
                sz.push_archive_entry(entry, reader).unwrap();
            }
            sz.finish().unwrap();
        }

        let items = read_archive_entries(&archive).expect("读取 7z 失败");
        let tree = build_archive_tree(&items);
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        // 目录优先，同层按 to_lowercase() 码点序（与 app.rs 的文件列表排序一致，
        // 中文因此按码点而非拼音排列）；子项紧随父目录。
        assert_eq!(
            names,
            vec!["有内容", "子层", "深层.bin", "空文件夹", "根文件.txt"]
        );
        // 含子项的目录有箭头，大小为后代之和
        assert!(tree[0].is_dir && tree[0].has_children && tree[0].size == 300);
        assert_eq!(tree[1].level, 1);
        assert_eq!(tree[2].level, 2);
        assert_eq!(tree[2].full_path, "有内容/子层/深层.bin");
        // 空目录无箭头，且不吞掉后续同层节点
        assert!(tree[3].is_dir && !tree[3].has_children);
        assert!(!tree[4].is_dir && tree[4].size == 100);

        std::fs::remove_file(&archive).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_archive_kind_code_is_tree() {
        // 归档不再复用文本面板，独立编码 5
        assert_eq!(PreviewKind::Archive.code(), 5);
        assert_eq!(PreviewKind::Text.code(), 2);
    }

    #[test]
    fn test_binary_detect() {
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_prev_bin_{}.dat", std::process::id()));
        std::fs::write(&p, [0u8, 1, 2, 3, 0, 5]).unwrap();
        let t = read_text_head(&p, 1024);
        // 二进制内容给出十六进制预览：含偏移行与提示头
        assert!(t.contains("十六进制预览"));
        assert!(t.contains("00000000"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn test_hex_dump_format() {
        let data: Vec<u8> = (0..20u8).collect();
        let d = hex_dump(&data);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("00000000"));
        assert!(lines[1].starts_with("00000010"));
        // ASCII 侧栏：0x00-0x13 均不可打印，全为 '.'
        assert!(lines[0].ends_with("................"));
        assert!(lines[1].contains("10 11 12 13"));
    }
}
