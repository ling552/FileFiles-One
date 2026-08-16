//! 回收站枚举与还原（Windows `$Recycle.Bin` 解析）。
//! 本期实现：浏览 + 打开 + 还原。

use super::metadata::Entry;
use std::sync::atomic::{AtomicI8, Ordering};

// -1=未知，0=非空，1=空。实际 Shell 查询更新缓存，侧边栏热路径只读原子值。
static EMPTY_CACHE: AtomicI8 = AtomicI8::new(-1);

pub fn cached_is_empty() -> Option<bool> {
    match EMPTY_CACHE.load(Ordering::Relaxed) {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// 回收站是否为空（侧栏图标据此显示空/满状态）。查询失败返回 None。
pub fn is_empty() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
        let mut info = SHQUERYRBINFO {
            cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
            ..Default::default()
        };
        // 空路径 = 查询所有驱动器回收站合计
        unsafe { SHQueryRecycleBinW(windows::core::PCWSTR::null(), &mut info).ok()? };
        let empty = info.i64NumItems == 0;
        EMPTY_CACHE.store(if empty { 1 } else { 0 }, Ordering::Relaxed);
        Some(empty)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 列出回收站中的已删除项目。非 Windows 或失败时返回空列表。
pub fn list_recycle_bin() -> Vec<Entry> {
    #[cfg(windows)]
    {
        windows_impl::list()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 把若干文件/文件夹移入回收站（可还原）。
/// Windows 使用 Shell 的 `SHFileOperationW` + `FOF_ALLOWUNDO`，与资源管理器一致；
/// 失败时返回错误。非 Windows 平台回退为永久删除。
#[allow(unused_variables)]
pub fn move_to_recycle_bin(paths: &[std::path::PathBuf]) -> std::io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        windows_impl::recycle(paths)
    }
    #[cfg(not(windows))]
    {
        for p in paths {
            if p.is_dir() {
                std::fs::remove_dir_all(p)?;
            } else if p.exists() {
                std::fs::remove_file(p)?;
            }
        }
        Ok(())
    }
}

/// 还原回收站项目到其原始位置。`r_path` 为 `$R` 实际文件路径。
#[allow(unused_variables)]
pub fn restore(r_path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows_impl::restore(r_path)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// 按原始路径从回收站还原（撤销删除用）。
/// 遍历回收站，找原路径匹配、删除时间最新的条目并还原到原位置。
#[allow(unused_variables)]
pub fn restore_to_original(orig: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows_impl::restore_to_original(orig)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// 彻底删除回收站项目：删除 `$R` 内容与配对的 `$I` 元数据。不可逆。
#[allow(unused_variables)]
pub fn delete_permanent(r_path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows_impl::delete_permanent(r_path)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::Entry;
    use crate::fs::metadata::classify;
    use std::path::{Path, PathBuf};

    /// 用 Shell `SHFileOperationW` 把若干路径移入回收站（FOF_ALLOWUNDO）。
    /// pFrom 为「双 NUL 结尾」的多字符串：每个路径以单 NUL 分隔，整体再附加一个 NUL。
    pub fn recycle(paths: &[PathBuf]) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::{
            SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
            FO_DELETE, SHFILEOPSTRUCTW,
        };

        // 构造双 NUL 结尾的宽字符缓冲：每个路径 + 单 NUL，末尾再加一个 NUL。
        let mut from: Vec<u16> = Vec::new();
        for p in paths {
            // 回收站要求绝对路径；尽量规范化，失败则用原路径。
            let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            // canonicalize 会带上 \\?\ verbatim 前缀，SHFileOperation 不接受，需剥离。
            let s = abs.as_os_str().to_string_lossy();
            let s = s
                .strip_prefix(r"\\?\")
                .map(|x| x.to_string())
                .unwrap_or_else(|| s.to_string());
            from.extend(std::ffi::OsStr::new(&s).encode_wide());
            from.push(0);
        }
        from.push(0); // 终结的双 NUL

        let mut op = SHFILEOPSTRUCTW {
            hwnd: std::ptr::null_mut(),
            wFunc: FO_DELETE as u32,
            pFrom: from.as_ptr(),
            pTo: std::ptr::null(),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
            fAnyOperationsAborted: 0,
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: std::ptr::null(),
        };

        let ret = unsafe { SHFileOperationW(&mut op) };
        if ret != 0 {
            return Err(std::io::Error::from_raw_os_error(ret));
        }
        if op.fAnyOperationsAborted != 0 {
            // ret==0 但仍被中止的罕见情形：通常是权限不足导致 Shell 静默放弃。
            // 返回 ACCESS_DENIED(5)，让上层据此请求管理员提权重试。
            return Err(std::io::Error::from_raw_os_error(5));
        }
        Ok(())
    }

    /// 遍历所有盘符下的 `$Recycle.Bin\<SID>\`，配对 `$I`(元数据) 与 `$R`(内容)。
    pub fn list() -> Vec<Entry> {
        let mut entries = Vec::new();
        for letter in b'A'..=b'Z' {
            let bin = format!("{}:\\$Recycle.Bin", letter as char);
            let bin_path = Path::new(&bin);
            if !bin_path.is_dir() {
                continue;
            }
            // 遍历每个 SID 子目录
            let sid_dirs = match std::fs::read_dir(bin_path) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for sid in sid_dirs.flatten() {
                let sid_path = sid.path();
                if !sid_path.is_dir() {
                    continue;
                }
                collect_from_sid(&sid_path, &mut entries);
            }
        }
        entries
    }

    /// 从单个 SID 目录收集 `$I*` 元数据，配对对应 `$R*` 内容文件。
    fn collect_from_sid(sid_path: &Path, out: &mut Vec<Entry>) {
        let rd = match std::fs::read_dir(sid_path) {
            Ok(rd) => rd,
            Err(_) => return,
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            // 只处理 $I 元数据文件
            if !name.starts_with("$I") {
                continue;
            }
            let i_path = ent.path();
            // 对应的 $R 文件：把 $I 换成 $R
            let r_name = format!("$R{}", &name[2..]);
            let r_path = sid_path.join(&r_name);
            if !r_path.exists() {
                continue;
            }
            if let Some((orig_path, size, del_ts)) = parse_i_file(&i_path) {
                let is_dir = r_path.is_dir();
                let orig = Path::new(&orig_path);
                let disp_name = orig
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| orig_path.clone());
                let (icon_class, icon_label, _kind) = classify(orig, is_dir);
                out.push(Entry {
                    name: disp_name,
                    // path 指向 $R 实际内容，供打开/还原使用
                    path: r_path.to_string_lossy().to_string(),
                    is_dir,
                    size_bytes: size,
                    modified_ts: del_ts,
                    kind: format!("已删除 · 原位置 {}", orig_path),
                    icon_label,
                    icon_class,
                });
            }
        }
    }

    /// 解析 `$I` 文件（Win10/11 版本2 格式）。
    /// 返回 (原始完整路径, 原始大小, 删除时间戳秒)。
    fn parse_i_file(path: &Path) -> Option<(String, u64, i64)> {
        let data = std::fs::read(path).ok()?;
        if data.len() < 28 {
            return None;
        }
        // 0..8 header，版本2 应为 2
        let header = u64::from_le_bytes(data[0..8].try_into().ok()?);
        if header != 2 {
            return None;
        }
        // 8..16 原始大小
        let size = u64::from_le_bytes(data[8..16].try_into().ok()?);
        // 16..24 删除时间 FILETIME（自 1601-01-01 起的 100ns 间隔）
        let filetime = u64::from_le_bytes(data[16..24].try_into().ok()?);
        let del_ts = filetime_to_unix(filetime);
        // 24..28 路径字符数（含结尾 NUL）
        let char_count = u32::from_le_bytes(data[24..28].try_into().ok()?) as usize;
        // 28.. UTF-16LE 原始完整路径
        let bytes_needed = char_count * 2;
        if data.len() < 28 + bytes_needed {
            return None;
        }
        let u16s: Vec<u16> = data[28..28 + bytes_needed]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let orig_path = String::from_utf16_lossy(&u16s);
        Some((orig_path, size, del_ts))
    }

    /// Windows FILETIME -> Unix 时间戳秒
    fn filetime_to_unix(ft: u64) -> i64 {
        // FILETIME 起点 1601-01-01，与 Unix 起点 1970-01-01 相差 11644473600 秒
        const EPOCH_DIFF: u64 = 11_644_473_600;
        let secs = ft / 10_000_000;
        secs.saturating_sub(EPOCH_DIFF) as i64
    }

    /// 校验路径指向回收站内的 `$R` 条目：路径须含 `$Recycle.Bin` 组件、
    /// 文件名以 `$R` 开头。防止调用方误传任意路径触发任意删除/移动。
    fn validate_recycle_entry(r: &Path) -> std::io::Result<()> {
        let in_bin = r.components().any(|c| {
            matches!(c, std::path::Component::Normal(n)
                if n.to_string_lossy().eq_ignore_ascii_case("$recycle.bin"))
        });
        let is_r = r
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("$R"))
            .unwrap_or(false);
        if in_bin && is_r {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "非回收站条目路径",
            ))
        }
    }

    /// 还原：把 `$R` 移回 `$I` 记录的原路径，并删除配对的 `$I`。
    pub fn restore(r_path: &str) -> std::io::Result<()> {
        let r = PathBuf::from(r_path);
        validate_recycle_entry(&r)?;
        let parent = r.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效回收站路径")
        })?;
        let r_name = r
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效文件名"))?;
        // $R... -> $I...
        let i_name = format!("$I{}", r_name.get(2..).unwrap_or_default());
        let i_path = parent.join(&i_name);
        let (orig_path, _size, _ts) = parse_i_file(&i_path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "无法解析回收站元数据")
        })?;
        let dest = PathBuf::from(&orig_path);
        if let Some(dp) = dest.parent() {
            std::fs::create_dir_all(dp)?;
        }
        std::fs::rename(&r, &dest)?;
        let _ = std::fs::remove_file(&i_path);
        Ok(())
    }

    /// 按原始路径从回收站还原：遍历所有回收站，匹配原路径、取删除时间最新的 $R 还原。
    pub fn restore_to_original(orig: &Path) -> std::io::Result<()> {
        let want = orig.to_string_lossy().to_lowercase();
        let mut best: Option<(PathBuf, i64)> = None; // (r_path, del_ts)

        for letter in b'A'..=b'Z' {
            let bin = format!("{}:\\$Recycle.Bin", letter as char);
            let bin_path = Path::new(&bin);
            if !bin_path.is_dir() {
                continue;
            }
            let sid_dirs = match std::fs::read_dir(bin_path) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for sid in sid_dirs.flatten() {
                let sid_path = sid.path();
                if !sid_path.is_dir() {
                    continue;
                }
                let rd = match std::fs::read_dir(&sid_path) {
                    Ok(rd) => rd,
                    Err(_) => continue,
                };
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().to_string();
                    if !name.starts_with("$I") {
                        continue;
                    }
                    let r_name = format!("$R{}", &name[2..]);
                    let r_path = sid_path.join(&r_name);
                    if !r_path.exists() {
                        continue;
                    }
                    if let Some((orig_path, _size, del_ts)) = parse_i_file(&ent.path()) {
                        if orig_path.to_lowercase() == want
                            && best.as_ref().map_or(true, |(_, ts)| del_ts > *ts)
                        {
                            best = Some((r_path, del_ts));
                        }
                    }
                }
            }
        }

        match best {
            Some((r_path, _)) => restore(&r_path.to_string_lossy()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "回收站中未找到该项",
            )),
        }
    }

    /// 彻底删除：删除 `$R`（文件或目录）与配对的 `$I` 元数据。
    pub fn delete_permanent(r_path: &str) -> std::io::Result<()> {
        let r = PathBuf::from(r_path);
        validate_recycle_entry(&r)?;
        let parent = r.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效回收站路径")
        })?;
        let r_name = r
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效文件名"))?;
        // 删除 $R 内容
        if r.is_dir() {
            std::fs::remove_dir_all(&r)?;
        } else if r.exists() {
            std::fs::remove_file(&r)?;
        }
        // 删除配对的 $I 元数据
        let i_name = format!("$I{}", r_name.get(2..).unwrap_or_default());
        let i_path = parent.join(&i_name);
        let _ = std::fs::remove_file(&i_path);
        Ok(())
    }
}
