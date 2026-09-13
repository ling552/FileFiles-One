//! 内嵌 rclone：WebDAV 挂载为虚拟磁盘 + SFTP 真机列表。
//!
//! 设计：
//! - rclone 二进制免手动安装：优先取程序目录 / %APPDATA%/FileFiles One/bin 下的
//!   rclone.exe，缺失时后台经 ureq 下载官方 current 包并解压（全程无终端窗口，
//!   见 fs::hidden::hidden_command，子进程一律 CREATE_NO_WINDOW + --no-console）。
//! - WebDAV 挂载：`rclone mount :webdav: <盘符>: --volname <显示名>`，凭据经环境变量
//!   传入（不在命令行暴露密码），挂载为固定磁盘后自动进入磁盘列表，
//!   图标与 D:/H: 等数据盘一致（见 thumbnail::IconRequest::DataDrive）。
//!   依赖 WinFsp；缺失时返回明确指引并回退 cloud:// 原生 PROPFIND 浏览。
//! - SFTP 列表：原生为占位 stub（需 libssh2/openssl，不引入），此处经
//!   `rclone lsjson :sftp:` 真机拉取，失败时透出 rclone 的 stderr 便于排查。

use crate::config::NetworkLocation;
use crate::fs::hidden::hidden_command;
use crate::fs::metadata::{classify, Entry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// rclone 挂载记录：显示名 ->（盘符，子进程 PID，日志路径）
struct MountInfo {
    drive: String,
    pid: u32,
    // 日志卸载后保留在临时目录供排查；路径暂未回读，仅为记录保留
    #[allow(dead_code)]
    log: PathBuf,
}

fn mounts() -> &'static Mutex<HashMap<String, MountInfo>> {
    static M: OnceLock<Mutex<HashMap<String, MountInfo>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 已挂载的盘符集合（供磁盘枚举去重/侧栏提示用，best-effort）
#[allow(dead_code)]
pub fn mounted_drives() -> Vec<String> {
    mounts()
        .lock()
        .map(|m| m.values().map(|v| v.drive.clone()).collect())
        .unwrap_or_default()
}

/// 该显示名是否已由本应用挂载
pub fn is_mounted_name(name: &str) -> Option<String> {
    mounts()
        .lock()
        .ok()?
        .get(name)
        .map(|v| v.drive.clone())
}

/// 正在挂载中的账户名集合（点击挂载后、成功/失败回调前）。
/// 用于设置页显示“挂载中...”禁用态，防止用户重复点击导致多进程抢盘符
/// （三次点击依旧显示挂载的根因之一：并发挂载互相杀进程）。
fn mounting() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// 标记开始挂载（UI 线程在 spawn 前调用，立即 push 可显示挂载中）
pub fn mark_mounting(name: &str) {
    if let Ok(mut m) = mounting().lock() {
        m.insert(name.to_string());
    }
}

/// 清除挂载中标记（成功/失败回调中调用，调用后需 push 刷新）
pub fn unmark_mounting(name: &str) {
    if let Ok(mut m) = mounting().lock() {
        m.remove(name);
    }
}

/// 是否正在挂载中
pub fn is_mounting(name: &str) -> bool {
    mounting().lock().map(|m| m.contains(name)).unwrap_or(false)
}

/// 用 `rclone obscure` 把明文密码转为 rclone 可接受的 obscured 形式。
/// 直接传明文会导致挂载瞬间失败：
/// `couldn't decrypt password: input too short when revealing password`，
/// 表现为“正在挂载后回到未挂载”。必须在后台线程调用（spawn 子进程等待）。
fn obscure_password(rclone: &Path, plain: &str) -> Result<String, String> {
    // 密码经 stdin 传入（`rclone obscure -`），不进命令行——Windows 上同用户
    // 的任意进程可经 WMI 读取子进程完整命令行，明文进 argv 即成泄露窗口
    let mut cmd = hidden_command(rclone);
    cmd.args(["obscure", "-"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("密码加密调用失败：{}", e))?;
    if let Some(mut si) = child.stdin.take() {
        use std::io::Write;
        // 写失败仅意味着 rclone 提前退出，结果由 wait_with_output 收割
        let _ = si.write_all(plain.as_bytes());
        let _ = si.write_all(b"\n");
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("密码加密调用失败：{}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "密码加密失败".to_string()
        } else {
            format!("密码加密失败：{}", err.chars().take(160).collect::<String>())
        });
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err("密码加密返回空".to_string());
    }
    Ok(s)
}

/// 读 rclone 日志尾部（失败时把 CRITICAL 首行透给状态栏，免去用户手动翻日志）
fn log_tail_short(log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else {
        return String::new();
    };
    // 取最后非空行，裁剪到 160 字符
    for line in text.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 去掉日期前缀，保留关键原因
        let short = t.chars().take(180).collect::<String>();
        return short;
    }
    String::new()
}

