//! 「打开方式」：查询文件默认关联程序名称，并弹出系统「打开方式」对话框。
//!
//! - 默认程序名：用 `AssocQueryStringW` 取扩展名关联的友好程序名（FriendlyAppName），
//!   失败时退回可执行文件名；无关联时返回"未知应用"。
//! - 更改关联：调用 `SHOpenWithDialog` 弹出与资源管理器一致的「打开方式」对话框，
//!   用户在其中选择并（可选）设为默认。

use std::path::Path;

/// 取文件的默认打开程序友好名称（用于属性页展示）。
/// 文件夹或无扩展名返回 None。
#[cfg(windows)]
pub fn default_app_name(path: &Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_EXECUTABLE, ASSOCSTR_FRIENDLYAPPNAME,
    };
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let ext = path.extension().and_then(|e| e.to_str())?;
    if ext.is_empty() {
        return None;
    }

    let query = |association: &str| -> Option<String> {
        let wide: Vec<u16> = association
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 先取友好名（如「照片」「Visual Studio Code」），失败再退回可执行文件路径
        for assoc in [ASSOCSTR_FRIENDLYAPPNAME, ASSOCSTR_EXECUTABLE] {
            unsafe {
                let mut len: u32 = 0;
                let _ = AssocQueryStringW(
                    ASSOCF_NONE,
                    assoc,
                    PCWSTR(wide.as_ptr()),
                    PCWSTR::null(),
                    None,
                    &mut len,
                );
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize];
                if AssocQueryStringW(
                    ASSOCF_NONE,
                    assoc,
                    PCWSTR(wide.as_ptr()),
                    PCWSTR::null(),
                    Some(windows::core::PWSTR(buf.as_mut_ptr())),
                    &mut len,
                )
                .is_ok()
                {
                    let s = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        return Some(if assoc == ASSOCSTR_EXECUTABLE {
                            Path::new(&s)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or(s)
                        } else {
                            s
                        });
                    }
                }
            }
        }
        None
    };

    let dotted = format!(".{}", ext);
    let user_choice: Option<String> = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{}\UserChoice",
            dotted
        ))
        .ok()
        .and_then(|key| key.get_value("ProgId").ok());

    if let Some(prog_id) = user_choice {
        if let Some(name) = query(&prog_id) {
            return Some(name);
        }
        if let Some(exe) = prog_id.strip_prefix("Applications\\") {
            return Some(exe.to_string());
        }
    }
    query(&dotted)
}

#[cfg(not(windows))]
pub fn default_app_name(_path: &Path) -> Option<String> {
    None
}

/// 弹出系统「打开方式」对话框（SHOpenWithDialog）。
/// `hwnd_isize` 为宿主窗口句柄。返回是否成功弹出。
#[cfg(windows)]
pub fn open_with_dialog(path: &str, hwnd_isize: isize) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        SHChangeNotify, SHOpenWithDialog, OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO,
        SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
    };

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let info = OPENASINFO {
            pcszFile: PCWSTR(wide.as_ptr()),
            pcszClass: PCWSTR::null(),
            // 允许把所选程序注册为该类型的可选项，并在选择后立即执行打开
            oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
        };
        let hwnd = HWND(hwnd_isize as *mut core::ffi::c_void);
        let ok = SHOpenWithDialog(Some(hwnd), &info).is_ok();
        // 关联已变更：广播 SHCNE_ASSOCCHANGED 通知 Shell 刷新图标缓存，
        // 否则随后立即重提取仍可能取到旧的关联图标（需重开文件夹才更新的根因）。
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        ok
    }
}

#[cfg(not(windows))]
pub fn open_with_dialog(_path: &str, _hwnd_isize: isize) -> bool {
    false
}

/// 读取文件所有者「域\用户」名称（属性「安全」页用）。
/// 通过 GetNamedSecurityInfoW 取 SID，再 LookupAccountSidW 解析为账户名。
/// 失败返回 None（UI 显示「—」）。
#[cfg(windows)]
pub fn file_owner(path: &Path) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
    };

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut owner_sid = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let rc = GetNamedSecurityInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner_sid),
            None,
            None,
            None,
            &mut sd,
        );
        if rc.is_err() || owner_sid.is_invalid() {
            return None;
        }

        // 第一次调用取所需缓冲长度
        let mut name_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut sid_use = SID_NAME_USE::default();
        let _ = LookupAccountSidW(
            windows::core::PCWSTR::null(),
            owner_sid,
            None,
            &mut name_len,
            None,
            &mut domain_len,
            &mut sid_use,
        );
        if name_len == 0 {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return None;
        }
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        let ok = LookupAccountSidW(
            windows::core::PCWSTR::null(),
            owner_sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut sid_use,
        )
        .is_ok();

        let _ = LocalFree(Some(HLOCAL(sd.0)));

        if !ok {
            return None;
        }
        let name_s = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain_s = String::from_utf16_lossy(&domain[..domain_len as usize]);
        if domain_s.is_empty() {
            Some(name_s)
        } else {
            Some(format!("{}\\{}", domain_s, name_s))
        }
    }
}

