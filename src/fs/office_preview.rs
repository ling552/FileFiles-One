//! Office 高保真预览：调用本机已安装的 Microsoft Office 把文档转为 PDF，
//! 再由 WebView2（Edge 内核 PDF 查看器）渲染展示。
//!
//! 为什么走「Office 转 PDF → Edge 显示」：
//! 1. 保真度：Word 分页/样式、Excel 网格/多工作表、PPT 版式/母版都由 Office
//!    自身排版引擎生成，与双击用 Office 打开看到的完全一致；纯文本抽取
//!    会丢失表格、图片、分页等全部版式信息。
//! 2. 零新增依赖：经 PowerShell COM 调用 Word/Excel/PowerPoint，不引入
//!    LibreOffice/第三方渲染库；未安装 Office 时自动回退纯文本预览。
//! 3. 不阻塞 UI：转换在后台线程执行并带缓存（源路径+大小+修改时间哈希），
//!    命中缓存秒开；未命中时先显示文本版，转换完成后自动升级为 PDF 版。
//!
//! 安全：PowerShell 脚本以只读方式打开文档（ReadOnly），不执行宏；
//! 临时 PDF 落在系统临时目录的本应用专属子目录，定期清理 7 天前的缓存。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 本机 Office 应用标识
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfficeApp {
    Word,
    Excel,
    PowerPoint,
}

/// 按扩展名判断所属 Office 应用（大小写不敏感）
pub fn office_app_for_ext(ext: &str) -> Option<OfficeApp> {
    match ext.to_ascii_lowercase().as_str() {
        "doc" | "docx" => Some(OfficeApp::Word),
        "xls" | "xlsx" => Some(OfficeApp::Excel),
        "ppt" | "pptx" => Some(OfficeApp::PowerPoint),
        _ => None,
    }
}

/// 是否 Office 文档（含旧版 doc/xls/ppt）
pub fn is_office_doc(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| office_app_for_ext(e).is_some())
        .unwrap_or(false)
}

/// 对应 Office 应用是否已安装：优先查 HKCR\<App>.Application，
/// 再查 App Paths 下的可执行文件（COM 注册缺失但程序在的兜底）。
#[cfg(windows)]
pub fn is_office_installed(app: OfficeApp) -> bool {
    use winreg::enums::*;
    use winreg::RegKey;
    let prog_id = match app {
        OfficeApp::Word => "Word.Application",
        OfficeApp::Excel => "Excel.Application",
        OfficeApp::PowerPoint => "PowerPoint.Application",
    };
    if RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey(prog_id)
        .is_ok()
    {
        return true;
    }
    let exe = match app {
        OfficeApp::Word => "WINWORD.EXE",
        OfficeApp::Excel => "EXCEL.EXE",
        OfficeApp::PowerPoint => "POWERPNT.EXE",
    };
    let sub = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}", exe);
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(&sub)
        .is_ok()
        || RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(&sub)
            .is_ok()
}

#[cfg(not(windows))]
pub fn is_office_installed(_app: OfficeApp) -> bool {
    false
}

/// 预览缓存目录
fn cache_dir() -> PathBuf {
    std::env::temp_dir()
        .join("FileFiles One")
        .join("office_preview")
}

/// 缓存文件名：源文件名 stem + 内容指纹（大小+修改时间+路径哈希），避免同名覆盖。
fn cached_pdf_path_for(src: &Path, meta: &std::fs::Metadata) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(src.to_string_lossy().as_bytes());
    h.update(meta.len().to_le_bytes());
    h.update(mtime.to_le_bytes());
    let hex = format!("{:x}", h.finalize());
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "doc".to_string());
    // 文件名过长会导致创建失败，截断 stem 到 40 字符（按字符数，非字节）
    let short: String = stem.chars().take(40).collect();
    let safe: String = short
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' || c >= '\u{4e00}' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache_dir().join(format!("{}_{}_{}.pdf", safe.trim(), meta.len(), &hex[..16]))
}

