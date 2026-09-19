//! 云存储虚拟文件系统：FTP / WebDAV / SFTP
//! 统一虚拟路径：cloud://<kind>/<name>[/sub/path]
//! 例如：cloud://ftp/MyFTP/docs/report.pdf
//! 列表通过对应协议客户端实时拉取，失败时返回错误提示条目而非崩溃。

use super::metadata::{classify, Entry};
use super::tasks::TaskControl;
use crate::config::{AppConfig, NetworkLocation};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

/// 是否为云存储账户根（cloud://kind/name，无子路径）
pub fn is_cloud_root(path: &str) -> bool {
    match parse_cloud_path(path) {
        Some((_, _, sub)) => sub.trim_matches('/').is_empty(),
        None => false,
    }
}

/// 主机显示标签：默认端口（http:80/https:443）省略端口，避免
/// “dav.pikpak.ai:443”这类误导性提示（实际 URL 已省略默认端口）
fn host_label(host: &str, port: u16, use_tls: bool) -> String {
    let default_port = if use_tls { 443 } else { 80 };
    if port == 0 || port == default_port {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    }
}

/// 挂载图标预设对应的 icon_class（"drive-cloud" 等，见 ui/file_icon.slint）。
/// 非预设（默认/自定义文件）返回 None，沿用通用 "drive" 类：
/// 默认与 D:/H: 等数据盘同系统图标，自定义文件由图标提取链路回填位图。
pub fn mount_icon_class(loc: &NetworkLocation) -> Option<String> {
    match loc.mount_icon_kind() {
        crate::config::MountIconKind::Preset(id) => Some(format!("drive-{}", id)),
        _ => None,
    }
}

/// 查询挂载盘符对应的 WebDAV 账户；命中且图标为预设时返回覆盖用的 icon_class。
/// 供此电脑视图的磁盘条目后处理与侧栏磁盘条目使用（盘符条目 path 形如 "Z:\"）。
pub fn mounted_drive_icon_class(config: &AppConfig, drive_letter: char) -> Option<String> {
    let letter = drive_letter.to_ascii_uppercase();
    config
        .network_locations
        .iter()
        .filter(|l| l.kind == "webdav")
        .find(|l| {
            l.drive
                .as_deref()
                .and_then(|d| d.chars().next())
                .map(|c| c.to_ascii_uppercase() == letter)
                .unwrap_or(false)
        })
        .and_then(mount_icon_class)
}

/// 该 WebDAV 账户当前是否已挂载为虚拟磁盘。
/// 双信号任一命中即算已挂载：内存挂载表（含刚挂载成功但盘符轮询尚有延迟的）
/// 或配置盘符真实存在于系统。仅凭配置盘符 + drive_in_use 会在轮询间隙误判为未挂载，
/// 导致按钮在挂载成功瞬间仍显示“挂载”。
pub fn is_webdav_mounted(loc: &NetworkLocation) -> bool {
    if loc.kind != "webdav" {
        return false;
    }
    // 内存表优先：本进程挂载成功即命中，不依赖系统盘符轮询
    if super::rclone::is_mounted_name(&loc.name).is_some() {
        return true;
    }
    // 盘符信号：记录盘符（上次成功挂载的回写值）或设定盘符（挂载设置的必填项）
    // 任一在系统里真实存在即算已挂载。只看记录盘符会在回写丢失时误判为未挂载，
    // 使同一账户在设置页显示「已挂载」而「此电脑」里却多出一个 WebDAV 位置条目。
    [loc.drive.as_deref(), loc.mount_drive.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|d| d.trim_end_matches(':').chars().next())
        .any(super::rclone::drive_in_use)
}

/// 用真实盘符校正 WebDAV 挂载记录，返回是否发生改动。
///
/// 挂载状态有三个来源：进程内挂载表、配置里的记录盘符、系统真实盘符。崩溃或
/// 回写回调未执行会让配置与实际分叉，表现为设置页显示「已挂载」而「此电脑」
/// 按未挂载渲染出 WebDAV 位置条目。此处以内存表与系统盘符为准回写配置：
/// 盘符真实存在则补记，已消失则清空（保留 mount_drive 设定，便于下次重挂）。
pub fn reconcile_webdav_mount_state(config: &mut AppConfig) -> bool {
    let mut changed = false;
    for loc in config
        .network_locations
        .iter_mut()
        .filter(|l| l.kind == "webdav")
    {
        // 内存表是权威值：本进程挂载成功即写入，实时反映真实盘符。
        // 表外（重启后收养前）退化为探测记录盘符与设定盘符是否真实存在。
        let live = super::rclone::is_mounted_name(&loc.name).or_else(|| {
            [loc.drive.as_deref(), loc.mount_drive.as_deref()]
                .into_iter()
                .flatten()
                .find_map(|d| {
                    let letter = d.trim_end_matches(':').chars().next()?;
                    super::rclone::drive_in_use(letter)
                        .then(|| format!("{}:", letter.to_ascii_uppercase()))
                })
        });
        match live {
            Some(drive) => {
                if loc.drive.as_deref() != Some(drive.as_str()) {
                    loc.drive = Some(drive);
                    changed = true;
                }
            }
            None => {
                if loc.drive.is_some() {
                    loc.drive = None;
                    changed = true;
                }
            }
        }
    }
    changed
}

/// 在 This PC 与 network:// 中展示的云存储条目
/// WebDAV 条目使用 drive 图标类（挂载为虚拟磁盘后与 D:/H: 等数据盘同图标，
/// 未挂载时同样显示数据盘系统图标而非黄色文件夹，见 IconRequest::DataDrive）；
/// 挂载图标为预设时改用 drive-<id> 矢量字形类。
/// 已挂载为虚拟磁盘的 WebDAV 不在此列出（真实盘符 Z:\ 已在磁盘列表中，
/// 与 D:/H: 一样显示容量条与数据盘图标，避免同一账户出现两个入口）。
pub fn list_cloud_roots(config: &AppConfig) -> Vec<Entry> {
    list_cloud_roots_filtered(config, true)
}

/// 云存储根列表（供 network:// 等管理视图使用，保留已挂载项）。
/// `hide_mounted` 为真时过滤已挂载的 WebDAV（This PC 用，避免与真实盘符重复）。
pub fn list_cloud_roots_filtered(config: &AppConfig, hide_mounted: bool) -> Vec<Entry> {
    config
        .network_locations
        .iter()
        .filter(|l| matches!(l.kind.as_str(), "ftp" | "webdav" | "sftp"))
        .filter(|l| !(hide_mounted && is_webdav_mounted(l)))
        .map(|l| {
            let (icon_class, icon_label) = match l.kind.as_str() {
                "ftp" => ("folder".to_string(), "FTP".to_string()),
                "sftp" => ("folder".to_string(), "SFTP".to_string()),
                // WebDAV 挂载为虚拟磁盘：图标与除 C 盘外的其它盘一致（数据盘图标）
                "webdav" => (mount_icon_class(l).unwrap_or_else(|| "drive".into()), "W".to_string()),
                _ => ("folder".to_string(), "云".to_string()),
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
                icon_class,
            }
        })
        .collect()
}

/// 解析并列出云存储目录内容；同步虚拟文件系统调用保持错误条目语义。
pub fn list_cloud_dir(cloud_path: &str, config: &AppConfig) -> Vec<Entry> {
    list_cloud_dir_result(cloud_path, &config.network_locations).unwrap_or_else(|e| {
        vec![Entry {
            name: format!("连接失败：{}", e),
            path: cloud_path.into(),
            is_dir: false,
            size_bytes: 0,
            modified_ts: 0,
            kind: "错误 — 请检查网络与凭据".into(),
            icon_label: "!".into(),
            icon_class: "default".into(),
        }]
    })
}

/// 后台目录加载使用错误返回值，以便状态栏显示失败原因。
pub fn list_cloud_dir_result(
    cloud_path: &str,
    locations: &[NetworkLocation],
) -> Result<Vec<Entry>, String> {
    let (kind, name, sub) = parse_cloud_path(cloud_path).ok_or("不是云存储路径")?;
    let loc = locations
        .iter()
        .find(|l| l.kind == kind && l.name == name)
        .ok_or("未找到云存储账号")?;
    match kind.as_str() {
        "ftp" => list_ftp(loc, &sub),
        "webdav" => list_webdav(loc, &sub),
        "sftp" => list_sftp(loc, &sub),
        _ => Err("未知云存储类型".into()),
    }
}