#[cfg(not(windows))]
pub fn file_owner(_path: &Path) -> Option<String> {
    None
}

/// 读取文件 DACL（自由访问控制列表），返回 (受托人, 类型, 权限) 三元组列表。
/// 用于属性「安全」页展示访问控制项。非 Windows 平台返回空。
#[cfg(windows)]
pub fn acl_entries(path: &Path) -> Vec<(String, String, String)> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        AclSizeInformation, GetAce, GetAclInformation, ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut out: Vec<(String, String, String)> = Vec::new();
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let rc = GetNamedSecurityInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl as *mut *mut ACL),
            None,
            &mut sd as *mut PSECURITY_DESCRIPTOR,
        );
        if rc.is_err() || dacl.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return out;
        }

        // 取 ACE 数量
        let mut info = ACL_SIZE_INFORMATION::default();
        let ok = GetAclInformation(
            dacl as *const ACL,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
        .is_ok();
        if !ok {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return out;
        }

        for i in 0..info.AceCount {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            if GetAce(
                dacl as *const ACL,
                i,
                &mut ace as *mut *mut std::ffi::c_void,
            )
            .is_err()
            {
                continue;
            }
            let base = ace as *const u8;
            // ACE_HEADER { AceType: u8, AceFlags: u8, AceSize: u16 }（4 字节）
            let ace_type = *base;
            // ACCESS_*_ACE { header(4), Mask: u32, SidStart: u32... }
            let mask = *(base.add(4) as *const u32);
            let sid = PSID(base.add(8) as *mut std::ffi::c_void);
            let trustee = account_name(sid);
            // AceType: 0=ACCESS_ALLOWED（允许）, 1=ACCESS_DENIED（拒绝）
            let kind = if ace_type == 1 { "拒绝" } else { "允许" };
            let access = mask_to_text(mask);
            out.push((trustee, kind.to_string(), access));
        }
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    out
}

/// 把 SID 解析为「域\用户」账户名。失败返回「未知账户」。
#[cfg(windows)]
unsafe fn account_name(sid: windows::Win32::Security::PSID) -> String {
    use windows::core::PWSTR;
    use windows::Win32::Security::{LookupAccountSidW, SID_NAME_USE};

    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_use = SID_NAME_USE::default();
    let _ = LookupAccountSidW(
        windows::core::PCWSTR::null(),
        sid,
        None,
        &mut name_len,
        None,
        &mut domain_len,
        &mut sid_use,
    );
    if name_len == 0 {
        return "未知账户".to_string();
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    let ok = LookupAccountSidW(
        windows::core::PCWSTR::null(),
        sid,
        Some(PWSTR(name.as_mut_ptr())),
        &mut name_len,
        Some(PWSTR(domain.as_mut_ptr())),
        &mut domain_len,
        &mut sid_use,
    )
    .is_ok();
    if !ok {
        return "未知账户".to_string();
    }
    let n = String::from_utf16_lossy(&name[..name_len as usize]);
    let d = String::from_utf16_lossy(&domain[..domain_len as usize]);
    if d.is_empty() {
        n
    } else {
        format!("{}\\{}", d, n)
    }
}

/// 把 NTFS 访问掩码解析为可读权限文本。
#[cfg(windows)]
fn mask_to_text(mask: u32) -> String {
    const FULL: u32 = 0x1F01FF;
    const MODIFY: u32 = 0x0301FF;
    const READ_EXECUTE: u32 = 0x0200A9;
    const READ: u32 = 0x020089;
    const WRITE: u32 = 0x020116;
    const GENERIC_ALL: u32 = 0x10000000;
    const GENERIC_READ: u32 = 0x80000000;
    const GENERIC_WRITE: u32 = 0x40000000;
    const GENERIC_EXECUTE: u32 = 0x20000000;
    match mask {
        m if m == FULL => "完全控制".to_string(),
        m if m == MODIFY => "修改".to_string(),
        m if m == READ_EXECUTE => "读取和执行".to_string(),
        m if m == READ => "读取".to_string(),
        m if m == WRITE => "写入".to_string(),
        _ => {
            if mask & GENERIC_ALL != 0 {
                return "完全控制".to_string();
            }
            let mut parts: Vec<&str> = Vec::new();
            if mask & GENERIC_READ != 0 {
                parts.push("读取");
            }
            if mask & GENERIC_WRITE != 0 {
                parts.push("写入");
            }
            if mask & GENERIC_EXECUTE != 0 {
                parts.push("执行");
            }
            if parts.is_empty() {
                "特殊".to_string()
            } else {
                parts.join("、")
            }
        }
    }
}

#[cfg(not(windows))]
pub fn acl_entries(_path: &Path) -> Vec<(String, String, String)> {
    Vec::new()
}