/// 缓存命中且比源文件新时直接返回（不做任何转换，UI 线程可安全调用）。
/// 附带 PDF 有效性校验：转换中断/超时的残留 partial 文件会被识别并删除，
/// 杜绝 WebView 打开损坏 PDF 报错（此前 Excel 预览即因此报错）。
pub fn cached_pdf_if_fresh(src: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(src).ok()?;
    // 超大文件不进 Office 转换（Office 打开即卡），上限 200MB
    if meta.len() > 200 * 1024 * 1024 {
        return None;
    }
    let out = cached_pdf_path_for(src, &meta);
    let out_meta = std::fs::metadata(&out).ok()?;
    if out_meta.len() == 0 || !is_valid_pdf(&out) {
        let _ = std::fs::remove_file(&out);
        return None;
    }
    let src_mtime = meta.modified().ok()?;
    let out_mtime = out_meta.modified().ok()?;
    if out_mtime >= src_mtime {
        Some(out)
    } else {
        None
    }
}

/// 快速校验 PDF 有效性：非空且以 %PDF 魔数开头。
/// Office 导出中断（超时杀进程、COM 异常退出）会留下非空但截断的文件，
/// 仅凭长度无法识别，必须校验魔数。
pub fn is_valid_pdf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 5];
    matches!(f.read_exact(&mut magic), Ok(())) && magic == *b"%PDF-"
}

/// 转换失败记忆（进程内）：同一文件（路径+大小+修改时间）短时间内失败过则
/// 直接回退，不再重复拉起 Office 空等超时。避免坏文件/顽固弹窗导致每次预览
/// 都卡数十秒（如 PowerPoint 首启协议弹窗、损坏文档导致的 COM 挂起）。
fn failed_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, (u64, u64, Instant)>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (u64, u64, Instant)>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 失败记忆有效期：10 分钟（源文件变化则自动失效）
const FAILED_TTL: Duration = Duration::from_secs(10 * 60);

fn failed_key(src: &Path, meta: &std::fs::Metadata) -> (String, u64, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (src.to_string_lossy().to_string(), meta.len(), mtime)
}

fn recently_failed(src: &Path, meta: &std::fs::Metadata) -> bool {
    let (key, len, mtime) = failed_key(src, meta);
    failed_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&key).cloned())
        .map(|(l, m, when)| l == len && m == mtime && when.elapsed() < FAILED_TTL)
        .unwrap_or(false)
}

fn mark_failed(src: &Path, meta: &std::fs::Metadata) {
    let (key, len, mtime) = failed_key(src, meta);
    if let Ok(mut c) = failed_cache().lock() {
        if c.len() >= 128 {
            c.clear();
        }
        c.insert(key, (len, mtime, Instant::now()));
    }
}

/// 阻塞式转换：有缓存直接返回；否则调用 Office 转 PDF（后台线程调用）。
/// Office 未安装/转换失败返回 None，调用方回退文本预览。
/// 同一文件 10 分钟内失败过则直接返回 None（不再重复拉起 Office 空等超时）。
pub fn convert_to_pdf_blocking(src: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(src).ok()?;
    if meta.len() > 200 * 1024 * 1024 {
        return None;
    }
    if recently_failed(src, &meta) {
        return None;
    }
    let fail = |src: &Path, meta: &std::fs::Metadata| {
        mark_failed(src, meta);
        None
    };
    let out = cached_pdf_path_for(src, &meta);
    if let Some(hit) = cached_pdf_if_fresh(src) {
        return Some(hit);
    }
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let app = office_app_for_ext(&ext)?;
    if !is_office_installed(app) {
        return fail(src, &meta);
    }
    if std::fs::create_dir_all(cache_dir()).is_err() {
        return fail(src, &meta);
    }
    cleanup_old_cache();
    if run_office_export(src, &out, app).is_err() {
        let _ = std::fs::remove_file(&out);
        return fail(src, &meta);
    }
    // 魔数校验：COM 调用返回成功不代表 PDF 完整（如进程被杀时留下截断文件）
    if !is_valid_pdf(&out) {
        let _ = std::fs::remove_file(&out);
        return fail(src, &meta);
    }
    Some(out)
}

