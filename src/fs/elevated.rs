//! 受保护位置的文件操作按需提权。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const ARG: &str = "--elevated-file-op";

#[derive(Clone, Copy)]
pub enum ElevatedOp {
    CreateDir,
    CreateFile,
    Rename,
    Recycle,
    /// 永久删除（不经回收站）。用于受保护文件：提权后回收仍失败时的兜底，
    /// 以及任何需要直接删除而不进回收站的场景。
    Delete,
}

impl ElevatedOp {
    fn name(self) -> &'static str {
        match self {
            Self::CreateDir => "mkdir",
            Self::CreateFile => "touch",
            Self::Rename => "rename",
            Self::Recycle => "recycle",
            Self::Delete => "delete",
        }
    }

    fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "mkdir" => Some(Self::CreateDir),
            "touch" => Some(Self::CreateFile),
            "rename" => Some(Self::Rename),
            "recycle" => Some(Self::Recycle),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// 判定一个 IO 错误是否「需要管理员权限才能完成」。
/// 受保护文件/目录的典型错误码：
/// - ERROR_ACCESS_DENIED(5)：最常见，无写/删除权限
/// - ERROR_PRIVILEGE_NOT_HELD(1314)：缺少所需特权
/// - ERROR_SHARING_VIOLATION(32)、ERROR_LOCK_VIOLATION(33)：文件被占用，
///   提权通常无效，但部分系统进程占用的文件需 TrustedInstaller 才能处理；
///   为保证「任何情况都能请求提权」，一并纳入（最终能否成功由提权进程决定）
fn is_permission_error(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    matches!(
        error.raw_os_error(),
        // ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION /
        // ERROR_PRIVILEGE_NOT_HELD；DE_ACCESSDENIED(120) 为 SHFileOperation 的拒绝访问码
        Some(5) | Some(32) | Some(33) | Some(120) | Some(1314)
    )
}

/// 若当前进程是提权文件操作子进程，则执行操作并返回 true。
pub fn handle_startup_args() -> bool {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(pos) = args.iter().position(|arg| arg == ARG) else {
        return false;
    };
    let Some(op) = args.get(pos + 1).and_then(|value| ElevatedOp::parse(value)) else {
        return true;
    };
    let values = &args[pos + 2..];
    let result = match op {
        ElevatedOp::CreateDir if values.len() == 2 => values[1]
            .to_str()
            .ok_or_else(invalid_name)
            .and_then(|name| {
                crate::fs::operations::new_folder(Path::new(&values[0]), name).map(|_| ())
            }),
        ElevatedOp::CreateFile if values.len() == 2 => values[1]
            .to_str()
            .ok_or_else(invalid_name)
            .and_then(|name| {
                crate::fs::operations::new_file(Path::new(&values[0]), name).map(|_| ())
            }),
        ElevatedOp::Rename if values.len() == 2 => values[1]
            .to_str()
            .ok_or_else(invalid_name)
            .and_then(|name| {
                crate::fs::operations::rename(Path::new(&values[0]), name).map(|_| ())
            }),
        ElevatedOp::Recycle if !values.is_empty() => {
            let paths: Vec<PathBuf> = values.iter().map(PathBuf::from).collect();
            // 提权后回收仍可能失败（系统文件/被占用文件无法移入回收站）。
            // 此时回退到永久删除，确保「请求管理员权限并删除」始终达成。
            match crate::fs::recyclebin::move_to_recycle_bin(&paths) {
                Ok(()) => Ok(()),
                Err(_) => permanent_delete_all(&paths),
            }
        }
        ElevatedOp::Delete if !values.is_empty() => {
            let paths: Vec<PathBuf> = values.iter().map(PathBuf::from).collect();
            permanent_delete_all(&paths)
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "无效的提权文件操作参数",
        )),
    };
    if let Err(error) = result {
        eprintln!("提权文件操作失败：{error}");
    }
    true
}

fn invalid_name() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "文件名不是有效 Unicode")
}

/// 永久删除一批路径（文件夹递归、文件直接删）。任一失败即返回首个错误。
fn permanent_delete_all(paths: &[PathBuf]) -> std::io::Result<()> {
    for p in paths {
        crate::fs::operations::delete(p)?;
    }
    Ok(())
}

/// 仅在权限被拒绝时请求 UAC；返回是否成功启动提权子进程。
/// 采用放宽后的权限错误判定（见 is_permission_error），覆盖更多「需要管理员」的情形。
pub fn retry_if_permission_denied(
    error: &std::io::Error,
    op: ElevatedOp,
    args: &[OsString],
) -> bool {
    if !is_permission_error(error) {
        return false;
    }
    run_as_admin(op, args)
}

#[cfg(windows)]
fn run_as_admin(op: ElevatedOp, args: &[OsString]) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    // 提权子进程不创建主窗口（handle_startup_args 直接返回），
    // 用 SW_HIDE 避免任何窗口/终端闪现；UAC 系统弹窗本身不受影响。
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn quote(value: &OsStr) -> Vec<u16> {
        let mut result = vec![b'"' as u16];
        let mut slashes = 0usize;
        for ch in value.encode_wide() {
            if ch == b'\\' as u16 {
                slashes += 1;
            } else if ch == b'"' as u16 {
                result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
                result.push(ch);
                slashes = 0;
            } else {
                result.extend(std::iter::repeat_n(b'\\' as u16, slashes));
                slashes = 0;
                result.push(ch);
            }
        }
        result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        result.push(b'"' as u16);
        result
    }

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut params: Vec<u16> = ARG.encode_utf16().collect();
    for value in std::iter::once(OsString::from(op.name())).chain(args.iter().cloned()) {
        params.push(b' ' as u16);
        params.extend(quote(&value));
    }
    params.push(0);
    let exe_w: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let runas_w: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            runas_w.as_ptr(),
            exe_w.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    };
    result as isize > 32
}

#[cfg(not(windows))]
fn run_as_admin(_op: ElevatedOp, _args: &[OsString]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_permission_errors_are_not_elevated() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert!(!retry_if_permission_denied(
            &error,
            ElevatedOp::CreateFile,
            &[]
        ));
    }

    #[test]
    fn elevated_operations_are_whitelisted() {
        assert!(ElevatedOp::parse(OsStr::new("mkdir")).is_some());
        assert!(ElevatedOp::parse(OsStr::new("powershell")).is_none());
    }
}