/// WebDAV vendor 自适应：坚果云（Nutstore）必须用 nutstore，否则 rclone 以 other
/// 握手/鉴权 quirks 不兼容会导致挂载超时（20s 未见盘符），而原生 PROPFIND 浏览正常，
/// 表现为“点三次挂载依旧是挂载”。
pub fn webdav_vendor_for(loc: &NetworkLocation) -> &'static str {
    let host = super::cloud::clean_host(&loc.host).to_ascii_lowercase();
    if host.contains("jianguoyun") || host.contains("nutstore") {
        "nutstore"
    } else {
        "other"
    }
}

// ── rclone 二进制定位与自动供给 ──

/// 程序目录（filefiles-one.exe 同级）
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// 用户级 bin 目录：%APPDATA%/FileFiles One/bin（免管理员可写）
fn user_bin_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("FileFiles One").join("bin"))
}

fn candidate_exes() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = exe_dir() {
        v.push(d.join("rclone.exe"));
    }
    if let Some(d) = user_bin_dir() {
        v.push(d.join("rclone.exe"));
    }
    // PATH 手工查找（不 spawn where.exe，避免终端闪现）
    if let Some(paths) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&paths) {
            let c = p.join("rclone.exe");
            v.push(c);
        }
    }
    v
}

/// 定位可用的 rclone.exe（不触发下载）
pub fn rclone_exe() -> Option<PathBuf> {
    candidate_exes().into_iter().find(|p| p.is_file())
}

/// 下载互斥：并发下载会交叉写同一目标文件，产生损坏的 rclone.exe
fn download_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// 确保 rclone 可用：缺失时后台下载官方 current 包并解压到用户 bin 目录。
/// 调用方必须在后台线程执行（阻塞网络最长数分钟，大包约 50MB）。
/// 全程无终端窗口（ureq 下载 + zip 解压，均为库内执行）。
pub fn ensure_rclone() -> Result<PathBuf, String> {
    if let Some(p) = rclone_exe() {
        return Ok(p);
    }
    // 等锁后双重检查：等锁期间其它线程可能已完成下载
    let _g = download_lock().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = rclone_exe() {
        return Ok(p);
    }
    let dir = user_bin_dir().ok_or("无法定位用户目录")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{}", e))?;
    // 官方 current 直链（Windows 64 位；ARM64 设备可用 x64 仿真运行，暂不分发 ARM 包）
    const URL: &str = "https://downloads.rclone.org/rclone-current-windows-amd64.zip";
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(600))
        .build();
    let resp = agent.get(URL).call().map_err(|e| format!("下载 rclone 失败：{}", e))?;
    if resp.status() >= 400 {
        return Err(format!("下载 rclone 失败（HTTP {}）", resp.status()));
    }
    let mut data = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(200 * 1024 * 1024)
        .read_to_end(&mut data)
        .map_err(|e| format!("读取 rclone 包失败：{}", e))?;
    // zip 内为 rclone-vX.Y.Z-windows-amd64/rclone.exe 单文件为主
    let cursor = std::io::Cursor::new(data);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|e| format!("解压 rclone 失败：{}", e))?;
    let mut out: Option<PathBuf> = None;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().replace('/', "\\");
        if !name.to_ascii_lowercase().ends_with("rclone.exe") || f.is_dir() {
            continue;
        }
        let dest = dir.join("rclone.exe");
        let mut buf = Vec::new();
        use std::io::Read as _;
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        // 先写临时名再原子改名：中断/并发时不会留下半个 rclone.exe
        let tmp = dir.join(format!("rclone.exe.{}.part", std::process::id()));
        std::fs::write(&tmp, &buf).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dest).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
        out = Some(dest);
        break;
    }
    out.ok_or_else(|| "rclone 压缩包内未找到 rclone.exe".to_string())
}