/// PowerShell 单引号转义（' → ''）
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// ps1 脚本名序号：多个文档并发转换时各自使用独立脚本文件，
/// 避免固定文件名被后写者覆盖、先启动的 PowerShell 读到别人的脚本
static PS1_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 调用本机 Office 导出 PDF。脚本写临时 .ps1 文件执行，避免命令行引号地狱与
/// 中文路径编码问题；整体超时 60 秒（Office 冷启动约 10~20 秒），超时杀进程。
#[cfg(windows)]
fn run_office_export(src: &Path, dst: &Path, app: OfficeApp) -> Result<(), String> {
    let src_str = src.to_string_lossy().to_string();
    let dst_str = dst.to_string_lossy().to_string();
    let body = match app {
        OfficeApp::Word => format!(
            "$ErrorActionPreference='Stop'\r\n\
             $src='{src}'; $dst='{dst}'\r\n\
             $w=New-Object -ComObject Word.Application\r\n\
             try {{\r\n\
             \x20 $w.Visible=$false; $w.DisplayAlerts=0\r\n\
             \x20 $d=$w.Documents.Open($src, $false, $true, $false)\r\n\
             \x20 $d.ExportAsFixedFormat($dst, 17)\r\n\
             \x20 $d.Close($false)\r\n\
             }} finally {{ $w.Quit(); \
             [System.Runtime.Interopservices.Marshal]::ReleaseComObject($w)|Out-Null }}\r\n",
            src = ps_quote(&src_str),
            dst = ps_quote(&dst_str),
        ),
        OfficeApp::Excel => format!(
            "$ErrorActionPreference='Stop'\r\n\
             $src='{src}'; $dst='{dst}'\r\n\
             $x=New-Object -ComObject Excel.Application\r\n\
             try {{\r\n\
             \x20 $x.Visible=$false; $x.DisplayAlerts=$false; $x.AskToUpdateLinks=$false\r\n\
             \x20 $b=$x.Workbooks.Open($src, $false, $true)\r\n\
             \x20 $b.ExportAsFixedFormat(0, $dst)\r\n\
             \x20 $b.Close($false)\r\n\
             }} finally {{ $x.Quit(); \
             [System.Runtime.Interopservices.Marshal]::ReleaseComObject($x)|Out-Null }}\r\n",
            src = ps_quote(&src_str),
            dst = ps_quote(&dst_str),
        ),
        OfficeApp::PowerPoint => format!(
            "$ErrorActionPreference='Stop'\r\n\
             $src='{src}'; $dst='{dst}'\r\n\
             $p=New-Object -ComObject PowerPoint.Application\r\n\
             try {{\r\n\
             \x20 $pres=$p.Presentations.Open($src, $true, $true, $false)\r\n\
             \x20 $pres.ExportAsFixedFormat($dst, 2)\r\n\
             \x20 $pres.Close()\r\n\
             }} finally {{ $p.Quit() }}\r\n",
            src = ps_quote(&src_str),
            dst = ps_quote(&dst_str),
        ),
    };
    // 脚本文件带 BOM 的 UTF-8：PowerShell 5.1 按 ANSI 读无 BOM 脚本会乱码中文路径
    let ps_path = cache_dir().join(format!(
        "ff_office_{}_{}.ps1",
        std::process::id(),
        PS1_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(&ps_path, &bytes).map_err(|e| e.to_string())?;
    // 60 秒超时：Office 冷启动约 10~20 秒；超时后由失败记忆熔断，
    // 同一文件 10 分钟内不再重复拉起 Office 空等。
    let result = run_powershell_script(&ps_path, Duration::from_secs(60));
    let _ = std::fs::remove_file(&ps_path);
    result
}

#[cfg(not(windows))]
fn run_office_export(_src: &Path, _dst: &Path, _app: OfficeApp) -> Result<(), String> {
    Err("非 Windows 平台不支持 Office 预览".to_string())
}

/// 执行 ps1 脚本并等待（轮询 try_wait 实现超时杀进程）
#[cfg(windows)]
fn run_powershell_script(ps_path: &Path, timeout: Duration) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &ps_path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("无法启动 PowerShell：{}", e))?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("Office 导出失败（退出码 {:?}）", status.code()))
                };
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Office 导出超时".to_string());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// 清理 7 天前的缓存 PDF 与残留脚本（ best-effort，忽略全部错误）
fn cleanup_old_cache() {
    let dir = cache_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let p = entry.path();
        let stale = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf") || e.eq_ignore_ascii_case("ps1"))
            .unwrap_or(false);
        if !stale {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs() > 7 * 24 * 3600)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_mapping_covers_all_office_exts() {
        assert_eq!(office_app_for_ext("docx"), Some(OfficeApp::Word));
        assert_eq!(office_app_for_ext("DOC"), Some(OfficeApp::Word));
        assert_eq!(office_app_for_ext("xlsx"), Some(OfficeApp::Excel));
        assert_eq!(office_app_for_ext("xls"), Some(OfficeApp::Excel));
        assert_eq!(office_app_for_ext("pptx"), Some(OfficeApp::PowerPoint));
        assert_eq!(office_app_for_ext("ppt"), Some(OfficeApp::PowerPoint));
        assert_eq!(office_app_for_ext("pdf"), None);
        assert_eq!(office_app_for_ext("txt"), None);
    }

    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("a'b"), "a''b");
        assert_eq!(ps_quote("无引号"), "无引号");
    }

    #[test]
    fn cache_path_is_stable_and_safe() {
        let dir = std::env::temp_dir();
        let src = dir.join("测试 文档'x'.docx");
        std::fs::write(&src, b"hello").unwrap();
        let meta = std::fs::metadata(&src).unwrap();
        let a = cached_pdf_path_for(&src, &meta);
        let b = cached_pdf_path_for(&src, &meta);
        assert_eq!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("pdf"));
        // 单引号已被清洗，不会破坏 PowerShell 脚本
        assert!(!a.file_name().unwrap().to_string_lossy().contains('\''));
        std::fs::remove_file(&src).ok();
    }

    #[test]
    fn missing_source_has_no_cache() {
        let p = std::env::temp_dir().join("ff_office_missing_测试.docx");
        let _ = std::fs::remove_file(&p);
        assert!(cached_pdf_if_fresh(&p).is_none());
    }

    #[test]
    fn pdf_magic_validation_rejects_truncated_exports() {
        let dir = std::env::temp_dir();
        // 非 PDF 内容（模拟 COM 中断留下的 partial 文件）判定无效
        let bad = dir.join("ff_office_bad.pdf");
        std::fs::write(&bad, b"not a pdf at all").unwrap();
        assert!(!is_valid_pdf(&bad));
        // 空文件无效
        let empty = dir.join("ff_office_empty.pdf");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_pdf(&empty));
        // 最小合法头有效
        let good = dir.join("ff_office_good.pdf");
        std::fs::write(&good, b"%PDF-1.7\ntrailer").unwrap();
        assert!(is_valid_pdf(&good));
        std::fs::remove_file(&bad).ok();
        std::fs::remove_file(&empty).ok();
        std::fs::remove_file(&good).ok();
    }

    #[test]
    fn failed_conversion_is_remembered_then_expires_by_source_change() {
        let dir = std::env::temp_dir();
        let src = dir.join("ff_office_fail_mem.docx");
        std::fs::write(&src, b"v1").unwrap();
        let meta = std::fs::metadata(&src).unwrap();
        assert!(!recently_failed(&src, &meta));
        mark_failed(&src, &meta);
        assert!(recently_failed(&src, &meta));
        // 源文件变化（大小改变）后记忆失效，允许重试
        std::fs::write(&src, b"v1-longer").unwrap();
        let meta2 = std::fs::metadata(&src).unwrap();
        assert!(!recently_failed(&src, &meta2));
        std::fs::remove_file(&src).ok();
    }
}
