//! 网络位置枚举：列出已映射的网络驱动器（DRIVE_REMOTE）。
//! 点击进入即走普通真实路径浏览（X:\）。

use super::metadata::Entry;

/// 列出已映射的网络驱动器。非 Windows 或无网络盘时返回空列表。
pub fn list_network_drives() -> Vec<Entry> {
    #[cfg(windows)]
    {
        windows_impl::list()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::Entry;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

    const DRIVE_REMOTE: u32 = 4;

    pub fn list() -> Vec<Entry> {
        let mut entries = Vec::new();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{}:\\", letter);
            // 宽字符串 root
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let dtype = unsafe { GetDriveTypeW(wide.as_ptr()) };
            if dtype != DRIVE_REMOTE {
                continue;
            }
            entries.push(Entry {
                name: format!("网络驱动器 ({}:)", letter),
                path: root.clone(),
                is_dir: true,
                size_bytes: 0,
                modified_ts: 0,
                kind: "网络位置".into(),
                icon_label: "网".into(),
                icon_class: "folder".into(),
            });
        }
        entries
    }

    /// 挂载 SMB 共享到自动选取的空闲盘符。返回盘符（如 "Z:"），失败 None。
    pub fn mount_smb(server: &str, user: &str, pass: &str) -> Option<String> {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::NetworkManagement::WNet::{
            WNetAddConnection2W, CONNECT_UPDATE_PROFILE, NETRESOURCEW, RESOURCETYPE_DISK,
        };

        let drive = find_free_drive()?;
        let local: Vec<u16> = format!("{}:", drive)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let remote: Vec<u16> = server.encode_utf16().chain(std::iter::once(0)).collect();
        let user_w: Vec<u16> = if user.is_empty() {
            vec![0]
        } else {
            user.encode_utf16().chain(std::iter::once(0)).collect()
        };
        let pass_w: Vec<u16> = if pass.is_empty() {
            vec![0]
        } else {
            pass.encode_utf16().chain(std::iter::once(0)).collect()
        };

        let mut nr = NETRESOURCEW::default();
        nr.dwType = RESOURCETYPE_DISK;
        nr.lpLocalName = PWSTR(local.as_ptr() as *mut u16);
        nr.lpRemoteName = PWSTR(remote.as_ptr() as *mut u16);

        let r = unsafe {
            WNetAddConnection2W(
                &nr,
                PCWSTR(pass_w.as_ptr()),
                PCWSTR(user_w.as_ptr()),
                CONNECT_UPDATE_PROFILE,
            )
        };
        if r.is_ok() {
            Some(format!("{}:", drive))
        } else {
            None
        }
    }

    /// 卸载盘符的网络连接。
    pub fn unmount_smb(drive: &str) -> bool {
        use windows::core::PCWSTR;
        use windows::Win32::NetworkManagement::WNet::{
            WNetCancelConnection2W, CONNECT_UPDATE_PROFILE,
        };
        let letter = drive.trim_end_matches(':');
        let local: Vec<u16> = format!("{}:", letter)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            WNetCancelConnection2W(PCWSTR(local.as_ptr()), CONNECT_UPDATE_PROFILE, true).is_ok()
        }
    }

    /// 找一个未使用的盘符（A-Z）。
    fn find_free_drive() -> Option<char> {
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                return Some((b'A' + i as u8) as char);
            }
        }
        None
    }
}

/// 挂载 SMB 共享到自动选取的空闲盘符。非 Windows 返回 None。
pub fn mount_smb(server: &str, user: &str, pass: &str) -> Option<String> {
    #[cfg(windows)]
    {
        windows_impl::mount_smb(server, user, pass)
    }
    #[cfg(not(windows))]
    {
        let _ = (server, user, pass);
        None
    }
}

/// 卸载盘符的网络连接。非 Windows 返回 false。
pub fn unmount_smb(drive: &str) -> bool {
    #[cfg(windows)]
    {
        windows_impl::unmount_smb(drive)
    }
    #[cfg(not(windows))]
    {
        let _ = drive;
        false
    }
}

/// 列出用户保存的网络位置连接（网络视图用）。SMB 已挂载的 path 为盘符，否则为 server；
/// 云存储（FTP/WebDAV/SFTP）为 cloud:// 虚拟路径。
pub fn list_saved(locations: &[crate::config::NetworkLocation]) -> Vec<Entry> {
    locations
        .iter()
        .map(|l| {
            let (path, kind, label) = match l.kind.as_str() {
                "ftp" | "webdav" | "sftp" => (
                    l.cloud_path(),
                    match l.kind.as_str() {
                        "ftp" => "FTP 位置",
                        "webdav" => "WebDAV 位置",
                        "sftp" => "SFTP 位置",
                        _ => "云存储",
                    }
                    .to_string(),
                    match l.kind.as_str() {
                        "ftp" => "FTP",
                        "webdav" => "DAV",
                        "sftp" => "SFTP",
                        _ => "云",
                    }
                    .to_string(),
                ),
                _ => (
                    l.drive.clone().unwrap_or_else(|| l.server.clone()),
                    "网络位置".to_string(),
                    "网".to_string(),
                ),
            };
            Entry {
                name: l.name.clone(),
                path,
                is_dir: true,
                size_bytes: 0,
                modified_ts: 0,
                kind,
                icon_label: label,
                icon_class: "folder".into(),
            }
        })
        .collect()
}
