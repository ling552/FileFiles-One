//! 云存储虚拟文件系统：FTP / WebDAV / SFTP
//! 统一虚拟路径：cloud://<kind>/<name>[/sub/path]
//! 例如：cloud://ftp/MyFTP/docs/report.pdf
//! 列表通过对应协议客户端实时拉取，失败时返回错误提示条目而非崩溃。

use super::metadata::{classify, Entry};
use crate::config::{AppConfig, NetworkLocation};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 是否为云存储虚拟路径
pub fn is_cloud_path(path: &str) -> bool {
    path.starts_with("cloud://")
}

/// 解析 cloud://kind/name[/sub] -> (kind, name, sub_path)
pub fn parse_cloud_path(path: &str) -> Option<(String, String, String)> {
    let rest = path.strip_prefix("cloud://")?;
    let mut parts = rest.splitn(3, '/');
    let kind = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    let sub = parts.next().unwrap_or("").to_string();
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    Some((kind, name, sub))
}

/// 在 This PC 与 network:// 中展示的云存储条目
pub fn list_cloud_roots(config: &AppConfig) -> Vec<Entry> {
    config
        .network_locations
        .iter()
        .filter(|l| matches!(l.kind.as_str(), "ftp" | "webdav" | "sftp"))
        .map(|l| {
            let (icon_class, icon_label) = match l.kind.as_str() {
                "ftp" => ("folder", "FTP"),
                "sftp" => ("folder", "SFTP"),
                "webdav" => ("folder", "DAV"),
                _ => ("folder", "云"),
            };
            Entry {
                name: l.name.clone(),
                path: l.cloud_path(),
                is_dir: true,
                size_bytes: 0,
                modified_ts: 0,
                kind: match l.kind.as_str() {
                    "ftp" => "FTP 位置".into(),
                    "sftp" => "SFTP 位置".into(),
                    "webdav" => "WebDAV 位置".into(),
                    _ => "云存储".into(),
                },
                icon_label: icon_label.into(),
                icon_class: icon_class.into(),
            }
        })
        .collect()
}

/// 解析并列出云存储目录内容
pub fn list_cloud_dir(cloud_path: &str, config: &AppConfig) -> Vec<Entry> {
    let Some((kind, name, sub)) = parse_cloud_path(cloud_path) else {
        return vec![];
    };
    let Some(loc) = config
        .network_locations
        .iter()
        .find(|l| l.kind == kind && l.name == name)
    else {
        return vec![Entry {
            name: "未找到云存储账号".into(),
            path: cloud_path.into(),
            is_dir: false,
            size_bytes: 0,
            modified_ts: 0,
            kind: "错误".into(),
            icon_label: "!".into(),
            icon_class: "default".into(),
        }];
    };
    let res = match kind.as_str() {
        "ftp" => list_ftp(loc, &sub),
        "webdav" => list_webdav(loc, &sub),
        "sftp" => list_sftp(loc, &sub),
        _ => Err("未知云存储类型".into()),
    };
    match res {
        Ok(entries) => entries,
        Err(e) => vec![Entry {
            name: format!("连接失败：{}", e),
            path: cloud_path.into(),
            is_dir: false,
            size_bytes: 0,
            modified_ts: 0,
            kind: "错误 — 请检查网络与凭据".into(),
            icon_label: "!".into(),
            icon_class: "default".into(),
        }],
    }
}

fn effective_port(loc: &NetworkLocation) -> u16 {
    if loc.port != 0 {
        loc.port
    } else {
        match loc.kind.as_str() {
            "ftp" => 21,
            "sftp" => 22,
            "webdav" => {
                if loc.use_tls {
                    443
                } else {
                    80
                }
            }
            _ => 0,
        }
    }
}

fn remote_base(loc: &NetworkLocation) -> String {
    let mut p = loc.remote_path.clone();
    if p.is_empty() {
        p = "/".into();
    }
    if !p.starts_with('/') {
        p = format!("/{}", p);
    }
    p
}

fn join_remote(base: &str, sub: &str) -> String {
    let mut b = base.trim_end_matches('/').to_string();
    if b.is_empty() {
        b = String::new();
    }
    let s = sub.trim_matches('/');
    if s.is_empty() {
        if b.is_empty() {
            "/".into()
        } else {
            b
        }
    } else if b.is_empty() {
        format!("/{}", s)
    } else {
        format!("{}/{}", b, s)
    }
}