/// 端口决策（rclone 模块复用，供 SFTP/挂载时保持一致）：
/// UI 端口框优先，其次主机栏显式端口（如 example.com:8080），最后协议默认
pub fn effective_port_pub(loc: &NetworkLocation) -> u16 {
    // 主机栏误填含端口（如 example.com:8080）时优先采用其显式端口，
    // 显式填写的端口字段（UI 端口框）优先级最高
    if loc.port != 0 {
        return loc.port;
    }
    if let Some(p) = explicit_port_in_host(&loc.host) {
        return p;
    }
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

fn remote_base(loc: &NetworkLocation) -> String {
    remote_base_pub(loc)
}

/// 公开版供 rclone 模块复用
pub fn remote_base_pub(loc: &NetworkLocation) -> String {
    // 主机栏误填完整 URL（如 https://host/dav/files）时，把其中的路径部分
    // 并入远程基路径，避免用户把 WebDAV 地址整体粘进“主机”导致 404/连接失败
    let mut extra = host_path_prefix(&loc.host);
    let mut p = loc.remote_path.clone();
    if p.is_empty() {
        p = "/".into();
    }
    if !p.starts_with('/') {
        p = format!("/{}", p);
    }
    if !extra.is_empty() {
        if !extra.starts_with('/') {
            extra = format!("/{}", extra);
        }
        // 去重：remote_path 已包含该前缀时不再拼接。按路径段比较：
        // extra="/dav" 不应匹配 p="/dav2/x" 这类共享字符串前缀的路径
        let extra_trimmed = extra.trim_end_matches('/');
        if p == extra_trimmed || p.starts_with(&format!("{}/", extra_trimmed)) {
            // 已含前缀，保持原样
        } else {
            p = format!("{}{}", extra_trimmed, p);
        }
    }
    p
}

/// 清洗主机输入：剥离 scheme（ftp:// https://）、用户信息（user:pass@）、
/// 路径/查询/片段（/dav/files?x=1），返回纯主机名（IPv6 保留括号）。
/// 如 "https://user:pw@example.com:8443/dav" -> "example.com"
pub fn clean_host(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return s;
    }
    // scheme
    if let Some(pos) = s.find("://") {
        s = s[pos + 3..].to_string();
    }
    // 路径/查询/片段
    for sep in ['/', '?', '#'] {
        if let Some(pos) = s.find(sep) {
            s.truncate(pos);
            break;
        }
    }
    // 用户信息
    if let Some(pos) = s.rfind('@') {
        s = s[pos + 1..].to_string();
    }
    // 端口后缀（IPv6 [::1]:8080 需保留括号内冒号）
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            let after = &s[end + 1..];
            if after.starts_with(':') {
                s.truncate(end + 1);
            }
            return s;
        }
        return s;
    }
    // 普通 host:port -> 去端口（端口由 explicit_port_in_host 另行解析）
    if let Some(pos) = s.rfind(':') {
        let after = &s[pos + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].contains(':') {
            s.truncate(pos);
        }
    }
    s
}

/// 主机栏中显式携带的端口（如 example.com:8080 / [::1]:8080），无则 None
fn explicit_port_in_host(raw: &str) -> Option<u16> {
    let mut s = raw.trim().to_string();
    if let Some(pos) = s.find("://") {
        s = s[pos + 3..].to_string();
    }
    for sep in ['/', '?', '#'] {
        if let Some(pos) = s.find(sep) {
            s.truncate(pos);
            break;
        }
    }
    if let Some(pos) = s.rfind('@') {
        s = s[pos + 1..].to_string();
    }
    // IPv6
    if s.starts_with('[') {
        let end = s.find(']')?;
        let after = &s[end + 1..];
        let port = after.strip_prefix(':')?;
        return port.parse::<u16>().ok();
    }
    let pos = s.rfind(':')?;
    // 避免把 IPv6 裸地址的冒号误作端口（多个冒号则放弃）
    if s.contains(':') && s.matches(':').count() != 1 {
        return None;
    }
    s[pos + 1..].parse::<u16>().ok()
}

/// 主机栏中误填的路径前缀（如粘贴完整 URL 时的 /dav/files），无则空串
fn host_path_prefix(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return String::new();
    }
    if let Some(pos) = s.find("://") {
        s = s[pos + 3..].to_string();
    } else if !s.contains('/') {
        return String::new();
    }
    // 去掉 userinfo/host:port，保留首个 / 之后
    let slash = s.find('/');
    let Some(pos) = slash else { return String::new() };
    let mut path = s[pos..].to_string();
    for sep in ['?', '#'] {
        if let Some(p) = path.find(sep) {
            path.truncate(p);
            break;
        }
    }
    if path.is_empty() || path == "/" {
        return String::new();
    }
    path
}

/// 拆分主机/端口/基路径（三者均经清洗，主机栏误填完整 URL 时仍可连接）。
/// 返回（纯主机，端口，基路径）。-rclone 挂载与原生 PROPFIND 共用。
pub fn split_host_port_base(loc: &NetworkLocation) -> Result<(String, u16, String), String> {
    let host = clean_host(&loc.host);
    if host.is_empty() {
        return Err("主机地址为空".to_string());
    }
    let port = effective_port_pub(loc);
    let base = remote_base_pub(loc);
    Ok((host, port, base))
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
    let url = webdav_url(loc, &sub)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let mut req = webdav_agent()
        .get(&url)
        .timeout(Duration::from_secs(60))
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req.call().map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
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


/// FTP 连接：统一经 FtpConn（明文/显式 FTPS），域名解析 + TCP 10 秒、
/// 控制连接读 20 秒。主机栏误填完整 URL 时自动清洗，FTPS 走 SChannel 系统 TLS。
/// 列表/新建/删除均在调用线程同步执行，不限时会把 UI 卡死在慢服务器上。
enum FtpConn {
    Plain(suppaftp::FtpStream),
    Secure(suppaftp::NativeTlsFtpStream),
}

impl FtpConn {
    fn set_passive(&mut self, passive: bool) {
        // suppaftp 默认即被动模式；仅主动模式需显式切换
        if !passive {
            match self {
                FtpConn::Plain(f) => f.set_mode(suppaftp::types::Mode::Active),
                FtpConn::Secure(f) => f.set_mode(suppaftp::types::Mode::Active),
            }
        }
    }
    fn login(&mut self, user: &str, pass: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.login(user, pass).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.login(user, pass).map_err(|e| map_ftp_err(&e))?,
        }
        Ok(())
    }
    fn cwd(&mut self, path: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.cwd(path).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.cwd(path).map_err(|e| map_ftp_err(&e))?,
        }
        Ok(())
    }
    /// 优先 MLSD（机器可读，无 POSIX/DOS 歧义）。
    /// Ok(Some)：列出成功；Ok(None)：服务器接受但目录为空（回退 LIST 由调用方定）；
    /// Err：命令被拒/中断。失败时 suppaftp 内部 data_connection_open 标志不会复位，
    /// 同一连接上的后续数据命令必然误报「Data connection is already open」，
    /// 调用方收到 Err 后必须重建连接再发其他数据命令
    fn try_mlsd(&mut self, path: &str) -> Result<Option<Vec<String>>, String> {
        let lines = match self {
            FtpConn::Plain(f) => f.mlsd(Some(path)).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.mlsd(Some(path)).map_err(|e| map_ftp_err(&e))?,
        };
        if lines.is_empty() { Ok(None) } else { Ok(Some(lines)) }
    }
    fn list(&mut self) -> Result<Vec<String>, String> {
        match self {
            FtpConn::Plain(f) => f.list(None).map_err(|e| map_ftp_err(&e)),
            FtpConn::Secure(f) => f.list(None).map_err(|e| map_ftp_err(&e)),
        }
    }
    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.mkdir(path).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.mkdir(path).map_err(|e| map_ftp_err(&e))?,
        }
        Ok(())
    }
    fn rm(&mut self, path: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.rm(path).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.rm(path).map_err(|e| map_ftp_err(&e))?,
        }
        Ok(())
    }
    fn rmdir(&mut self, path: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.rmdir(path).map_err(|e| map_ftp_err(&e))?,
            FtpConn::Secure(f) => f.rmdir(path).map_err(|e| map_ftp_err(&e))?,
        }
        Ok(())
    }
    fn quit(&mut self) {
        match self {
            FtpConn::Plain(f) => { let _ = f.quit(); }
            FtpConn::Secure(f) => { let _ = f.quit(); }
        }
    }
}

/// FTP 错误中文映射：把 suppaftp 的英文/数字错误转为可操作提示
fn map_ftp_err(e: &suppaftp::types::FtpError) -> String {
    let s = e.to_string();
    if s.contains("530") || s.to_lowercase().contains("login") || s.to_lowercase().contains("auth") {
        return format!("登录失败（用户名/密码错误）：{}", s);
    }
    if s.contains("550") {
        return format!("路径不存在或无权限（550）：{}", s);
    }
    if s.contains("421") || s.to_lowercase().contains("timeout") || s.to_lowercase().contains("timed out") {
        return format!("连接超时：{}", s);
    }
    s
}

fn connect_ftp(loc: &NetworkLocation) -> Result<FtpConn, String> {
    use std::net::ToSocketAddrs;
    let (host, port, _) = split_host_port_base(loc)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("主机解析失败（{}）：{}", host, e))?
        .next()
        .ok_or_else(|| format!("主机解析失败（{}）：无可用地址", host))?;
    if loc.use_tls {
        // 显式 FTPS：先明文连上再升级 TLS（SChannel，无额外系统依赖）
        let plain = suppaftp::NativeTlsFtpStream::connect_timeout(addr, Duration::from_secs(10))
            .map_err(|e| format!("FTPS 连接失败（{}:{}）：{}", host, port, map_ftp_err(&e)))?;
        let connector = suppaftp::NativeTlsConnector::from(
            suppaftp::native_tls::TlsConnector::new().map_err(|e| format!("TLS 初始化失败：{}", e))?,
        );
        let secure = plain
            .into_secure(connector, &host)
            .map_err(|e| format!("FTPS 握手失败（{}）：{}", host, map_ftp_err(&e)))?;
        let _ = secure
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(20)));
        let mut conn = FtpConn::Secure(secure);
        conn.set_passive(loc.passive);
        Ok(conn)
    } else {
        let ftp = suppaftp::FtpStream::connect_timeout(addr, Duration::from_secs(10))
            .map_err(|e| format!("FTP 连接失败（{}:{}）：{}", host, port, map_ftp_err(&e)))?;
        let _ = ftp
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(20)));
        let mut conn = FtpConn::Plain(ftp);
        conn.set_passive(loc.passive);
        Ok(conn)
    }
}

fn ftp_file_to_entry(
    name: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
    base_path: &str,
) -> Option<Entry> {
    if name == "." || name == ".." || name.is_empty() {
        return None;
    }
    let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
    let (cls, lbl, kd) = if is_dir {
        ("folder".to_string(), "F".to_string(), "文件夹".to_string())
    } else {
        let (c, l, k) = classify(Path::new(&name), false);
        (c, l, k)
    };
    Some(Entry {
        name: name.clone(),
        path,
        is_dir,
        size_bytes: size,
        modified_ts: mtime,
        kind: kd,
        icon_label: lbl,
        icon_class: cls,
    })
}

