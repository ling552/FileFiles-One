//! 应用内更新：通过 GitHub Releases 检查新版本、带进度下载安装程序、启动安装。
//!
//! 设计要点：网络请求全部在后台线程执行（ureq 为阻塞式 API），进度通过
//! 回调闭包上报，由调用方（main.rs）在闭包内用 `slint::invoke_from_event_loop`
//! 回到主线程刷新 UI —— 与 `fs::tasks` 后台任务同一模式。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// GitHub 仓库标识（owner/name），检查更新与「关于」页链接共用
pub const REPO: &str = "ling552/FileFiles-One";
/// 仓库主页地址（「关于」页展示 + 浏览器打开）
pub const REPO_URL: &str = "https://github.com/ling552/FileFiles-One";
/// 当前应用版本（编译时取自 Cargo.toml）
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 最新 Release 的关键信息
#[derive(Clone)]
pub struct ReleaseInfo {
    /// 版本号（已去掉 tag 的 "v" 前缀），如 "0.2.0"
    pub version: String,
    /// 更新说明（Release 正文，可能为空）
    pub notes: String,
    /// 安装程序资产的直链下载地址
    pub asset_url: String,
    /// 安装程序文件名，如 "FileFiles-One-Setup-0.2.0.exe"
    pub asset_name: String,
    /// 安装程序大小（字节；API 提供，用于进度分母）
    pub asset_size: u64,
}