/// 下载 WebDAV 文件到应用专属临时目录，返回可交给现有预览/打开器的本地路径。
pub fn download_webdav_file(path: &str, config: &AppConfig) -> Result<PathBuf, String> {
    let (kind, name, sub) = parse_cloud_path(path).ok_or("不是云存储路径")?;
    if kind != "webdav" {
        return Err("当前仅支持 WebDAV 文件下载".into());
    }
    let loc = config
        .network_locations
        .iter()
        .find(|l| l.kind == kind && l.name == name)
        .ok_or("未找到 WebDAV 账号")?;
    let remote = join_remote(&remote_base(loc), &sub);
    let port = effective_port(loc);
    let scheme = if loc.use_tls { "https" } else { "http" };
    let url = if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        format!("{}://{}{}", scheme, loc.host, remote)
    } else {
        format!("{}://{}:{}{}", scheme, loc.host, port, remote)
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build();
    let mut req = agent.get(&url);
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    if resp.status() >= 400 {
        return Err(format!("WebDAV 返回 {}", resp.status()));
    }
    let len = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
    if len > MAX_DOWNLOAD {
        return Err("远程文件超过 256 MB 预览/打开上限".into());
    }
    let mut hash = Sha256::new();
    hash.update(path.as_bytes());
    hash.update(len.to_le_bytes());
    let key = format!("{:x}", hash.finalize());
    let ext = Path::new(&sub).extension().and_then(|e| e.to_str()).unwrap_or("");
    let dir = std::env::temp_dir().join("FileFiles One").join("cloud_preview");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out = dir.join(if ext.is_empty() { key[..24].to_string() } else { format!("{}.{}", &key[..24], ext) });
    if out.is_file() {
        return Ok(out);
    }
    let tmp = out.with_extension("part");
    use std::io::Read;
    // take() 在流式下载过程中强制限额：服务器省略 Content-Length 时
    // 也不会把无界响应体全部落盘后才检查大小
    let mut reader = resp.into_reader().take(MAX_DOWNLOAD + 1);
    let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let copied = std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string());
    if copied.is_err() || copied.unwrap_or(0) > MAX_DOWNLOAD {
        let _ = std::fs::remove_file(&tmp);
        return Err("下载远程文件失败或超过大小上限".into());
    }
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    cleanup_cloud_cache();
    Ok(out)
}

/// 清理七天前的云端预览缓存。
pub fn cleanup_cloud_cache() {
    let dir = std::env::temp_dir().join("FileFiles One").join("cloud_preview");
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let old = entry.metadata().ok().and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d > std::time::Duration::from_secs(7 * 24 * 3600));
        if old { let _ = std::fs::remove_file(entry.path()); }
    }
}


/// FTP 连接（限时）：域名解析 + TCP 连接 10 秒、控制连接读 20 秒。
/// 列表/新建/删除均在调用线程同步执行，不限时会把 UI 卡死在慢服务器上。
fn connect_ftp(loc: &NetworkLocation) -> Result<suppaftp::FtpStream, String> {
    use std::net::ToSocketAddrs;
    use suppaftp::FtpStream;
    let port = effective_port(loc);
    let addr = (loc.host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("主机解析失败：{}", e))?
        .next()
        .ok_or("主机解析失败")?;
    let mut ftp = FtpStream::connect_timeout(addr, std::time::Duration::from_secs(10))
        .map_err(|e| e.to_string())?;
    let _ = ftp
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(20)));
    Ok(ftp)
}

fn list_ftp(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let mut ftp = connect_ftp(loc)?;
    let user = if loc.username.is_empty() { "anonymous".to_string() } else { loc.username.clone() };
    let pass = loc.password.clone();
    ftp.login(&user, &pass).map_err(|e| e.to_string())?;
    let remote = join_remote(&remote_base(loc), sub);
    ftp.cwd(&remote).map_err(|e| e.to_string())?;
    let list = ftp.list(None).map_err(|e| e.to_string())?;
    let _ = ftp.quit();
    let prefix = format!("cloud://{}/{}", loc.kind, loc.name);
    let sub_prefix = if sub.is_empty() { String::new() } else { format!("/{}", sub.trim_matches('/')) };
    let base_path = format!("{}{}", prefix, sub_prefix);
    let mut entries = Vec::new();
    for line in list {
        if let Some((name, is_dir, size, mtime)) = parse_ftp_line(&line) {
            if name == "." || name == ".." {
                continue;
            }
            let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
            // is_dir 直接来自 FTP 解析，非 classify 推断
            let (cls, lbl, kd) = if is_dir {
                ("folder".to_string(), "F".to_string(), "文件夹".to_string())
            } else {
                let (c, l, k) = classify(Path::new(&name), false);
                (c, l, k)
            };
            entries.push(Entry {
                name: name.clone(),
                path,
                is_dir,
                size_bytes: size,
                modified_ts: mtime,
                kind: kd,
                icon_label: lbl,
                icon_class: cls,
            });
        }
    }
    Ok(entries)
}