fn unix_ts_from_system(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn list_ftp(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let mut ftp = connect_ftp(loc)?;
    let user = if loc.username.is_empty() { "anonymous".to_string() } else { loc.username.clone() };
    let pass = loc.password.clone();
    ftp.login(&user, &pass)?;
    let remote = join_remote(&remote_base(loc), sub);
    ftp.cwd(&remote)?;
    let prefix = format!("cloud://{}/{}", loc.kind, loc.name);
    let sub_prefix = if sub.is_empty() { String::new() } else { format!("/{}", sub.trim_matches('/')) };
    let base_path = format!("{}{}", prefix, sub_prefix);
    // 1) 优先 MLSD：机器可读（type=dir/file;size=;modify=），无解析歧义
    match ftp.try_mlsd(&remote) {
        Ok(Some(lines)) => {
            let mut entries = Vec::new();
            for line in lines {
                if let Ok(f) = suppaftp::list::ListParser::parse_mlsd(&line) {
                    let name = f.name().to_string();
                    let is_dir = f.is_directory();
                    let size = f.size() as u64;
                    let mtime = unix_ts_from_system(f.modified());
                    if let Some(e) = ftp_file_to_entry(name, is_dir, size, mtime, &base_path) {
                        entries.push(e);
                    }
                }
            }
            ftp.quit();
            return Ok(entries);
        }
        Ok(None) => {
            // 服务器接受 MLSD 但目录为空：连接状态干净，按空目录返回（不再 LIST）
            ftp.quit();
            return Ok(Vec::new());
        }
        Err(_mlsd_err) => {
            // MLSD 被拒（500/502 不支持）或数据连接异常：suppaftp 内部标志未复位，
            // 原连接上的 LIST 会误报「Data connection is already open」——必须重建连接
            ftp.quit();
            ftp = connect_ftp(loc)?;
            ftp.login(&user, &pass)?;
            ftp.cwd(&remote)?;
        }
    }
    // 2) 回退 LIST：逐行先 POSIX 后 DOS（覆盖 IIS 等 Windows FTP 的 <DIR> 格式）
    let list = ftp.list()?;
    ftp.quit();
    let mut entries = Vec::new();
    for line in list {
        // 跳过 total 行与空行
        let t = line.trim();
        if t.is_empty() || t.starts_with("total ") {
            continue;
        }
        let parsed = suppaftp::list::ListParser::parse_posix(&line)
            .or_else(|_| suppaftp::list::ListParser::parse_dos(&line));
        if let Ok(f) = parsed {
            let name = f.name().to_string();
            let is_dir = f.is_directory();
            let size = f.size() as u64;
            let mtime = unix_ts_from_system(f.modified());
            if let Some(e) = ftp_file_to_entry(name, is_dir, size, mtime, &base_path) {
                entries.push(e);
            }
        }
        // 无法解析的行直接跳过（不阻断整个目录，旧实现直接丢弃 DOS 全目录）
    }
    Ok(entries)
}

#[allow(dead_code)]
fn parse_ftp_line(line: &str) -> Option<(String, bool, u64, i64)> {
    // 兼容旧单测：先 POSIX 后 DOS，时间统一归 0（精确时间走 ListParser 路径）
    if let Ok(f) = suppaftp::list::ListParser::parse_posix(line) {
        return Some((f.name().to_string(), f.is_directory(), f.size() as u64, 0));
    }
    if let Ok(f) = suppaftp::list::ListParser::parse_dos(line) {
        return Some((f.name().to_string(), f.is_directory(), f.size() as u64, 0));
    }
    // 极简回退：Unix ls -l 启发式（供异常行宽容）
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let perms = parts[0];
    let is_dir = perms.starts_with('d');
    let size: u64 = parts[4].parse().unwrap_or(0);
    let name = parts[8..].join(" ");
    Some((name, is_dir, size, 0))
}

// ---------------- WebDAV ----------------

/// 全进程复用连接池；各请求自行设置超时，避免每次进目录重复 TLS 握手。
/// 连接级超时在 agent 上统一兜底：连接 10s、单次 socket 读/写 60s——
/// 大文件 PUT/GET 不设整体超时（会掐断长传输），挂起由读写超时发现。
fn webdav_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(60))
            .build()
    })
}

