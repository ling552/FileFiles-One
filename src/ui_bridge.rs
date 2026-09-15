//! Rust ↔ Slint 桥接：把 AppCore 状态推送到 UI 模型

use crate::app::{AppCore, TabKind};
use crate::fs::{disk, metadata};
use crate::{
    AclAce, AppState, CertInfo, Crumb, CustomTagDef, FileEntry, MainWindow, MetaRow, NavItem,
    NetAccount, TabInfo,
};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 缩略图加载代数：每次重建 entries 模型自增，后台线程据此丢弃过期结果，
/// 避免快速切换目录时旧目录的缩略图错填到新目录的行上。
static THUMB_GEN: AtomicU64 = AtomicU64::new(0);

/// 右侧面板独立的缩略图代数（与左侧互不干扰）
static R_THUMB_GEN: AtomicU64 = AtomicU64::new(0);

/// Git 状态查询代数：每次左侧目录推送自增。libgit2 对大仓库的全工作区
/// status 可达数秒，必须放后台线程；回填前比对代数，目录已切换则丢弃。
static GIT_GEN: AtomicU64 = AtomicU64::new(0);

/// 缩略图回填目标面板：左侧主视图 entries / 右侧双面板 r_entries
#[derive(Clone, Copy, PartialEq)]
enum ThumbSide {
    Left,
    Right,
}

impl ThumbSide {
    fn generation(&self) -> &'static AtomicU64 {
        match self {
            ThumbSide::Left => &THUMB_GEN,
            ThumbSide::Right => &R_THUMB_GEN,
        }
    }
}

/// 缩略图请求边长。网格最大显示约 116px，128px 足够覆盖并将每张 RGBA 缓存
/// 从 256KiB 降至 64KiB，显著减少浏览图片/视频目录后的常驻内存。
const THUMB_SIZE: u32 = 128;

/// 判断扩展名是否为图片或视频（内置图标模式下仍为这两类显示真实缩略图）
fn is_media(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        // 图片
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff"
        // 视频
        | "mp4" | "mov" | "avi" | "mkv" | "wmv" | "flv" | "m4v" | "webm" | "mpg" | "mpeg"
    )
}

/// 根据设置与条目来源生成明确的图标请求。
/// `device://` 只使用类型/设备请求，不能进入真实路径 Shell 提取器。
/// `config` 用于 WebDAV 挂载图标三态路由：
/// 预设 → None（FileIcon 矢量字形）；自定义文件 → 无条件 RealPath 提取；
/// 默认 → 系统模式取数据盘图标（IconRequest::DataDrive），矢量模式 None。
fn icon_request_for_entry(
    e: &metadata::Entry,
    system_icons: bool,
    config: &crate::config::AppConfig,
) -> Option<crate::fs::thumbnail::IconRequest> {
    use crate::fs::thumbnail::IconRequest;

    if e.path.starts_with("device://") {
        if !system_icons {
            return None;
        }
        if e.icon_class == "device" {
            return Some(IconRequest::Device);
        }
        let extension = Path::new(&e.name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_string();
        return Some(IconRequest::Type {
            extension,
            is_dir: e.is_dir,
        });
    }
    if e.path.starts_with("cloud://") {
        // WebDAV 账户根（cloud://webdav/Name，无子路径）：按挂载图标三态路由，
        // 未挂载也生效（条目即虚拟磁盘预览，默认与 D:/H: 等数据盘同图标）。
        // 子项（cloud://webdav/Name/子路径）必须走正常文件/文件夹图标，
        // 不得复用数据盘图标，否则文件与文件夹分不清、全显示为硬盘图标。
        let is_root = crate::fs::cloud::is_cloud_root(&e.path);
        if is_root {
            if let Some(loc) = cloud_webdav_of(config, &e.path) {
                match loc.mount_icon_kind() {
                    // 预设：icon_class 已改写为 drive-<id>，由 FileIcon 矢量渲染
                    crate::config::MountIconKind::Preset(_) => return None,
                    // 自定义图标文件：两种图标模式都提取位图（用户显式选择）
                    crate::config::MountIconKind::File(p) => {
                        return Some(IconRequest::RealPath {
                            path: p,
                            is_dir: false,
                            mtime: 0,
                        });
                    }
                    // 默认：数据盘系统图标（与 D:/H: 同一张）；矢量模式走内置 drive 矢量
                    crate::config::MountIconKind::Default => {
                        return if system_icons {
                            Some(IconRequest::DataDrive)
                        } else {
                            None
                        };
                    }
                }
            }
            // 找不到账户的兜底：WebDAV 账户根仍取数据盘图标
            if e.icon_class == "drive" {
                return if system_icons {
                    Some(IconRequest::DataDrive)
                } else {
                    None
                };
            }
        }
        // 子项与非 WebDAV 云存储：内置模式走矢量（icon_class 已由 classify 正确分类，
        // 文件夹 folder、文件按扩展名），系统图标模式按类型取系统图标
        if !system_icons {
            return None;
        }
        let extension = Path::new(&e.name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_string();
        return Some(IconRequest::Type {
            extension,
            is_dir: e.is_dir,
        });
    }
    if crate::fs::virtualfs::is_virtual(&e.path) {
        return None;
    }
    // 挂载盘根（如 "Z:\"，WebDAV rclone 虚拟磁盘）：默认态强制数据盘图标
    // （不提取 WinFsp 卷自身图标，保证与 D:/H: 一致），预设/自定义按设置路由
    if e.icon_class == "drive" && is_drive_root_path(&e.path) {
        if let Some(loc) = webdav_of_drive(config, &e.path) {
            return match loc.mount_icon_kind() {
                crate::config::MountIconKind::Preset(_) => None, // icon_class 已被改写，防御分支
                crate::config::MountIconKind::File(p) => Some(IconRequest::RealPath {
                    path: p,
                    is_dir: false,
                    mtime: 0,
                }),
                crate::config::MountIconKind::Default => {
                    if system_icons {
                        Some(IconRequest::DataDrive)
                    } else {
                        None
                    }
                }
            };
        }
    }
    if system_icons || (!e.is_dir && is_media(&e.path)) {
        Some(IconRequest::RealPath {
            path: e.path.clone(),
            is_dir: e.is_dir,
            mtime: e.modified_ts,
        })
    } else {
        None
    }
}

/// 是否为驱动器根路径（如 "C:\"、"Z:/"）
fn is_drive_root_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// cloud:// 虚拟路径对应的 WebDAV 账户（仅 kind == webdav 时命中）
fn cloud_webdav_of<'a>(
    config: &'a crate::config::AppConfig,
    cloud_path: &str,
) -> Option<&'a crate::config::NetworkLocation> {
    let (kind, name, _) = crate::fs::cloud::parse_cloud_path(cloud_path)?;
    if kind != "webdav" {
        return None;
    }
    config
        .network_locations
        .iter()
        .find(|l| l.kind == kind && l.name == name)
}

/// 盘符根路径（如 "Z:\"）对应的 WebDAV 挂载配置
fn webdav_of_drive<'a>(
    config: &'a crate::config::AppConfig,
    drive_root: &str,
) -> Option<&'a crate::config::NetworkLocation> {
    let letter = drive_root.chars().next()?;
    config.network_locations.iter().find(|l| {
        l.kind == "webdav"
            && l.drive
                .as_deref()
                .and_then(|d| d.chars().next())
                .map(|c| c.to_ascii_uppercase() == letter.to_ascii_uppercase())
                .unwrap_or(false)
    })
}

/// 由缓存的图标像素构建 Slint 图像（必须在 UI 线程调用）。
pub(crate) fn image_from(ic: &crate::fs::thumbnail::IconPixels) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(ic.w, ic.h);
    buf.make_mut_bytes().copy_from_slice(&ic.pixels);
    Image::from_rgba8(buf)
}

// ── 图标像素 → Slint Image 共享缓存（按 Arc 指针键）──
// 同一图标（同类型/同路径/同 Stock）在全目录只保留一份像素缓冲，
// N 行共享 1 份 Image，避免大目录下每行复制 64KB 缓冲导致内存暴涨
// （500 项目录从 ~32MB 降到 ~几 MB）。Image Clone 共享底层缓冲，零拷贝。
thread_local! {
    static ICON_IMAGE_CACHE: RefCell<HashMap<usize, (Arc<crate::fs::thumbnail::IconPixels>, Image)>> =
        RefCell::new(HashMap::new());
}

/// 取共享的 Slint 图像（同一 IconPixels 实例全进程共享一份缓冲）。
/// 必须在 UI 线程调用；缓存持有对应 Arc 防止地址复用误命中。
pub(crate) fn image_cached(ic: &Arc<crate::fs::thumbnail::IconPixels>) -> Image {
    let key = Arc::as_ptr(ic) as usize;
    if let Some(img) = ICON_IMAGE_CACHE.with(|c| c.borrow().get(&key).map(|(_, i)| i.clone())) {
        return img;
    }
    let img = image_from(ic);
    ICON_IMAGE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        // 上限防无限增长：达 128 项整体清空重建，图标种类通常远小于该值
        if c.len() >= 128 {
            c.clear();
        }
        c.insert(key, (ic.clone(), img.clone()));
    });
    img
}

/// 清空图标像素 → Slint 图像共享缓存（必须在 UI 线程调用）。
/// 图标缓存失效（更改默认应用/切换图标来源）后同步调用，否则旧像素的 Arc
/// 被此处持有，旧图标继续显示且内存不释放。
pub(crate) fn clear_icon_image_cache() {
    ICON_IMAGE_CACHE.with(|c| c.borrow_mut().clear());
}

// ── 选中状态影子副本：refresh 系列据此只回填变化的行 ──
// 旧实现对全模型逐行 row_data()（整行克隆，含 7 个字符串与图像句柄），
// 数千行目录下每次单击/框选都会产生数 MB 的分配抖动与可感延迟。
thread_local! {
    static PUSHED_SELECTION: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    static R_PUSHED_SELECTION: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
}

/// 把选中布尔表同步到条目模型：仅对与影子副本不一致的行做 row_data/set_row_data。
/// `shadow` 记录上次已推送的选中状态，必须在 push_entries/push_right 重建模型后
/// 同步为当时烘焙进行的选中值。
fn sync_selection_to_model(
    model: &slint::ModelRc<FileEntry>,
    selected: &[bool],
    shadow: &mut Vec<bool>,
) {
    let rows = model.row_count();
    for fi in 0..rows {
        let sel = selected.get(fi).copied().unwrap_or(false);
        if shadow.get(fi).copied() != Some(sel) {
            if let Some(mut row) = model.row_data(fi) {
                row.selected = sel;
                model.set_row_data(fi, row);
            }
        }
    }
    shadow.clear();
    shadow.extend_from_slice(&selected[..rows.min(selected.len())]);
    shadow.resize(rows, false);
}

/// 由选中布尔表构建「选中下标」模型（网格选中卡片覆盖层的数据源）
fn selected_indices_model(selected: &[bool]) -> slint::ModelRc<i32> {
    let idx: Vec<i32> = selected
        .iter()
        .enumerate()
        .filter(|(_, &s)| s)
        .map(|(i, _)| i as i32)
        .collect();
    slint::ModelRc::new(slint::VecModel::from(idx))
}