// ── WinFsp 检测（rclone mount Windows 必需）──

/// WinFsp 是否已安装：查驱动文件 + 注册表（任一命中即视为可用）
#[cfg(windows)]
pub fn winfsp_installed() -> bool {
    const DLL: [&str; 2] = [
        "C:\\Program Files\\WinFsp\\bin\\winfsp-x64.dll",
        "C:\\Program Files (x86)\\WinFsp\\bin\\winfsp-x64.dll",
    ];
    if DLL.iter().any(|p| Path::new(p).is_file()) {
        return true;
    }
    winreg_check_winfsp()
}

#[cfg(windows)]
fn winreg_check_winfsp() -> bool {
    use winreg::enums::*;
    use winreg::RegKey;
    // WinFsp 在 HKLM\SOFTWARE\WinFsp / WOW6432Node 下注册
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for sub in ["SOFTWARE\\WinFsp", "SOFTWARE\\WOW6432Node\\WinFsp"] {
            if RegKey::predef(root).open_subkey(sub).is_ok() {
                return true;
            }
        }
    }
    false
}

#[cfg(not(windows))]
pub fn winfsp_installed() -> bool {
    false
}

pub fn winfsp_download_url() -> &'static str {
    "https://winfsp.dev/rel/"
}

// ── 盘符分配（Z 向下，避开 A/B 软驱与 C 系统盘）──

/// 找一个空闲盘符，优先 Z→D（A/B 为历史软驱、C 为系统盘，均跳过）。
/// 无窗口：仅调 GetLogicalDrives 位图，不访问磁盘内容，不会卡住。
#[cfg(windows)]
pub fn find_free_drive_top() -> Option<char> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    for c in ('D'..='Z').rev() {
        let i = (c as u8 - b'A') as u32;
        if mask & (1 << i) == 0 {
            return Some(c);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn find_free_drive_top() -> Option<char> {
    None
}

/// 盘符是否已被系统占用
#[cfg(windows)]
pub fn drive_in_use(letter: char) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let up = letter.to_ascii_uppercase() as u8;
    if !(b'A'..=b'Z').contains(&up) {
        return true;
    }
    let mask = unsafe { GetLogicalDrives() };
    let i = (up - b'A') as u32;
    mask & (1 << i) != 0
}

#[cfg(not(windows))]
pub fn drive_in_use(_letter: char) -> bool {
    false
}

// ── WebDAV 挂载为虚拟磁盘 ──

/// 组装 `rclone mount` 完整参数（含子命令本身），纯函数便于单测。
/// 挂载设置映射：
/// - `mount_readonly` → `--read-only`（只读虚拟盘，禁止写入）
/// - `mount_max_size_gb` → `--vfs-disk-space-total-size <N>GB`（容量条按该值显示）
fn build_mount_args(loc: &NetworkLocation, drive: &str, log: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "mount".into(),
        ":webdav:".into(),
        drive.into(),
        "--volname".into(),
        loc.name.clone(),
        "--vfs-cache-mode".into(),
        "writes".into(),
        "--log-file".into(),
        log.into(),
        "--log-level".into(),
        "ERROR".into(),
        "--no-console".into(),
    ];
    if loc.mount_readonly {
        args.push("--read-only".into());
    }
    if let Some(gb) = loc.mount_max_size_gb {
        if gb > 0 {
            args.push("--vfs-disk-space-total-size".into());
            args.push(format!("{}GB", gb));
        }
    }
    args
}

/// 挂载设置对话框可选的盘符列表：D～Z 中未被占用的盘符，
/// 外加 `keep`（该账户当前已挂载的盘符，便于换设置时保留原盘符）。
#[cfg(windows)]
pub fn available_drive_letters(keep: Option<char>) -> Vec<char> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    let keep = keep.map(|c| c.to_ascii_uppercase());
    let mut v = Vec::new();
    for c in 'D'..='Z' {
        let i = (c as u8 - b'A') as u32;
        if mask & (1 << i) == 0 || Some(c) == keep {
            v.push(c);
        }
    }
    v
}

#[cfg(not(windows))]
pub fn available_drive_letters(_keep: Option<char>) -> Vec<char> {
    Vec::new()
}