/// 构造 WebDAV 完整 URL（主机栏误填完整 URL 时自动清洗合并）
fn webdav_url(loc: &NetworkLocation, sub: &str) -> Result<String, String> {
    let (host, port, base) = split_host_port_base(loc)?;
    let scheme = if loc.use_tls { "https" } else { "http" };
    let target = join_remote(&base, sub);
    // 路径段逐段 percent-encode（中文/空格文件名直拼会导致 400/404），
    // 保留 / 分隔符；已含 %XX 的不再二次编码由服务器容错
    let encoded = target
        .split('/')
        .map(|seg| {
            if seg.is_empty() || seg.contains('%') {
                seg.to_string()
            } else {
                percent_encode_segment(seg)
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    let target = if encoded.starts_with('/') { encoded } else { format!("/{}", encoded) };
    if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        Ok(format!("{}://{}{}", scheme, host, target))
    } else {
        Ok(format!("{}://{}:{}{}", scheme, host, port, target))
    }
}

/// 路径段 percent-encode（RFC3986 unreserved 外全部编码，UTF-8 按字节）
fn percent_encode_segment(seg: &str) -> String {
    let mut out = String::new();
    for b in seg.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// ureq 错误中文映射（连接/超时/认证/状态码分开提示，可操作）
fn map_webdav_err(e: ureq::Error, host: &str, port: u16, use_tls: bool) -> String {
    let label = host_label(host, port, use_tls);
    match e {
        ureq::Error::Status(401, _) => "认证失败（401）：请检查用户名与密码（部分服务如 PikPak 需用 WebDAV 专用的账号/密码，而非登录密码）".to_string(),
        ureq::Error::Status(403, _) => "无权限（403）：账号无权访问该路径".to_string(),
        ureq::Error::Status(404, _) => "路径不存在（404）：请检查远程路径是否以 /dav 等正确前缀开头".to_string(),
        ureq::Error::Status(code, _) => format!("WebDAV 返回 {}（{}），请检查地址、端口与是否勾选 HTTPS", code, label),
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            if s.to_lowercase().contains("timed out") || s.to_lowercase().contains("timeout") {
                format!("连接超时（{}，10s）：请检查主机与端口", label)
            } else if s.to_lowercase().contains("dns") || s.to_lowercase().contains("resolve") || s.to_lowercase().contains("failed to lookup") {
                format!("主机解析失败（{}）：请检查主机地址", host)
            } else if s.to_lowercase().contains("connection refused") {
                format!("连接被拒（{}）：端口或服务未开放", label)
            } else {
                format!("连接失败（{}）：{}", label, s)
            }
        }
    }
}

fn webdav_use_tls(loc: &NetworkLocation) -> bool {
    loc.use_tls
}

fn list_webdav(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let mut url = webdav_url(loc, sub)?;
    // PROPFIND 目标为集合时必须以斜杠结尾：部分实现（PikPak/Nginx）
    // 对无斜杠的集合请求返回 400，导致目录打不开
    if !url.ends_with('/') {
        url.push('/');
    }
    // 标准 PROPFIND 体：部分实现（PikPak/群晖/Nginx）对空体返回 400/411，
    // 必须带 XML + Content-Type；Depth:1 取本级 + 直接子项
    const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:displayname/></d:prop></d:propfind>"#;
    let mut req = webdav_agent()
        .request("PROPFIND", &url)
        .timeout(Duration::from_secs(15))
        .set("Depth", "1")
        .set("Content-Type", "application/xml; charset=utf-8")
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        // ureq 2 的 basic auth 需手动 header（用户名含中文/特殊字符时按 UTF-8）
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req.send_string(BODY).map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
    if resp.status() >= 400 {
        return Err(format!("WebDAV 返回 {}（{}），请检查地址与凭据", resp.status(), url));
    }
    let body = resp.into_string().map_err(|e| format!("读取目录失败：{}", e))?;
    parse_webdav_propfind(&body, &url, loc, sub)
}

/// 取 XML 限定名的本地部分并小写：服务端前缀各异（D: / d: / oc: / 无前缀），
/// Nextcloud 等使用小写 d:，仅匹配 "D:" 会把整个目录列表静默解析为空
fn xml_local_lower(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    let local = s.rsplit(':').next().unwrap_or(&s);
    local.to_ascii_lowercase()
}

/// 解析 HTTP 日期（WebDAV getlastmodified，如 "Wed, 12 Sep 2026 08:00:00 GMT"）
/// 返回 Unix 时间戳秒，解析失败返回 0（未知时间，UI 显示为空而非 1970）
fn parse_http_date(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    // 常见格式：RFC2822（IMF-fixdate）与 RFC850 / asctime 变体，统一尝试
    // chrono 的 parse_from_rfc2822 可处理 "Wed, 12 Sep 2026 08:00:00 +0000"，
    // 但 WebDAV 常用 "GMT" 后缀，需先替换为 +0000
    let normalized = s.replace("GMT", "+0000").replace("UTC", "+0000");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(&normalized) {
        return dt.timestamp();
    }
    // 尝试 "%a, %d %b %Y %H:%M:%S %z"（与 rfc2822 等价，容错多空格）
    if let Ok(dt) = chrono::DateTime::parse_from_str(&normalized, "%a, %d %b %Y %H:%M:%S %z") {
        return dt.timestamp();
    }
    // 容错：部分实现星期字段与实际日期不符（如测试手写 Wed 实为 Sat），
    // chrono 会校验星期而失败，此时剥离星期重试
    if let Some(comma) = normalized.find(", ") {
        let without_weekday = normalized[comma + 2..].trim();
        if let Ok(dt) =
            chrono::DateTime::parse_from_str(without_weekday, "%d %b %Y %H:%M:%S %z")
        {
            return dt.timestamp();
        }
    }
    // ISO8601 兜底（部分国产实现返回 ISO 时间）
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp();
    }
    0
}

fn parse_webdav_propfind(xml: &str, base_url: &str, loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut cur_href = String::new();
    let mut cur_displayname = String::new();
    let mut cur_lastmodified = String::new();
    let mut cur_size: u64 = 0;
    let mut in_href = false;
    let mut in_getcontentlength = false;
    let mut in_displayname = false;
    let mut in_getlastmodified = false;
    // 是否为集合（文件夹）：<collection> 或自闭合 <collection/> 均置真。
    // quick-xml 对自闭合标签产生 Empty 事件而非 Start，必须同时处理，
    // 否则 PikPak/Nginx 等返回 <D:collection/> 的目录会被误判为文件，
    // 导致文件与文件夹分不清、双击文件夹误走文件下载而返回 400。
    let mut cur_is_collection = false;
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
    // 归一化比对用：去末尾斜杠 + percent-decode
    let norm = |s: &str| -> String {
        urlencoding_decode(s).trim_end_matches('/').to_string()
    };
    let base_norm_full = norm(&base_href_norm);
    let base_norm_path = norm(base_url_path);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match xml_local_lower(e.name().as_ref()).as_str() {
                "response" => {
                    cur_href.clear();
                    cur_displayname.clear();
                    cur_lastmodified.clear();
                    cur_is_collection = false;
                    cur_size = 0;
                }
                "href" => in_href = true,
                "getcontentlength" => in_getcontentlength = true,
                "displayname" => in_displayname = true,
                "getlastmodified" => in_getlastmodified = true,
                "collection" => cur_is_collection = true,
                _ => {}
            },
            // 自闭合标签：<d:collection/>、空 <d:getcontentlength/> 等
            Ok(Event::Empty(e)) => {
                if xml_local_lower(e.name().as_ref()) == "collection" {
                    cur_is_collection = true;
                }
            }
            Ok(Event::End(e)) => match xml_local_lower(e.name().as_ref()).as_str() {
                "response" => {
                    // 跳过自身目录：比较完整 URL、路径部分、decode 后三者
                    let href = cur_href.clone();
                    if href.is_empty() {
                        continue;
                    }
                    let decoded = urlencoding_decode(&href);
                    let trimmed = decoded.trim_end_matches('/');
                    let href_norm = norm(&href);
                    // href 可能为完整 URL（含 scheme/host）或纯路径，需同时比对
                    // 另需比对 decode 后的路径部分（中文/空格 percent 编码时）
                    let href_path = href
                        .split_once("://")
                        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
                        .unwrap_or(href.as_str());
                    let is_self = trimmed == base_url.trim_end_matches('/')
                        || href_norm == base_norm_full
                        || href_norm == base_norm_path
                        || norm(href_path) == base_norm_path
                        || norm(&decoded) == base_norm_path;
                    if !is_self {
                        // 名称优先用 displayname（服务端已解码，更可靠），
                        // 缺失时回退 href 末段 decode
                        let mut name = urlencoding_decode(cur_displayname.trim());
                        if name.is_empty() {
                            name = href
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .unwrap_or(&href)
                                .to_string();
                            name = urlencoding_decode(&name);
                        }
                        // 部分实现 displayname 返回完整路径，取末段
                        if name.contains('/') {
                            name = name
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .unwrap_or(&name)
                                .to_string();
                        }
                        if !name.is_empty() {
                            // 文件夹判定三要素：collection 标记优先，
                            // 其次 href 末尾斜杠，最后无 size 且 displayname 无扩展名不作为依据
                            // （避免把无 Content-Length 的空文件误判为文件夹）
                            let is_dir = cur_is_collection || href.ends_with('/');
                            let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
                            let (cls, lbl, kd) = if is_dir {
                                ("folder".into(), "F".into(), "文件夹".into())
                            } else {
                                let (c, l, k) = classify(Path::new(&name), false);
                                (c, l, k)
                            };
                            let mtime = parse_http_date(&cur_lastmodified);
                            entries.push(Entry {
                                name: name.clone(),
                                path,
                                is_dir,
                                size_bytes: if is_dir { 0 } else { cur_size },
                                modified_ts: mtime,
                                kind: kd,
                                icon_label: lbl,
                                icon_class: cls,
                            });
                        }
                    }
                }
                "href" => in_href = false,
                "getcontentlength" => in_getcontentlength = false,
                "displayname" => in_displayname = false,
                "getlastmodified" => in_getlastmodified = false,
                "collection" => {}
                _ => {}
            },
            Ok(Event::Text(e)) => {
                let t = e.unescape().unwrap_or_default().to_string();
                if in_href {
                    cur_href = t;
                } else if in_getcontentlength {
                    // 空目录的 getcontentlength 可能为空文本，保持 0 即可
                    if !t.trim().is_empty() {
                        cur_size = t.trim().parse().unwrap_or(0);
                    }
                } else if in_displayname {
                    cur_displayname = t;
                } else if in_getlastmodified {
                    cur_lastmodified = t;
                }
            }
            // CDATA 内的 displayname（含特殊字符时服务端用 CDATA 包裹）
            Ok(Event::CData(e)) => {
                let t = String::from_utf8_lossy(&e).to_string();
                if in_displayname {
                    cur_displayname = t;
                } else if in_href {
                    cur_href = t;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }
    // 目录优先、文件随后，与本地磁盘排序习惯一致（最终排序仍由 TabSession.rebuild 按设置执行，
    // 此处预排序保证未开启文件夹优先时云目录也不杂乱）
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
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
// 说明：不引入 libssh2/openssl（需 perl 编译），SFTP 真机传输经内嵌 rclone
// （:sftp: + lsjson，无终端窗口）。账号可正常添加、展示与管理，列表为真实远端内容。
fn list_sftp(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    super::rclone::list_sftp_via_rclone(loc, sub)
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
    // webdav_url 内部已拼接远程基路径，此处仅传相对子路径 + 新建名称，
    // 旧实现误把 remote_base 拼入 sub 导致基路径重复（/dav/dav/...）而 404
    let rel = join_remote(sub, name);
    let url = webdav_url(loc, &rel)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let mut req = webdav_agent()
        .request("MKCOL", &url)
        .timeout(Duration::from_secs(15))
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:{}", loc.username, loc.password));
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req.call().map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
    if resp.status() >= 400 { return Err(format!("新建文件夹失败（MKCOL {}）", resp.status())); }
    Ok(())
}
fn sftp_mkdir(_loc: &NetworkLocation, _sub: &str, _name: &str) -> Result<(), String> {
    Err("SFTP 新建文件夹暂不支持（rclone 列表为只读浏览），请用其它 SFTP 客户端创建".into())
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
    let url = webdav_url(loc, sub)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let send_delete = |target: &str| -> Result<u16, String> {
        let mut req = webdav_agent()
            .request("DELETE", target)
            .timeout(Duration::from_secs(15))
            .set("User-Agent", "FileFiles-One/WebDAV");
        if !loc.username.is_empty() {
            let cred = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:{}", loc.username, loc.password));
            req = req.set("Authorization", &format!("Basic {}", cred));
        }
        let resp = req.call().map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
        Ok(resp.status())
    };
    let status = send_delete(&url)?;
    // 部分服务器（坚果云）对集合 DELETE 无斜杠时返回 403：按集合形式重试
    let status = if status >= 400 && !url.ends_with('/') {
        send_delete(&format!("{}/", url))?
    } else {
        status
    };
    if status >= 400 {
        return Err(format!("删除失败（DELETE {}）", status));
    }
    Ok(())
}
fn sftp_delete(_loc: &NetworkLocation, _sub: &str) -> Result<(), String> {
    Err("SFTP 删除暂不支持（rclone 列表为只读浏览），请用其它 SFTP 客户端删除".into())
}

// ──────────── 云端写入操作（上传 / 下载 / 复制 / 移动 / 建目录）────────────
//
// 所有函数均为同步阻塞实现，必须由任务系统在后台线程调用；粘贴/移动/复制
// 到 cloud:// 的任务在 fs::tasks 的云端任务运行器中逐项调到这里。
// 进度回调 progress(已传字节, 总字节) 内部按 100ms 节流；ctrl 取消时返回
// Err(已取消)，任务层以 TaskControl::is_cancelled 区分「取消」与「失败」。
//
// 冲突策略：上传/下载/服务端复制一律覆盖同名（与资源管理器「覆盖」语义
// 一致）；WebDAV COPY/MOVE 显式 Overwrite: T，PUT 天然覆盖，FTP STOR 覆盖。
//
// SFTP 无流式进度（rclone 子命令不回传字节），进度按整文件粒度上报。

/// 进度回调：参数为 (已传输字节, 总字节)，总字节未知时为 0
pub type CloudProgress<'a> = &'a mut (dyn FnMut(u64, u64) + 'a);

/// 取消哨兵错误文案：任务层以 TaskControl::is_cancelled 判定
pub const CLOUD_CANCELLED: &str = "已取消";

/// 该云账号是否允许写入。WebDAV 与其虚拟磁盘共用「只读挂载」开关：
/// 账号勾选只读后，原生浏览（cloud://）与虚拟磁盘口径一致，均禁止写入。
/// FTP / SFTP 默认可写（服务端权限不足时由具体操作返回 5xx/权限错误）。
pub fn cloud_writable(loc: &NetworkLocation) -> bool {
    !(loc.kind == "webdav" && loc.mount_readonly)
}

/// 按完整 cloud:// 路径解析账号配置（kind+name 定位）
fn account_of(cloud_path: &str, config: &AppConfig) -> Result<NetworkLocation, String> {
    let (kind, name, _) = parse_cloud_path(cloud_path).ok_or("不是云存储路径")?;
    config
        .network_locations
        .iter()
        .find(|l| l.kind == kind && l.name == name)
        .cloned()
        .ok_or_else(|| "未找到云存储账号".to_string())
}

/// FTP 登录公共段：连接 + 匿名/凭据登录
fn ftp_login(loc: &NetworkLocation) -> Result<FtpConn, String> {
    let mut ftp = connect_ftp(loc)?;
    let user = if loc.username.is_empty() {
        "anonymous".to_string()
    } else {
        loc.username.clone()
    };
    ftp.login(&user, &loc.password)?;
    Ok(ftp)
}

/// 上传进度节流状态
struct UploadState {
    sent: u64,
    last_emit: Instant,
}

impl UploadState {
    fn new() -> Self {
        Self {
            sent: 0,
            last_emit: Instant::now() - Duration::from_secs(1),
        }
    }
    fn tick(&mut self, delta: u64, total: u64, progress: &mut (dyn FnMut(u64, u64) + '_)) {
        self.sent += delta;
        let now = Instant::now();
        if now.duration_since(self.last_emit) >= Duration::from_millis(100) {
            self.last_emit = now;
            (progress)(self.sent, total);
        }
    }
    fn finish(&mut self, total: u64, progress: &mut (dyn FnMut(u64, u64) + '_)) {
        (progress)(self.sent, if total > 0 { total } else { self.sent });
    }
}

/// 包装本地文件读取：逐块响应暂停/取消并累计进度（WebDAV PUT 上传用）。
struct CloudUpReader<'a> {
    inner: std::fs::File,
    ctrl: &'a TaskControl,
    total: u64,
    state: &'a mut UploadState,
    progress: CloudProgress<'a>,
}

impl std::io::Read for CloudUpReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ctrl.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                CLOUD_CANCELLED,
            ));
        }
        self.ctrl.wait_if_paused();
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.state
                .tick(n as u64, self.total, &mut *self.progress);
        }
        Ok(n)
    }
}