// ── 侧栏图标异步加载：build_sidebar 只读缓存不阻塞 UI；未命中项记录到 ──
// 待加载集合，构建结束后由后台线程提取，完成后回事件循环重建侧栏补上。
thread_local! {
    static SIDEBAR_UI_WEAK: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
    static SIDEBAR_PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn sidebar_attempted() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 注册主窗口弱引用，供侧栏图标后台加载完成后触发重建（main 中调用一次）
pub fn init_sidebar_warm(weak: slint::Weak<MainWindow>) {
    SIDEBAR_UI_WEAK.with(|w| *w.borrow_mut() = Some(weak));
}

/// 记录侧栏缺失的图标（每进程每项只尝试一次，防失败重试循环）
fn note_sidebar_icon_missing(key: String) {
    if sidebar_attempted()
        .lock()
        .map(|mut s| s.insert(key.clone()))
        .unwrap_or(false)
    {
        SIDEBAR_PENDING.with(|p| p.borrow_mut().push(key));
    }
}

/// build_sidebar 结束时调用：有待加载项则派后台线程提取，完成后重建侧栏
fn flush_sidebar_warm() {
    let keys: Vec<String> = SIDEBAR_PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if keys.is_empty() {
        return;
    }
    let Some(weak) = SIDEBAR_UI_WEAK.with(|w| w.borrow().clone()) else {
        return;
    };
    std::thread::spawn(move || {
        for key in &keys {
            if let Some(path) = key.strip_prefix("special:") {
                if let Some(arc) = crate::fs::thumbnail::special_dir_icon_cached(path, 128) {
                    crate::fs::thumbnail::sidebar_icon_set(path, arc);
                }
            } else if key == "device:" {
                if let Some(arc) = crate::fs::thumbnail::load_cached_request(
                    &crate::fs::thumbnail::IconRequest::Device,
                    128,
                ) {
                    crate::fs::thumbnail::sidebar_icon_set("__device__", arc);
                }
            } else if key == "datadrive:" {
                // WebDAV 挂载盘默认态：与 D:/H: 同一张数据盘系统图标
                if let Some(arc) = crate::fs::thumbnail::load_cached_request(
                    &crate::fs::thumbnail::IconRequest::DataDrive,
                    128,
                ) {
                    crate::fs::thumbnail::sidebar_icon_set("__datadrive__", arc);
                }
            } else if let Some(path) = key.strip_prefix("iconfile:") {
                // 挂载图标自定义文件（.ico/.exe/.dll）：is_dir=false 提取文件自带图标
                if let Some(arc) = crate::fs::thumbnail::load_cached(path, false, 0, 128) {
                    crate::fs::thumbnail::sidebar_icon_set(path, arc);
                }
            } else if let Some(path) = key.strip_prefix("path:") {
                if let Some(arc) = crate::fs::thumbnail::load_cached(path, true, 0, 128) {
                    crate::fs::thumbnail::sidebar_icon_set(path, arc);
                }
            }
        }
        let _ = weak.upgrade_in_event_loop(|ui| {
            ui.global::<AppState>().invoke_devices_changed();
        });
    });
}

/// 为侧边栏条目提取系统图标（仅在 icon-source=系统图标 时调用）。
/// 只读缓存（不阻塞 UI 线程）：命中即显示系统图标；未命中回退内置 glyph、
/// 并记录到待加载集合，由后台线程提取后重建侧栏补上。
fn sidebar_icon(path: &str, is_dir: bool) -> (Image, bool) {
    if crate::fs::virtualfs::is_virtual(path) {
        return (Image::default(), false);
    }
    // 优先读永不淘汰的侧栏专用缓存，避免随主缓存清理回退为内置图标
    if let Some(ic) = crate::fs::thumbnail::sidebar_icon_get(path) {
        return (image_cached(&ic), true);
    }
    match crate::fs::thumbnail::cached(path, is_dir, 0) {
        Some(ic) => {
            crate::fs::thumbnail::sidebar_icon_set(path, ic.clone());
            (image_cached(&ic), true)
        }
        None => {
            note_sidebar_icon_missing(format!("path:{}", path));
            (Image::default(), false)
        }
    }
}

/// 侧栏数据盘图标（WebDAV 挂载盘默认态，与 D:/H: 同一张系统图标）：
/// 只读缓存不阻塞 UI，未命中记录待加载键由后台线程提取后重建侧栏
fn sidebar_data_drive() -> (Image, bool) {
    if let Some(ic) = crate::fs::thumbnail::sidebar_icon_get("__datadrive__") {
        return (image_cached(&ic), true);
    }
    match crate::fs::thumbnail::cached_request(&crate::fs::thumbnail::IconRequest::DataDrive) {
        Some(ic) => {
            crate::fs::thumbnail::sidebar_icon_set("__datadrive__", ic.clone());
            (image_cached(&ic), true)
        }
        None => {
            note_sidebar_icon_missing("datadrive:".into());
            (Image::default(), false)
        }
    }
}

/// 侧栏自定义图标文件位图（挂载图标 "file:<路径>"）：
/// 只读缓存不阻塞 UI，未命中记录待加载键由后台线程提取（is_dir=false）
fn sidebar_icon_file(path: &str) -> (Image, bool) {
    if crate::fs::virtualfs::is_virtual(path) {
        return (Image::default(), false);
    }
    if let Some(ic) = crate::fs::thumbnail::sidebar_icon_get(path) {
        return (image_cached(&ic), true);
    }
    match crate::fs::thumbnail::cached(path, false, 0) {
        Some(ic) => {
            crate::fs::thumbnail::sidebar_icon_set(path, ic.clone());
            (image_cached(&ic), true)
        }
        None => {
            note_sidebar_icon_missing(format!("iconfile:{}", path));
            (Image::default(), false)
        }
    }
}

/// 为"此电脑"平铺视图构建驱动器容量字段：(已用比例, 副标题, 容量条颜色)。
/// 非"此电脑"视图或非驱动器条目返回 (0.0, 空, 透明)，平铺视图据此不绘制容量条。
fn disk_fields(
    e: &metadata::Entry,
    this_pc: bool,
    disks: &[disk::DiskInfo],
) -> (f32, SharedString, slint::Brush) {
    let transparent = slint::Brush::SolidColor(slint::Color::from_argb_u8(0, 0, 0, 0));
    if !this_pc || e.icon_class != "drive" {
        return (0.0, SharedString::new(), transparent);
    }
    match disks.iter().find(|d| d.root == e.path) {
        Some(d) => {
            let ratio = d.used_ratio();
            let info = format!(
                "可用 {} / 共 {}",
                metadata::human_size(d.free),
                metadata::human_size(d.total)
            );
            // 接近写满（>=90%）用红色提示，否则用主题蓝
            let color = if ratio >= 0.9 {
                slint::Color::from_rgb_u8(0xe5, 0x39, 0x35)
            } else {
                slint::Color::from_rgb_u8(0x00, 0x78, 0xd4)
            };
            (ratio, info.into(), slint::Brush::SolidColor(color))
        }
        None => (0.0, SharedString::new(), transparent),
    }
}

/// 异步图标加载任务：(行下标, 明确的图标请求)
type IconJob = (usize, crate::fs::thumbnail::IconRequest);

/// 把当前目录条目推送到 UI（entries / crumbs / 标题 / 状态栏）
/// 解析 "#rrggbb" / "#aarrggbb" 为 Slint Color
fn hex_to_color(hex: &str) -> slint::Color {
    let default = slint::Color::from_rgb_u8(0, 120, 212);
    let h = hex.trim_start_matches('#');
    // 按字节切片前先验证全为 ASCII 十六进制字符：配置可被篡改，
    // 含多字节字符且总长恰为 6/8 时字节边界切片会 panic
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return default;
    }
    let b = h.as_bytes();
    let parse = |s: &[u8]| u8::from_str_radix(std::str::from_utf8(s).unwrap_or("0"), 16).unwrap_or(0);
    match b.len() {
        6 => slint::Color::from_rgb_u8(parse(&b[0..2]), parse(&b[2..4]), parse(&b[4..6])),
        8 => slint::Color::from_argb_u8(
            parse(&b[0..2]),
            parse(&b[2..4]),
            parse(&b[4..6]),
            parse(&b[6..8]),
        ),
        _ => default,
    }
}

/// 某路径所含自定义标签的颜色列表（按 custom_tags 定义顺序），用于渲染额外彩色圆点
fn custom_tag_colors(config: &crate::config::AppConfig, path: &str) -> ModelRc<slint::Brush> {
    let colors: Vec<slint::Brush> = config
        .custom_tags
        .iter()
        .filter(|t| config.has_tag(path, &crate::config::AppConfig::custom_tag_key(&t.id)))
        .map(|t| slint::Brush::SolidColor(hex_to_color(&t.color)))
        .collect();
    ModelRc::new(VecModel::from(colors))
}

pub fn push_entries(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let tab = core.active_tab();

    // 图标来源："system" 全部文件取系统图标/缩略图；"builtin" 仅图片/视频取真实缩略图，其余用矢量图
    let icon_source = core.config.settings.icon_source.clone();
    let system_icons = icon_source == "system";

    // "此电脑"平铺视图：预取一次驱动器容量信息，用于绘制每个盘符的容量条
    let cur_is_this_pc =
        tab.history.current().to_string_lossy() == crate::fs::virtualfs::THIS_PC_PATH;
    let disks: Vec<disk::DiskInfo> = if cur_is_this_pc {
        disk::list_disks()
    } else {
        Vec::new()
    };

    // 先同步查图标缓存：命中的条目首帧即显示系统图标（无闪烁），未命中的留待异步加载。
    let icon_requests: Vec<Option<crate::fs::thumbnail::IconRequest>> = tab
        .filtered
        .iter()
        .map(|&ei| icon_request_for_entry(&tab.entries[ei], system_icons, &core.config))
        .collect();
    let cached_icons: Vec<Option<std::sync::Arc<crate::fs::thumbnail::IconPixels>>> = icon_requests
        .iter()
        .map(|request| {
            request
                .as_ref()
                .and_then(crate::fs::thumbnail::cached_request)
        })
        .collect();

    // Git 状态改为后台线程计算（libgit2 对大仓库的 status 扫描可达数秒，
    // 同步执行会让「打开文件夹」明显卡顿）。这里先以空状态推送条目，
    // 结果就绪后按代数校验逐行回填徽章与分支名。
    let cur_path = tab.history.current().clone();

    // 文件条目
    let rows: Vec<FileEntry> = tab
        .filtered
        .iter()
        .enumerate()
        .map(|(fi, &ei)| {
            let e = &tab.entries[ei];
            // 缓存命中：直接构建图像预填，否则留空由后台线程回填
            let (thumb, has_thumb) = match &cached_icons[fi] {
                Some(ic) => (image_cached(ic), true),
                None => (Image::default(), false),
            };
            let (disk_ratio, disk_info, disk_color) = disk_fields(e, cur_is_this_pc, &disks);
            FileEntry {
                name: e.name.clone().into(),
                path: e.path.clone().into(),
                is_dir: e.is_dir,
                size: metadata::human_size(e.size_bytes).into(),
                size_bytes: e.size_bytes as i32,
                modified: metadata::fmt_ts_label(e.modified_ts).into(),
                modified_ts: e.modified_ts as i32,
                kind: e.kind.clone().into(),
                icon_label: e.icon_label.clone().into(),
                icon_class: e.icon_class.clone().into(),
                selected: tab.selected.get(fi).copied().unwrap_or(false),
                tag_important: core.config.has_tag(&e.path, "important"),
                tag_archive: core.config.has_tag(&e.path, "archive"),
                tag_done: core.config.has_tag(&e.path, "done"),
                custom_tag_colors: custom_tag_colors(&core.config, &e.path),
                // Git 徽章由后台线程计算完成后回填（见 spawn_git_status）
                git_status: SharedString::new(),
                thumb,
                has_thumb,
                has_files: false,
                disk_ratio,
                disk_info,
                disk_color,
            }
        })
        .collect();

    // 仅收集"需要图标且缓存未命中"的条目交给后台异步加载。
    let mut jobs: Vec<IconJob> = Vec::new();
    for (fi, request) in icon_requests.into_iter().enumerate() {
        if let Some(request) = request {
            if cached_icons[fi].is_none() {
                jobs.push((fi, request));
            }
        }
    }

    state.set_entries(ModelRc::new(VecModel::from(rows)));
    // 模型重建已烘焙选中值，同步影子副本；并推送选中下标供网格覆盖层使用
    PUSHED_SELECTION.with(|shadow| {
        let mut s = shadow.borrow_mut();
        s.clear();
        s.extend_from_slice(&tab.selected);
    });
    state.set_selected_indices(selected_indices_model(&tab.selected));
    // 选中计数：网格选中卡片据此决定是否展开完整名称（仅单选展开）
    state.set_selected_count(
        core.active_tab().selected.iter().filter(|&&s| s).count() as i32,
    );

    // 导航到新目录后重置列表滚动位置到顶部（各列表视图监听此 token 变化归零 viewport-y）
    state.set_scroll_top_token(state.get_scroll_top_token() + 1);

    // 每次重建模型自增代数，后台线程据此丢弃过期目录的结果
    let generation = THUMB_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if !jobs.is_empty() {
        spawn_thumbnails(ui, jobs, generation, ThumbSide::Left);
    }
    // 内置图标模式：后台检测各文件夹是否含子项，回填 has-files 以显示「有文件的文件夹」图标
    if !system_icons {
        let dirs: Vec<(usize, String)> = tab
            .filtered
            .iter()
            .enumerate()
            .filter_map(|(fi, &ei)| {
                let e = &tab.entries[ei];
                if e.is_dir && !crate::fs::virtualfs::is_virtual(&e.path) {
                    Some((fi, e.path.clone()))
                } else {
                    None
                }
            })
            .collect();
        spawn_folder_hasfiles(ui, dirs, generation, ThumbSide::Left);
    }

    // 面包屑与标题：设置页固定显示「设置」（地址 setting）；虚拟路径使用友好名称
    let cur = tab.history.current();
    let cur_str = cur.to_string_lossy().to_string();
    if tab.kind == TabKind::Settings {
        state.set_current_vpath("setting".into());
        let crumbs = vec![Crumb {
            name: "设置".into(),
            path: "setting".into(),
        }];
        state.set_crumbs(ModelRc::new(VecModel::from(crumbs)));
        state.set_current_title("设置".into());
        state.set_current_subtitle("应用设置".into());
    } else if crate::fs::virtualfs::is_virtual(&cur_str) {
        state.set_current_vpath(cur_str.clone().into());
        let title = crate::fs::virtualfs::friendly_title(&cur_str);
        let crumbs = vec![Crumb {
            name: title.clone().into(),
            path: cur_str.clone().into(),
        }];
        state.set_crumbs(ModelRc::new(VecModel::from(crumbs)));
        state.set_current_title(title.into());
        let subtitle = format!("{} 个项目", tab.entries.len());
        state.set_current_subtitle(subtitle.into());
    } else {
        state.set_current_vpath(cur_str.clone().into());
        let crumbs = build_crumbs(cur);
        state.set_crumbs(ModelRc::new(VecModel::from(crumbs)));

        let title = cur
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cur.to_string_lossy().to_string());
        state.set_current_title(title.into());

        let dir_count = tab.entries.iter().filter(|e| e.is_dir).count();
        let file_count = tab.entries.len() - dir_count;
        let subtitle = if tab.search.is_empty() {
            format!(
                "{} 个项目 ({} 个文件夹，{} 个文件)",
                tab.entries.len(),
                dir_count,
                file_count
            )
        } else {
            format!(
                "过滤结果：{} / {} 个项目",
                tab.filtered.len(),
                tab.entries.len()
            )
        };
        state.set_current_subtitle(subtitle.into());
    }

    // 导航可用性
    state.set_can_back(tab.history.can_back());
    state.set_can_forward(tab.history.can_forward());

    // 排序状态
    state.set_sort_key(tab.sort_key.clone().into());
    state.set_sort_asc(tab.sort_asc);

    // Git 分支先清空（避免残留上一个目录的分支芯片），后台计算就绪后回填
    state.set_git_branch(SharedString::new());
    spawn_git_status(ui, cur_path);

    // 状态栏与详情：按活动面板同步 sel-*（双面板右面板活动时不能用左面板
    // 选中状态覆盖「sel-* = 最近交互面板」语义——如 watcher 软刷新左目录时）
    update_status(ui, core);
    let right_active = state.get_dual_pane() && state.get_active_pane() == "right";
    update_selection_pane(ui, core, right_active);
}

