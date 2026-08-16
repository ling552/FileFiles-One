//! 系统剪贴板交互：文本写入 + 文件复制/剪切（CF_HDROP）。
//!
//! 文件操作走 Windows 系统剪贴板（CF_HDROP + 「Preferred DropEffect」标记复制/剪切），
//! 这样跨文件夹、跨盘、跨标签页以及与资源管理器之间都能正确复制/剪切/粘贴，
//! 不再依赖应用内部易失的剪贴板状态。

#[cfg(windows)]
const CF_HDROP: u32 = 15;
// DROPEFFECT 常量（避免引入 Win32_System_Ole feature）
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;

/// 将文本写入系统剪贴板，成功返回 true
#[cfg(windows)]
pub fn set_text(text: &str) -> bool {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    // CF_UNICODETEXT 标准剪贴板格式编号
    const CF_UNICODETEXT: u32 = 13;

    // 以 NUL 结尾的 UTF-16
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();

        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if hmem.is_null() {
            CloseClipboard();
            return false;
        }
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            GlobalFree(hmem);
            CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        GlobalUnlock(hmem);

        // 所有权移交给系统；成功后不可再释放 hmem，失败须释放防泄漏
        let ok = !SetClipboardData(CF_UNICODETEXT, hmem as *mut c_void).is_null();
        if !ok {
            GlobalFree(hmem);
        }
        CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
pub fn set_text(_text: &str) -> bool {
    false
}

/// 注册并缓存「Preferred DropEffect」剪贴板格式（标记复制 vs 剪切）。
#[cfg(windows)]
fn drop_effect_format() -> u32 {
    use std::sync::OnceLock;
    static FMT: OnceLock<u32> = OnceLock::new();
    *FMT.get_or_init(|| {
        use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
        let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
        // 注册失败返回 0，调用方据此跳过标记读写
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    })
}

/// 把文件路径写入系统剪贴板（CF_HDROP）。`cut` 为真标记为剪切（移动）语义。
/// 成功返回 true。
#[cfg(windows)]
pub fn set_files(paths: &[std::path::PathBuf], cut: bool) -> bool {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    // 构造 DROPFILES(20B) + 宽字符路径列表（每项以 \0 结尾，整体以 \0\0 结尾）
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&20u32.to_ne_bytes()); // pFiles：文件列表偏移
    buf.extend_from_slice(&0i32.to_ne_bytes()); // pt.x
    buf.extend_from_slice(&0i32.to_ne_bytes()); // pt.y
    buf.extend_from_slice(&0i32.to_ne_bytes()); // fNC
    buf.extend_from_slice(&1i32.to_ne_bytes()); // fWide = TRUE（Unicode）
    for p in paths {
        let w: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2) };
        buf.extend_from_slice(bytes);
    }
    buf.extend_from_slice(&[0u8, 0u8]); // 列表结尾额外 \0

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();

        // CF_HDROP 数据块
        let hmem = GlobalAlloc(GMEM_MOVEABLE, buf.len());
        if hmem.is_null() {
            CloseClipboard();
            return false;
        }
        let dst = GlobalLock(hmem) as *mut u8;
        if dst.is_null() {
            GlobalFree(hmem);
            CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        GlobalUnlock(hmem);
        let ok = !SetClipboardData(CF_HDROP, hmem as *mut c_void).is_null();
        if !ok {
            GlobalFree(hmem);
        }

        // Preferred DropEffect：DWORD 标记复制/剪切（所有权仅在写入成功时移交）
        let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
        let h2 = GlobalAlloc(GMEM_MOVEABLE, 4);
        if !h2.is_null() {
            let mut owned = true;
            let d2 = GlobalLock(h2) as *mut u32;
            if !d2.is_null() {
                *d2 = effect;
                GlobalUnlock(h2);
                let fmt = drop_effect_format();
                if fmt != 0 && !SetClipboardData(fmt, h2 as *mut c_void).is_null() {
                    owned = false;
                }
            }
            if owned {
                GlobalFree(h2);
            }
        }
        CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[std::path::PathBuf], _cut: bool) -> bool {
    false
}

/// 从系统剪贴板读取文件（CF_HDROP），返回 (路径列表, 是否剪切语义)。无文件返回 None。
#[cfg(windows)]
pub fn get_files() -> Option<(Vec<std::path::PathBuf>, bool)> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let hdrop = GetClipboardData(CF_HDROP);
        if hdrop.is_null() {
            CloseClipboard();
            return None;
        }
        // CF_HDROP 的剪贴板句柄可直接作为 HDROP 传给 DragQueryFileW
        let count = DragQueryFileW(hdrop as HDROP, 0xFFFFFFFF, std::ptr::null_mut(), 0);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop as HDROP, i, std::ptr::null_mut(), 0);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; (len + 1) as usize];
            DragQueryFileW(hdrop as HDROP, i, buf.as_mut_ptr(), (len + 1) as u32);
            // 去掉末尾 NUL
            if buf.last() == Some(&0) {
                buf.pop();
            }
            if let Ok(s) = String::from_utf16(&buf) {
                paths.push(std::path::PathBuf::from(s));
            }
        }

        // Preferred DropEffect：判断剪切（含 MOVE 位）。
        // 剪贴板所有者可能是任意进程，数据块大小不可信：
        // 不足 4 字节不按 u32 解引用（防越界读），GlobalSize 在锁定前查询即可
        let mut cut = false;
        let fmt = drop_effect_format();
        if fmt != 0 {
            let h = GetClipboardData(fmt);
            if !h.is_null() && GlobalSize(h) >= std::mem::size_of::<u32>() {
                let p = GlobalLock(h as *mut std::ffi::c_void) as *const u32;
                if !p.is_null() {
                    cut = *p & DROPEFFECT_MOVE != 0;
                    GlobalUnlock(h as *mut std::ffi::c_void);
                }
            }
        }
        CloseClipboard();
        if paths.is_empty() {
            None
        } else {
            Some((paths, cut))
        }
    }
}

#[cfg(not(windows))]
pub fn get_files() -> Option<(Vec<std::path::PathBuf>, bool)> {
    None
}

/// 清空系统剪贴板中的文件数据（剪切粘贴完成后调用，避免重复移动）。
#[cfg(windows)]
pub fn clear_files() -> bool {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard,
    };
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        let ok = EmptyClipboard() != 0;
        CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
pub fn clear_files() -> bool {
    false
}
