//! 数字签名检测（属性对话框「数字签名」页）
//!
//! 通过解析 PE 文件头的「证书表」（IMAGE_DIRECTORY_ENTRY_SECURITY，索引 4）
//! 判断可执行文件是否内嵌 Authenticode 签名。该方式纯字节解析、跨平台可用，
//! 不验证证书链有效性（链校验需 Windows WinVerifyTrust，此处仅判断"是否签名"）。

use std::path::Path;

/// 签名检测结果
pub enum SignStatus {
    /// 非可执行文件，无签名概念
    NotApplicable,
    /// 可执行文件但未找到内嵌签名
    Unsigned,
    /// 含内嵌 Authenticode 签名（携带证书表字节大小）
    Signed { cert_bytes: u32 },
    /// 文件无法读取或 PE 头损坏
    Error(String),
}

/// 可能带 Authenticode 签名的可执行/库扩展名
const PE_EXTS: &[&str] = &[
    "exe", "dll", "sys", "msi", "ocx", "scr", "cpl", "efi", "drv", "mui",
];

fn is_pe_ext(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    PE_EXTS.contains(&ext.as_str())
}

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 检测指定文件的数字签名状态
pub fn detect(path: &Path) -> SignStatus {
    if !is_pe_ext(path) {
        return SignStatus::NotApplicable;
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return SignStatus::Error(format!("无法读取文件：{}", e)),
    };
    // DOS 头：MZ 魔数 + 0x3C 处的 e_lfanew 指向 PE 头
    if data.len() < 0x40 || rd_u16(&data, 0) != Some(0x5A4D) {
        return SignStatus::Error("不是有效的可执行文件".into());
    }
    let pe_off = match rd_u32(&data, 0x3C) {
        Some(o) => o as usize,
        None => return SignStatus::Error("PE 头偏移读取失败".into()),
    };
    // PE 签名 "PE\0\0"
    if rd_u32(&data, pe_off) != Some(0x0000_4550) {
        return SignStatus::Error("缺少 PE 签名".into());
    }
    // COFF 文件头在 PE 签名后 4 字节；可选头紧随其后（偏移 +24）
    let opt_off = pe_off + 24;
    // 可选头魔数：0x10B = PE32，0x20B = PE32+（64 位），决定数据目录起始偏移
    let magic = match rd_u16(&data, opt_off) {
        Some(m) => m,
        None => return SignStatus::Error("可选头读取失败".into()),
    };
    // 数据目录相对可选头起始的偏移：PE32 为 96，PE32+ 为 112
    let dir_base = match magic {
        0x10B => opt_off + 96,
        0x20B => opt_off + 112,
        _ => return SignStatus::Error("未知的 PE 可选头格式".into()),
    };
    // 证书表是第 5 个数据目录（索引 4），每个目录 8 字节（RVA/大小，此处为文件偏移/大小）
    let sec_entry = dir_base + 4 * 8;
    let cert_off = rd_u32(&data, sec_entry);
    let cert_size = rd_u32(&data, sec_entry + 4);
    match (cert_off, cert_size) {
        (Some(off), Some(size)) if off != 0 && size != 0 => SignStatus::Signed { cert_bytes: size },
        (Some(_), Some(_)) => SignStatus::Unsigned,
        _ => SignStatus::Error("证书表读取失败".into()),
    }
}

/// 解析 PE 证书表中的证书链（subject / issuer / 有效期起止）。
/// 用于属性「数字签名」页展示签名者证书链。失败或无签名返回空。
pub fn cert_chain(path: &Path) -> Vec<(String, String, String, String)> {
    if !is_pe_ext(path) {
        return Vec::new();
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let (cert_off, cert_size) = match locate_cert_table(&data) {
        Some(v) => v,
        None => return Vec::new(),
    };
    // 证书表是 WIN_CERTIFICATE 结构数组：dwLength(4) + wRevision(2) + wCertificateType(2) + bCertificate[]
    let table = match cert_off.checked_add(cert_size).and_then(|end| data.get(cert_off..end)) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= table.len() {
        let dw_len =
            u32::from_le_bytes([table[pos], table[pos + 1], table[pos + 2], table[pos + 3]])
                as usize;
        if dw_len < 8 || pos + dw_len > table.len() {
            break;
        }
        let cert_type = u16::from_le_bytes([table[pos + 6], table[pos + 7]]);
        let body = &table[pos + 8..pos + dw_len];
        // wCertificateType == 2 → PKCS#7 SignedData
        if cert_type == 0x0002 {
            out.extend(parse_pkcs7_certs(body));
        }
        pos += dw_len;
    }
    out
}

/// 定位 PE 证书表的（文件偏移, 字节数）。无签名或非 PE 返回 None。
fn locate_cert_table(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 0x40 || rd_u16(data, 0) != Some(0x5A4D) {
        return None;
    }
    let pe_off = rd_u32(data, 0x3C)? as usize;
    if rd_u32(data, pe_off) != Some(0x0000_4550) {
        return None;
    }
    // 各偏移相加均用 checked_add：恶意 PE 的偏移字段接近 u32::MAX 时，
    // 32 位目标上回绕会绕过下面的长度校验导致越界切片
    let opt_off = pe_off.checked_add(24)?;
    let magic = rd_u16(data, opt_off)?;
    let dir_base = match magic {
        0x10B => opt_off.checked_add(96)?,
        0x20B => opt_off.checked_add(112)?,
        _ => return None,
    };
    let sec_entry = dir_base.checked_add(4 * 8)?;
    let off = rd_u32(data, sec_entry)? as usize;
    let size = rd_u32(data, sec_entry.checked_add(4)?)? as usize;
    if off == 0 || size == 0 || off.checked_add(size)? > data.len() {
        return None;
    }
    Some((off, size))
}

/// 从 PKCS#7 SignedData 字节中提取证书链：扫描 DER 证书 SEQUENCE（0x30），
/// 逐个尝试用 x509-parser 解析，成功者取 subject/issuer/有效期。
fn parse_pkcs7_certs(pkcs7: &[u8]) -> Vec<(String, String, String, String)> {
    let mut certs = Vec::new();
    let mut i = 0;
    while i < pkcs7.len() {
        if pkcs7[i] == 0x30 {
            match x509_parser::parse_x509_certificate(&pkcs7[i..]) {
                Ok((rem, cert)) => {
                    let subject = cert.subject().to_string();
                    let issuer = cert.issuer().to_string();
                    let v = cert.validity();
                    let from = format!("{}", v.not_before);
                    let to = format!("{}", v.not_after);
                    certs.push((subject, issuer, from, to));
                    // 跳过已解析的证书字节（consumed = 原长度 - 剩余长度）
                    let consumed = pkcs7[i..].len() - rem.len();
                    i += consumed.max(1);
                    continue;
                }
                Err(_) => {
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    certs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_applicable() {
        assert!(matches!(
            detect(Path::new("readme.txt")),
            SignStatus::NotApplicable
        ));
    }

    #[test]
    fn test_bad_pe() {
        // 扩展名像 PE 但内容不是 → Error
        let mut p = std::env::temp_dir();
        p.push(format!("filefiles_sig_{}.exe", std::process::id()));
        std::fs::write(&p, b"not a real exe").unwrap();
        assert!(matches!(detect(&p), SignStatus::Error(_)));
        std::fs::remove_file(&p).ok();
    }
}