/// 解析 "x.y.z" 版本号为数字元组（缺段按 0，非数字段截断）
fn parse_ver(v: &str) -> (u64, u64, u64) {
    let mut it = v.trim().trim_start_matches(['v', 'V']).split('.').map(|s| {
        s.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// 语义化比较：latest 是否比 current 更新
pub fn is_newer(latest: &str, current: &str) -> bool {
    parse_ver(latest) > parse_ver(current)
}

/// 请求 GitHub 获取最新 Release。
/// 优先走 REST API；API 返回 403/429（未认证限流，共享出口 IP 时常见）时
/// 回退到网页跳转端点解析最新版本号，避免「检查更新」直接报错。
/// 返回 Err(用户可读的中文错误信息)。在后台线程调用（阻塞最长 ~30s）。
pub fn check_latest() -> Result<ReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        // GitHub API 要求 User-Agent，否则 403
        .set("User-Agent", "FileFiles-One-Updater")
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .call();
    let resp = match resp {
        Ok(r) => r,
        // 404：仓库尚无任何 Release
        Err(ureq::Error::Status(404, _)) => return Err("仓库暂无发布版本".to_string()),
        // 403/429：API 未认证限流，改走网页端点（不受 API 配额限制）
        Err(ureq::Error::Status(code, _)) if code == 403 || code == 429 => {
            return check_latest_via_web()
                .map_err(|e| format!("GitHub 接口限流（{code}），备用通道失败：{e}"));
        }
        Err(ureq::Error::Status(code, _)) => return Err(format!("GitHub 接口返回 {code}")),
        Err(ureq::Error::Transport(t)) => return Err(format!("网络请求失败：{t}")),
    };

    let json: serde_json::Value = resp.into_json().map_err(|e| format!("解析响应失败：{e}"))?;

    let tag = json["tag_name"].as_str().unwrap_or_default();
    if tag.is_empty() {
        return Err("响应中缺少版本号".to_string());
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let notes = json["body"].as_str().unwrap_or_default().trim().to_string();

    // 在资产列表中定位 Windows 安装程序（.exe；兼容打包成 zip 的情形）
    let assets = json["assets"].as_array().cloned().unwrap_or_default();
    let asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .map(|n| n.to_ascii_lowercase().ends_with(".exe"))
                .unwrap_or(false)
        })
        .or_else(|| assets.first())
        .ok_or_else(|| "该版本未附带安装程序".to_string())?;

    Ok(ReleaseInfo {
        version,
        notes,
        asset_url: asset["browser_download_url"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        asset_name: asset["name"].as_str().unwrap_or("update.exe").to_string(),
        asset_size: asset["size"].as_u64().unwrap_or(0),
    })
}

/// 备用检查通道：请求 `releases/latest` 网页端点（禁止自动重定向），
/// 从 302 跳转的 Location 头解析最新 tag，再按发布命名规则构造安装包直链。
/// 该端点不消耗 API 配额；缺点是拿不到更新说明与资产大小（下载时以
/// Content-Length 兜底显示进度）。
fn check_latest_via_web() -> Result<ReleaseInfo, String> {
    let agent = ureq::builder().redirects(0).build();
    let url = format!("https://github.com/{REPO}/releases/latest");
    let resp = agent
        .get(&url)
        .set("User-Agent", "FileFiles-One-Updater")
        .timeout(Duration::from_secs(15))
        .call();
    // redirects(0) 下 3xx 可能作为 Ok 或 Status 错误返回，两种都取响应
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) if (300..400).contains(&code) => r,
        Err(ureq::Error::Status(code, _)) => return Err(format!("网页端点返回 {code}")),
        Err(ureq::Error::Transport(t)) => return Err(format!("网络请求失败：{t}")),
    };
    let loc = resp.header("location").unwrap_or_default();
    // 形如 https://github.com/{repo}/releases/tag/v0.3.0
    let tag = loc
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    // tag 限白名单字符：跳转头来自网络响应，混入路径分隔符/特殊字符
    // 会进入后续构造的下载 URL
    let valid = !tag.is_empty()
        && tag.starts_with(['v', 'V'])
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid {
        return Err("无法从跳转地址解析版本号".to_string());
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let asset_name = format!("FileFiles-One-Setup-{version}.exe");
    Ok(ReleaseInfo {
        version,
        notes: String::new(),
        asset_url: format!("https://github.com/{REPO}/releases/download/{tag}/{asset_name}"),
        asset_name,
        asset_size: 0,
    })
}

/// 流式下载安装程序到系统临时目录，返回落盘路径。
///
/// `progress(已下载字节, 总字节, 瞬时速度 B/s)` 约每 150ms 回调一次
/// （另在开始与结束各回调一次）；`cancel` 置位后尽快中断并清理半成品。
/// 在后台线程调用。
pub fn download(
    info: &ReleaseInfo,
    cancel: &Arc<AtomicBool>,
    progress: impl Fn(u64, u64, f64),
) -> Result<PathBuf, String> {
    // 下载地址必须走 HTTPS（地址来自 API 响应 JSON，属不可信输入）
    if !info.asset_url.starts_with("https://") {
        return Err("下载地址无效".to_string());
    }
    let resp = ureq::get(&info.asset_url)
        .set("User-Agent", "FileFiles-One-Updater")
        .timeout(Duration::from_secs(3600))
        .call()
        .map_err(|e| format!("下载请求失败：{e}"))?;

    // 优先信任响应头的 Content-Length，缺失时退回 API 报告的资产大小
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(info.asset_size);

    // 资产名来自远端 JSON：只保留文件名部分并替换非法字符，防止
    // 路径分隔符把下载产物写到临时目录之外；附带进程号避免
    // 同机进程预置同名文件在下载与执行之间做替换
    let stem = Path::new(&info.asset_name)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.replace(['/', '\\', ':', '"', '<', '>', '|', '?', '*'], "_"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "FileFiles-One-Setup".to_string());
    let dest = std::env::temp_dir().join(format!("{stem}.{}.exe", std::process::id()));
    let mut file = File::create(&dest).map_err(|e| format!("创建临时文件失败：{e}"))?;

    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    // 速度按上报间隔内的增量计算，平滑且无需滑动窗口
    let mut last_report = Instant::now();
    let mut last_bytes: u64 = 0;
    progress(0, total, 0.0);

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&dest);
            return Err("已取消".to_string());
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&dest);
                return Err(format!("下载中断：{e}"));
            }
        };
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入失败：{e}"))?;
        downloaded += n as u64;

        let dt = last_report.elapsed();
        if dt >= Duration::from_millis(150) {
            let speed = (downloaded - last_bytes) as f64 / dt.as_secs_f64();
            progress(downloaded, total.max(downloaded), speed);
            last_report = Instant::now();
            last_bytes = downloaded;
        }
    }
    file.flush().map_err(|e| format!("写入失败：{e}"))?;
    progress(downloaded, downloaded.max(total), 0.0);
    Ok(dest)
}

/// 启动已下载的安装程序（分离进程）。成功后调用方应退出应用以便覆盖安装。
pub fn run_installer(path: &Path) -> Result<(), String> {
    std::process::Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动安装程序失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 版本号解析() {
        assert_eq!(parse_ver("0.1.0"), (0, 1, 0));
        assert_eq!(parse_ver("v1.2.3"), (1, 2, 3));
        assert_eq!(parse_ver("V2.0"), (2, 0, 0));
        assert_eq!(parse_ver("1.2.3-beta"), (1, 2, 3));
        assert_eq!(parse_ver(""), (0, 0, 0));
    }

    #[test]
    fn 版本比较() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }
}