fn parse_ftp_line(line: &str) -> Option<(String, bool, u64, i64)> {
    // Unix ls -l 格式：drwxr-xr-x 1 user group 4096 Jan 02 15:04 dirname
    // 或 -rw-r--r-- 1 user group 12345 Jan 02 15:04 filename
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let perms = parts[0];
    let is_dir = perms.starts_with('d');
    let size: u64 = parts[4].parse().unwrap_or(0);
    let name = parts[8..].join(" ");
    // 时间解析简化：返回 0，未能解析则用 0
    Some((name, is_dir, size, 0))
}

// ---------------- WebDAV ----------------
fn list_webdav(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let port = effective_port(loc);
    let scheme = if loc.use_tls { "https" } else { "http" };
    let base = remote_base(loc);
    let target = join_remote(&base, sub);
    // 构造 URL：scheme://host:port/target
    let url = if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        format!("{}://{}{}", scheme, loc.host, target)
    } else {
        format!("{}://{}:{}{}", scheme, loc.host, port, target)
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let req = agent.request("PROPFIND", &url).set("Depth", "1");
    let req = if !loc.username.is_empty() {
        // ureq 2 的 basic auth 需手动 header
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req.set("Authorization", &format!("Basic {}", cred))
    } else {
        req
    };
    let resp = req.send_string("").map_err(|e| e.to_string())?;
    if resp.status() >= 400 {
        return Err(format!("WebDAV 返回 {}", resp.status()));
    }
    let body = resp.into_string().map_err(|e| e.to_string())?;
    parse_webdav_propfind(&body, &url, loc, sub)
}

/// 取 XML 限定名的本地部分并小写：服务端前缀各异（D: / d: / oc: / 无前缀），
/// Nextcloud 等使用小写 d:，仅匹配 "D:" 会把整个目录列表静默解析为空
fn xml_local_lower(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    let local = s.rsplit(':').next().unwrap_or(&s);
    local.to_ascii_lowercase()
}

