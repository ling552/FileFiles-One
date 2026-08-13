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
    /// 视频：内嵌播放（Media Foundation 子窗口渲染，含音频）
    Video,
    /// 归档：列出压缩包内文件（内容区复用文本面板显示清单）
    Archive,
    /// 其它：仅展示图标与基础信息
    Info,
}

impl PreviewKind {
    /// 传给 Slint 的整型编码（与 preview_window.slint / quick_look.slint 约定一致）
    /// 0 信息 / 1 图片 / 2 文本 / 3 文件夹 / 4 视频 / 5 归档树。
    /// 归档改为可展开/折叠的树形列表，单独占用编码 5。
    pub fn code(self) -> i32 {
        match self {
            PreviewKind::Info => 0,
            PreviewKind::Image => 1,
            PreviewKind::Text => 2,
            PreviewKind::Folder => 3,
            PreviewKind::Video => 4,
            PreviewKind::Archive => 5,
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
const BINARY_EXTS: &[&str] = &[
    // 可执行/系统二进制
    "exe", "msi", "dll", "sys", "com", "scr",
    // 磁盘映像/安装包/容器
    "iso", "img", "vhd", "vhdx", "cab", "msu", "dmp", "pdb", "deb", "rpm", "appimage", "dmg", "pkg",
    // 旧版 Office / PDF：无轻量解析路径，显示文件信息
    "doc", "xls", "xlsx", "ppt", "pptx", "pdf",
];

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 是否支持「渲染视图」（WebView2 显示网页效果，与源码视图可切换）：
/// Markdown 转 HTML 渲染；HTML/HTM 直接渲染；PHP 渲染其中的静态 HTML 部分；
/// DOCX 抽取正文后转 HTML 渲染。
pub fn renderable_web(path: &Path) -> bool {
    matches!(
        ext_of(path).as_str(),
        "md" | "markdown" | "html" | "htm" | "php" | "docx"
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
    } else if is_archive_kind(&ext, path) {
        PreviewKind::Archive
    } else if is_binary_kind(&ext) {
        // EXE/MSI/Office/PDF 等已知类型二进制：显示应用/文件基本信息（含版本资源）
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
fn is_archive_kind(ext: &str, _path: &Path) -> bool {
    matches!(ext, "zip" | "7z" | "tar" | "gz" | "tgz")
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

/// 抽取 Word 文档（.docx）正文：zip 容器读 word/document.xml，
/// 段落/换行标签转行、剥离其余 XML 标签、解码常见实体。
/// 读取失败返回 None（调用方回退其它预览方式）。
pub fn office_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(f).ok()?;
    let mut raw = String::new();
    zip.by_name("word/document.xml")
        .ok()?
        .read_to_string(&mut raw)
        .ok()?;
    // 段落 / 换行 / 制表符标签 → 对应文本字符
    let raw = raw
        .replace("</w:p>", "\n")
        .replace("<w:br/>", "\n")
        .replace("<w:tab/>", "\t");
    // 剥离剩余 XML 标签
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    // 解码常见 XML 实体
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#160;", " ")
        .replace("&amp;", "&");
    Some(text)
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

/// 读取归档原始条目 (名, 大小, 是否目录)。按格式读取条目名/大小/是否目录，
/// 上限 2000 项防超大归档卡顿。复用 tasks.rs 已验证的读取路径
/// （zip::ZipArchive / sevenz_rust::SevenZReader / tar::Archive）。
pub fn read_archive_entries(path: &Path) -> Result<Vec<(String, u64, bool)>, String> {
    use std::io::Read;
    let ext = ext_of(path);
    // 用闭包包裹使 `?` 可用并保持原有各格式分支不变
    let result: Result<Vec<(String, u64, bool)>, String> = (|| {
        let mut items: Vec<(String, u64, bool)> = Vec::new();
        match ext.as_str() {
            "zip" => {
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
                    // 普通 .gz 单文件：用解压后的原始文件名与解压大小
                    let mut f2 = std::fs::File::open(path).map_err(|e| e.to_string())?;
                    let mut dec = flate2::read::GzDecoder::new(&mut f2);
                    let mut buf = Vec::new();
                    let size = dec.read_to_end(&mut buf).unwrap_or(0) as u64;
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
        assert_eq!(kind_of(Path::new("a.7z"), false), PreviewKind::Archive);
        assert_eq!(kind_of(Path::new("anything"), true), PreviewKind::Folder);
        // 大小写不敏感
        assert_eq!(kind_of(Path::new("A.PNG"), false), PreviewKind::Image);
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
