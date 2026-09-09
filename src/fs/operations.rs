//! 文件系统操作：目录读取、复制、移动、删除、重命名、新建

use super::metadata::{classify, unix_ts, Entry};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 读取目录内容，返回条目列表（文件夹优先，再按名称排序）
/// `show_hidden` 为 false 时过滤掉带隐藏属性的项目；
/// `show_protected` 为 false 时过滤掉「受保护的操作系统项目」（HIDDEN+SYSTEM）。
pub fn read_dir(path: &Path, show_hidden: bool, show_protected: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for dirent in fs::read_dir(path)? {
        let dirent = match dirent {
            Ok(d) => d,
            Err(_) => continue,
        };
        let p = dirent.path();
        let meta = match dirent.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        let name = dirent.file_name().to_string_lossy().to_string();
        if should_hide(&name, &meta, show_hidden, show_protected) {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let (icon_class, icon_label, kind) = classify(&p, is_dir);

        entries.push(Entry {
            name,
            path: p.to_string_lossy().to_string(),
            is_dir,
            size_bytes: if is_dir { 0 } else { meta.len() },
            modified_ts: unix_ts(modified),
            kind,
            icon_label,
            icon_class,
        });
    }
    Ok(entries)
}

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

/// 永不显示的系统文件：无论“显示隐藏/受保护”如何组合均隐藏，且禁止任何操作触及。
fn is_always_hidden(name: &str) -> bool {
    name.eq_ignore_ascii_case("desktop.ini") || name.eq_ignore_ascii_case("thumbs.db")
}

/// 目录视图与文件夹详情共用的过滤规则，语义对齐资源管理器的两个独立选项：
/// 「显示隐藏的文件」（show_hidden）与「隐藏受保护的操作系统文件」（show_protected）。
pub(crate) fn should_hide(
    name: &str,
    meta: &fs::Metadata,
    show_hidden: bool,
    show_protected: bool,
) -> bool {
    if is_always_hidden(name) {
        return true;
    }
    let attrs = file_attributes(meta);
    if !show_protected && is_protected_attrs(attrs) {
        return true;
    }
    !show_hidden && is_hidden_name_or_attrs(name, attrs)
}

/// 调用方在对具体文件执行任何操作前调用：永不操作的文件返回 true。
pub fn is_forbidden_target(name: &str) -> bool {
    is_always_hidden(name)
}

/// 详情统计使用的默认过滤规则（与资源管理器及目录视图一致）。
pub(crate) fn is_hidden_entry(
    name: &str,
    meta: &fs::Metadata,
    show_hidden: bool,
    show_protected: bool,
) -> bool {
    should_hide(name, meta, show_hidden, show_protected)
}

/// 「受保护的操作系统项目」判定：资源管理器要求 HIDDEN 与 SYSTEM 同时置位。
/// 只看 SYSTEM 会把资源管理器可见的普通系统文件一并藏掉，导致条目数偏少。
pub(crate) fn is_protected_attrs(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_HIDDEN != 0 && attrs & FILE_ATTRIBUTE_SYSTEM != 0
}

/// 隐藏项判定。Windows 下只认 HIDDEN 属性——点开头是 Unix 约定，
/// 资源管理器并不因此隐藏（如 .cargo / .config 均正常显示）。
pub(crate) fn is_hidden_name_or_attrs(name: &str, attrs: u32) -> bool {
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        return true;
    }
    #[cfg(windows)]
    {
        let _ = name;
        false
    }
    #[cfg(not(windows))]
    {
        name.starts_with('.')
    }
}

fn file_attributes(meta: &fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        0
    }
}

fn is_forbidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(is_always_hidden)
        .unwrap_or(false)
}

/// 重命名：禁止操作永不显示的系统文件
pub fn rename(old: &Path, new_name: &str) -> io::Result<PathBuf> {
    if is_forbidden_path(old)
        || new_name.eq_ignore_ascii_case("desktop.ini")
        || new_name.eq_ignore_ascii_case("thumbs.db")
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "系统保护文件，禁止操作",
        ));
    }
    let parent = old.parent().unwrap_or(Path::new("."));
    let new_path = parent.join(new_name);
    fs::rename(old, &new_path)?;
    Ok(new_path)
}