/// 后台异步加载缩略图/系统图标，批量回填到 entries 模型。
///
/// 设计（两阶段 + 批量，降低“占位图标 → 正确图标”的等待感）：
/// - 类型图标（扩展名/文件夹/设备）先行：按类型共享，一次提取全目录同类行
///   受益，通常百毫秒内完成首绘；
/// - 具体文件缩略图（图片/视频/exe）随后：仍串行提取（COM 单线程），但每攒
///   一批统一回一次事件循环，避免每行一次跨线程往返的开销；
/// - 回到 UI 线程后先比对 generation：与当前全局代数不一致说明目录已切换，
///   直接丢弃，避免旧目录缩略图错填到新目录。
fn spawn_thumbnails(ui: &MainWindow, jobs: Vec<IconJob>, generation: u64, side: ThumbSide) {
    // 类型任务优先：同类型共享缓存，首次提取后同类行均为缓存命中
    let (type_jobs, path_jobs): (Vec<IconJob>, Vec<IconJob>) = jobs
        .into_iter()
        .partition(|(_, req)| crate::fs::thumbnail::request_is_shared_type(req));
    let weak = ui.as_weak();
    let gen_ref = side.generation();
    std::thread::spawn(move || {
        // 批量回 UI 线程把图标写回对应行（代数校验避免旧目录图标填到新目录）
        let flush = |batch: Vec<(usize, std::sync::Arc<crate::fs::thumbnail::IconPixels>)>| {
            if batch.is_empty() {
                return;
            }
            if gen_ref.load(Ordering::SeqCst) != generation {
                return;
            }
            let weak2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if gen_ref.load(Ordering::SeqCst) != generation {
                    return;
                }
                let Some(ui) = weak2.upgrade() else { return };
                let state = ui.global::<AppState>();
                let model = match side {
                    ThumbSide::Left => state.get_entries(),
                    ThumbSide::Right => state.get_r_entries(),
                };
                let mut sel_img: Option<Image> = None;
                for (row, icon) in &batch {
                    if let Some(mut entry) = model.row_data(*row) {
                        let img = image_cached(icon);
                        entry.thumb = img.clone();
                        entry.has_thumb = true;
                        let is_selected = entry.selected;
                        model.set_row_data(*row, entry);
                        // 左侧：若该行正是当前选中项，同步刷新右侧详情面板预览
                        if side == ThumbSide::Left && is_selected {
                            sel_img = Some(img);
                        }
                    }
                }
                if let Some(img) = sel_img {
                    state.set_sel_thumb(img);
                    state.set_sel_has_thumb(true);
                }
            });
        };

        // 阶段一：类型图标（快，批量 16 行一刷，首绘最快）
        let mut batch: Vec<(usize, std::sync::Arc<crate::fs::thumbnail::IconPixels>)> = Vec::new();
        let mut failed: Vec<IconJob> = Vec::new();
        for (row, request) in type_jobs {
            if gen_ref.load(Ordering::SeqCst) != generation {
                return;
            }
            // 走带缓存的入口：命中同类型缓存即零提取
            match crate::fs::thumbnail::load_cached_request(&request, THUMB_SIZE) {
                Some(icon) => {
                    batch.push((row, icon));
                    if batch.len() >= 16 {
                        flush(std::mem::take(&mut batch));
                    }
                }
                // 提取失败（Shell/COM 未就绪、杀软占用等瞬时错误）：记下稍后重试
                None => failed.push((row, request)),
            }
        }
        flush(std::mem::take(&mut batch));

        // 阶段二：具体文件缩略图（慢，批量 4 行一刷，渐进呈现）
        for (row, request) in path_jobs {
            // 已切换目录：提前结束本批，省去无谓的图标提取
            if gen_ref.load(Ordering::SeqCst) != generation {
                return;
            }
            match crate::fs::thumbnail::load_cached_request(&request, THUMB_SIZE) {
                Some(icon) => {
                    batch.push((row, icon));
                    if batch.len() >= 4 {
                        flush(std::mem::take(&mut batch));
                    }
                }
                None => failed.push((row, request)),
            }
        }
        flush(std::mem::take(&mut batch));
        // 失败行延迟重试一次：瞬时失败通常数百毫秒内恢复，
        // 避免个别文件停留在内置矢量图（重试仍失败则由 stock 回退兼底）
        if !failed.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(600));
            for (row, request) in failed {
                if gen_ref.load(Ordering::SeqCst) != generation {
                    return;
                }
                if let Some(icon) = crate::fs::thumbnail::load_cached_request(&request, THUMB_SIZE)
                {
                    batch.push((row, icon));
                    if batch.len() >= 8 {
                        flush(std::mem::take(&mut batch));
                    }
                }
            }
            flush(std::mem::take(&mut batch));
        }
    });
}

/// 后台检测各文件夹是否含子项，仅当含子项时回填 has-files=true（内置图标模式
/// 用于显示「有文件的文件夹」图标）。逐项 read_dir 只取首项，开销极小；
/// 代数校验避免旧目录结果填到新目录。
fn spawn_folder_hasfiles(
    ui: &MainWindow,
    dirs: Vec<(usize, String)>,
    generation: u64,
    side: ThumbSide,
) {
    if dirs.is_empty() {
        return;
    }
    let weak = ui.as_weak();
    let gen_ref = side.generation();
    std::thread::spawn(move || {
        for (row, path) in dirs {
            if gen_ref.load(Ordering::SeqCst) != generation {
                return;
            }
            let has = std::fs::read_dir(&path)
                .map(|mut rd| rd.next().is_some())
                .unwrap_or(false);
            if !has {
                continue;
            }
            let weak2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if gen_ref.load(Ordering::SeqCst) != generation {
                    return;
                }
                let Some(ui) = weak2.upgrade() else { return };
                let state = ui.global::<AppState>();
                let model = match side {
                    ThumbSide::Left => state.get_entries(),
                    ThumbSide::Right => state.get_r_entries(),
                };
                if let Some(mut entry) = model.row_data(row) {
                    if !entry.has_files {
                        entry.has_files = true;
                        model.set_row_data(row, entry);
                    }
                }
            });
        }
    });
}

/// 后台计算当前目录的 Git 分支与工作区状态，就绪后回填到左侧 entries 模型。
///
/// libgit2 的全工作区 status 在大仓库上可达数秒，绝不能在 UI 线程同步执行。
/// 每次调用自增 GIT_GEN；工作线程完成后回到 UI 线程比对代数——目录已再次
/// 切换（代数不匹配）则整批丢弃，避免旧目录的徽章错填到新目录。
fn spawn_git_status(ui: &MainWindow, dir: PathBuf) {
    let generation = GIT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if crate::fs::virtualfs::is_virtual(&dir.to_string_lossy()) {
        return;
    }
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        // 启动前再校验一次：快速连续导航时跳过已过期的扫描，避免线程堆积
        if GIT_GEN.load(Ordering::SeqCst) != generation {
            return;
        }
        let info = crate::git::status_for_dir(&dir);
        let _ = slint::invoke_from_event_loop(move || {
            if GIT_GEN.load(Ordering::SeqCst) != generation {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(info) = info else {
                return; // 非 Git 仓库：分支芯片保持隐藏、徽章保持空
            };
            let state = ui.global::<AppState>();
            state.set_git_branch(info.branch.clone().into());
            let model = state.get_entries();
            for fi in 0..model.row_count() {
                if let Some(mut row) = model.row_data(fi) {
                    let s = info.status_of(&row.path, row.is_dir);
                    if row.git_status != s.as_str() {
                        row.git_status = s.into();
                        model.set_row_data(fi, row);
                    }
                }
            }
        });
    });
}

/// 把右侧独立面板（双面板视图）的状态推送到 UI 的 r-* 属性。
/// 与左侧一致：按图标来源设置异步加载系统图标/缩略图，回填到 r_entries 模型。
pub fn push_right(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let tab = &core.right_pane;

    // 图标来源与左侧保持一致
    let system_icons = core.config.settings.icon_source == "system";

    // 先同步查缓存预填（与 push_entries 同逻辑）
    let icon_requests: Vec<Option<crate::fs::thumbnail::IconRequest>> = tab
        .filtered
        .iter()
        .map(|&ei| icon_request_for_entry(&tab.entries[ei], system_icons, &core.config))
        .collect();
    let cached_icons: Vec<Option<std::sync::Arc<crate::fs::thumbnail::IconPixels>>> = icon_requests
        .iter()
        .map(|request| {
            request
                .as_ref()
                .and_then(crate::fs::thumbnail::cached_request)
        })
        .collect();

    let rows: Vec<FileEntry> = tab
        .filtered
        .iter()
        .enumerate()
        .map(|(fi, &ei)| {
            let e = &tab.entries[ei];
            let (thumb, has_thumb) = match &cached_icons[fi] {
                Some(ic) => (image_cached(ic), true),
                None => (Image::default(), false),
            };
            FileEntry {
                name: e.name.clone().into(),
                path: e.path.clone().into(),
                is_dir: e.is_dir,
                size: metadata::human_size(e.size_bytes).into(),
                size_bytes: e.size_bytes as i32,
                modified: metadata::fmt_ts_label(e.modified_ts).into(),
                modified_ts: e.modified_ts as i32,
                kind: e.kind.clone().into(),
                icon_label: e.icon_label.clone().into(),
                icon_class: e.icon_class.clone().into(),
                selected: tab.selected.get(fi).copied().unwrap_or(false),
                tag_important: core.config.has_tag(&e.path, "important"),
                tag_archive: core.config.has_tag(&e.path, "archive"),
                tag_done: core.config.has_tag(&e.path, "done"),
                custom_tag_colors: custom_tag_colors(&core.config, &e.path),
                // 右面板暂不展示 Git 徽章
                git_status: SharedString::new(),
                thumb,
                has_thumb,
                has_files: false,
                // 右面板不展示"此电脑"平铺视图，容量字段恒为默认
                disk_ratio: 0.0,
                disk_info: SharedString::new(),
                disk_color: slint::Brush::SolidColor(slint::Color::from_argb_u8(0, 0, 0, 0)),
            }
        })
        .collect();

    // 仅收集缓存未命中的条目交给后台异步加载
    let mut jobs: Vec<IconJob> = Vec::new();
    for (fi, request) in icon_requests.into_iter().enumerate() {
        if let Some(request) = request {
            if cached_icons[fi].is_none() {
                jobs.push((fi, request));
            }
        }
    }

    state.set_r_entries(ModelRc::new(VecModel::from(rows)));
    // 模型重建已烘焙选中值，同步影子副本；并推送选中下标供右面板网格覆盖层使用
    R_PUSHED_SELECTION.with(|shadow| {
        let mut s = shadow.borrow_mut();
        s.clear();
        s.extend_from_slice(&tab.selected);
    });
    state.set_r_selected_indices(selected_indices_model(&tab.selected));
    state.set_r_selected_count(tab.selected.iter().filter(|&&s| s).count() as i32);

    // 导航到新目录后重置右面板滚动位置到顶部
    state.set_r_scroll_top_token(state.get_r_scroll_top_token() + 1);

    let generation = R_THUMB_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if !jobs.is_empty() {
        spawn_thumbnails(ui, jobs, generation, ThumbSide::Right);
    }
    if !system_icons {
        let dirs: Vec<(usize, String)> = tab
            .filtered
            .iter()
            .enumerate()
            .filter_map(|(fi, &ei)| {
                let e = &tab.entries[ei];
                if e.is_dir && !crate::fs::virtualfs::is_virtual(&e.path) {
                    Some((fi, e.path.clone()))
                } else {
                    None
                }
            })
            .collect();
        spawn_folder_hasfiles(ui, dirs, generation, ThumbSide::Right);
    }

    let cur = tab.history.current();
    let cur_str = cur.to_string_lossy().to_string();
    state.set_r_current_vpath(cur_str.clone().into());

    // 右侧面包屑（供双面板共用工具栏地址栏显示）
    let crumbs = build_crumbs(cur);
    state.set_r_crumbs(ModelRc::new(VecModel::from(crumbs)));
    let title = cur
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cur_str.clone());
    state.set_r_current_title(title.into());

    let dir_count = tab.entries.iter().filter(|e| e.is_dir).count();
    let file_count = tab.entries.len() - dir_count;
    state.set_r_current_subtitle(
        format!(
            "{} 个项目 ({} 个文件夹，{} 个文件)",
            tab.entries.len(),
            dir_count,
            file_count
        )
        .into(),
    );
    state.set_r_can_back(tab.history.can_back());
    state.set_r_can_forward(tab.history.can_forward());
}