fn parse_webdav_propfind(xml: &str, base_url: &str, loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut cur_href = String::new();
    let mut cur_is_dir = false;
    let mut cur_size: u64 = 0;
    let mut in_href = false;
    let mut in_getcontentlength = false;
    let mut in_resourcetype_collection = false;
    let mut buf = Vec::new();
    let prefix = format!("cloud://{}/{}", loc.kind, loc.name);
    let sub_prefix = if sub.is_empty() { String::new() } else { format!("/{}", sub.trim_matches('/')) };
    let base_path = format!("{}{}", prefix, sub_prefix);
    let base_href_norm = base_url.to_string();
    // 服务端返回的 href 常为纯路径（/dav/sub/），与请求 URL 的路径部分比对才能正确跳过自身
    let base_url_path = base_url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("/");
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match xml_local_lower(e.name().as_ref()).as_str() {
                "response" => {
                    cur_href.clear();
                    cur_is_dir = false;
                    cur_size = 0;
                }
                "href" => in_href = true,
                "getcontentlength" => in_getcontentlength = true,
                "collection" => in_resourcetype_collection = true,
                _ => {}
            },
            Ok(Event::End(e)) => match xml_local_lower(e.name().as_ref()).as_str() {
                "response" => {
                    // 跳过自身目录
                    let href = cur_href.clone();
                    let decoded = urlencoding_decode(&href);
                    let trimmed = decoded.trim_end_matches('/');
                    let is_self = trimmed == base_url.trim_end_matches('/')
                        || trimmed == base_href_norm.trim_end_matches('/')
                        || trimmed == base_url_path.trim_end_matches('/');
                    if !is_self && !href.is_empty() {
                        let name = href
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or(&href)
                            .to_string();
                        let name = urlencoding_decode(&name);
                        if !name.is_empty() {
                            let is_dir = in_resourcetype_collection || cur_is_dir || href.ends_with('/');
                            let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
                            let (cls, lbl, kd) = if is_dir {
                                ("folder".into(), "F".into(), "文件夹".into())
                            } else {
                                let (c, l, k) = classify(Path::new(&name), false);
                                (c, l, k)
                            };
                            entries.push(Entry {
                                name: name.clone(),
                                path,
                                is_dir,
                                size_bytes: cur_size,
                                modified_ts: 0,
                                kind: kd,
                                icon_label: lbl,
                                icon_class: cls,
                            });
                        }
                    }
                    in_resourcetype_collection = false;
                }
                "href" => in_href = false,
                "getcontentlength" => in_getcontentlength = false,
                "collection" => {}
                _ => {}
            },            Ok(Event::Text(e)) => {
                let t = e.unescape().unwrap_or_default().to_string();
                if in_href {
                    cur_href = t;
                } else if in_getcontentlength {
                    cur_size = t.parse().unwrap_or(0);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

fn urlencoding_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                if let (Some(hv), Some(lv)) = (h.to_digit(16), l.to_digit(16)) {
                    out.push((hv * 16 + lv) as u8);
                    continue;
                }
                out.extend(format!("%{}{}", h, l).as_bytes());
                continue;
            }
            out.push(b'%');
            if let Some(h) = hi {
                let mut buf = [0; 4];
                out.extend(h.encode_utf8(&mut buf).as_bytes());
            }
            continue;
        }
        let mut buf = [0; 4];
        out.extend(c.encode_utf8(&mut buf).as_bytes());
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------- SFTP ----------------
// 说明：为避免引入 libssh2/openssl 系统依赖（需 perl 编译），本版本 SFTP 以轻量 stub 呈现。
// 账号可正常添加、展示于此电脑/侧栏/网络位置，列表阶段返回友好提示而非崩溃。
// 如需真实 SFTP 传输，可后续替换为 ssh2 / russh 实现（接口保持一致）。
fn list_sftp(loc: &NetworkLocation, _sub: &str) -> Result<Vec<Entry>, String> {
    Err(format!(
        "SFTP 账号“{}”已保存（{}:{}），真实 SFTP 传输需在后续版本接入 libssh2；当前为演示占位，可正常管理账号与路径 \"{}\"",
        loc.name, loc.host, effective_port(loc), loc.remote_path
    ))
}

/// 根据虚拟路径返回上级虚拟路径（用于“上一级”导航）
pub fn parent_cloud_path(path: &str) -> Option<String> {
    let (kind, name, sub) = parse_cloud_path(path)?;
    if sub.is_empty() {
        return None;
    }
    let trimmed = sub.trim_end_matches('/');
    if let Some(pos) = trimmed.rfind('/') {
        Some(format!("cloud://{}/{}/{}", kind, name, &trimmed[..pos]))
    } else {
        Some(format!("cloud://{}/{}", kind, name))
    }
}

/// 在云存储中创建文件夹，返回新虚拟路径
pub fn create_dir(cloud_parent: &str, name: &str, config: &AppConfig) -> Result<String, String> {
    let (kind, acc_name, sub) = parse_cloud_path(cloud_parent).ok_or("不是云存储路径")?;
    let loc = config
        .network_locations
        .iter()
        .find(|l| l.kind == kind && l.name == acc_name)
        .ok_or("未找到云存储账号")?;
    match kind.as_str() {
        "ftp" => ftp_mkdir(loc, &sub, name),
        "webdav" => webdav_mkdir(loc, &sub, name),
        "sftp" => sftp_mkdir(loc, &sub, name),
        _ => Err("未知云存储类型".into()),
    }?;
    Ok(format!("{}/{}", cloud_parent.trim_end_matches('/'), name))
}

fn ftp_mkdir(loc: &NetworkLocation, sub: &str, name: &str) -> Result<(), String> {
    let mut ftp = connect_ftp(loc)?;
    let user = if loc.username.is_empty() { "anonymous".into() } else { loc.username.clone() };
    ftp.login(&user, &loc.password).map_err(|e| e.to_string())?;
    let remote = join_remote(&join_remote(&remote_base(loc), sub), name);
    ftp.mkdir(&remote).map_err(|e| e.to_string())?;
    let _ = ftp.quit();
    Ok(())
}
fn webdav_mkdir(loc: &NetworkLocation, sub: &str, name: &str) -> Result<(), String> {
    let port = effective_port(loc);
    let scheme = if loc.use_tls { "https" } else { "http" };
    let base = remote_base(loc);
    let target = join_remote(&join_remote(&base, sub), name);
    let url = if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        format!("{}://{}{}", scheme, loc.host, target)
    } else {
        format!("{}://{}:{}{}", scheme, loc.host, port, target)
    };
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let req = agent.request("MKCOL", &url);
    let req = if !loc.username.is_empty() {
        let cred = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:{}", loc.username, loc.password));
        req.set("Authorization", &format!("Basic {}", cred))
    } else { req };
    let resp = req.call().map_err(|e| e.to_string())?;
    if resp.status() >= 400 { return Err(format!("MKCOL 返回 {}", resp.status())); }
    Ok(())
}
fn sftp_mkdir(_loc: &NetworkLocation, _sub: &str, _name: &str) -> Result<(), String> {
    Err("SFTP 创建文件夹为演示占位，后续接入 libssh2 后可用".into())
}