/// 永久删除（不经回收站）。回收站清空等不可逆场景使用；
/// 普通删除请走 `recyclebin::move_to_recycle_bin`。
/// 永不显示的系统文件禁止删除。
#[allow(dead_code)]
pub fn delete(path: &Path) -> io::Result<()> {
    if is_forbidden_path(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "系统保护文件，禁止删除",
        ));
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// 递归复制目录或文件，自动处理同名冲突（追加 副本）。
/// 同步实现，UI 粘贴路径现走 `tasks` 异步队列；此处保留供测试与同步调用。
/// 永不显示的系统文件禁止复制。
#[allow(dead_code)]
pub fn copy_into(src: &Path, dst_dir: &Path) -> io::Result<PathBuf> {
    if is_forbidden_path(src) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "系统保护文件，禁止操作",
        ));
    }
    let file_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无效源路径"))?;
    let mut target = dst_dir.join(file_name);
    target = resolve_conflict(target);

    if src.is_dir() {
        copy_dir_recursive(src, &target)?;
    } else {
        fs::copy(src, &target)?;
    }
    Ok(target)
}

/// 移动（同盘 rename，跨盘 复制后删除）。
/// 同步实现，UI 粘贴路径现走 `tasks` 异步队列；此处保留供测试与同步调用。
/// 永不显示的系统文件禁止移动。
#[allow(dead_code)]
pub fn move_into(src: &Path, dst_dir: &Path) -> io::Result<PathBuf> {
    if is_forbidden_path(src) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "系统保护文件，禁止操作",
        ));
    }
    let file_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无效源路径"))?;
    let mut target = dst_dir.join(file_name);
    target = resolve_conflict(target);

    match fs::rename(src, &target) {
        Ok(_) => Ok(target),
        Err(_) => {
            // 跨盘：复制后删除源
            if src.is_dir() {
                copy_dir_recursive(src, &target)?;
                fs::remove_dir_all(src)?;
            } else {
                fs::copy(src, &target)?;
                fs::remove_file(src)?;
            }
            Ok(target)
        }
    }
}

/// 解决同名冲突：name -> name (2) -> name (3)
pub(crate) fn resolve_conflict(mut target: PathBuf) -> PathBuf {
    if !target.exists() {
        return target;
    }
    let parent = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = target.extension().map(|e| e.to_string_lossy().to_string());

    let mut n = 2;
    loop {
        let candidate_name = match &ext {
            Some(e) => format!("{} ({}).{}", stem, n, e),
            None => format!("{} ({})", stem, n),
        };
        target = parent.join(candidate_name);
        if !target.exists() {
            return target;
        }
        n += 1;
        if n > 9999 {
            return target;
        }
    }
}

#[allow(dead_code)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 新建文件夹，自动避免重名
pub fn new_folder(parent: &Path, base: &str) -> io::Result<PathBuf> {
    let target = resolve_conflict(parent.join(base));
    fs::create_dir(&target)?;
    Ok(target)
}

/// 新建空文件，自动避免重名
pub fn new_file(parent: &Path, base: &str) -> io::Result<PathBuf> {
    let target = resolve_conflict(parent.join(base));
    fs::File::create(&target)?;
    Ok(target)
}

/// 支持的归档格式（ZIP 容器家族均按 Zip 处理：msix/appx/apk 等本质为 ZIP）
#[derive(Clone, Copy, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    SevenZ,
    Tar,
    TarGz,
}

/// 判断路径的归档格式（按扩展名）。非归档返回 None。
/// msix/msixbundle/appx/appxbundle/apk/aab/ipa 等本质为 ZIP 容器，按 Zip 处理；
/// cab(MSCF)/iso(ISO9660)/vhd 并非 ZIP，强行解析必然报错，不支持解包。
pub fn is_archive(path: &Path) -> Option<ArchiveFormat> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "zip"
        | "msix"
        | "msixbundle"
        | "appx"
        | "appxbundle"
        | "apk"
        | "aab"
        | "ipa" => Some(ArchiveFormat::Zip),
        "7z" => Some(ArchiveFormat::SevenZ),
        "tar" => Some(ArchiveFormat::Tar),
        "gz" | "tgz" => Some(ArchiveFormat::TarGz),
        _ => None,
    }
}