/// 仅就地更新右侧面板条目模型的选中标记（保持双击连续触发，不重建模型）
pub fn refresh_right_selection(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let model = state.get_r_entries();
    let tab = &core.right_pane;
    R_PUSHED_SELECTION.with(|shadow| {
        sync_selection_to_model(&model, &tab.selected, &mut shadow.borrow_mut());
    });
    state.set_r_selected_count(tab.selected.iter().filter(|&&s| s).count() as i32);
    state.set_r_selected_indices(selected_indices_model(&tab.selected));
}

/// 构建面包屑链
fn build_crumbs(path: &Path) -> Vec<Crumb> {
    let mut crumbs = Vec::new();
    let mut acc = PathBuf::new();
    let mut comps = path.components().peekable();
    while let Some(comp) = comps.next() {
        acc.push(comp.as_os_str());
        let name = match comp {
            std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().to_string(),
            std::path::Component::RootDir => continue,
            other => other.as_os_str().to_string_lossy().to_string(),
        };
        let crumb_path = if acc.components().count() == 1 {
            let mut p = acc.clone();
            p.push("\\");
            p
        } else {
            acc.clone()
        };
        crumbs.push(Crumb {
            name: name.into(),
            path: crumb_path.to_string_lossy().to_string().into(),
        });
    }
    crumbs
}

/// 更新状态栏文本
pub fn update_status(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let tab = core.active_tab();
    let sel = tab.selected.iter().filter(|&&s| s).count();
    let total = tab.entries.len();
    let cur = tab.history.current();

    // 只查询当前路径所在的单个驱动器：全盘枚举（GetVolumeInformationW 等）
    // 在存在休眠硬盘/失联网络驱动器时可能阻塞数秒，而状态栏每次刷新都会走到这里
    let mut disk_part = String::new();
    if let Some(letter) = cur.to_string_lossy().chars().next() {
        if letter.is_ascii_alphabetic() {
            if let Some(d) = disk::cached_disk_info_of(letter) {
                disk_part = format!(" · {} 剩余 {}", d.name, metadata::human_size(d.free));
            }
        }
    }

    let text = if sel > 0 {
        format!("已选择 {} 项，共 {} 项{}", sel, total, disk_part)
    } else {
        // 无选中时在左下角展示文件夹 / 文件数量明细
        let dir_count = tab.entries.iter().filter(|e| e.is_dir).count();
        let file_count = total - dir_count;
        if tab.search.is_empty() {
            format!(
                "共 {} 项 ({} 个文件夹，{} 个文件){}",
                total, dir_count, file_count, disk_part
            )
        } else {
            format!(
                "过滤结果：{} / {} 项{}",
                tab.filtered.len(),
                total,
                disk_part
            )
        }
    };
    state.set_status_text(text.into());
}

/// 更新详情面板与选中信息（活动标签 = 左面板）
pub fn update_selection(ui: &MainWindow, core: &AppCore) {
    update_selection_pane(ui, core, false);
}