/// 上传本地文件到云端目录 `dst_dir`（cloud:// 目录路径），同名覆盖。
pub fn upload_to_cloud(
    local: &Path,
    dst_dir: &str,
    config: &AppConfig,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    let loc = account_of(dst_dir, config)?;
    if !cloud_writable(&loc) {
        return Err("该账号设置为只读，不允许写入云端".into());
    }
    let (_, _, sub) = parse_cloud_path(dst_dir).ok_or("不是云存储路径")?;
    let fname = local
        .file_name()
        .ok_or("本地路径缺少文件名")?
        .to_string_lossy()
        .to_string();
    let target_sub = join_remote(&sub, &fname);
    match loc.kind.as_str() {
        "webdav" => webdav_upload_file(&loc, &target_sub, local, ctrl, progress),
        "ftp" => ftp_upload_file(&loc, &target_sub, local, ctrl, progress),
        "sftp" => {
            let total = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
            (progress)(0, total);
            let r = super::rclone::run_sftp_command(
                &loc,
                &[
                    "copyto",
                    &local.to_string_lossy(),
                    &super::rclone::sftp_remote(&loc, &target_sub),
                    "--log-level",
                    "ERROR",
                    "--no-console",
                ],
                1800,
                "SFTP 上传",
            );
            if r.is_ok() {
                (progress)(total, total);
            }
            r.map(|_| ())
        }
        _ => Err("未知云存储类型".into()),
    }
}

/// 下载云端文件 `src`（cloud:// 文件路径）到本地目录 `dst_dir`，同名覆盖。
pub fn download_from_cloud(
    src: &str,
    dst_dir: &Path,
    config: &AppConfig,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    let loc = account_of(src, config)?;
    let (_, _, sub) = parse_cloud_path(src).ok_or("不是云存储路径")?;
    let fname = sub
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    if fname.is_empty() {
        return Err("云端路径缺少文件名".into());
    }
    let local = dst_dir.join(&fname);
    match loc.kind.as_str() {
        "webdav" => webdav_download_file(&loc, &sub, &local, ctrl, progress),
        "ftp" => ftp_download_file(&loc, &sub, &local, ctrl, progress),
        "sftp" => {
            // 总大小需另一次 lsjson 往返才能拿到，按完成粒度上报
            (progress)(0, 0);
            let r = super::rclone::run_sftp_command(
                &loc,
                &[
                    "copyto",
                    &super::rclone::sftp_remote(&loc, &sub),
                    &local.to_string_lossy(),
                    "--log-level",
                    "ERROR",
                    "--no-console",
                ],
                1800,
                "SFTP 下载",
            );
            if r.is_ok() {
                let sz = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
                (progress)(sz, sz);
            }
            r.map(|_| ())
        }
        _ => Err("未知云存储类型".into()),
    }
}

/// 同账号云端复制：WebDAV 走服务端 COPY；SFTP 走 rclone copyto；
/// FTP 无服务端复制语义，经本地临时文件中转（下载→上传）。
pub fn cloud_copy_same(
    src: &str,
    dst_dir: &str,
    config: &AppConfig,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    let src_loc = account_of(src, config)?;
    let dst_loc = account_of(dst_dir, config)?;
    if !cloud_writable(&dst_loc) {
        return Err("目标账号设置为只读，不允许写入云端".into());
    }
    let (_, _, src_sub) = parse_cloud_path(src).ok_or("不是云存储路径")?;
    let (_, _, dst_sub) = parse_cloud_path(dst_dir).ok_or("不是云存储路径")?;
    let fname = src_sub
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    if fname.is_empty() {
        return Err("云端路径缺少文件名".into());
    }
    let target_sub = join_remote(&dst_sub, &fname);
    match (src_loc.kind.as_str(), dst_loc.kind.as_str()) {
        ("webdav", "webdav") if src_loc.name == dst_loc.name => {
            webdav_copy_move(&src_loc, &src_sub, &target_sub, false)
        }
        ("sftp", "sftp") if src_loc.name == dst_loc.name => super::rclone::run_sftp_command(
            &src_loc,
            &[
                "copyto",
                &super::rclone::sftp_remote(&src_loc, &src_sub),
                &super::rclone::sftp_remote(&dst_loc, &target_sub),
                "--log-level",
                "ERROR",
                "--no-console",
            ],
            1800,
            "SFTP 复制",
        )
        .map(|_| ()),
        // 同一 FTP 账号：无服务端复制，目录递归 + 文件临时中转
        ("ftp", "ftp") if src_loc.name == dst_loc.name => {
            // 目录判定：CWD 成功即目录
            let mut ftp = ftp_login(&src_loc)?;
            let remote = join_remote(&remote_base(&src_loc), &src_sub);
            let is_dir = ftp.cwd(&remote).is_ok();
            let _ = ftp.quit();
            if is_dir {
                ftp_copy_tree(&src_loc, &src_sub, &target_sub, ctrl, progress)
            } else {
                ftp_copy_via_local(&src_loc, &src_sub, &target_sub, ctrl, progress)
            }
        }
        _ => Err("跨账号复制暂不支持服务端直传".into()),
    }
}

/// FTP 目录复制：逐级建目录 + 子项复制（文件走本地临时中转）。
fn ftp_copy_tree(
    loc: &NetworkLocation,
    src_sub: &str,
    dst_sub: &str,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    // 目标建目录（已存在容忍）
    {
        let mut ftp = ftp_login(loc)?;
        let dst_remote = join_remote(&remote_base(loc), dst_sub);
        let r: Result<(), String> = match ftp.mkdir(&dst_remote) {
            Ok(()) => Ok(()),
            Err(_) => {
                let ok = ftp.cwd(&dst_remote).is_ok();
                if ok {
                    Ok(())
                } else {
                    Err("FTP 新建文件夹失败".into())
                }
            }
        };
        let _ = ftp.quit();
        r?;
    }
    let children = list_ftp(loc, src_sub)?;
    for c in children {
        if ctrl.is_cancelled() {
            return Err(CLOUD_CANCELLED.to_string());
        }
        let (_, _, child_sub) = parse_cloud_path(&c.path).ok_or("不是云存储路径")?;
        let child_dst = join_remote(dst_sub, &c.name);
        if c.is_dir {
            ftp_copy_tree(loc, &child_sub, &child_dst, ctrl, progress)?;
        } else {
            ftp_copy_via_local(loc, &child_sub, &child_dst, ctrl, progress)?;
        }
    }
    Ok(())
}

/// 同账号云端移动/重命名：WebDAV MOVE；FTP RNFR+RNTO（支持跨目录）；
/// SFTP rclone moveto。`new_name` 为 Some 时即重命名语义。
pub fn cloud_move_same(
    src: &str,
    dst_dir: &str,
    new_name: Option<&str>,
    config: &AppConfig,
) -> Result<(), String> {
    let src_loc = account_of(src, config)?;
    let dst_loc = account_of(dst_dir, config)?;
    if !cloud_writable(&dst_loc) {
        return Err("目标账号设置为只读，不允许写入云端".into());
    }
    let (_, _, src_sub) = parse_cloud_path(src).ok_or("不是云存储路径")?;
    let (_, _, dst_sub) = parse_cloud_path(dst_dir).ok_or("不是云存储路径")?;
    let src_name = src_sub
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    if src_name.is_empty() {
        return Err("云端路径缺少文件名".into());
    }
    let target_sub = join_remote(&dst_sub, new_name.unwrap_or(&src_name));
    match (src_loc.kind.as_str(), dst_loc.kind.as_str()) {
        ("webdav", "webdav") if src_loc.name == dst_loc.name => {
            webdav_copy_move(&src_loc, &src_sub, &target_sub, true)
        }
        ("sftp", "sftp") if src_loc.name == dst_loc.name => super::rclone::run_sftp_command(
            &src_loc,
            &[
                "moveto",
                &super::rclone::sftp_remote(&src_loc, &src_sub),
                &super::rclone::sftp_remote(&dst_loc, &target_sub),
                "--log-level",
                "ERROR",
                "--no-console",
            ],
            1800,
            "SFTP 移动",
        )
        .map(|_| ()),
        ("ftp", "ftp") if src_loc.name == dst_loc.name => {
            let mut ftp = ftp_login(&src_loc)?;
            let from = join_remote(&remote_base(&src_loc), &src_sub);
            let to = join_remote(&remote_base(&src_loc), &target_sub);
            let r = ftp.rename(&from, &to);
            let _ = ftp.quit();
            r
        }
        _ => Err("跨账号移动暂不支持服务端直传".into()),
    }
}