/// 判断路径是否为可解压归档（任意支持格式）。保留旧名以兼容 UI 调用。
pub fn is_zip_archive(path: &Path) -> bool {
    is_archive(path).is_some()
}

/// 归档基名：单项用其文件名（去扩展名），多项用「首项 等」。
/// 压缩任务入队时用它与 `resolve_conflict` 确定归档输出路径。
pub fn archive_stem(items: &[PathBuf]) -> String {
    let first_name = items[0]
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "归档".to_string());
    if items.len() == 1 {
        items[0]
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(first_name)
    } else {
        format!("{} 等", first_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// 「受保护的操作系统项目」必须 HIDDEN+SYSTEM 同时置位：只看 SYSTEM 会把
    /// 资源管理器可见的普通系统文件也藏掉，导致条目数比资源管理器少。
    #[test]
    fn protected_requires_both_hidden_and_system() {
        assert!(is_protected_attrs(
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
        ));
        assert!(!is_protected_attrs(FILE_ATTRIBUTE_SYSTEM));
        assert!(!is_protected_attrs(FILE_ATTRIBUTE_HIDDEN));
        assert!(!is_protected_attrs(0));
    }

    /// Windows 下点开头不是隐藏（.cargo/.config 在资源管理器中正常显示），
    /// 只有 HIDDEN 属性才算隐藏项。
    #[test]
    fn dot_prefix_is_not_hidden_on_windows() {
        assert!(is_hidden_name_or_attrs("anything", FILE_ATTRIBUTE_HIDDEN));
        assert!(!is_hidden_name_or_attrs("visible.txt", 0));
        #[cfg(windows)]
        assert!(!is_hidden_name_or_attrs(".cargo", 0));
        #[cfg(not(windows))]
        assert!(is_hidden_name_or_attrs(".cargo", 0));
    }

    // 创建隔离的临时测试目录
    fn temp_dir() -> PathBuf {
        let mut d = env::temp_dir();
        let unique = format!(
            "filefiles_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        d.push(unique);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_new_folder_and_conflict() {
        let dir = temp_dir();
        let a = new_folder(&dir, "测试").unwrap();
        assert!(a.is_dir());
        // 同名再建应得到 "测试 (2)"
        let b = new_folder(&dir, "测试").unwrap();
        assert!(b.is_dir());
        assert_ne!(a, b);
        assert!(b.file_name().unwrap().to_string_lossy().contains("(2)"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_new_file_and_rename() {
        let dir = temp_dir();
        let f = new_file(&dir, "笔记.txt").unwrap();
        assert!(f.is_file());
        let renamed = rename(&f, "新笔记.txt").unwrap();
        assert!(renamed.is_file());
        assert!(!f.exists());
        assert_eq!(renamed.file_name().unwrap().to_string_lossy(), "新笔记.txt");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_copy_and_move() {
        let dir = temp_dir();
        let src_dir = dir.join("源");
        let dst_dir = dir.join("目标");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();
        let file = src_dir.join("数据.bin");
        fs::write(&file, b"hello").unwrap();

        // 复制：源仍在，目标出现
        let copied = copy_into(&file, &dst_dir).unwrap();
        assert!(file.exists());
        assert!(copied.exists());
        assert_eq!(fs::read(&copied).unwrap(), b"hello");

        // 移动：源消失
        let moved = move_into(&file, &dst_dir).unwrap();
        assert!(!file.exists());
        assert!(moved.exists());

        fs::remove_dir_all(&dir).ok();
    }

    // 归档压缩/解压的往返测试迁至 fs::tasks（覆盖真实的后台流式实现）

    #[test]
    fn test_read_dir_classify() {
        let dir = temp_dir();
        fs::write(dir.join("a.rs"), b"fn main(){}").unwrap();
        fs::create_dir(dir.join("子目录")).unwrap();
        let entries = read_dir(&dir, true, true).unwrap();
        assert_eq!(entries.len(), 2);
        let rs = entries.iter().find(|e| e.name == "a.rs").unwrap();
        assert_eq!(rs.icon_class, "code");
        assert_eq!(rs.icon_label, "RS");
        let sub = entries.iter().find(|e| e.name == "子目录").unwrap();
        assert!(sub.is_dir);
        fs::remove_dir_all(&dir).ok();
    }
}