/// 更新详情面板与选中信息，按面板取数：`right` 为真时数据源为右侧独立面板。
/// sel-* 全局属性语义为「最近交互面板的选中项」，属性对话框 / 详情栏 /
/// ActionBar「解压」按钮可见性均由此驱动。
pub fn update_selection_pane(ui: &MainWindow, core: &AppCore, right: bool) {
    let state = ui.global::<AppState>();
    let tab = core.pane(right);
    let sel_idx: Vec<usize> = tab
        .selected
        .iter()
        .enumerate()
        .filter(|(_, &s)| s)
        .map(|(i, _)| i)
        .collect();
    // 选中数量：驱动 ActionBar 按钮可用性（复制/剪切/删除等需 >0，重命名需 ==1）
    state.set_sel_count(sel_idx.len() as i32);

    // 工具栏「标记」下拉对勾：选中项是否「全部」含某标签（空选中则为否）。
    let all_tag = |key: &str| {
        !sel_idx.is_empty()
            && sel_idx.iter().all(|&fi| {
                tab.entry_at(fi)
                    .map(|e| core.config.has_tag(&e.path, key))
                    .unwrap_or(false)
            })
    };
    state.set_sel_tag_important(all_tag("important"));
    state.set_sel_tag_archive(all_tag("archive"));
    state.set_sel_tag_done(all_tag("done"));
    // 自定义标签对勾：按 custom_tags 顺序，每项表示选中项是否「全部」含该标签
    let custom_bools: Vec<bool> = core
        .config
        .custom_tags
        .iter()
        .map(|t| all_tag(&crate::config::AppConfig::custom_tag_key(&t.id)))
        .collect();
    state.set_sel_custom_tag_bools(ModelRc::new(VecModel::from(custom_bools)));

    if sel_idx.len() == 1 {
        if let Some(e) = tab.entry_at(sel_idx[0]) {
            state.set_has_selection(true);
            state.set_sel_is_archive(crate::fs::operations::is_zip_archive(Path::new(&e.path)));
            state.set_sel_is_dir(e.is_dir);
            // 切换选中项时重置文件夹大小计算状态：新文件夹需重新点「计算」
            state.set_sel_size_calculating(false);
            state.set_sel_path(e.path.clone().into());
            // 单选 Office 文档时预热：停留约 1 秒后后台把文档转成 PDF 缓存，
            // 按空格预览即命中缓存秒开（否则每次都要等 Office 冷启动 10~20 秒）。
            // 预览已打开时不预热 —— 预览自身已在后台转换，重复投递只会空等锁。
            if !e.is_dir && !state.get_quicklook_open() {
                crate::fs::office_preview::request_warmup(Path::new(&e.path));
            }
            // 设置开启「计算文件夹大小」时选中即自动后台统计（大文件夹期间显示"计算中"）
            if e.is_dir && core.config.settings.calc_folder_size {
                state.invoke_calculate_folder_size();
            }
            state.set_sel_name(e.name.clone().into());
            state.set_sel_kind(e.kind.clone().into());
            let loc = Path::new(&e.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            state.set_sel_location(loc.into());
            state.set_sel_size(
                if e.is_dir {
                    "—".to_string()
                } else {
                    format!(
                        "{} ({} 字节)",
                        metadata::human_size(e.size_bytes),
                        e.size_bytes
                    )
                }
                .into(),
            );
            state.set_sel_modified(metadata::fmt_ts_full(e.modified_ts).into());
            // 慢属性只在打开「属性」对话框时惰性读取，普通单击不访问磁盘/ACL/注册表。
            state.set_sel_created("—".into());
            state.set_sel_open_with(SharedString::new());
            state.set_sel_owner("—".into());
            state.set_sel_attributes("普通".into());
            state.set_sel_readonly(false);
            state.set_sel_acl_entries(ModelRc::new(VecModel::from(Vec::<AclAce>::new())));
            state.set_sel_sign_status(0);
            state.set_sel_sign_detail(SharedString::new());
            state.set_sel_cert_chain(ModelRc::new(VecModel::from(Vec::<CertInfo>::new())));
            state.set_sel_icon_label(e.icon_label.clone().into());
            state.set_sel_icon_class(e.icon_class.clone().into());
            // 从对应面板的条目模型读取该行已生成的缩略图/系统图标位图，填充右侧详情预览
            let model = if right {
                state.get_r_entries()
            } else {
                state.get_entries()
            };
            if let Some(row) = model.row_data(sel_idx[0]) {
                state.set_sel_has_thumb(row.has_thumb);
                state.set_sel_thumb(row.thumb.clone());
            } else {
                state.set_sel_has_thumb(false);
            }
            return;
        }
    }
    state.set_has_selection(false);
    state.set_sel_is_archive(false);
    state.set_sel_is_dir(false);
    state.set_sel_size_calculating(false);
    state.set_sel_has_thumb(false);
}

/// 仅就地更新条目模型的选中标记，避免重建 entries 模型导致
/// `for` 元素被销毁、双击事件无法连续触发的问题。
pub fn refresh_selection(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let model = state.get_entries();
    let tab = core.active_tab();
    PUSHED_SELECTION.with(|shadow| {
        sync_selection_to_model(&model, &tab.selected, &mut shadow.borrow_mut());
    });
    state.set_selected_count(tab.selected.iter().filter(|&&s| s).count() as i32);
    // 网格选中卡片覆盖层按选中下标渲染（O(选中数) 而非 O(条目数)）
    state.set_selected_indices(selected_indices_model(&tab.selected));
    update_status(ui, core);
    update_selection(ui, core);
}

/// 构建侧边栏导航项（真实磁盘 + 系统目录 + 标签）
/// `collapsed` 为当前已折叠的分区标签集合，折叠分区下的条目不再生成。
/// `config` 提供标签计数与高亮状态。
pub fn build_sidebar(
    active_path: &Path,
    collapsed: &HashSet<String>,
    config: &crate::config::AppConfig,
) -> ModelRc<NavItem> {
    let mut items: Vec<NavItem> = Vec::new();
    let active_str = active_path.to_string_lossy().to_string();

    // 当前所在分区是否被折叠（随 header 推进而更新）
    let mut section_collapsed;

    // 图标来源：system=系统图标（侧栏也读取系统图标），builtin=内置矢量/字符图标
    let system_icons = config.settings.icon_source == "system";

    let mk = |label: &str, path: String, icon: &str, badge: &str, active: bool| {
        let (thumb, has_thumb) = if system_icons {
            sidebar_icon(&path, true)
        } else {
            (Image::default(), false)
        };
        NavItem {
            label: label.into(),
            path: path.into(),
            icon: icon.into(),
            icon_class: "".into(),
            badge: badge.into(),
            is_header: false,
            is_disk: false,
            disk_ratio: 0.0,
            disk_info: "".into(),
            disk_color: slint::Brush::SolidColor(slint::Color::from_rgb_u8(0, 120, 212)),
            active,
            collapsed: false,
            is_tree: false,
            thumb,
            has_thumb: has_thumb,
        }
    };
    let header = |label: &str, is_collapsed: bool| NavItem {
        label: label.into(),
        path: "".into(),
        icon: "".into(),
        icon_class: "".into(),
        badge: "".into(),
        is_header: true,
        is_disk: false,
        disk_ratio: 0.0,
        disk_info: "".into(),
        disk_color: slint::Brush::default(),
        active: false,
        collapsed: is_collapsed,
        is_tree: false,
        thumb: Image::default(),
        has_thumb: false,
    };

    // 快速访问：优先读取系统真实数据（与资源管理器一致，含固定文件夹 + 常用文件夹）
    section_collapsed = collapsed.contains("快速访问");
    items.push(header("快速访问", section_collapsed));
    if !section_collapsed {
        // 已知系统文件夹 → 专属 MDL2 图标（Segoe MDL2 Assets 字体渲染）；
        // 用户固定的普通文件夹保持原有图标（系统位图 / 首字符色块）
        let known: Vec<(String, &str)> = [
            (dirs::desktop_dir(), "\u{E7F8}"),  // 桌面（显示器）
            (dirs::download_dir(), "\u{E896}"), // 下载
            (dirs::document_dir(), "\u{E8A5}"), // 文档
            (dirs::picture_dir(), "\u{EB9F}"),  // 图片
            (dirs::audio_dir(), "\u{E8D6}"),    // 音乐
            (dirs::video_dir(), "\u{E714}"),    // 视频
        ]
        .into_iter()
        .filter_map(|(p, g)| {
            p.map(|p| (p.to_string_lossy().trim_end_matches('\\').to_lowercase(), g))
        })
        .collect();
        // 已知系统文件夹的图标策略：
        // - 系统图标模式：按路径提取 Shell 专属图标（桌面/下载/文档等在系统中带标识），
        //   与资源管理器显示一致；提取失败回退 MDL2 glyph
        // - 内置图标模式：MDL2 字体 glyph（现有内置样式）
        let glyphize = |mut item: NavItem, glyph: &str| {
            if system_icons {
                // 优先侧栏专用缓存，其次特殊目录缓存；命中即写入专用缓存持久化
                let ic = crate::fs::thumbnail::sidebar_icon_get(item.path.as_str()).or_else(|| {
                    crate::fs::thumbnail::special_dir_icon_cache_only(item.path.as_str())
                        .map(|a| {
                            crate::fs::thumbnail::sidebar_icon_set(item.path.as_str(), a.clone());
                            a
                        })
                });
                if let Some(ic) = ic {
                    item.thumb = image_cached(&ic);
                    item.has_thumb = true;
                    return item;
                }
                // 未命中：记录待后台提取，完成后重建侧栏补上系统图标
                note_sidebar_icon_missing(format!("special:{}", item.path.as_str()));
            }
            item.icon = glyph.into();
            item.icon_class = "qa-glyph".into();
            item.has_thumb = false;
            item
        };
        let qa = crate::fs::quickaccess::list();
        if qa.is_empty() {
            // 回退：系统标准目录（非 Windows 或读取失败时仍可用）
            let dirs = [
                ("桌面", dirs::desktop_dir(), "\u{E7F8}"),
                ("下载", dirs::download_dir(), "\u{E896}"),
                ("文档", dirs::document_dir(), "\u{E8A5}"),
                ("图片", dirs::picture_dir(), "\u{EB9F}"),
                ("音乐", dirs::audio_dir(), "\u{E8D6}"),
                ("视频", dirs::video_dir(), "\u{E714}"),
            ];
            if let Some(home) = dirs::home_dir() {
                let hs = home.to_string_lossy().to_string();
                let active = active_str == hs;
                items.push(glyphize(mk("主目录", hs, "", "", active), "\u{E80F}"));
            }
            for (label, dir, glyph) in dirs.iter() {
                if let Some(p) = dir {
                    let ps = p.to_string_lossy().to_string();
                    let active = active_str == ps;
                    items.push(glyphize(mk(label, ps, "", "", active), glyph));
                }
            }
        } else {
            for it in qa {
                let active = active_str == it.path;
                let hit = known
                    .iter()
                    .find(|(p, _)| *p == it.path.trim_end_matches('\\').to_lowercase())
                    .map(|(_, g)| *g);
                if let Some(glyph) = hit {
                    items.push(glyphize(
                        mk(&it.name, it.path.clone(), "", "", active),
                        glyph,
                    ));
                } else {
                    // 图标取显示名首字符（中文文件夹名友好）
                    let icon = it
                        .name
                        .chars()
                        .next()
                        .map(|c| c.to_string())
                        .unwrap_or_default();
                    items.push(mk(&it.name, it.path.clone(), &icon, "", active));
                }
            }
        }
    }

    // 此电脑：可点击导航的树节点（点文字进入"此电脑"界面，点 chevron 展开/折叠），
    // 展开后列出所有驱动器（含使用率进度条）与已识别的便携设备。
    section_collapsed = collapsed.contains("此电脑");
    let this_pc_active = active_str == crate::fs::virtualfs::THIS_PC_PATH;
    items.push(NavItem {
        label: "此电脑".into(),
        path: crate::fs::virtualfs::THIS_PC_PATH.into(),
        icon: "电".into(),
        icon_class: "".into(),
        badge: "".into(),
        is_header: false,
        is_disk: false,
        disk_ratio: 0.0,
        disk_info: "".into(),
        disk_color: slint::Brush::SolidColor(slint::Color::from_rgb_u8(0, 120, 212)),
        active: this_pc_active,
        collapsed: section_collapsed,
        is_tree: true,
        thumb: Image::default(),
        has_thumb: false,
    });
    if !section_collapsed {
        // 驱动器：用真实容量构建带使用率进度条的磁盘条目
        for disk in crate::fs::disk::cached_disks() {
            let ratio = disk.used_ratio();
            // 与资源管理器一致的副标题："X 可用，共 Y"
            let info = if disk.total > 0 {
                format!(
                    "{} 可用，共 {}",
                    metadata::human_size(disk.free),
                    metadata::human_size(disk.total)
                )
            } else {
                String::new()
            };
            // 使用率 >90% 标红警示，否则蓝色
            let bar = if ratio > 0.9 {
                slint::Color::from_rgb_u8(0xd1, 0x34, 0x38)
            } else {
                slint::Color::from_rgb_u8(0x00, 0x78, 0xd4)
            };
            // 挂载盘（WebDAV rclone 虚拟磁盘）：按挂载图标设置路由——
            // 预设 → drive-<id> 矢量字形；自定义文件 → 提取位图；
            // 默认 → 数据盘图标（与 D:/H: 一致，不提取 WinFsp 卷自身图标）。
            // 普通盘保持原策略：系统模式提取真实盘符图标，矢量模式内置 drive 矢量。
            let mount_loc = config.network_locations.iter().find(|l| {
                l.kind == "webdav"
                    && l.drive
                        .as_deref()
                        .and_then(|d| d.chars().next())
                        .map(|c| {
                            disk.letter
                                .chars()
                                .next()
                                .map(|dl| c.to_ascii_uppercase() == dl.to_ascii_uppercase())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
            });
            let (disk_class, disk_thumb, disk_has_thumb) =
                match mount_loc.map(|l| l.mount_icon_kind()) {
                    Some(crate::config::MountIconKind::Preset(id)) => (
                        format!("drive-{}", id),
                        Image::default(),
                        false,
                    ),
                    Some(crate::config::MountIconKind::File(p)) => {
                        let (t, h) = sidebar_icon_file(&p);
                        ("drive".to_string(), t, h)
                    }
                    _ => {
                        if system_icons {
                            if mount_loc.is_some() {
                                let (t, h) = sidebar_data_drive();
                                ("drive".to_string(), t, h)
                            } else {
                                let (t, h) = sidebar_icon(&disk.root, true);
                                ("drive".to_string(), t, h)
                            }
                        } else {
                            ("drive".to_string(), Image::default(), false)
                        }
                    }
                };
            items.push(NavItem {
                label: disk.name.clone().into(),
                path: disk.root.clone().into(),
                icon: disk.letter.clone().into(),
                icon_class: disk_class.into(),
                badge: "".into(),
                is_header: false,
                is_disk: true,
                disk_ratio: ratio,
                disk_info: info.into(),
                disk_color: slint::Brush::SolidColor(bar),
                active: active_str == disk.root,
                collapsed: false,
                is_tree: false,
                thumb: disk_thumb,
                has_thumb: disk_has_thumb,
            });
        }
        // 便携设备（手机 / 平板等）：同步显示在"此电脑"下
        for dev in crate::fs::devices::cached_devices() {
            let (device_thumb, device_has_thumb) = if system_icons {
                // 优先侧栏专用缓存；未命中回退内置图标并记录，由后台线程提取后重建补上
                if let Some(ic) = crate::fs::thumbnail::sidebar_icon_get("__device__") {
                    (image_cached(&ic), true)
                } else if let Some(icon) = crate::fs::thumbnail::cached_request(
                    &crate::fs::thumbnail::IconRequest::Device,
                ) {
                    crate::fs::thumbnail::sidebar_icon_set("__device__", icon.clone());
                    (image_cached(&icon), true)
                } else {
                    note_sidebar_icon_missing("device:".into());
                    (Image::default(), false)
                }
            } else {
                (Image::default(), false)
            };
            items.push(NavItem {
                label: dev.name.clone().into(),
                path: dev.path.clone().into(),
                icon: "机".into(),
                icon_class: "device".into(),
                badge: "".into(),
                is_header: false,
                is_disk: false,
                disk_ratio: 0.0,
                disk_info: dev.kind.clone().into(),
                disk_color: slint::Brush::SolidColor(slint::Color::from_rgb_u8(0x2a, 0x9d, 0x8f)),
                active: active_str == dev.path,
                collapsed: false,
                is_tree: false,
                thumb: device_thumb,
                has_thumb: device_has_thumb,
            });
        }
        // 云存储（FTP/WebDAV/SFTP）：在“此电脑”展开时一并列出，便于直达。
        // 已挂载为虚拟磁盘的 WebDAV 不再单列（真实盘符 Z:\ 已在上方磁盘区，
        // 与 D:/H: 一样带容量条显示，避免同一账户出现磁盘 + 云两项重复）。
        // 未挂载图标统一：WebDAV=与 D:/H: 等非 Windows 磁盘同款数据盘图标；
        // FTP/SFTP=网格地球（Globe）。SMB 未挂载时不进侧栏（cloud://smb 不可浏览），
        // 其挂载后以盘符形式出现在上方磁盘区。
        for loc in &config.network_locations {
            if !matches!(loc.kind.as_str(), "ftp" | "webdav" | "sftp") {
                continue;
            }
            if crate::fs::cloud::is_webdav_mounted(loc) {
                continue;
            }
            let vpath = loc.cloud_path();
            // 侧栏图标分支说明（sidebar.slint）：has_thumb=位图；icon_class=="qa-glyph"
            // =MDL2 字形；其余=「首字符色块」。FTP/SFTP 必须走 qa-glyph 分支才能画出
            // 地球形，落空会变成空白色块；WebDAV 统一取数据盘位图（与 D:/H: 同源），
            // 位图未就绪时回退云端字形并登记后台加载
            let (glyph, class, cloud_thumb, cloud_has_thumb) = if loc.kind == "webdav" {
                let (t, h) = sidebar_data_drive();
                if h {
                    (String::new(), String::new(), t, h)
                } else {
                    ("\u{E753}".to_string(), "qa-glyph".to_string(), Image::default(), false)
                }
            } else {
                // FTP / SFTP：网格地球（Globe）
                ("\u{E774}".to_string(), "qa-glyph".to_string(), Image::default(), false)
            };
            items.push(NavItem {
                label: loc.name.clone().into(),
                path: vpath.clone().into(),
                icon: glyph.into(),
                icon_class: class.into(),
                badge: "".into(),
                is_header: false,
                is_disk: false,
                disk_ratio: 0.0,
                disk_info: loc.display_server().into(),
                disk_color: slint::Brush::SolidColor(slint::Color::from_rgb_u8(0x00, 0x78, 0xd4)),
                active: active_str == vpath,
                collapsed: false,
                is_tree: false,
                thumb: cloud_thumb,
                has_thumb: cloud_has_thumb,
            });
        }
    }

    // 标签（真实计数 + tag:// 虚拟路径）
    section_collapsed = collapsed.contains("标签");
    items.push(header("标签", section_collapsed));
    if !section_collapsed {
        let tag_defs = [
            ("important", "重要", (0xd1u8, 0x34u8, 0x38u8)),
            ("archive", "待归档", (0xff, 0x8c, 0x00)),
            ("done", "已完成", (0x10, 0x7c, 0x10)),
        ];
        for (key, label, (r, g, b)) in tag_defs {
            let vpath = format!("tag://{}", key);
            let cnt = config.count(key);
            let badge = if cnt > 0 {
                cnt.to_string()
            } else {
                String::new()
            };
            items.push(NavItem {
                label: label.into(),
                path: vpath.clone().into(),
                icon: "●".into(),
                icon_class: "".into(),
                badge: badge.into(),
                is_header: false,
                is_disk: false,
                disk_ratio: 0.0,
                disk_info: "".into(),
                disk_color: slint::Brush::SolidColor(slint::Color::from_rgb_u8(r, g, b)),
                active: active_str == vpath,
                collapsed: false,
                is_tree: false,
                thumb: Image::default(),
                has_thumb: false,
            });
        }
        // 自定义标签（顺序与工具栏「标记」下拉一致，显示于固定标签之下）
        for ct in &config.custom_tags {
            let key = crate::config::AppConfig::custom_tag_key(&ct.id);
            let vpath = format!("tag://{}", key);
            let cnt = config.count(&key);
            let badge = if cnt > 0 {
                cnt.to_string()
            } else {
                String::new()
            };
            items.push(NavItem {
                label: ct.name.clone().into(),
                path: vpath.clone().into(),
                icon: "●".into(),
                icon_class: "".into(),
                badge: badge.into(),
                is_header: false,
                is_disk: false,
                disk_ratio: 0.0,
                disk_info: "".into(),
                disk_color: slint::Brush::SolidColor(hex_to_color(&ct.color)),
                active: active_str == vpath,
                collapsed: false,
                is_tree: false,
                thumb: Image::default(),
                has_thumb: false,
            });
        }
    }

    // 系统（回收站 / 网络位置，均为虚拟路径，应用内浏览）
    section_collapsed = collapsed.contains("系统");
    items.push(header("系统", section_collapsed));
    if !section_collapsed {
        // 真实系统图标（Stock 图标）：回收站按空/满取对应图标（跟随系统状态变化），
        // 网络位置取「网络」图标；提取失败回退到 MDL2 矢量字形（非文字色块）。
        let mk_sys = |label: &str, path: &str, stock, glyph: &str, active: bool| {
            let mut item = mk(label, path.to_string(), "", "", active);
            match crate::fs::thumbnail::stock_icon_cached(stock, THUMB_SIZE) {
                Some(icon) => {
                    item.thumb = image_cached(&icon);
                    item.has_thumb = true;
                }
                None => {
                    item.icon = glyph.into();
                    item.icon_class = "qa-glyph".into();
                    item.has_thumb = false;
                }
            }
            item
        };
        let recycle_stock = match crate::fs::recyclebin::cached_is_empty() {
            Some(false) => crate::fs::thumbnail::StockIcon::RecyclerFull,
            _ => crate::fs::thumbnail::StockIcon::RecyclerEmpty,
        };
        items.push(mk_sys(
            "回收站",
            "recycle://",
            recycle_stock,
            "\u{E74D}",
            active_str == "recycle://",
        ));
        items.push(mk_sys(
            "网络位置",
            "network://",
            crate::fs::thumbnail::StockIcon::Network,
            "\u{E968}",
            active_str == "network://",
        ));
    }

    let rc = ModelRc::new(VecModel::from(items));
    // 后台补齐本次构建中缺失的侧栏图标，完成后自动重建侧栏（此时命中缓存）
    flush_sidebar_warm();
    rc
}

/// 把已保存的网络位置推送到设置「云存储账号」页的列表模型
pub fn push_network_locations(ui: &MainWindow, core: &AppCore) {
    let items: Vec<NetAccount> = core
        .config
        .network_locations
        .iter()
        .map(|l| {
            // WebDAV 已挂载判定：内存挂载表或配置盘符真实存在任一即算，
            // 避免单信号延迟导致按钮在成功瞬间仍显示“挂载”
            let mounted = l.kind == "webdav" && crate::fs::cloud::is_webdav_mounted(l);
            let mounting =
                l.kind == "webdav" && !mounted && crate::fs::rclone::is_mounting(&l.name);
            // 已设置的挂载盘符（mount_drive 单字母转 "X:" 显示，未设置为空串）
            let mount_setting = if l.kind == "webdav" {
                l.mount_drive
                    .as_deref()
                    .map(|d| {
                        let t = d.trim().trim_end_matches(':').to_ascii_uppercase();
                        if t.is_empty() {
                            String::new()
                        } else {
                            format!("{}:", t.chars().next().unwrap_or('Z'))
                        }
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            NetAccount {
                name: l.name.clone().into(),
                server: if !l.host.is_empty() { l.display_server().into() } else { l.server.clone().into() },
                drive: l.drive.clone().unwrap_or_default().into(),
                kind: l.kind.clone().into(),
                mounted,
                mounting,
                mount_setting: mount_setting.into(),
            }
        })
        .collect();
    ui.global::<AppState>()
        .set_net_locations(ModelRc::new(VecModel::from(items)));
}

/// 推送自定义标签定义到 UI（启动与增删后调用，「标记」下拉与侧栏据此同步显示）
pub fn push_custom_tags(ui: &MainWindow, core: &AppCore) {
    let items: Vec<CustomTagDef> = core
        .config
        .custom_tags
        .iter()
        .map(|t| CustomTagDef {
            id: t.id.clone().into(),
            name: t.name.clone().into(),
            color: slint::Brush::SolidColor(hex_to_color(&t.color)),
        })
        .collect();
    ui.global::<AppState>()
        .set_custom_tags(ModelRc::new(VecModel::from(items)));
}

/// 供哈希回调返回 SharedString
pub fn hash_to_shared(path: &Path, algo: &str) -> SharedString {
    match crate::fs::hash::compute(path, algo) {
        Ok(v) => v.into(),
        Err(e) => format!("计算失败：{}", e).into(),
    }
}

/// 供校验回调返回 (结果文案, 状态码)。状态码：1 匹配 / 2 不匹配 / 3 错误
pub fn verify_result(path: &Path, expected: &str) -> (SharedString, i32) {
    match crate::fs::hash::verify(path, expected) {
        Ok((algo, true)) => (format!("匹配（{}）", algo).into(), 1),
        Ok((algo, false)) => (format!("不匹配（已按 {} 比对）", algo).into(), 2),
        Err(e) => (format!("校验失败：{}", e).into(), 3),
    }
}

/// 惰性填充属性对话框需要的文件系统、注册表、安全与签名字段。
pub fn fill_properties(state: &AppState, path: &Path, is_dir: bool) {
    let created = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .map(metadata::full_time)
        .unwrap_or_else(|| "—".to_string());
    state.set_sel_created(created.into());
    let open_with = if is_dir {
        String::new()
    } else {
        crate::fs::openwith::default_app_name(path).unwrap_or_default()
    };
    state.set_sel_open_with(open_with.into());
    fill_security(state, path);
    fill_signature(state, path, is_dir);
}

/// 填充属性「安全」页：所有者 + 属性摘要 + 只读标志。
/// 所有者读取依赖平台 API，非 Windows 平台留作占位。
fn fill_security(state: &AppState, path: &Path) {
    let meta = std::fs::metadata(path).ok();
    // 只读标志：跨平台可用（permissions.readonly）
    let readonly = meta
        .as_ref()
        .map(|m| m.permissions().readonly())
        .unwrap_or(false);
    state.set_sel_readonly(readonly);

    // 属性摘要：只读 / 隐藏（隐藏判断为 Windows 专属，其它平台按文件名以 . 开头近似）
    let mut attrs: Vec<&str> = Vec::new();
    if readonly {
        attrs.push("只读");
    }
    let hidden = is_hidden(path);
    if hidden {
        attrs.push("隐藏");
    }
    let attr_text = if attrs.is_empty() {
        "普通".to_string()
    } else {
        attrs.join("、")
    };
    state.set_sel_attributes(attr_text.into());

    // 所有者
    let owner = file_owner(path).unwrap_or_else(|| "—".to_string());
    state.set_sel_owner(owner.into());

    // DACL 访问控制项
    let aces: Vec<AclAce> = crate::fs::openwith::acl_entries(path)
        .into_iter()
        .map(|(trustee, kind, access)| AclAce {
            trustee: trustee.into(),
            kind: kind.into(),
            access: access.into(),
        })
        .collect();
    state.set_sel_acl_entries(ModelRc::new(VecModel::from(aces)));
}

/// 填充属性「详细信息」页：按文件类型解析扩展元数据（EXIF / 音频标签 / Office 核心属性）。
/// 在打开属性对话框时一次性填充，避免每次切换选中项都解析。
pub fn fill_details(state: &AppState, path: &Path) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let mut rows: Vec<MetaRow> = Vec::new();
    match ext.as_str() {
        "jpg" | "jpeg" | "tiff" | "tif" | "heic" | "webp" => rows.extend(read_exif(path)),
        "mp3" | "flac" | "m4a" | "ogg" | "wav" | "aac" | "wma" => rows.extend(read_audio(path)),
        "docx" | "xlsx" | "pptx" => rows.extend(read_office(path)),
        _ => {}
    }
    if rows.is_empty() {
        rows.push(MetaRow {
            label: "说明".into(),
            value: "此文件类型无扩展元数据".into(),
        });
    }
    state.set_sel_meta_rows(ModelRc::new(VecModel::from(rows)));
}

/// 读取图片 EXIF 信息（相机型号、拍摄参数等）。
fn read_exif(path: &Path) -> Vec<MetaRow> {
    use exif::{In, Reader, Tag};
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut buf = std::io::BufReader::new(&file);
    let exif = match Reader::new().read_from_container(&mut buf) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let tags: [(Tag, &str); 9] = [
        (Tag::Make, "相机制造商"),
        (Tag::Model, "相机型号"),
        (Tag::DateTime, "拍摄时间"),
        (Tag::ExposureTime, "曝光时间"),
        (Tag::FNumber, "光圈"),
        (Tag::ISOSpeed, "ISO 感光度"),
        (Tag::FocalLength, "焦距"),
        (Tag::PixelXDimension, "图像宽度"),
        (Tag::PixelYDimension, "图像高度"),
    ];
    let mut rows = Vec::new();
    for (tag, label) in tags {
        if let Some(f) = exif.get_field(tag, In::PRIMARY) {
            let v = format!("{}", f.display_value().with_unit(&exif));
            rows.push(MetaRow {
                label: label.into(),
                value: v.into(),
            });
        }
    }
    rows
}

/// 读取音频标签（标题、艺术家、专辑、时长等）。
fn read_audio(path: &Path) -> Vec<MetaRow> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::ItemKey;
    let tf = match lofty::probe::read_from_path(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut rows = Vec::new();
    let dur = tf.properties().duration();
    rows.push(MetaRow {
        label: "时长".into(),
        value: format!("{}:{:02}", dur.as_secs() / 60, dur.as_secs() % 60).into(),
    });
    if let Some(tag) = tf.primary_tag() {
        for (key, label) in [
            (ItemKey::TrackTitle, "标题"),
            (ItemKey::TrackArtist, "艺术家"),
            (ItemKey::AlbumTitle, "专辑"),
            (ItemKey::Year, "年份"),
            (ItemKey::Genre, "流派"),
        ] {
            if let Some(v) = tag.get_string(&key) {
                rows.push(MetaRow {
                    label: label.into(),
                    value: v.to_string().into(),
                });
            }
        }
    }
    rows
}

/// 读取 Office 文档（docx/xlsx/pptx）核心属性：标题、作者、修改者、创建/修改时间。
/// OOXML 是 ZIP 包，docProps/core.xml 存核心属性（Dublin Core 命名空间）。
fn read_office(path: &Path) -> Vec<MetaRow> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    let mut xml = String::new();
    for i in 0..archive.len() {
        if let Ok(mut f) = archive.by_index(i) {
            if f.name() == "docProps/core.xml" {
                let _ = f.read_to_string(&mut xml);
                break;
            }
        }
    }
    if xml.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for (tag, label) in [
        ("dc:title", "标题"),
        ("dc:creator", "作者"),
        ("cp:lastModifiedBy", "最后修改者"),
        ("dcterms:created", "创建时间"),
        ("dcterms:modified", "修改时间"),
    ] {
        if let Some(v) = extract_xml_tag(&xml, tag) {
            rows.push(MetaRow {
                label: label.into(),
                value: v.into(),
            });
        }
    }
    rows
}

/// 从 XML 文本中提取 `<tag ...>value</tag>` 的内容（简单字符串解析，避免引入 XML 库）。
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let gt = xml[start..].find('>')? + start + 1;
    let end = xml[gt..].find(&close)? + gt;
    Some(xml[gt..end].trim().to_string())
}

/// 判断文件是否隐藏。Windows 读取隐藏属性位；其它平台按 . 前缀近似。
#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    std::fs::metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// 读取文件所有者名称（Windows：Shell 取 System.FileOwner；其它平台返回 None）。
#[cfg(windows)]
fn file_owner(path: &Path) -> Option<String> {
    crate::fs::openwith::file_owner(path)
}

#[cfg(not(windows))]
fn file_owner(_path: &Path) -> Option<String> {
    None
}

/// 填充属性「数字签名」页：状态码 + 说明文本。
/// 状态码：0 不适用 / 1 已签名 / 2 未签名 / 3 错误
fn fill_signature(state: &AppState, path: &Path, is_dir: bool) {
    use crate::fs::signature::{cert_chain, detect, SignStatus};
    // 默认清空证书链（仅 Signed 分支填充）
    state.set_sel_cert_chain(ModelRc::new(VecModel::from(Vec::<CertInfo>::new())));
    if is_dir {
        state.set_sel_sign_status(0);
        state.set_sel_sign_detail("文件夹没有数字签名。".into());
        return;
    }
    match detect(path) {
        SignStatus::NotApplicable => {
            state.set_sel_sign_status(0);
            state.set_sel_sign_detail("该文件类型通常不包含数字签名。".into());
        }
        SignStatus::Signed { cert_bytes } => {
            state.set_sel_sign_status(1);
            state.set_sel_sign_detail(
                format!(
                    "检测到内嵌 Authenticode 签名（证书数据 {}）。",
                    metadata::human_size(cert_bytes as u64)
                )
                .into(),
            );
            let chain: Vec<CertInfo> = cert_chain(path)
                .into_iter()
                .map(|(s, i, f, t)| CertInfo {
                    subject: s.into(),
                    issuer: i.into(),
                    valid_from: f.into(),
                    valid_to: t.into(),
                })
                .collect();
            state.set_sel_cert_chain(ModelRc::new(VecModel::from(chain)));
        }
        SignStatus::Unsigned => {
            state.set_sel_sign_status(2);
            state.set_sel_sign_detail("可执行文件，但未找到内嵌数字签名。".into());
        }
        SignStatus::Error(e) => {
            state.set_sel_sign_status(3);
            state.set_sel_sign_detail(format!("读取签名信息失败：{}", e).into());
        }
    }
}

/// Quick Look 大图提取尺寸（图片预览用，比列表缩略图更大更清晰）
// 预览位图提取上限：更高的源分辨率让滚轮放大查看时保持清晰
pub(crate) const QL_IMAGE_SIZE: u32 = 1600;

/// 填充 Quick Look 预览内容：根据选中项类型设置 ql-* 属性。
/// 内容由 preview_host 镜像到独立预览窗口的 PreviewState；窗口尺寸不再由
/// 内容驱动（main.rs::apply_preview_initial_size 只在首次打开时给初值）。
/// `right` 为真时预览右侧面板的选中项（双面板右侧活动）。
/// 文件夹递归统计放到调用方后台执行，避免空格键阻塞 UI。
/// 返回是否成功设置（无选中项返回 false，调用方据此决定是否打开浮层）。
pub fn fill_quicklook(ui: &MainWindow, core: &AppCore, right: bool) -> bool {
    use crate::fs::preview::{self, PreviewKind};
    let state = ui.global::<AppState>();
    let tab = core.pane(right);
    let Some(fi) = tab.first_selected() else {
        return false;
    };
    let Some(e) = tab.entry_at(fi) else {
        return false;
    };

    // 云端文件：预览内容取自本地缓存（调用方 open_cloud_quicklook 已后台下载完成，
    // 此处秒级命中；未命中时同步下载为兜底）。云目录不下载，按虚拟文件夹统计展示
    let prepared_path = if e.path.starts_with("cloud://") && !e.is_dir {
        crate::fs::cloud::download_webdav_file(&e.path, &core.config).unwrap_or_else(|_| PathBuf::from(&e.path))
    } else {
        PathBuf::from(&e.path)
    };
    let path = prepared_path.as_path();
    let kind = preview::kind_of(path, e.is_dir);
    state.set_ql_kind(kind.code());
    state.set_ql_name(e.name.clone().into());
    state.set_ql_icon_class(e.icon_class.clone().into());
    // 所有类型加载态默认开启：窗口显示第一帧前先盖住内容区，
    // 避免复用窗口的旧画面/透明背景闪一下再出新内容。
    // 同步类型（文本/归档/文件夹/信息）由 main.rs 在显示后一帧关闭；
    // 图片/视频由各自的后台就绪回调关闭。
    state.set_ql_loading(true);
    // 默认清空上一次的图片/文本，避免切换文件时残留旧内容
    state.set_ql_has_image(false);
    state.set_ql_text("".into());
    state.set_ql_code_kw("".into());
    state.set_ql_code_str("".into());
    state.set_ql_code_cmt("".into());
    state.set_ql_info("".into());
    // 渲染/源码切换状态复位（Markdown/HTML/PHP/Office/PDF 在 Text 分支重新置位）
    state.set_ql_can_render(false);
    state.set_ql_web_mode(false);
    state.set_ql_office_doc(false);
    state.set_ql_office_pending(false);
    // 重置上一次内容的图片尺寸：视频/音频不设置宽高，若残留旧值，
    // 预览窗口会按上一次图片的尺寸打开（视频无法以合适大小预览）。
    state.set_ql_img_w(0);
    state.set_ql_img_h(0);
    // 播放状态复位：上一首的进度/暂停/静音/速率不带到新文件（音频控制条用）
    state.set_ql_video_position(0);
    state.set_ql_video_duration(0);
    state.set_ql_video_paused(false);
    state.set_ql_video_muted(false);
    state.set_ql_video_rate(1.0);
    // 慢速文件系统异步读取标记复位（Text/Archive 分支按需置真）
    state.set_ql_loading_async(false);
    // 条目的真实缩略图/系统图标（取对应面板列表模型已生成的位图），头部与大图标优先显示
    let model = if right {
        state.get_r_entries()
    } else {
        state.get_entries()
    };
    if let Some(row) = model.row_data(fi) {
        state.set_ql_thumb(row.thumb.clone());
        state.set_ql_has_thumb(row.has_thumb);
    } else {
        state.set_ql_has_thumb(false);
    }

    let size_text = if e.is_dir {
        String::new()
    } else {
        format!("{} · {}", e.kind, metadata::human_size(e.size_bytes))
    };
    // 慢速文件系统（应用挂载的 WebDAV/rclone 虚拟盘、SMB 网络盘）：
    // 同步读取实为网络请求，会卡死 UI 线程数秒，文本/音频改为后台读取回填
    let slow = !e.is_dir && is_slow_preview_fs(&core.config, path);

    match kind {
        PreviewKind::Image => {
            // 真实像素尺寸：仅解析文件头，不解码整图。窗口在显示前就据此定尺寸，
            // 位图解码交给 main.rs 打开窗口后的后台线程（大图不再卡住空格键）。
            let (iw, ih) = imagesize::size(path)
                .map(|d| (d.width as i32, d.height as i32))
                .unwrap_or((0, 0));
            state.set_ql_img_w(iw);
            state.set_ql_img_h(ih);
            state.set_ql_loading(true);
            state.set_ql_subtitle(if iw > 0 && ih > 0 {
                format!("{}×{} 像素 · {}", iw, ih, metadata::human_size(e.size_bytes)).into()
            } else {
                size_text.into()
            });
        }
        PreviewKind::Text => {
            state.set_ql_subtitle(size_text.into());
            // Markdown/HTML/PHP/Office/PDF：支持渲染视图，默认以渲染模式打开
            if preview::renderable_web(path) {
                state.set_ql_can_render(true);
                state.set_ql_web_mode(true);
            }
            // 文档源码视图也显示抽取出的可读内容；抽取失败时保留明确错误，而不是十六进制乱码。
            // Office（含旧版 doc/xls/ppt）与 PDF 走 document_text 高保真通道。
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            // Office 文档：标记高保真模式（隐藏渲染/源码切换）。
            // 类型判定按扩展名（零 IO）；缓存命中/安装检测涉及磁盘访问，
            // 慢速文件系统时随正文一起放后台（见 spawn_slow_text_fill）。
            let is_office = crate::fs::office_preview::is_office_doc(path);
            if is_office {
                state.set_ql_office_doc(true);
            }
            if slow {
                // 慢速文件系统：正文读取（最长可到数秒）放后台，
                // 窗口先以加载态显示，读取完成后回填正文并收起加载动画。
                state.set_ql_loading_async(true);
                spawn_slow_text_fill(ui, path, ext, is_office);
            } else {
                if is_office {
                    let fresh = crate::fs::office_preview::cached_pdf_if_fresh(path).is_some();
                    let installed = crate::fs::office_preview::office_app_for_ext(&ext)
                        .map(|a| crate::fs::office_preview::is_office_installed(a))
                        .unwrap_or(false);
                    state.set_ql_office_pending(!fresh && installed);
                }
                let text = if is_office || ext == "pdf" {
                    preview::document_text(path).unwrap_or_else(|e| format!("文档内容暂时无法预览：{}", e))
                } else {
                    preview::read_text_head(path, 64 * 1024)
                };
                let layers = crate::fs::highlight::highlight(&text, &ext);
                state.set_ql_text(layers.base.into());
                state.set_ql_code_kw(layers.keywords.into());
                state.set_ql_code_str(layers.strings.into());
                state.set_ql_code_cmt(layers.comments.into());
            }
        }
        PreviewKind::Video => {
            // 视频：窗口在显示前就按真实画面尺寸定型（与图片分支同理），避免固定窗口。
            // ql_img_w/h 在本函数开头已清零（防上一次图片尺寸残留），此处探测成功才赋值，
            // 失败时保持 0，由 MF 就绪回调回填并重定窗口。
            // 慢速文件系统（WebDAV/SMB 挂载盘）跳过同步探测：Shell 属性读取可能阻塞数秒，
            // 留待 MF 异步就绪后重定窗口。
            if slow {
                state.set_ql_subtitle(size_text.into());
            } else if let Some((vw, vh)) =
                crate::fs::video_preview::probe_display_size(&path.to_string_lossy())
            {
                if vw > 0 && vh > 0 {
                    state.set_ql_img_w(vw as i32);
                    state.set_ql_img_h(vh as i32);
                    state.set_ql_subtitle(
                        format!("{}×{} 像素 · {}", vw, vh, size_text).into(),
                    );
                } else {
                    state.set_ql_subtitle(size_text.into());
                }
            } else {
                state.set_ql_subtitle(size_text.into());
            }
        }
        PreviewKind::Audio => {
            // 音频：提取元数据与封面，显示专用音频播放器 UI
            let mut subtitle = size_text.clone();
            if slow {
                // 慢速文件系统：标签/封面读取放后台；加载动画由媒体就绪回调收起
                spawn_slow_audio_fill(ui, path, size_text);
            } else {
                use lofty::file::{AudioFile, TaggedFileExt};
                if let Ok(tf) = lofty::probe::read_from_path(path) {
                    let duration = tf.properties().duration();
                    let time_str = format!("{}:{:02}", duration.as_secs() / 60, duration.as_secs() % 60);

                    // 提取标签信息与封面
                    let mut meta_parts = Vec::new();
                    if let Some(tag) = tf.primary_tag() {
                        if let Some(title) = tag.get_string(&lofty::tag::ItemKey::TrackTitle) {
                            meta_parts.push(format!("标题：{}", title));
                        }
                        if let Some(artist) = tag.get_string(&lofty::tag::ItemKey::TrackArtist) {
                            meta_parts.push(format!("艺术家：{}", artist));
                        }
                        if let Some(album) = tag.get_string(&lofty::tag::ItemKey::AlbumTitle) {
                            meta_parts.push(format!("专辑：{}", album));
                        }

                        // 提取内嵌封面图片
                        let pictures = tag.pictures();
                        if let Some(pic) = pictures.first() {
                            if let Ok(img) = image::load_from_memory(pic.data()) {
                                let rgba = img.to_rgba8();
                                let (w, h) = (rgba.width(), rgba.height());
                                let pixels: Vec<u8> = rgba.into_raw();
                                let slint_img = slint::Image::from_rgba8(
                                    slint::SharedPixelBuffer::clone_from_slice(&pixels, w, h)
                                );
                                state.set_ql_thumb(slint_img);
                                state.set_ql_has_thumb(true);
                            }
                        }
                    }

                    subtitle = if meta_parts.is_empty() {
                        format!("音频 · {} · {}", time_str, size_text)
                    } else {
                        format!("{} · 音频 · {} · {}", meta_parts.join(" · "), time_str, size_text)
                    };
                }
                state.set_ql_subtitle(subtitle.into());
            }
        }
        PreviewKind::Archive => {
            // 归档：内容为可展开/折叠的树（kind==5），由 preview_host 读取归档并
            // 生成节点模型；此处只给出「类型 · 压缩包体积」副标题，
            // 归档内统计由 preview_host 追加。
            state.set_ql_subtitle(size_text.into());
            if slow {
                // 慢速文件系统：整包读取是网络请求，置异步标记，
                // preview_host::push_content 据此把归档读取放后台线程
                state.set_ql_loading_async(true);
            }
        }
        PreviewKind::Folder => {
            state.set_ql_subtitle("文件夹".into());
            state.set_ql_info("正在计算子项目与文件总大小…".into());
        }
        PreviewKind::Info => {
            state.set_ql_subtitle(size_text.into());
            let mut info = format!(
                "{}\n位置：{}\n修改时间：{}",
                e.kind,
                Path::new(&e.path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                metadata::fmt_ts_full(e.modified_ts)
            );
            // 可执行文件带版本资源时追加描述/公司/版本/产品（应用信息）
            for (k, v) in preview::exe_version_info(path) {
                info.push_str(&format!("\n{}：{}", k, v));
            }
            state.set_ql_info(info.into());
        }
    }
    true
}

/// 慢速文件系统判定：应用自身挂载的 WebDAV/rclone 虚拟盘（按配置的盘符匹配）、
/// SMB/映射网络驱动器（GetDriveTypeW == DRIVE_REMOTE）。这类路径上的文件读取
/// 实为网络请求，预览内容必须放后台线程，否则 UI 线程被卡死数秒。
fn is_slow_preview_fs(config: &crate::config::AppConfig, path: &Path) -> bool {
    let s = path.to_string_lossy();
    // 应用自身挂载的 WebDAV 虚拟磁盘（drive / mount_drive 任一匹配盘符）
    let letter = s.chars().next().map(|c| c.to_ascii_uppercase());
    if letter.is_some()
        && config.network_locations.iter().any(|l| {
            l.kind == "webdav"
                && [
                    l.drive.as_deref(),
                    l.mount_drive.as_deref(),
                ]
                .into_iter()
                .flatten()
                .filter_map(|d| d.chars().next())
                .any(|c| c.to_ascii_uppercase() == letter.unwrap())
        })
    {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
        // 取卷根："X:\a\b.txt" → "X:\"；UNC（\\server\share\...）取 \\server\share。
        // splitn 产生前导空段（来自 \\ 起始），须按下标取第 3/4 段重组双反斜杠根，
        // filter+单反斜杠拼接会得到非法根（GetDriveTypeW 误判为本地盘）
        let root: String = {
            let b = s.as_bytes();
            if b.len() >= 2 && b[1] == b':' {
                format!("{}\\", &s[..2])
            } else if s.starts_with("\\\\") {
                let parts: Vec<&str> = s.splitn(4, '\\').collect();
                let (server, share) = match (parts.get(2), parts.get(3)) {
                    (Some(sv), Some(sh)) => (*sv, sh.split('\\').next().unwrap_or("")),
                    _ => return false,
                };
                if server.is_empty() || share.is_empty() {
                    return false;
                }
                format!("\\\\{}\\{}", server, share)
            } else {
                return false;
            }
        };
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY：root 以 NUL 结尾的合法 Windows 路径，GetDriveTypeW 仅读该字符串。
        // 返回 4 = DRIVE_REMOTE（网络驱动器；windows-sys 0.59 未导出该常量，用文档值）
        unsafe { GetDriveTypeW(wide.as_ptr()) == 4u32 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 慢速文件系统上的文本预览：正文读取与 Office 缓存探测放后台线程，
/// 完成后回填正文/染色层并收起加载动画。预览已关闭或已切换文件时丢弃结果。
fn spawn_slow_text_fill(ui: &MainWindow, path: &Path, ext: String, is_office: bool) {
    let path_buf = PathBuf::from(path);
    let key = path.to_string_lossy().into_owned();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        // Office 缓存命中与安装检测（涉及磁盘/注册表访问）一并放后台
        let fresh = is_office && crate::fs::office_preview::cached_pdf_if_fresh(&path_buf).is_some();
        let installed = crate::fs::office_preview::office_app_for_ext(&ext)
            .map(|a| crate::fs::office_preview::is_office_installed(a))
            .unwrap_or(false);
        let text = if is_office || ext == "pdf" {
            crate::fs::preview::document_text(&path_buf)
                .unwrap_or_else(|e| format!("文档内容暂时无法预览：{}", e))
        } else {
            crate::fs::preview::read_text_head(&path_buf, 64 * 1024)
        };
        let layers = crate::fs::highlight::highlight(&text, &ext);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let state = ui.global::<AppState>();
            // 预览已关闭或已切换到其它文件：丢弃迟到的结果
            if !state.get_quicklook_open() || state.get_sel_path() != key.as_str() {
                return;
            }
            state.set_ql_office_pending(is_office && !fresh && installed);
            state.set_ql_text(layers.base.clone().into());
            state.set_ql_code_kw(layers.keywords.clone().into());
            state.set_ql_code_str(layers.strings.clone().into());
            state.set_ql_code_cmt(layers.comments.clone().into());
            // 正文就绪：收起加载动画（主窗口与独立预览窗口两份状态）
            state.set_ql_loading(false);
            state.set_ql_loading_async(false);
            // 同步到独立预览窗口的源码文本层（渲染/源码切换即时生效）
            if let Some(pw) = crate::preview_host::window() {
                let dst = pw.global::<crate::PreviewState>();
                dst.set_text_content(layers.base.into());
                dst.set_code_kw(layers.keywords.into());
                dst.set_code_str(layers.strings.into());
                dst.set_code_cmt(layers.comments.into());
                crate::preview_host::set_loading(&pw, false);
            }
        });
    });
}

/// 慢速文件系统上的音频预览：标签/封面读取放后台线程，
/// 完成后回填副标题与封面（主窗口与独立预览窗口同步）。加载动画由音频就绪回调收起。
fn spawn_slow_audio_fill(ui: &MainWindow, path: &Path, size_text: String) {
    let path_buf = PathBuf::from(path);
    let key = path.to_string_lossy().into_owned();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        use lofty::file::{AudioFile, TaggedFileExt};

        let mut subtitle = size_text.clone();
        let mut cover: Option<(Vec<u8>, u32, u32)> = None;
        if let Ok(tf) = lofty::probe::read_from_path(&path_buf) {
            let duration = tf.properties().duration();
            let time_str = format!("{}:{:02}", duration.as_secs() / 60, duration.as_secs() % 60);

            let mut meta_parts = Vec::new();
            if let Some(tag) = tf.primary_tag() {
                if let Some(title) = tag.get_string(&lofty::tag::ItemKey::TrackTitle) {
                    meta_parts.push(format!("标题：{}", title));
                }
                if let Some(artist) = tag.get_string(&lofty::tag::ItemKey::TrackArtist) {
                    meta_parts.push(format!("艺术家：{}", artist));
                }
                if let Some(album) = tag.get_string(&lofty::tag::ItemKey::AlbumTitle) {
                    meta_parts.push(format!("专辑：{}", album));
                }
                if let Some(pic) = tag.pictures().first() {
                    if let Ok(img) = image::load_from_memory(pic.data()) {
                        let rgba = img.to_rgba8();
                        let (w, h) = (rgba.width(), rgba.height());
                        cover = Some((rgba.into_raw(), w, h));
                    }
                }
            }

            subtitle = if meta_parts.is_empty() {
                format!("音频 · {} · {}", time_str, size_text)
            } else {
                format!("{} · 音频 · {} · {}", meta_parts.join(" · "), time_str, size_text)
            };
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let state = ui.global::<AppState>();
            // 预览已关闭或已切换到其它文件：丢弃迟到的结果
            if !state.get_quicklook_open() || state.get_sel_path() != key.as_str() {
                return;
            }
            state.set_ql_subtitle(subtitle.clone().into());
            if let Some((pixels, w, h)) = cover {
                let img = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                    &pixels, w, h,
                ));
                state.set_ql_thumb(img.clone());
                state.set_ql_has_thumb(true);
                // 同步到独立预览窗口（封面卡片即时换成内嵌封面）
                if let Some(pw) = crate::preview_host::window() {
                    pw.global::<crate::PreviewState>().set_thumb(img);
                    pw.global::<crate::PreviewState>().set_has_thumb(true);
                }
            }
            // 副标题同步到独立预览窗口头部
            if let Some(pw) = crate::preview_host::window() {
                crate::preview_host::set_subtitle(&pw, &subtitle);
            }
        });
    });
}