/// 按完整 cloud:// 目录路径创建目录（账号根目录天然存在，直接成功）。
/// 目录已存在视为成功（WebDAV 405 / FTP 550-exists / rclone mkdir 幂等）。
pub fn cloud_mkdir_full(cloud_dir: &str, config: &AppConfig) -> Result<(), String> {
    let loc = account_of(cloud_dir, config)?;
    if !cloud_writable(&loc) {
        return Err("该账号设置为只读，不允许写入云端".into());
    }
    let (_, _, sub) = parse_cloud_path(cloud_dir).ok_or("不是云存储路径")?;
    if sub.trim_matches('/').is_empty() {
        return Ok(());
    }
    match loc.kind.as_str() {
        "webdav" => {
            let url = webdav_url(&loc, &sub)?;
            let (host, port, _) = split_host_port_base(&loc)?;
            let use_tls = webdav_use_tls(&loc);
            let mut req = webdav_agent()
                .request("MKCOL", &url)
                .timeout(Duration::from_secs(15))
                .set("User-Agent", "FileFiles-One/WebDAV");
            if !loc.username.is_empty() {
                let cred = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("{}:{}", loc.username, loc.password),
                );
                req = req.set("Authorization", &format!("Basic {}", cred));
            }
            let resp = req
                .call()
                .map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
            // 405 = 集合已存在（MKCOL 对已存在资源返回 405 Method Not Allowed）
            if resp.status() >= 400 && resp.status() != 405 {
                return Err(format!("新建云端文件夹失败（MKCOL {}）", resp.status()));
            }
            Ok(())
        }
        "ftp" => {
            let mut ftp = ftp_login(&loc)?;
            let remote = join_remote(&remote_base(&loc), &sub);
            match ftp.mkdir(&remote) {
                Ok(()) => {
                    let _ = ftp.quit();
                    Ok(())
                }
                Err(_) => {
                    // 已存在时 MKD 报 550：尝试进入验证
                    let ok = ftp.cwd(&remote).is_ok();
                    let _ = ftp.quit();
                    if ok {
                        Ok(())
                    } else {
                        Err("FTP 新建文件夹失败".into())
                    }
                }
            }
        }
        "sftp" => super::rclone::run_sftp_command(
            &loc,
            &[
                "mkdir",
                &super::rclone::sftp_remote(&loc, &sub),
                "--log-level",
                "ERROR",
                "--no-console",
            ],
            60,
            "SFTP 新建文件夹",
        )
        .map(|_| ()),
        _ => Err("未知云存储类型".into()),
    }
}

// ── WebDAV 传输原语 ──

/// WebDAV PUT 上传（带 Content-Length，避免服务器拒绝 chunked PUT）。
fn webdav_upload_file(
    loc: &NetworkLocation,
    sub: &str,
    local: &Path,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    let url = webdav_url(loc, sub)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let size = std::fs::metadata(local)
        .map_err(|e| format!("读取本地文件失败：{}", e))?
        .len();
    let file = std::fs::File::open(local).map_err(|e| format!("打开本地文件失败：{}", e))?;
    let mut state = UploadState::new();
    (progress)(0, size);
    let mut req = webdav_agent()
        .put(&url)
        // 大文件上传不能设整体超时；连接超时由 agent 级 timeout_connect 兜底
        .set("Content-Length", &size.to_string())
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    // reborrow：reader 只借用本次调用，send 返回后 progress 仍可上报终值
    let reader = CloudUpReader {
        inner: file,
        ctrl,
        total: size,
        state: &mut state,
        progress: &mut *progress,
    };
    let resp = req.send(reader).map_err(|e| {
        if ctrl.is_cancelled() {
            CLOUD_CANCELLED.to_string()
        } else {
            map_webdav_err(e, &host, port, use_tls)
        }
    })?;
    if resp.status() >= 400 {
        return Err(format!("上传失败（PUT {}）", resp.status()));
    }
    state.finish(size, &mut *progress);
    Ok(())
}

/// WebDAV GET 下载（流式分块落盘，逐块响应暂停/取消并上报进度）。
fn webdav_download_file(
    loc: &NetworkLocation,
    sub: &str,
    local: &Path,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    use std::io::Read;
    let url = webdav_url(loc, sub)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let mut req = webdav_agent()
        .get(&url)
        // 大文件下载不设整体超时；单次 socket 读由 agent 级 timeout_read 兜底
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req
        .call()
        .map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
    if resp.status() >= 400 {
        return Err(format!("下载失败（GET {}）", resp.status()));
    }
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let tmp = local.with_extension("part");
    let mut out =
        std::fs::File::create(&tmp).map_err(|e| format!("创建本地文件失败：{}", e))?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        if ctrl.is_cancelled() {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            return Err(CLOUD_CANCELLED.to_string());
        }
        ctrl.wait_if_paused();
        let n = reader.read(&mut buf).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("下载中断：{}", e)
        })?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n]).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("写入本地文件失败：{}", e)
        })?;
        done += n as u64;
        let now = Instant::now();
        if now.duration_since(last_emit) >= Duration::from_millis(100) {
            last_emit = now;
            (progress)(done, total);
        }
    }
    drop(out);
    std::fs::rename(&tmp, local).map_err(|e| format!("落盘失败：{}", e))?;
    (progress)(done, if total > 0 { total } else { done });
    Ok(())
}

/// WebDAV COPY / MOVE（服务端操作，Overwrite: T 覆盖同名目标）。
fn webdav_copy_move(
    loc: &NetworkLocation,
    src_sub: &str,
    dst_sub: &str,
    is_move: bool,
) -> Result<(), String> {
    let src_url = webdav_url(loc, src_sub)?;
    let dst_url = webdav_url(loc, dst_sub)?;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    let method = if is_move { "MOVE" } else { "COPY" };
    let mut req = webdav_agent()
        .request(method, &src_url)
        .timeout(Duration::from_secs(60))
        .set("Destination", &dst_url)
        .set("Overwrite", "T")
        .set("User-Agent", "FileFiles-One/WebDAV");
    if !loc.username.is_empty() {
        let cred = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", loc.username, loc.password),
        );
        req = req.set("Authorization", &format!("Basic {}", cred));
    }
    let resp = req
        .call()
        .map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
    if resp.status() >= 400 {
        let op = if is_move { "移动" } else { "复制" };
        return Err(format!("云端{}失败（{} {}）", op, method, resp.status()));
    }
    Ok(())
}

// ── FTP 传输原语 ──

impl FtpConn {
    /// 流式上传：把 DataStream（impl Write）交给回调写完再 finalize。
    /// 写入中途出错时放弃流（连接脏化），由调用方整体失败。
    fn upload_stream(
        &mut self,
        remote: &str,
        write: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
    ) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => {
                let mut ds = f.put_with_stream(remote).map_err(|e| map_ftp_err(&e))?;
                match write(&mut ds) {
                    Ok(()) => f.finalize_put_stream(ds).map_err(|e| map_ftp_err(&e)),
                    Err(e) => {
                        drop(ds);
                        Err(e.to_string())
                    }
                }
            }
            FtpConn::Secure(f) => {
                let mut ds = f.put_with_stream(remote).map_err(|e| map_ftp_err(&e))?;
                match write(&mut ds) {
                    Ok(()) => f.finalize_put_stream(ds).map_err(|e| map_ftp_err(&e)),
                    Err(e) => {
                        drop(ds);
                        Err(e.to_string())
                    }
                }
            }
        }
    }

    /// 流式下载：把 DataStream（impl Read）交给回调读完再 finalize。
    /// 读中途出错时 abort 数据连接（发送 ABOR 复位控制连接状态）。
    fn download_stream(
        &mut self,
        remote: &str,
        read: impl FnOnce(&mut dyn std::io::Read) -> std::io::Result<()>,
    ) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => {
                let mut ds = f.retr_as_stream(remote).map_err(|e| map_ftp_err(&e))?;
                match read(&mut ds) {
                    Ok(()) => f.finalize_retr_stream(ds).map_err(|e| map_ftp_err(&e)),
                    Err(e) => {
                        let _ = f.abort(ds);
                        Err(e.to_string())
                    }
                }
            }
            FtpConn::Secure(f) => {
                let mut ds = f.retr_as_stream(remote).map_err(|e| map_ftp_err(&e))?;
                match read(&mut ds) {
                    Ok(()) => f.finalize_retr_stream(ds).map_err(|e| map_ftp_err(&e)),
                    Err(e) => {
                        let _ = f.abort(ds);
                        Err(e.to_string())
                    }
                }
            }
        }
    }

    /// 服务端重命名/移动（RNFR + RNTO，支持跨目录，取决于服务器实现）
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        match self {
            FtpConn::Plain(f) => f.rename(from, to).map_err(|e| map_ftp_err(&e)),
            FtpConn::Secure(f) => f.rename(from, to).map_err(|e| map_ftp_err(&e)),
        }
    }

    /// 远端文件大小（SIZE 命令，失败返回 0 = 总量未知）
    fn size(&mut self, remote: &str) -> u64 {
        match self {
            FtpConn::Plain(f) => f.size(remote).unwrap_or(0) as u64,
            FtpConn::Secure(f) => f.size(remote).unwrap_or(0) as u64,
        }
    }
}