/// 弹出系统文件选择对话框挑选挂载图标文件（.ico/.exe/.dll）。
/// 返回 `Ok(None)` 表示用户取消。必须在后台线程调用：
/// 对话框关闭前本调用同步阻塞（等待 PowerShell 子进程退出）。
/// WinForms OpenFileDialog 要求 STA 线程，故必须带 `-STA`；
/// 终端经 hidden_command 隐藏（CREATE_NO_WINDOW），不闪黑框。
#[cfg(windows)]
pub fn pick_icon_file() -> Result<Option<String>, String> {
    use std::process::Stdio;
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
$d = New-Object System.Windows.Forms.OpenFileDialog; \
$d.Filter = '图标文件|*.ico;*.exe;*.dll'; \
$d.Title = '选择挂载图标'; \
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Out.Write($d.FileName) }";
    let out = hidden_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("无法启动文件选择对话框：{}", e))?;
    // PowerShell 启动失败/崩溃时 stdout 为空，不检查退出码会误判为“用户取消”
    if !out.status.success() {
        return Err(format!(
            "无法打开文件选择对话框（PowerShell 退出码 {}）",
            out.status.code().unwrap_or(-1)
        ));
    }
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if picked.is_empty() {
        return Ok(None); // 用户取消
    }
    if !Path::new(&picked).is_file() {
        return Err("所选图标文件不存在".to_string());
    }
    Ok(Some(picked))
}

#[cfg(not(windows))]
pub fn pick_icon_file() -> Result<Option<String>, String> {
    Ok(None)
}

fn log_path_for(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c >= '\u{4e00}' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir()
        .join("FileFiles One")
        .join(format!("rclone-{}.log", safe))
}

fn webdav_origin(loc: &NetworkLocation) -> Result<String, String> {
    let (host, port, base) = super::cloud::split_host_port_base(loc)?;
    let scheme = if loc.use_tls { "https" } else { "http" };
    let default_port = if loc.use_tls { 443 } else { 80 };
    let url = if port == 0 || port == default_port {
        format!("{}://{}{}", scheme, host, base)
    } else {
        format!("{}://{}:{}{}", scheme, host, port, base)
    };
    Ok(url)
}