/// 推送标签页列表到 UI
pub fn push_tabs(ui: &MainWindow, core: &AppCore) {
    let state = ui.global::<AppState>();
    let rows: Vec<TabInfo> = core
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabInfo {
            title: t.title().into(),
            path: t.history.current().to_string_lossy().to_string().into(),
            active: i == core.active,
            kind: (if t.kind == TabKind::Settings {
                "settings"
            } else {
                "files"
            })
            .into(),
        })
        .collect();
    state.set_tabs(ModelRc::new(VecModel::from(rows)));
    state.set_active_tab(core.active as i32);

    // 主体视图随活动标签页类型切换（设置页 / 文件浏览）
    let view = if core.active_tab().kind == TabKind::Settings {
        "settings"
    } else {
        "files"
    };
    state.set_active_view(view.into());
}

#[cfg(test)]
mod icon_request_tests {
    use super::*;
    use crate::fs::thumbnail::IconRequest;

    fn entry(name: &str, path: &str, is_dir: bool, icon_class: &str) -> metadata::Entry {
        metadata::Entry {
            name: name.into(),
            path: path.into(),
            is_dir,
            size_bytes: 0,
            modified_ts: 7,
            kind: String::new(),
            icon_label: String::new(),
            icon_class: icon_class.into(),
        }
    }