/// 删除云存储文件或文件夹（文件直删，文件夹递归）
pub fn delete_cloud(path: &str, config: &AppConfig) -> Result<(), String> {
    let (kind, name, sub) = parse_cloud_path(path).ok_or("不是云存储路径")?;
    // 账号根目录（sub 为空）对应整个远程基础路径，删除会清空远端全部内容；
    // 移除账号请走设置页，此处必须拒绝
    if sub.trim_matches('/').is_empty() {
        return Err("云存储账号根目录不可删除；如需移除账号请到 设置 → 云存储账号".into());
    }
    let loc = config.network_locations.iter().find(|l| l.kind == kind && l.name == name).ok_or("未找到账号")?;
    // sub 为待删对象相对路径
    match kind.as_str() {
        "ftp" => ftp_delete(loc, &sub),
        "webdav" => webdav_delete(loc, &sub),
        "sftp" => sftp_delete(loc, &sub),
        _ => Err("未知类型".into()),
    }
}
fn ftp_delete(loc: &NetworkLocation, sub: &str) -> Result<(), String> {
    let mut ftp = connect_ftp(loc)?;
    let user = if loc.username.is_empty() { "anonymous".into() } else { loc.username.clone() };
    ftp.login(&user, &loc.password).map_err(|e| e.to_string())?;
    let remote = join_remote(&remote_base(loc), sub);
    // 先尝试删文件，失败再删目录
    if ftp.rm(&remote).is_err() {
        ftp.rmdir(&remote).map_err(|e| e.to_string())?;
    }
    let _ = ftp.quit();
    Ok(())
}
fn webdav_delete(loc: &NetworkLocation, sub: &str) -> Result<(), String> {
    let port = effective_port(loc);
    let scheme = if loc.use_tls { "https" } else { "http" };
    let target = join_remote(&remote_base(loc), sub);
    let url = if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        format!("{}://{}{}", scheme, loc.host, target)
    } else {
        format!("{}://{}:{}{}", scheme, loc.host, port, target)
    };
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let req = agent.request("DELETE", &url);
    let req = if !loc.username.is_empty() {
        let cred = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:{}", loc.username, loc.password));
        req.set("Authorization", &format!("Basic {}", cred))
    } else { req };
    let resp = req.call().map_err(|e| e.to_string())?;
    if resp.status() >= 400 { return Err(format!("DELETE 返回 {}", resp.status())); }
    Ok(())
}
fn sftp_delete(_loc: &NetworkLocation, _sub: &str) -> Result<(), String> {
    Err("SFTP 删除为演示占位，后续接入 libssh2 后可用".into())
}

#[cfg(test)]
mod tests {
    use super::urlencoding_decode;

    #[test]
    fn decodes_utf8_percent_sequences() {
        assert_eq!(urlencoding_decode("%E4%B8%AD%E6%96%87%20A.txt"), "中文 A.txt");
    }

    #[test]
    fn keeps_invalid_percent_sequences() {
        assert_eq!(urlencoding_decode("bad%ZZ.txt"), "bad%ZZ.txt");
        assert_eq!(urlencoding_decode("tail%"), "tail%");
    }
}