fn ftp_upload_file(
    loc: &NetworkLocation,
    sub: &str,
    local: &Path,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let size = std::fs::metadata(local)
        .map_err(|e| format!("读取本地文件失败：{}", e))?
        .len();
    let mut file = std::fs::File::open(local).map_err(|e| format!("打开本地文件失败：{}", e))?;
    let mut ftp = ftp_login(loc)?;
    let remote = join_remote(&remote_base(loc), sub);
    let mut state = UploadState::new();
    (progress)(0, size);
    let mut buf = vec![0u8; 64 * 1024];
    let res = ftp.upload_stream(&remote, |w| {
        loop {
            if ctrl.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    CLOUD_CANCELLED,
                ));
            }
            ctrl.wait_if_paused();
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            state.tick(n as u64, size, &mut *progress);
        }
        Ok(())
    });
    let _ = ftp.quit();
    res?;
    state.finish(size, &mut *progress);
    Ok(())
}

fn ftp_download_file(
    loc: &NetworkLocation,
    sub: &str,
    local: &Path,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    use std::io::Read;
    let mut ftp = ftp_login(loc)?;
    let remote = join_remote(&remote_base(loc), sub);
    let total = ftp.size(&remote);
    let tmp = local.with_extension("part");
    let mut out =
        std::fs::File::create(&tmp).map_err(|e| format!("创建本地文件失败：{}", e))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let res = ftp.download_stream(&remote, |ds| {
        loop {
            if ctrl.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    CLOUD_CANCELLED,
                ));
            }
            ctrl.wait_if_paused();
            let n = ds.read(&mut buf)?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            out.write_all(&buf[..n])?;
            done += n as u64;
            let now = Instant::now();
            if now.duration_since(last_emit) >= Duration::from_millis(100) {
                last_emit = now;
                (progress)(done, total);
            }
        }
        Ok(())
    });
    let _ = ftp.quit();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        if ctrl.is_cancelled() {
            return Err(CLOUD_CANCELLED.to_string());
        }
        return Err(e);
    }
    drop(out);
    std::fs::rename(&tmp, local).map_err(|e| format!("落盘失败：{}", e))?;
    (progress)(done, if total > 0 { total } else { done });
    Ok(())
}

/// 云端条目元信息（文件/目录判定 + 大小），供任务层决定递归或直传
pub struct CloudStat {
    pub is_dir: bool,
    pub size: u64,
}

/// 判定云端路径是文件还是目录并取大小（单次往返）。
/// 任务层据此展开递归；错误返回 Err（路径不可达）。
pub fn cloud_stat(path: &str, config: &AppConfig) -> Result<CloudStat, String> {
    let loc = account_of(path, config)?;
    let (_, _, sub) = parse_cloud_path(path).ok_or("不是云存储路径")?;
    if sub.trim_matches('/').is_empty() {
        return Ok(CloudStat { is_dir: true, size: 0 });
    }
    match loc.kind.as_str() {
        "webdav" => webdav_stat(&loc, &sub),
        "ftp" => {
            let mut ftp = ftp_login(&loc)?;
            let remote = join_remote(&remote_base(&loc), &sub);
            // 目录判定：CWD 成功即目录；否则 SIZE 取文件大小
            if ftp.cwd(&remote).is_ok() {
                let _ = ftp.quit();
                return Ok(CloudStat { is_dir: true, size: 0 });
            }
            let size = ftp.size(&remote);
            let _ = ftp.quit();
            Ok(CloudStat { is_dir: false, size })
        }
        "sftp" => {
            let out = super::rclone::run_sftp_command(
                &loc,
                &["lsjson", &super::rclone::sftp_remote(&loc, &sub)],
                30,
                "SFTP 元信息",
            )?;
            let v: serde_json::Value =
                serde_json::from_str(&out).map_err(|e| format!("解析元信息失败：{}", e))?;
            let first = v
                .as_array()
                .and_then(|a| a.first())
                .ok_or("远端路径不存在")?;
            Ok(CloudStat {
                is_dir: first.get("IsDir").and_then(|b| b.as_bool()).unwrap_or(false),
                size: first
                    .get("Size")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0),
            })
        }
        _ => Err("未知云存储类型".into()),
    }
}