    fn webdav_config(mount_icon: &str, drive: Option<&str>) -> crate::config::AppConfig {
        let mut config = crate::config::AppConfig::default();
        config.network_locations.push(crate::config::NetworkLocation {
            name: "PikPak".into(),
            server: String::new(),
            kind: "webdav".into(),
            drive: drive.map(|d| d.to_string()),
            host: "dav.example.com".into(),
            port: 0,
            remote_path: "/".into(),
            username: String::new(),
            password: String::new(),
            use_tls: false,
            passive: true,
            mount_drive: drive.map(|d| d.to_string()),
            mount_readonly: false,
            mount_max_size_gb: None,
            mount_icon: mount_icon.into(),
        });
        config
    }

    #[test]
    fn device_entries_never_become_real_path_requests() {
        let cfg = crate::config::AppConfig::default();
        let device = entry("手机", "device://id", true, "device");
        assert_eq!(
            icon_request_for_entry(&device, true, &cfg),
            Some(IconRequest::Device)
        );
        assert_eq!(icon_request_for_entry(&device, false, &cfg), None);

        let file = entry("报告.PDF", "device://id\u{1}object", false, "document");
        assert_eq!(
            icon_request_for_entry(&file, true, &cfg),
            Some(IconRequest::Type {
                extension: "PDF".into(),
                is_dir: false,
            })
        );
    }