/// 挂载全程互斥锁：mount_webdav 入口 try_lock，并发调用快速失败
fn mount_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// 挂载 WebDAV 为虚拟磁盘（固定磁盘外观，图标与 D:/H: 一致）。
/// 成功返回盘符（如 "Z:"），失败返回中文原因（含 WinFsp/rclone 指引）。
/// 必须在后台线程调用：内部含 rclone 启动 + 最长 20 秒的盘符出现轮询。
/// 盘符取 `loc.mount_drive`（挂载设置对话框中的必填项），不再自动分配；
/// 只读/最大空间等参数见 `build_mount_args`。
pub fn mount_webdav(loc: &NetworkLocation) -> Result<String, String> {
    // 模块级防重入：UI 层 mounting 标记之外的兜底闸门，自动挂载与手动点击
    // 并发时后者快速失败，不再互抢盘符/互杀进程
    let _mount_guard = match mount_mutex().try_lock() {
        Ok(g) => g,
        Err(_) => return Err("已有挂载操作正在进行中，请稍候再试".to_string()),
    };
    // 已挂载且与设定盘符一致（盘符仍存在）则直接返回；否则清理残留记录后重挂
    let mut old_letter: Option<char> = None;
    if let Some(drive) = is_mounted_name(&loc.name) {
        let letter = drive.trim_end_matches(':').chars().next().unwrap_or('?');
        let want = loc
            .mount_drive
            .as_deref()
            .map(|s| s.trim().trim_end_matches(':').to_ascii_uppercase());
        let same = want.map_or(false, |w| {
            w.chars().next().map_or(false, |c| c.eq_ignore_ascii_case(&letter))
        });
        if same && drive_in_use(letter) {
            return Ok(drive);
        }
        let _ = unmount_by_name(&loc.name);
        old_letter = Some(letter);
    }
    if !winfsp_installed() {
        return Err(format!(
            "需要 WinFsp 才能把 WebDAV 挂载为虚拟磁盘（挂载后图标与 D:/H: 一致）。请先安装 WinFsp（{}），安装后重试；当前已回退为内置 WebDAV 浏览。",
            winfsp_download_url()
        ));
    }
    let rclone = match rclone_exe() {
        Some(p) => p,
        // 现场触发下载并透出失败原因：原先仅提示“后台下载中”，但调用方可能
        // 未触发下载（失败被吞），用户按提示重试永远不会成功
        None => ensure_rclone()?,
    };
    let drive_letter = loc
        .mount_drive
        .as_deref()
        .and_then(|s| s.trim().trim_end_matches(':').chars().next())
        .map(|c| c.to_ascii_uppercase())
        .ok_or_else(|| "尚未设置挂载盘符，请先在挂载设置中选择盘符".to_string())?;
    if !('D'..='Z').contains(&drive_letter) {
        return Err(format!("盘符 {} 无效（可用范围 D:～Z:）", drive_letter));
    }
    if drive_in_use(drive_letter) {
        // 换盘符/重挂场景：taskkill 后 WinFsp 回收旧盘符需要数百毫秒，短暂等待
        if old_letter == Some(drive_letter) {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) && drive_in_use(drive_letter) {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if drive_in_use(drive_letter) {
            return Err(format!(
                "盘符 {} 已被其它设备或程序占用，请在挂载设置中更换盘符",
                drive_letter
            ));
        }
    }
    let drive = format!("{}:", drive_letter);
    let origin = webdav_origin(loc)?;

    let log = log_path_for(&loc.name);
    if let Some(p) = log.parent() {
        let _ = std::fs::create_dir_all(p);
    }

    // 凭据经环境变量传入，不出现在命令行（任务管理器不可见密码）。
    // 注意：RCLONE_WEBDAV_PASS 必须传 obscured 后的值，直接传明文 rclone 会
    // `couldn't decrypt password ... is it obscured?` 瞬间退出，表现为
    // “正在挂载后回到未挂载”。此处经 `rclone obscure` 转换后再传入。
    let mut cmd = hidden_command(&rclone);
    cmd.env("RCLONE_WEBDAV_URL", &origin);
    cmd.env("RCLONE_WEBDAV_VENDOR", webdav_vendor_for(loc));
    if !loc.username.is_empty() {
        cmd.env("RCLONE_WEBDAV_USER", &loc.username);
    }
    if !loc.password.is_empty() {
        let obscured = obscure_password(&rclone, &loc.password)?;
        cmd.env("RCLONE_WEBDAV_PASS", obscured);
    }
    cmd.args(build_mount_args(loc, &drive, &log.to_string_lossy()));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Windows 下 rclone mount 为前台常驻进程：spawn 后不 wait，靠盘符轮询确认成功
    let child = cmd.spawn().map_err(|e| format!("启动 rclone 挂载失败：{}", e))?;
    let pid = child.id();
    // 有意泄漏 Child 句柄（挂载期间进程需常驻）：以 PID 跟踪，卸载时 taskkill
    std::mem::forget(child);

    // 轮询盘符出现（rclone + WinFsp 初始化约 2～8 秒），20 秒超时
    let start = Instant::now();
    let timeout = Duration::from_secs(20);
    let mut seen = false;
    while start.elapsed() < timeout {
        if drive_in_use(drive_letter) {
            // 再确认可列目录（WinFsp 挂载点刚出现时首列可能 ENOENT，短暂重试）
            if std::fs::read_dir(format!("{}\\", drive)).is_ok() {
                seen = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    if !seen {
        // 超时或瞬间退出：杀掉刚起的进程并把日志尾部透给状态栏，
        // 否则用户只看到“正在挂载后回到未挂载”，不知是密码/地址问题
        let _ = kill_pid(pid);
        let tail = log_tail_short(&log);
        if tail.contains("couldn't decrypt password") || tail.contains("is it obscured") {
            return Err(format!(
                "挂载失败：rclone 密码解密失败（{}），请重新保存密码后重试。日志 {}",
                tail.chars().take(120).collect::<String>(),
                log.to_string_lossy()
            ));
        }
        if !tail.is_empty() {
            return Err(format!(
                "rclone 挂载失败（20s 未见 {}）：{}。日志 {}",
                drive, tail, log.to_string_lossy()
            ));
        }
        return Err(format!(
            "rclone 挂载超时（20s 未见 {}），可能为地址/凭据错误或网络不通。详情见日志 {}",
            drive,
            log.to_string_lossy()
        ));
    }
    // 锁中毒时经 into_inner 恢复：盘符已挂上，记录丢失会导致之后无法卸载
    if let Ok(mut m) = mounts().lock().map_err(|e| e.into_inner()) {
        m.insert(
            loc.name.clone(),
            MountInfo {
                drive: drive.clone(),
                pid,
                log,
            },
        );
    }
    Ok(drive)
}

/// 按显示名卸载（kill rclone 进程 + 清理记录）。盘符由 WinFsp 自动回收。
pub fn unmount_by_name(name: &str) -> bool {
    let info = mounts().lock().ok().and_then(|mut m| m.remove(name));
    let Some(info) = info else { return false };
    if kill_pid(info.pid).is_err() {
        // taskkill 启动失败（非 kill 失败）：记录放回表中，保留稍后重试卸载的机会，
        // 否则 UI 显示已卸载而盘符仍挂着，且表中已无记录无法再次卸载
        if let Ok(mut m) = mounts().lock().map_err(|e| e.into_inner()) {
            m.insert(name.to_string(), info);
        }
        return false;
    }
    // 日志保留供排查，不删除
    true
}

/// 按盘符卸载（遍历挂载表匹配）
pub fn unmount_drive(drive: &str) -> bool {
    let name = mounts().lock().ok().and_then(|m| {
        m.iter()
            .find(|(_, v)| v.drive.eq_ignore_ascii_case(drive))
            .map(|(k, _)| k.clone())
    });
    name.map(|n| unmount_by_name(&n)).unwrap_or(false)
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> std::io::Result<()> {
    // taskkill 经 hidden_command，无终端闪现
    let _ = hidden_command("taskkill")
        // /FI 限定映像名：PID 是可复用资源，rclone 退出后 PID 被其它进程占用时，
        // 不带过滤的 taskkill 会误杀无关进程
        .args(["/F", "/FI", "IMAGENAME eq rclone.exe", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(not(windows))]
fn kill_pid(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

// ── SFTP 真机列表（rclone lsjson，无终端）──

/// 经 rclone 拉取 SFTP 目录（JSON），转为应用 Entry。
/// 必须在后台线程调用（spawn rclone 子进程并等待，最长 30 秒）。
pub fn list_sftp_via_rclone(loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let rclone = rclone_exe().ok_or_else(|| {
        "未找到 rclone，SFTP 需由 rclone 提供传输（原生为占位）。请联网后重试，后台将自动下载 rclone".to_string()
    })?;
    let sub = sub.trim_matches('/');
    // 环境变量传参，避免密码出现在命令行；PASS 必须为 obscured，见 obscure_password
    let mut cmd = hidden_command(&rclone);
    cmd.env("RCLONE_SFTP_HOST", super::cloud::clean_host(&loc.host));
    let port = super::cloud::effective_port_pub(loc);
    cmd.env("RCLONE_SFTP_PORT", port.to_string());
    if !loc.username.is_empty() {
        cmd.env("RCLONE_SFTP_USER", &loc.username);
    }
    if !loc.password.is_empty() {
        let obscured = obscure_password(&rclone, &loc.password)?;
        cmd.env("RCLONE_SFTP_PASS", obscured);
    }
    // 首次连接免交互：沿用系统 known_hosts，缺失时 rclone 会报错并指引，
    // 不静默跳过主机密钥校验（防中间人）。
    let base = super::cloud::remote_base_pub(loc);
    let full = if sub.is_empty() {
        base.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), sub)
    };
    let remote = if full.is_empty() || full == "/" {
        ":sftp:".to_string()
    } else {
        format!(":sftp:{}", full.trim_start_matches('/'))
    };
    cmd.args(["lsjson", &remote, "--log-level", "ERROR", "--no-console"]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 rclone 失败：{}", e))?;
    let pid = child.id();
    // 真实超时（最长 30 秒）：网络挂起时 lsjson 可能无限阻塞，须 kill 收尾。
    // wait_with_output 移入回收线程（读空管道防缓冲区写满死锁），主线程限时等待
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let out = loop {
        let now = Instant::now();
        if now >= deadline {
            let _ = kill_pid(pid);
            return Err("SFTP 列表超时（30s 无响应），已终止 rclone".to_string());
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Ok(out)) => break out,
            Ok(Err(e)) => return Err(format!("等待 rclone 失败：{}", e)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("rclone 列表线程异常退出".to_string());
            }
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // 裁剪 rclone 的冗长前缀，保留首个有效行
        let short = err.lines().next().unwrap_or("rclone 返回错误").to_string();
        let short = short.chars().take(220).collect::<String>();
        return Err(if short.is_empty() {
            format!("SFTP 连接失败（{}:{}）", loc.host, port)
        } else {
            format!("SFTP 连接失败：{}", short)
        });
    }
    parse_lsjson(&out.stdout, loc, sub)
}

/// 解析 `rclone lsjson` 数组为 Entry（目录优先交由调用方排序）。
fn parse_lsjson(data: &[u8], loc: &NetworkLocation, sub: &str) -> Result<Vec<Entry>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| format!("解析目录失败：{}", e))?;
    let arr = v.as_array().ok_or("目录格式异常")?;
    let prefix = format!("cloud://{}/{}", loc.kind, loc.name);
    let sub_prefix = if sub.is_empty() {
        String::new()
    } else {
        format!("/{}", sub.trim_matches('/'))
    };
    let base_path = format!("{}{}", prefix, sub_prefix);
    let mut entries = Vec::new();
    for item in arr {
        let name = item["Name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let is_dir = item["IsDir"].as_bool().unwrap_or(false);
        let size = item["Size"].as_u64().unwrap_or(0);
        let mtime = item["ModTime"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
        let (cls, lbl, kd) = if is_dir {
            ("folder".to_string(), "F".to_string(), "文件夹".to_string())
        } else {
            let (c, l, k) = classify(Path::new(&name), false);
            (c, l, k)
        };
        entries.push(Entry {
            name,
            path,
            is_dir,
            size_bytes: size,
            modified_ts: mtime,
            kind: kd,
            icon_label: lbl,
            icon_class: cls,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_table_tracks_by_name() {
        let name = "test-mount-table-unique";
        assert!(is_mounted_name(name).is_none());
    }

    #[test]
    fn free_drive_scan_never_returns_reserved() {
        // A/B/C 永不作为自动挂载盘符（历史软驱/系统盘）
        if let Some(c) = find_free_drive_top() {
            assert!(!matches!(c, 'A' | 'B' | 'C'));
        }
    }

    fn test_loc() -> crate::config::NetworkLocation {
        crate::config::NetworkLocation {
            name: "测试云盘".into(),
            server: String::new(),
            kind: "webdav".into(),
            drive: None,
            host: "dav.example.com".into(),
            port: 0,
            remote_path: "/".into(),
            username: String::new(),
            password: String::new(),
            use_tls: false,
            passive: true,
            mount_drive: Some("Z".into()),
            mount_readonly: false,
            mount_max_size_gb: None,
            mount_icon: String::new(),
        }
    }

    #[test]
    fn mount_args_reflect_mount_settings() {
        let mut loc = test_loc();
        // 默认（可写、不限制空间）：无只读与空间参数
        let base = build_mount_args(&loc, "Z:", "C:\\tmp\\x.log");
        assert!(!base.iter().any(|a| a == "--read-only"));
        assert!(!base.iter().any(|a| a == "--vfs-disk-space-total-size"));
        assert!(base.iter().any(|a| a == "mount"));
        assert!(base.iter().any(|a| a == "Z:"));

        // 只读 + 最大空间 500GB：参数成对追加，值带 GB 后缀
        loc.mount_readonly = true;
        loc.mount_max_size_gb = Some(500);
        let adv = build_mount_args(&loc, "Y:", "C:\\tmp\\x.log");
        assert!(adv.iter().any(|a| a == "--read-only"));
        let idx = adv.iter().position(|a| a == "--vfs-disk-space-total-size").unwrap();
        assert_eq!(adv[idx + 1], "500GB");
        // 0GB 视为不限制（不追加参数）
        loc.mount_max_size_gb = Some(0);
        assert!(!build_mount_args(&loc, "Y:", "C:\\tmp\\x.log")
            .iter()
            .any(|a| a == "--vfs-disk-space-total-size"));
    }

    #[test]
    fn available_letters_stay_in_d_to_z() {
        // 可选盘符恒在 D～Z（排除软驱 A/B 与系统盘 C 所在的低位段）
        assert!(available_drive_letters(None).iter().all(|c| ('D'..='Z').contains(c)));
        // keep 参数（当前挂载盘符）必须保留在列表中，供换设置时沿用
        assert!(available_drive_letters(Some('D')).contains(&'D'));
    }
}