/// WebDAV Depth:0 PROPFIND：解析自身条目的 collection 标记与大小。
fn webdav_stat(loc: &NetworkLocation, sub: &str) -> Result<CloudStat, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let (host, port, _) = split_host_port_base(loc)?;
    let use_tls = webdav_use_tls(loc);
    const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;
    let send_propfind = |url: &str| -> Result<(u16, String), String> {
        let mut req = webdav_agent()
            .request("PROPFIND", url)
            .timeout(Duration::from_secs(15))
            .set("Depth", "0")
            .set("Content-Type", "application/xml; charset=utf-8")
            .set("User-Agent", "FileFiles-One/WebDAV");
        if !loc.username.is_empty() {
            let cred = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("{}:{}", loc.username, loc.password),
            );
            req = req.set("Authorization", &format!("Basic {}", cred));
        }
        let resp = req
            .send_string(BODY)
            .map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
        let status = resp.status();
        let body = resp.into_string().unwrap_or_default();
        Ok((status, body))
    };
    // 文件路径不能带尾斜杠、集合路径部分实现要求尾斜杠：先按原始路径，
    // 404/400 时按集合形式（加斜杠）重试一次
    let url_plain = webdav_url(loc, sub)?;
    let (status, xml) = send_propfind(&url_plain)?;
    let (status, xml) = if (status == 404 || status == 400) && !url_plain.ends_with('/') {
        let url_dir = format!("{}/", url_plain);
        let r = send_propfind(&url_dir)?;
        (r.0, r.1)
    } else {
        (status, xml)
    };
    if status >= 400 && status != 404 {
        return Err(format!("读取云端元信息失败（PROPFIND {}）", status));
    }
    if status == 404 {
        // 坚果云等实现只支持对集合 PROPFIND（对文件返回 404）：
        // 回退到父目录 Depth:1 列表，按名称查找自身条目
        let name = sub.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if name.is_empty() {
            return Err("云端路径不存在".into());
        }
        let trimmed = sub.trim_end_matches('/');
        let parent_sub = match trimmed.rfind('/') {
            Some(i) => &trimmed[..i],
            None => "",
        };
        let mut url_parent = webdav_url(loc, parent_sub)?;
        if !url_parent.ends_with('/') {
            url_parent.push('/');
        }
        let mut req = webdav_agent()
            .request("PROPFIND", &url_parent)
            .timeout(Duration::from_secs(15))
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8")
            .set("User-Agent", "FileFiles-One/WebDAV");
        if !loc.username.is_empty() {
            let cred = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("{}:{}", loc.username, loc.password),
            );
            req = req.set("Authorization", &format!("Basic {}", cred));
        }
        let resp = req
            .send_string(BODY)
            .map_err(|e| map_webdav_err(e, &host, port, use_tls))?;
        if resp.status() >= 400 {
            return Err("云端路径不存在".into());
        }
        let parent_xml = resp
            .into_string()
            .map_err(|e| format!("读取元信息失败：{}", e))?;
        let entries = parse_webdav_propfind(&parent_xml, &url_parent, loc, parent_sub)?;
        let hit = entries
            .into_iter()
            .find(|e| e.name == name)
            .ok_or("云端路径不存在")?;
        return Ok(CloudStat {
            is_dir: hit.is_dir,
            size: hit.size_bytes,
        });
    }
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut is_dir = false;
    let mut size: u64 = 0;
    let mut in_length = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = xml_local_lower(e.name().as_ref());
                match name.as_str() {
                    "collection" => is_dir = true,
                    "getcontentlength" => in_length = true,
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_length {
                    size = t.unescape().unwrap_or_default().trim().parse().unwrap_or(0);
                }
            }
            Ok(Event::End(e)) => {
                if xml_local_lower(e.name().as_ref()) == "getcontentlength" {
                    in_length = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("解析元信息失败：{}", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(CloudStat { is_dir, size })
}

/// FTP 无服务端复制：经本地临时文件中转（下载 → 上传），结束清理临时文件。
/// 总进度按两段折算：下载占前半（0~50%），上传占后半（50%~100%）。
fn ftp_copy_via_local(
    loc: &NetworkLocation,
    src_sub: &str,
    dst_sub: &str,
    ctrl: &TaskControl,
    progress: CloudProgress,
) -> Result<(), String> {
    let dir = std::env::temp_dir()
        .join("FileFiles One")
        .join("cloud_tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{}", e))?;
    let fname = src_sub
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("file");
    let tmp = dir.join(format!("copy_{}_{}", std::process::id(), fname));
    let r = (|| -> Result<(), String> {
        // 前半段：下载（进度折半计入总进度）
        {
            let half: CloudProgress = &mut |done: u64, total: u64| {
                (progress)(done / 2, total.saturating_mul(2));
            };
            ftp_download_file(loc, src_sub, &tmp, ctrl, half)?;
        }
        // 后半段：上传（进度从 50% 起计入）
        let base = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let second: CloudProgress = &mut |done: u64, total: u64| {
            (progress)(base + done, base.saturating_add(total));
        };
        ftp_upload_file(loc, dst_sub, &tmp, ctrl, second)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    r
}

#[cfg(test)]
mod tests {
    use super::{clean_host, is_cloud_root, parse_http_date, urlencoding_decode};

    #[test]
    fn decodes_utf8_percent_sequences() {
        assert_eq!(urlencoding_decode("%E4%B8%AD%E6%96%87%20A.txt"), "中文 A.txt");
    }

    #[test]
    fn keeps_invalid_percent_sequences() {
        assert_eq!(urlencoding_decode("bad%ZZ.txt"), "bad%ZZ.txt");
        assert_eq!(urlencoding_decode("tail%"), "tail%");
    }

    #[test]
    fn host_input_tolerates_full_url_paste() {
        // 用户常把完整 WebDAV 地址粘进“主机”栏，清洗后仍可连接
        assert_eq!(clean_host("https://example.com/dav/files"), "example.com");
        assert_eq!(clean_host("http://user:pw@example.com:8080/dav?x=1"), "example.com");
        assert_eq!(clean_host("example.com:8080"), "example.com");
        assert_eq!(clean_host("  example.com  "), "example.com");
    }

    #[test]
    fn cloud_root_detection_only_for_account_root() {
        assert!(is_cloud_root("cloud://webdav/PikPak"));
        assert!(is_cloud_root("cloud://webdav/PikPak/"));
        assert!(!is_cloud_root("cloud://webdav/PikPak/My Pack"));
        assert!(!is_cloud_root("cloud://webdav/PikPak/a/b.txt"));
    }

    #[test]
    fn cloud_listing_reports_missing_account() {
        let error = match super::list_cloud_dir_result("cloud://webdav/Missing", &[]) {
            Err(error) => error,
            Ok(_) => panic!("缺失账号必须返回错误"),
        };
        assert!(error.contains("未找到云存储账号"));
    }

    #[test]
    fn http_date_parses_webdav_lastmodified() {
        // 标准 IMF-fixdate（星期需与日期相符，2026-09-12 为周六）
        assert!(parse_http_date("Sat, 12 Sep 2026 08:00:00 GMT") > 0);
        // 容错：星期不符时仍能解析出时间（部分实现/手写日期星期错误）
        assert!(parse_http_date("Wed, 12 Sep 2026 08:00:00 GMT") > 0);
        assert_eq!(parse_http_date(""), 0);
        assert_eq!(parse_http_date("not-a-date"), 0);
    }

    #[test]
    fn self_closing_collection_is_detected_as_dir() {
        // PikPak/Nginx 返回自闭合 <D:collection/>，必须判为文件夹，
        // 否则文件夹被当成文件下载而返回 400，且图标分不清
        let loc = crate::config::NetworkLocation {
            name: "PikPak".into(),
            server: String::new(),
            kind: "webdav".into(),
            drive: None,
            host: "dav.example.com".into(),
            port: 0,
            remote_path: "/".into(),
            username: String::new(),
            password: String::new(),
            use_tls: true,
            passive: true,
            mount_drive: None,
            mount_readonly: false,
            mount_max_size_gb: None,
            mount_icon: String::new(),
        };
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:displayname>/</D:displayname></D:prop></D:propstat></D:response>
<D:response><D:href>/My%20Pack/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:displayname>My Pack</D:displayname><D:getlastmodified>Sat, 12 Sep 2026 08:00:00 GMT</D:getlastmodified></D:prop></D:propstat></D:response>
<D:response><D:href>/report.pdf</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>1234</D:getcontentlength><D:displayname>report.pdf</D:displayname></D:prop></D:propstat></D:response>
</D:multistatus>"#;
        let entries =
            super::parse_webdav_propfind(xml, "https://dav.example.com/", &loc, "").unwrap();
        assert_eq!(entries.len(), 2);
        let dir = entries.iter().find(|e| e.name == "My Pack").expect("应解析出文件夹");
        assert!(dir.is_dir, "自闭合 collection 必须判为文件夹");
        assert_eq!(dir.icon_class, "folder");
        assert!(dir.modified_ts > 0, "应解析出修改时间");
        let file = entries.iter().find(|e| e.name == "report.pdf").expect("应解析出文件");
        assert!(!file.is_dir);
        assert_eq!(file.size_bytes, 1234);
    }

    /// 诊断：坚果云 PROPFIND 行为探测（真实网络）
    #[test]
    #[ignore]
    fn diag_jianguoyun_propfind() {
        use super::{cloud_stat, list_cloud_dir_result};
        let config = crate::config::AppConfig::load();
        let loc = config
            .network_locations
            .iter()
            .find(|l| l.kind == "webdav" && l.host.contains("jianguoyun"))
            .expect("无坚果云账号");
        let root = loc.cloud_path();
        let dir = format!("{}/FileFilesOne自测", root.trim_end_matches('/'));
        let file = format!("{}/upload-me.txt", dir);
        for p in [&root, &dir, &file] {
            match list_cloud_dir_result(p, &config.network_locations) {
                Ok(v) => println!(
                    "list {:?} -> {} 项: {:?}",
                    p,
                    v.len(),
                    v.iter().map(|e| (e.name.clone(), e.is_dir)).take(8).collect::<Vec<_>>()
                ),
                Err(e) => println!("list {:?} -> ERR {}", p, e),
            }
        }
        match cloud_stat(&file, &config) {
            Ok(s) => println!("stat file -> is_dir={} size={}", s.is_dir, s.size),
            Err(e) => println!("stat file -> ERR {}", e),
        }
    }

    /// 坚果云 WebDAV 写操作全链路自测（真实网络，需已配置坚果云账号）。
    /// 默认忽略，显式运行：cargo test cloud_write_roundtrip -- --ignored --nocapture
    /// 流程：建目录 → 上传 → 元信息 → 下载比对 → 复制 → 重命名 → 列目录 → 清理。
    #[test]
    #[ignore]
    fn cloud_write_roundtrip_on_jianguoyun() {
        use super::super::tasks::TaskControl;
        use super::{
            cloud_copy_same, cloud_mkdir_full, cloud_move_same, cloud_stat, delete_cloud,
            download_from_cloud, list_cloud_dir_result, upload_to_cloud, CloudProgress,
        };
        use std::sync::Arc;

        let config = crate::config::AppConfig::load();
        let loc = config
            .network_locations
            .iter()
            .find(|l| l.kind == "webdav" && l.host.contains("jianguoyun"))
            .expect("配置中未找到坚果云账号（host 含 jianguoyun 的 webdav）");
        let root = loc.cloud_path();
        let test_dir = format!("{}/FileFilesOne自测", root.trim_end_matches('/'));
        let ctrl = Arc::new(TaskControl::new());

        // 0) 清理上次残留（失败忽略：可能本就不存在）
        let _ = delete_cloud(&format!("{}/sub", test_dir), &config);
        let _ = delete_cloud(&format!("{}/upload-me.txt", test_dir), &config);
        let _ = delete_cloud(&test_dir, &config);

        // 1) 建目录
        cloud_mkdir_full(&test_dir, &config).expect("MKCOL 失败");
        println!("✓ MKCOL {}", test_dir);

        // 2) 本地构造文件并上传：本地文件名即云端文件名（upload-me.txt）
        let local = std::env::temp_dir().join("upload-me.txt");
        std::fs::write(&local, b"FileFiles One jianguoyun write test\r\n").unwrap();
        let quiet2: CloudProgress = &mut |_, _| {};
        upload_to_cloud(&local, &test_dir, &config, &ctrl, quiet2).expect("PUT 上传失败");
        println!("✓ PUT upload-me.txt");

        // 3) 元信息：应为文件且大小一致
        let file_cloud = format!("{}/upload-me.txt", test_dir);
        let st = cloud_stat(&file_cloud, &config).expect("PROPFIND 元信息失败");
        assert!(!st.is_dir, "upload-me.txt 应判定为文件");
        assert_eq!(st.size, 37, "上传后大小应一致");
        println!("✓ PROPFIND is_dir={} size={}", st.is_dir, st.size);

        // 4) 下载并比对内容
        let dl_dir = std::env::temp_dir().join("ffone_jianguoyun_dl");
        std::fs::create_dir_all(&dl_dir).unwrap();
        let quiet3: CloudProgress = &mut |_, _| {};
        download_from_cloud(&file_cloud, &dl_dir, &config, &ctrl, quiet3)
            .expect("GET 下载失败");
        let got = std::fs::read(dl_dir.join("upload-me.txt")).unwrap();
        assert_eq!(got, b"FileFiles One jianguoyun write test\r\n", "下载内容应一致");
        println!("✓ GET 内容比对一致");

        // 5) 云内复制到子目录（COPY 保留原名）
        let sub = format!("{}/sub", test_dir);
        cloud_mkdir_full(&sub, &config).expect("MKCOL sub 失败");
        let cp: CloudProgress = &mut |_, _| {};
        cloud_copy_same(&file_cloud, &sub, &config, &ctrl, cp)
            .expect("COPY 失败（坚果云应支持服务端 COPY）");
        assert!(
            cloud_stat(&format!("{}/upload-me.txt", sub), &config).is_ok(),
            "复制后 sub/upload-me.txt 应存在"
        );
        println!("✓ COPY 服务端复制");

        // 6) 重命名（MOVE）
        let src_in_sub = format!("{}/upload-me.txt", sub);
        cloud_move_same(&src_in_sub, &sub, Some("renamed.txt"), &config)
            .expect("MOVE 重命名失败");
        assert!(cloud_stat(&format!("{}/renamed.txt", sub), &config).is_ok());
        println!("✓ MOVE 重命名");

        // 7) 列目录
        let entries = list_cloud_dir_result(&sub, &config.network_locations)
            .expect("PROPFIND 列目录失败");
        assert!(
            entries.iter().any(|e| e.name == "renamed.txt"),
            "列目录应看到 renamed.txt"
        );
        println!("✓ PROPFIND 列目录 {} 项", entries.len());

        // 8) 清理（坚果云服务端策略：一级目录 DELETE 返回 403，
        // 二级以下正常——测试目录属一级，残留可手动在坚果云客户端删除）
        delete_cloud(&sub, &config).expect("清理 sub 失败");
        if let Err(e) = delete_cloud(&test_dir, &config) {
            println!("⚠ 顶层测试目录删除被拒（坚果云一级目录 403，属服务端策略）：{}", e);
        }
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_dir_all(&dl_dir);
        println!("✓ 清理完成——坚果云写操作全链路通过");
    }
}