    #[test]
    fn local_and_other_virtual_entries_keep_existing_policy() {
        let cfg = crate::config::AppConfig::default();
        let local = entry("readme.txt", r"C:\readme.txt", false, "document");
        assert!(matches!(
            icon_request_for_entry(&local, true, &cfg),
            Some(IconRequest::RealPath { .. })
        ));
        assert_eq!(icon_request_for_entry(&local, false, &cfg), None);

        let tag = entry("重要", "tag://important", true, "folder");
        assert_eq!(icon_request_for_entry(&tag, true, &cfg), None);
    }

    #[test]
    fn webdav_drive_uses_data_drive_icon_not_folder() {
        // 无账户配置的兜底：WebDAV 账户根仍取数据盘系统图标，与 D:/H: 一致，而非文件夹
        let cfg = crate::config::AppConfig::default();
        let dav = entry("PikPak", "cloud://webdav/PikPak", true, "drive");
        assert_eq!(
            icon_request_for_entry(&dav, true, &cfg),
            Some(IconRequest::DataDrive)
        );
        assert_eq!(icon_request_for_entry(&dav, false, &cfg), None);
        // 子项不得复用数据盘图标：文件夹走文件夹类型，文件走扩展名类型，
        // 否则云端目录全显示为硬盘图标、文件与文件夹分不清
        let child_dir = entry("My Pack", "cloud://webdav/PikPak/My Pack", true, "folder");
        assert_eq!(
            icon_request_for_entry(&child_dir, true, &cfg),
            Some(IconRequest::Type {
                extension: "".into(),
                is_dir: true,
            })
        );
        assert_eq!(icon_request_for_entry(&child_dir, false, &cfg), None);
        let child_file = entry(
            "report.pdf",
            "cloud://webdav/PikPak/report.pdf",
            false,
            "default",
        );
        assert_eq!(
            icon_request_for_entry(&child_file, true, &cfg),
            Some(IconRequest::Type {
                extension: "pdf".into(),
                is_dir: false,
            })
        );
        // FTP 仍走文件夹类型图标
        let ftp = entry("MyFTP", "cloud://ftp/MyFTP", true, "folder");
        assert_eq!(
            icon_request_for_entry(&ftp, true, &cfg),
            Some(IconRequest::Type {
                extension: "".into(),
                is_dir: true,
            })
        );
    }

    #[test]
    fn webdav_mount_icon_three_states_route() {
        // cloud:// 账户条目：默认 → 数据盘；预设 → 矢量（None）；自定义文件 → 无条件提取
        let dav = entry("PikPak", "cloud://webdav/PikPak", true, "drive");

        let cfg = webdav_config("", None);
        assert_eq!(
            icon_request_for_entry(&dav, true, &cfg),
            Some(IconRequest::DataDrive)
        );
        assert_eq!(icon_request_for_entry(&dav, false, &cfg), None);

        let cfg = webdav_config("cloud", None);
        assert_eq!(icon_request_for_entry(&dav, true, &cfg), None);
        assert_eq!(icon_request_for_entry(&dav, false, &cfg), None);

        let cfg = webdav_config(r"file:C:\icons\cloud.ico", None);
        assert_eq!(
            icon_request_for_entry(&dav, false, &cfg),
            Some(IconRequest::RealPath {
                path: r"C:\icons\cloud.ico".into(),
                is_dir: false,
                mtime: 0,
            })
        );
    }

    #[test]
    fn webdav_mounted_drive_root_routes_by_icon() {
        // 挂载盘根（"Z:\"）：默认 → 数据盘而非提取 WinFsp 卷图标；自定义文件 → 提取
        let root = entry("PikPak (Z:)", r"Z:\", true, "drive");

        let cfg = webdav_config("", Some("Z:"));
        assert_eq!(
            icon_request_for_entry(&root, true, &cfg),
            Some(IconRequest::DataDrive)
        );
        assert_eq!(icon_request_for_entry(&root, false, &cfg), None);

        // 非挂载盘（无对应配置）保持原策略：系统模式提取真实盘符图标
        let plain_cfg = crate::config::AppConfig::default();
        assert!(matches!(
            icon_request_for_entry(&root, true, &plain_cfg),
            Some(IconRequest::RealPath { .. })
        ));

        let cfg = webdav_config(r"file:C:\icons\dav.ico", Some("Z:"));
        assert!(matches!(
            icon_request_for_entry(&root, false, &cfg),
            Some(IconRequest::RealPath { path, .. }) if path == r"C:\icons\dav.ico"
        ));
    }
}
