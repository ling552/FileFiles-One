//! 缩略图 / 系统图标统一提取
//!
//! 使用 Windows Shell 的 `IShellItemImageFactory::GetImage`，与资源管理器同源：
//! - 图片：返回真实缩略图
//! - 视频：返回首帧缩略图
//! - 其它文件：返回该文件类型注册的系统图标（含第三方应用注册的图标）
//!
//! 返回 RGBA8 像素 + 宽 + 高，便于在后台线程间传递（SharedPixelBuffer 非 Send）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// 已解码的图标像素：线程安全，可放入全局缓存并跨线程共享（Arc 避免重复拷贝）。
pub struct IconPixels {
    pub pixels: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 简易 LRU 缓存：逻辑时钟打点，超限时从最旧开始逐条淘汰。
/// 相比旧的「满则整体清空」，逐条淘汰让最近仍在浏览的图标保持命中，
/// 清理后回到上一目录不再需要整批重新提取；字节上限把大图标
/// （256px≈256KiB）与普通图标（128px≈64KiB）统一纳入同一预算。
struct LruMap<V> {
    map: HashMap<String, Slot<V>>,
    tick: u64,
    bytes: usize,
    cap_entries: usize,
    cap_bytes: usize,
}

struct Slot<V> {
    v: V,
    tick: u64,
    bytes: usize,
}

impl<V> LruMap<V> {
    fn new(cap_entries: usize, cap_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            bytes: 0,
            cap_entries,
            cap_bytes,
        }
    }

    /// 命中并刷新访问序号。
    fn get(&mut self, key: &str) -> Option<&V> {
        if let Some(slot) = self.map.get_mut(key) {
            self.tick += 1;
            slot.tick = self.tick;
            Some(&slot.v)
        } else {
            None
        }
    }

    fn get_cloned(&mut self, key: &str) -> Option<V>
    where
        V: Clone,
    {
        self.get(key).cloned()
    }

    /// 插入（同键替换时按字节差量记账并视为最新），随后按需淘汰。
    fn insert(&mut self, key: String, v: V, bytes: usize) {
        match self.map.get_mut(&key) {
            Some(slot) => {
                self.bytes = self.bytes + bytes - slot.bytes;
                self.tick += 1;
                slot.bytes = bytes;
                slot.tick = self.tick;
                slot.v = v;
            }
            None => {
                self.tick += 1;
                self.map.insert(key, Slot { v, tick: self.tick, bytes });
                self.bytes += bytes;
            }
        }
        self.evict();
    }

    /// 超出条数或字节预算时，按访问序号从最旧删除到预算的 3/4（摊销）。
    fn evict(&mut self) {
        if self.map.len() <= self.cap_entries && self.bytes <= self.cap_bytes {
            return;
        }
        // saturating：测试/极端配置可能传 usize::MAX 关闭某一维预算
        let target_entries = self.cap_entries.saturating_mul(3) / 4;
        let target_bytes = self.cap_bytes.saturating_mul(3) / 4;
        let mut by_age: Vec<(u64, String)> = self
            .map
            .iter()
            .map(|(k, s)| (s.tick, k.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(t, _)| *t);
        for (_, k) in by_age {
            if self.map.len() <= target_entries && self.bytes <= target_bytes {
                break;
            }
            if let Some(slot) = self.map.remove(&k) {
                self.bytes -= slot.bytes;
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }
}

/// 按"文件类型"（扩展名 / 文件夹）共享的图标缓存。
/// 系统图标对绝大多数文件只取决于扩展名，故同扩展名的所有文件共用一张图，
/// 把"目录里 N 个文件 N 次 Shell 调用"降到"每种类型一次"，是提速的核心。
fn type_cache() -> &'static Mutex<LruMap<Arc<IconPixels>>> {
    static C: OnceLock<Mutex<LruMap<Arc<IconPixels>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(LruMap::new(256, 16 * 1024 * 1024)))
}

/// 按"具体文件"缓存的图标（真实缩略图 / exe 自带图标等，随文件内容变化）。
/// 键含修改时间戳，文件更新后旧键自然失效。
fn path_cache() -> &'static Mutex<LruMap<Arc<IconPixels>>> {
    static C: OnceLock<Mutex<LruMap<Arc<IconPixels>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(LruMap::new(512, 32 * 1024 * 1024)))
}

/// 侧栏专用图标缓存（按真实路径/特殊键）。侧栏条目少（~几十个），
/// 独立于会被内存守护清理的 type/path 缓存，保证系统图标模式下侧栏
/// 图标不随主缓存清理而回退为内置图标；同样给出上限兜底。
fn sidebar_cache() -> &'static Mutex<LruMap<Arc<IconPixels>>> {
    static C: OnceLock<Mutex<LruMap<Arc<IconPixels>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(LruMap::new(128, 8 * 1024 * 1024)))
}

/// 读侧栏专用缓存（不触发提取）
pub fn sidebar_icon_get(key: &str) -> Option<Arc<IconPixels>> {
    let mut c = sidebar_cache().lock().ok()?;
    c.get_cloned(key)
}

/// 写侧栏专用缓存（后台预热线程提取成功后调用）
pub fn sidebar_icon_set(key: &str, icon: Arc<IconPixels>) {
    let bytes = icon.pixels.len();
    if let Ok(mut c) = sidebar_cache().lock() {
        c.insert(key.to_string(), icon, bytes);
    }
}

/// 文件夹在类型缓存中的特殊键（用控制字符避免与任何扩展名冲突）。
const DIR_KEY: &str = "\u{0}<dir>";
/// 无扩展名普通文件的类型缓存键，不能与文件夹共用空字符串。
const FILE_KEY: &str = "\u{0}<file>";
/// 便携设备在类型缓存中的特殊键。
const DEVICE_KEY: &str = "\u{0}<device>";
/// 通用数据盘在类型缓存中的特殊键（WebDAV 等挂载的虚拟磁盘用，
/// 与除 C 盘外的其它盘共用同一张系统盘符图标，而非黄色文件夹）。
const DATA_DRIVE_KEY: &str = "\u{0}<datadrive>";

/// 图标提取请求。虚拟设备路径只能构造成 Type/Device，避免误入真实路径提取器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconRequest {
    RealPath {
        path: String,
        is_dir: bool,
        mtime: i64,
    },
    Type {
        extension: String,
        is_dir: bool,
    },
    Device,
    /// 通用数据盘图标（非 C 盘的标准盘符图标）：供 cloud:// WebDAV 等
    /// 虚拟磁盘条目使用，保证与 D:/H: 等数据盘显示同一张系统图标。
    DataDrive,
}

fn normalize_extension(extension: &str) -> String {
    extension.trim().trim_start_matches('.').to_lowercase()
}

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 该扩展名的图标是否取决于"具体文件内容"（不能按扩展名共享）。
/// 包含：自带图标的可执行/快捷方式/图标文件，以及需要真实缩略图的图片/视频。
fn per_file_icon(ext: &str) -> bool {
    matches!(
        ext,
        // 自带图标
        "exe" | "lnk" | "ico" | "msi" | "scr" | "cpl"
        // 图片（真实缩略图）
        | "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff"
        // 视频（首帧缩略图）
        | "mp4" | "mov" | "avi" | "mkv" | "wmv" | "flv" | "m4v" | "webm" | "mpg" | "mpeg"
    )
}

/// 缓存归类：按类型共享 or 按具体文件。
enum Kind {
    Type(String),
    Path(String),
}

/// 是否为驱动器根（如 "C:\"、"D:/"）。驱动器需取真实盘符图标，
/// 不能走文件夹通用图标（否则磁盘显示成黄色文件夹）。
fn is_drive_root(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn kind_of(path: &str, is_dir: bool, mtime: i64) -> Kind {
    if is_dir {
        // 驱动器根：按"具体路径"取真实盘符图标（IShellItemImageFactory），
        // 各盘符图标不同，故以路径为缓存键。
        if is_drive_root(path) {
            return Kind::Path(format!("{}|drive", path));
        }
        return Kind::Type(DIR_KEY.to_string());
    }
    let ext = ext_of(path);
    if per_file_icon(&ext) {
        Kind::Path(format!("{}|{}", path, mtime))
    } else if ext.is_empty() {
        Kind::Type(FILE_KEY.to_string())
    } else {
        Kind::Type(ext)
    }
}

fn request_kind(request: &IconRequest) -> Kind {
    match request {
        IconRequest::RealPath {
            path,
            is_dir,
            mtime,
        } => kind_of(path, *is_dir, *mtime),
        IconRequest::Type { extension, is_dir } => Kind::Type(if *is_dir {
            DIR_KEY.to_string()
        } else {
            let extension = normalize_extension(extension);
            if extension.is_empty() {
                FILE_KEY.to_string()
            } else {
                extension
            }
        }),
        IconRequest::Device => Kind::Type(DEVICE_KEY.to_string()),
        IconRequest::DataDrive => Kind::Type(DATA_DRIVE_KEY.to_string()),
    }
}

/// 仅查询请求对应的缓存，不访问磁盘或 Windows Shell。
pub fn cached_request(request: &IconRequest) -> Option<Arc<IconPixels>> {
    match request_kind(request) {
        Kind::Type(k) => {
            let mut c = type_cache().lock().ok()?;
            c.get_cloned(&k)
        }
        Kind::Path(k) => {
            let mut c = path_cache().lock().ok()?;
            c.get_cloned(&k)
        }
    }
}

/// 该请求是否走「按类型共享」缓存（扩展名/文件夹/设备/Stock）。
/// UI 侧据此把图标任务分两阶段：类型图标先行批量回填（快、去重），
/// 具体文件缩略图随后跟进，避免慢缩略图挡住快类型图标的首绘。
pub fn request_is_shared_type(request: &IconRequest) -> bool {
    matches!(request_kind(request), Kind::Type(_))
}

/// 仅查询缓存（同步、零磁盘/Shell 访问）。供 UI 线程在构建条目时预填，
/// 命中即首帧显示系统图标，彻底消除"先内置图标后异步替换"的闪烁。
pub fn cached(path: &str, is_dir: bool, mtime: i64) -> Option<Arc<IconPixels>> {
    match kind_of(path, is_dir, mtime) {
        Kind::Type(k) => {
            let mut c = type_cache().lock().ok()?;
            c.get_cloned(&k)
        }
        Kind::Path(k) => {
            let mut c = path_cache().lock().ok()?;
            c.get_cloned(&k)
        }
    }
}

fn valid_pixels(pixels: &[u8], w: u32, h: u32) -> bool {
    w > 0
        && h > 0
        && w.saturating_mul(h).saturating_mul(4) as usize == pixels.len()
        && pixels.chunks_exact(4).any(|pixel| pixel[3] != 0)
}

/// 仅接受完整像素缓冲，避免无效系统图标覆盖内置回退图标。
fn valid_icon(raw: Option<(Vec<u8>, u32, u32)>) -> Option<(Vec<u8>, u32, u32)> {
    let (pixels, w, h) = raw?;
    valid_pixels(&pixels, w, h).then_some((pixels, w, h))
}


pub fn load_cached_request(request: &IconRequest, size: u32) -> Option<Arc<IconPixels>> {
    if size == 0 {
        return None;
    }
    if let Some(c) = cached_request(request) {
        return Some(c);
    }
    let kind = request_kind(request);
    let raw = match (request, &kind) {
        (IconRequest::RealPath { path, .. }, Kind::Type(key)) => {
            let dotted = if key == DIR_KEY || key == FILE_KEY {
                String::new()
            } else {
                format!(".{}", key)
            };
            // 真实文件优先走真实路径的 Shell 提取（仅取关联图标）：
            // 与资源管理器同源，能解析每用户关联选择（含 AppX/UWP 商店应用，
            // 如 Win11 记事本接管 .txt）。伪文件名 + USEFILEATTRIBUTES 只查
            // HKCR 经典 ProgID，HKCR\.txt 无默认值且 AppX 图标不在经典注册表
            // 可读范围内，会错误回退到「通用文档」图标（与资源管理器不一致）。
            if key == DIR_KEY || key == FILE_KEY {
                // 普通文件夹共用一张通用图标（DIR_KEY 按类型共享）：这里必须用伪文件名
                // 走类型提取，不能按真实路径取图——否则第一个带 desktop.ini 自定义图标的
                // 文件夹会污染整个共享键，使目录内所有文件夹都显示成它的那张图标。
                // 桌面/下载/文档等已知特殊目录的专属图标由 special_dir_icon_cached
                // 按路径单独提取，不经过此分支。
                extract_type_icon(&dotted, key == DIR_KEY, size).or_else(|| extract(path, size))
            } else {
                extract_icon_only(path, size)
                    .or_else(|| extract_type_icon(&dotted, false, size))
                    .or_else(|| extract(path, size))
            }
        }
        (IconRequest::RealPath { path, .. }, Kind::Path(_)) => extract(path, size).or_else(|| {
            let ext = ext_of(path);
            let dotted = if ext.is_empty() {
                String::new()
            } else {
                format!(".{}", ext)
            };
            extract_type_icon(&dotted, false, size)
        }),
        (IconRequest::Type { extension, is_dir }, _) => {
            let ext = normalize_extension(extension);
            let dotted = if *is_dir || ext.is_empty() {
                String::new()
            } else {
                format!(".{}", ext)
            };
            extract_type_icon(&dotted, *is_dir, size)
        }
        (IconRequest::Device, _) => extract_device_icon(size),
        (IconRequest::DataDrive, _) => extract_data_drive_icon(size),
    };
    // 全部提取路径失败时的最终回退：Shell Stock 图标（文件夹/文档），
    // 保证行内始终显示系统图标而非内置矢量图。
    // 数据盘请求回退为固定驱动器 Stock 图标，保证与 D:/H: 同类的盘符外观，
    // 而非黄色文件夹或通用文档。
    let raw = match raw {
        Some(r) => Some(r),
        None => {
            if matches!(request, IconRequest::DataDrive) {
                extract_stock_drive_fixed(size)
            } else {
                let is_dir = matches!(
                    request,
                    IconRequest::RealPath {
                        is_dir: true,
                        ..
                    } | IconRequest::Type {
                        is_dir: true,
                        ..
                    }
                );
                stock_fallback(is_dir, size)
            }
        }
    };
    let (pixels, w, h) = valid_icon(raw)?;
    let bytes = pixels.len();
    let arc = Arc::new(IconPixels { pixels, w, h });
    match kind {
        Kind::Type(k) => {
            if let Ok(mut c) = type_cache().lock() {
                // 类型键空间有界（扩展名/特殊键），LRU 逐条淘汰兜底防异常膨胀
                c.insert(k, arc.clone(), bytes);
            }
        }
        Kind::Path(k) => {
            if let Ok(mut c) = path_cache().lock() {
                // 128px RGBA 图标约 64 KiB；按字节 LRU 限制具体路径缓存规模，
                // 避免浏览大量图片/视频后常驻内存持续增长（上限 32MB）。
                // 类型图标仍由独立缓存共享。
                c.insert(k, arc.clone(), bytes);
            }
        }
    }
    Some(arc)
}

#[cfg(not(windows))]
pub fn load_cached_request(_request: &IconRequest, _size: u32) -> Option<Arc<IconPixels>> {
    None
}

/// 获取图标像素：命中缓存直接返回，否则提取并写入缓存。供后台线程调用。
/// 失败不写缓存（允许后续重试），并对真实文件回退到扩展名类型图标，
/// 避免该行永远停留在内置矢量图。
#[cfg(windows)]
pub fn load_cached(path: &str, is_dir: bool, mtime: i64, size: u32) -> Option<Arc<IconPixels>> {
    load_cached_request(
        &IconRequest::RealPath {
            path: path.to_string(),
            is_dir,
            mtime,
        },
        size,
    )
}

#[cfg(not(windows))]
pub fn load_cached(_path: &str, _is_dir: bool, _mtime: i64, _size: u32) -> Option<Arc<IconPixels>> {
    None
}

/// 提取全部失败时的 Stock 回退：文件夹 → SIID_FOLDER，文件 → SIID_DOCNOASSOC（通用文档图标）。
#[cfg(windows)]
fn stock_fallback(is_dir: bool, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::UI::Shell::{SIID_DOCNOASSOC, SIID_FOLDER};
    extract_stock(if is_dir { SIID_FOLDER } else { SIID_DOCNOASSOC }, size)
}

/// 按"具体路径"提取特殊系统文件夹（桌面/下载/文档等）的专属图标。
/// 普通文件夹走 DIR_KEY 类型缓存共享同一张通用图标；这些已知文件夹在
/// Shell 中有带标识的专属图标，须以路径为键单独提取与缓存。
/// 实现：优先用 SHGetFileInfo 直接取真实路径的系统图标索引（不带
/// USEFILEATTRIBUTES），再经 IImageList JUMBO 取高清 HICON——与资源管理器
/// 同源，能拿到桌面/下载/文档等带标识的专属图标；IShellItemImageFactory
/// 的 GetImage 对文件夹常只返回通用黄色文件夹，故仅作回退。
#[cfg(windows)]
pub fn special_dir_icon_cached(path: &str, size: u32) -> Option<Arc<IconPixels>> {
    let key = format!("{}|specialdir", path);
    {
        let mut c = path_cache().lock().ok()?;
        if let Some(hit) = c.get_cloned(&key) {
            return Some(hit);
        }
    }
    // 主路径：真实路径图标（专属图标，如桌面的显示器、下载的箭头）
    // 回退：缩略图工厂（通用文件夹，至少不为空）
    let raw = extract_path_icon(path, size).or_else(|| extract(path, size));
    let (pixels, w, h) = valid_icon(raw)?;
    let bytes = pixels.len();
    let arc = Arc::new(IconPixels { pixels, w, h });
    if let Ok(mut c) = path_cache().lock() {
        c.insert(key, arc.clone(), bytes);
    }
    Some(arc)
}

#[cfg(not(windows))]
pub fn special_dir_icon_cached(_path: &str, _size: u32) -> Option<Arc<IconPixels>> {
    None
}

/// 仅查缓存的特殊目录图标（不触发提取，供 UI 线程 build_sidebar 无阻塞调用）。
/// 未命中返回 None，调用方回退内置 glyph；后台预热线程会填充缓存。
#[cfg(windows)]
pub fn special_dir_icon_cache_only(path: &str) -> Option<Arc<IconPixels>> {
    let key = format!("{}|specialdir", path);
    let mut c = path_cache().lock().ok()?;
    c.get_cloned(&key)
}

#[cfg(not(windows))]
pub fn special_dir_icon_cache_only(_path: &str) -> Option<Arc<IconPixels>> {
    None
}

/// 清空全部图标缓存（类型缓存 + 路径缓存）。
/// 用户在系统「打开方式」对话框更改默认应用后调用：文件类型关联图标已变，
/// 旧缓存必须失效，随后重载目录即可显示新图标。
/// 注意：调用方应在 UI 线程同步调用 `ui_bridge::clear_icon_image_cache`，
/// 否则 Slint 图像共享缓存仍持有旧像素的 Arc，旧图标继续显示。
pub fn clear_all_caches() {
    clear_type_cache();
    if let Ok(mut c) = path_cache().lock() {
        c.clear();
    }
}

/// 仅清空「按类型共享」的图标缓存（扩展名/文件夹/设备/Stock）。
/// 默认应用切换后的延迟补刷用：图片/视频真实缩略图不受关联变化影响，
/// 保留路径缓存可使第二次重载秒完成，只有类型图标重新提取。
pub fn clear_type_cache() {
    if let Ok(mut c) = type_cache().lock() {
        c.clear();
    }
}

/// 提取指定路径的缩略图/图标，返回 (RGBA 像素, 宽, 高)。
/// `size` 为期望的方形边长（像素）。失败返回 None。
#[cfg(windows)]
pub fn extract(path: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
        SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
    };

    // 本线程初始化 COM（后台线程必须）；已初始化时返回 S_FALSE，忽略即可。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }

    // 路径转宽字符（以 NUL 结尾）
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // 创建 IShellItemImageFactory
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(windows::core::PCWSTR(wide.as_ptr()), None).ok()?;

        // 请求缩略图/图标位图；RESIZETOFIT 保持纵横比，BIGGERSIZEOK 允许更大尺寸以提清晰度。
        // 缩略图提供器失败（部分格式缺失/损坏/被占用）时改用 ICONONLY 取类型关联图标，
        // 避免「部分文件」因提取失败而永远停留在内置矢量图。
        let hbitmap = factory
            .GetImage(
                SIZE {
                    cx: size as i32,
                    cy: size as i32,
                },
                SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK,
            )
            .or_else(|_| {
                factory.GetImage(
                    SIZE {
                        cx: size as i32,
                        cy: size as i32,
                    },
                    SIIGBF_ICONONLY,
                )
            })
            .ok()?;

        if hbitmap.is_invalid() {
            return None;
        }

        // 取位图尺寸
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(hbitmap.0));
            return None;
        }
        let w = bm.bmWidth as u32;
        let h = bm.bmHeight as u32;

        // 构造自顶向下 32bpp BITMAPINFO（biHeight 取负值得到 top-down 行序）
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bm.bmWidth,
            biHeight: -(bm.bmHeight), // 负值 = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];

        let hdc = GetDC(None);
        let scanned = GetDIBits(
            hdc,
            hbitmap,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(hbitmap.0));

        if scanned == 0 {
            return None;
        }

        // GetDIBits 输出为 BGRA，转为 RGBA；同时检测 alpha 是否全 0
        let mut any_alpha = false;
        let mut i = 0;
        while i < buf.len() {
            buf.swap(i, i + 2); // B <-> R
            if buf[i + 3] != 0 {
                any_alpha = true;
            }
            i += 4;
        }
        // 部分系统图标源位深 <32bpp，alpha 通道为 0，需强制不透明
        if !any_alpha {
            let mut j = 3;
            while j < buf.len() {
                buf[j] = 255;
                j += 4;
            }
        }

        Some((buf, w, h))
    }
}

/// 非 Windows 平台占位实现。
#[cfg(not(windows))]
pub fn extract(_path: &str, _size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// 按真实路径提取「文件类型关联图标」，跳过缩略图提供器（仅 SIIGBF_ICONONLY）。
///
/// 与 `extract` 的区别：强制只取图标，不走缩略图管线——供类型图标链路使用，
/// 避免 pdf/docx 等带预览处理器的文档从「关联图标」变成「内容缩略图」，
/// 与资源管理器小图标视图的行为保持一致。同样能解析每用户关联
/// （AppX/UWP 商店应用接管 .txt 等类型的场景）。
#[cfg(windows)]
pub fn extract_icon_only(path: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(windows::core::PCWSTR(wide.as_ptr()), None).ok()?;
        let hbitmap = factory
            .GetImage(
                SIZE {
                    cx: size as i32,
                    cy: size as i32,
                },
                SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;
        if hbitmap.is_invalid() {
            return None;
        }

        let mut bm = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(hbitmap.0));
            return None;
        }
        let w = bm.bmWidth as u32;
        let h = bm.bmHeight as u32;

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bm.bmWidth,
            biHeight: -(bm.bmHeight),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        let scanned = GetDIBits(
            hdc,
            hbitmap,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(hbitmap.0));
        if scanned == 0 {
            return None;
        }

        // BGRA → RGBA；alpha 全 0 时强制不透明（低色深图标源）
        let mut any_alpha = false;
        let mut i = 0;
        while i < buf.len() {
            buf.swap(i, i + 2);
            if buf[i + 3] != 0 {
                any_alpha = true;
            }
            i += 4;
        }
        if !any_alpha {
            let mut j = 3;
            while j < buf.len() {
                buf[j] = 255;
                j += 4;
            }
        }
        Some((buf, w, h))
    }
}

#[cfg(not(windows))]
pub fn extract_icon_only(_path: &str, _size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// 提取 Windows 通用便携设备图标。
#[cfg(windows)]
fn extract_device_icon(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::UI::Shell::SIID_DEVICECELLPHONE;
    extract_stock(SIID_DEVICECELLPHONE, size)
}

/// 固定驱动器 Stock 图标（SIID_DRIVEFIXED）：数据盘回退用，
/// 与 D:/H: 等数据盘的系统盘符外观同类（非 C 盘的 Windows 标识盘）。
#[cfg(windows)]
fn extract_stock_drive_fixed(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::UI::Shell::SIID_DRIVEFIXED;
    extract_stock(SIID_DRIVEFIXED, size)
}

#[cfg(not(windows))]
fn extract_stock_drive_fixed(_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// 通用数据盘图标：取首个非 C 盘固定盘的真实系统盘符图标，
/// 保证 WebDAV 等虚拟磁盘与 D:/H: 显示同一张图标。
/// 后台线程调用（可访问磁盘枚举）；命中缓存后 UI 线程秒回。
#[cfg(windows)]
fn extract_data_drive_icon(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    // 只取一次磁盘快照并全程复用：两次 cached_disks() 之间快照可能被并发填充，
    // 会让 list_disks 的兜底结果与下面的类型判定不一致
    let disks = crate::fs::disk::cached_disks();
    let mut roots: Vec<String> = disks.iter().map(|d| d.root.clone()).collect();
    if roots.is_empty() {
        roots = crate::fs::disk::list_disks().into_iter().map(|d| d.root).collect();
    }
    // 排序：固定盘优先、盘符升序，且 C: 永远排最后（取“除 C 以外的其它盘”）
    let mut candidates: Vec<(u8, String)> = Vec::new();
    // 若快照为空（首启），用 roots 兜底：D: 优先、C: 最后
    if disks.is_empty() {
        let mut sorted = roots;
        sorted.sort();
        // C: 沉底
        sorted.sort_by_key(|r| r.to_ascii_uppercase().starts_with("C:"));
        for r in sorted {
            if r.len() == 3 && r.as_bytes()[1] == b':' {
                // 首启未知类型，一律按普通盘处理（C: 除外优先）
                let prio = if r.to_ascii_uppercase().starts_with("C:") { 9 } else { 0 };
                candidates.push((prio, r));
            }
        }
    } else {
        for d in disks {
            if d.root.len() != 3 {
                continue;
            }
            let is_c = d.letter.eq_ignore_ascii_case("C");
            // 固定盘最优先（0），可移动/网络/其它次之（1），C 盘沉底（+10）
            let base = match d.kind {
                crate::fs::disk::DriveKind::Fixed => 0,
                _ => 1,
            };
            let prio = base + if is_c { 10 } else { 0 };
            candidates.push((prio, d.root));
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    }
    for (_, root) in candidates {
        if let Some(raw) = extract(&root, size) {
            if valid_icon(Some(raw.clone())).is_some() {
                return Some(raw);
            }
        }
        // 真实路径图标兜底（与特殊目录同源，资源管理器盘符图标）
        if let Some(raw) = extract_path_icon(&root, size) {
            if valid_icon(Some(raw.clone())).is_some() {
                return Some(raw);
            }
        }
    }
    // 本机仅有 C 盘或全部提取失败：回退固定驱动器 Stock 图标
    extract_stock_drive_fixed(size)
}

#[cfg(not(windows))]
fn extract_data_drive_icon(_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// 按真实路径取 Shell 系统图标（不带 USEFILEATTRIBUTES），
/// 专供桌面/下载/文档等已知文件夹与盘符：返回资源管理器同款专属图标，
/// 而非 IShellItemImageFactory 常给的通用黄色文件夹。
#[cfg(windows)]
pub fn extract_path_icon(path: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    };
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Controls::{IImageList, ILD_NORMAL};
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
        SHGFI_SYSICONINDEX, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};

    if size == 0 || path.is_empty() {
        return None;
    }
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
        // 真实路径：不带 USEFILEATTRIBUTES，Shell 按磁盘实际对象查图标
        // （已知文件夹的 desktop.ini / 注册表图标、盘符的卷图标均生效）。
        let mut info = SHFILEINFOW::default();
        let res = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if res == 0 {
            return None;
        }
        let icon_idx = info.iIcon;
        let mut hicon_opt: Option<HICON> = None;
        if size >= 48 {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_JUMBO as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }
        if hicon_opt.is_none() {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_EXTRALARGE as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }
        if hicon_opt.is_none() {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_LARGE as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }
        let hicon = match hicon_opt {
            Some(h) => h,
            None => {
                let mut info2 = SHFILEINFOW::default();
                let res2 = SHGetFileInfoW(
                    PCWSTR(wide.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    Some(&mut info2),
                    std::mem::size_of::<SHFILEINFOW>() as u32,
                    SHGFI_ICON | SHGFI_LARGEICON,
                );
                if res2 == 0 || info2.hIcon.is_invalid() {
                    return None;
                }
                info2.hIcon
            }
        };
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            Some(screen_dc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        );
        let Ok(dib) = dib else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        };
        if bits_ptr.is_null() || dib.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        }
        let prev = SelectObject(mem_dc, HGDIOBJ(dib.0));
        let _ = DrawIconEx(
            mem_dc,
            0,
            0,
            hicon,
            size as i32,
            size as i32,
            0,
            None,
            DI_NORMAL,
        );
        SelectObject(mem_dc, prev);
        let len = (size * size * 4) as usize;
        let mut buf = vec![0u8; len];
        std::ptr::copy_nonoverlapping(bits_ptr as *const u8, buf.as_mut_ptr(), len);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        let _ = DestroyIcon(hicon);
        let mut any_alpha = false;
        let mut i = 0;
        while i < buf.len() {
            buf.swap(i, i + 2);
            if buf[i + 3] != 0 {
                any_alpha = true;
            }
            i += 4;
        }
        if !any_alpha {
            let mut j = 3;
            while j < buf.len() {
                buf[j] = 255;
                j += 4;
            }
        }
        Some((buf, size, size))
    }
}

#[cfg(not(windows))]
pub fn extract_path_icon(_path: &str, _size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// 常用 Shell 备用（Stock）图标：侧栏系统节点用。
/// 回收站区分空/满两种状态（跟随系统实际状态变化）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StockIcon {
    RecyclerEmpty,
    RecyclerFull,
    Network,
}

/// 获取 Stock 图标像素（带类型缓存；空/满回收站分开缓存）。
#[cfg(windows)]
pub fn stock_icon_cached(which: StockIcon, size: u32) -> Option<Arc<IconPixels>> {
    use windows::Win32::UI::Shell::{SIID_MYNETWORK, SIID_RECYCLER, SIID_RECYCLERFULL};

    let (key, siid) = match which {
        StockIcon::RecyclerEmpty => ("\u{0}<stock:recycler>", SIID_RECYCLER),
        StockIcon::RecyclerFull => ("\u{0}<stock:recycler-full>", SIID_RECYCLERFULL),
        StockIcon::Network => ("\u{0}<stock:network>", SIID_MYNETWORK),
    };
    {
        let mut c = type_cache().lock().ok()?;
        if let Some(hit) = c.get_cloned(key) {
            return Some(hit);
        }
    }
    let (pixels, w, h) = extract_stock(siid, size)?;
    let bytes = pixels.len();
    let arc = Arc::new(IconPixels { pixels, w, h });
    if let Ok(mut c) = type_cache().lock() {
        c.insert(key.to_string(), arc.clone(), bytes);
    }
    Some(arc)
}

#[cfg(not(windows))]
pub fn stock_icon_cached(_which: StockIcon, _size: u32) -> Option<Arc<IconPixels>> {
    None
}

/// 通用 Stock 图标提取：SHGetStockIconInfo 取 HICON 后栅格化为 RGBA。
#[cfg(windows)]
fn extract_stock(
    siid: windows::Win32::UI::Shell::SHSTOCKICONID,
    size: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::UI::Shell::{
        SHGetStockIconInfo, SHGSI_ICON, SHGSI_LARGEICON, SHSTOCKICONINFO,
    };

    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = SHGetStockIconInfo(siid, SHGSI_ICON | SHGSI_LARGEICON, &mut info) {
            eprintln!("[icon-debug] SHGetStockIconInfo failed: {:?}", e);
            return None;
        }
    }
    if info.hIcon.is_invalid() {
        eprintln!("[icon-debug] stock hIcon invalid");
        return None;
    }
    match render_hicon(info.hIcon, size) {
        Some(r) => Some(r),
        None => {
            eprintln!("[icon-debug] render_hicon(stock) failed");
            None
        }
    }
}

#[cfg(not(windows))]
fn extract_device_icon(_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

#[cfg(windows)]
fn render_hicon(
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    size: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL};

    unsafe {
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            Some(screen_dc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        );
        let Ok(dib) = dib else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        };
        if bits_ptr.is_null() || dib.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        }

        let prev = SelectObject(mem_dc, HGDIOBJ(dib.0));
        let _ = DrawIconEx(
            mem_dc,
            0,
            0,
            hicon,
            size as i32,
            size as i32,
            0,
            None,
            DI_NORMAL,
        );
        SelectObject(mem_dc, prev);

        let len = (size * size * 4) as usize;
        let mut buf = vec![0u8; len];
        std::ptr::copy_nonoverlapping(bits_ptr as *const u8, buf.as_mut_ptr(), len);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        let _ = DestroyIcon(hicon);

        let mut any_alpha = false;
        let mut i = 0;
        while i < buf.len() {
            buf.swap(i, i + 2);
            if buf[i + 3] != 0 {
                any_alpha = true;
            }
            i += 4;
        }
        if !any_alpha {
            let mut j = 3;
            while j < buf.len() {
                buf[j] = 255;
                j += 4;
            }
        }
        Some((buf, size, size))
    }
}

/// 按"文件类型"提取系统关联图标（无需真实文件存在）。
///
/// 用于"新增"菜单等场景：根据扩展名（如 ".docx"）取该类型在系统注册的图标。
/// `ext` 为空字符串时取文件夹图标。通过 `SHGFI_USEFILEATTRIBUTES` 让 Shell 仅按
/// 扩展名/属性查关联图标，不访问磁盘上的具体文件。
///
/// 图标质量策略：优先取 SHIL_JUMBO(256x256) → SHIL_EXTRALARGE(48x48) → SHIL_LARGE(32x32)，
/// 用最高可用的分辨率源图标在目标尺寸下绘制，避免 32px 源被放大到 128px 时的模糊。
/// 返回 (RGBA 像素, 宽, 高)，失败返回 None（调用方回退到内置矢量图）。
#[cfg(windows)]
pub fn extract_type_icon(ext: &str, is_dir: bool, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Controls::{IImageList, ILD_NORMAL};
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
        SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};

    if size == 0 {
        return None;
    }
    // 目录与无扩展名普通文件都没有扩展名，必须由调用方明确区分属性。
    let name = if is_dir {
        "folder".to_string()
    } else if ext.is_empty() {
        "file".to_string()
    } else {
        format!("x{}", ext)
    };
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // IImageList 是 COM 接口，后台线程调用时需初始化 COM（已初始化则忽略返回）
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);

        let mut info = SHFILEINFOW::default();
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        // 第一步：获取系统图标索引（不取 HICON，仅取 iIcon 供 IImageList 查找）
        let res = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        );
        if res == 0 {
            return None;
        }
        let icon_idx = info.iIcon;

        // 第二步：从高分辨率图像列表取 HICON（JUMBO=256, EXTRALARGE=48, LARGE=32）
        // 优先用大于目标尺寸的源图标，DrawIconEx 缩小绘制比放大清晰得多
        let mut hicon_opt: Option<HICON> = None;
        if size >= 48 {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_JUMBO as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }
        if hicon_opt.is_none() {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_EXTRALARGE as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }
        if hicon_opt.is_none() {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_LARGE as i32) {
                if let Ok(ic) = list.GetIcon(icon_idx, ILD_NORMAL.0 as u32) {
                    hicon_opt = Some(ic);
                }
            }
        }

        // 回退：IImageList 全部失败时，用 SHGFI_ICON|SHGFI_LARGEICON 直接拿 32x32 HICON
        let hicon = match hicon_opt {
            Some(h) => h,
            None => {
                let mut info2 = SHFILEINFOW::default();
                let res2 = SHGetFileInfoW(
                    PCWSTR(wide.as_ptr()),
                    attrs,
                    Some(&mut info2),
                    std::mem::size_of::<SHFILEINFOW>() as u32,
                    SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
                );
                if res2 == 0 || info2.hIcon.is_invalid() {
                    return None;
                }
                info2.hIcon
            }
        };

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        // 创建零初始化的 32bpp 顶向下 DIB（透明背景），DrawIconEx 会按 alpha 混合到其上
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            Some(screen_dc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        );
        let Ok(dib) = dib else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        };
        if bits_ptr.is_null() || dib.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return None;
        }

        let prev = SelectObject(mem_dc, HGDIOBJ(dib.0));
        let _ = DrawIconEx(
            mem_dc,
            0,
            0,
            hicon,
            size as i32,
            size as i32,
            0,
            None,
            DI_NORMAL,
        );
        SelectObject(mem_dc, prev);

        // 从 DIB 内存读出像素（BGRA，顶向下），拷贝出来后再释放 GDI 资源
        let len = (size * size * 4) as usize;
        let mut buf = vec![0u8; len];
        std::ptr::copy_nonoverlapping(bits_ptr as *const u8, buf.as_mut_ptr(), len);

        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        let _ = DestroyIcon(hicon);

        // BGRA → RGBA
        let mut any_alpha = false;
        let mut i = 0;
        while i < buf.len() {
            buf.swap(i, i + 2);
            if buf[i + 3] != 0 {
                any_alpha = true;
            }
            i += 4;
        }
        // 整图无 alpha：强制不透明，避免整块透明（白板）
        if !any_alpha {
            let mut j = 3;
            while j < buf.len() {
                buf[j] = 255;
                j += 4;
            }
        }

        Some((buf, size, size))
    }
}

#[cfg(not(windows))]
pub fn extract_type_icon(_ext: &str, _is_dir: bool, _size: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LRU 基本行为：容量内保留全部；超限时从最旧访问的条目开始
    /// 淘汰到预算的 3/4，最近访问的条目存活（整体清空策略做不到）。
    #[test]
    fn lru_evicts_oldest_and_keeps_recent() {
        let mut lru: LruMap<u32> = LruMap::new(8, usize::MAX);
        for (i, k) in ["a", "b", "c", "d", "e", "f", "g", "h"].iter().enumerate() {
            lru.insert(k.to_string(), i as u32, 10);
        }
        // 访问 a：a 成为最新
        assert_eq!(lru.get_cloned("a"), Some(0));
        lru.insert("i".into(), 9, 10);
        assert_eq!(lru.get_cloned("a"), Some(0), "最近访问的 a 必须存活");
        assert_eq!(lru.get_cloned("b"), None, "最旧的 b 应被淘汰");
        assert_eq!(lru.get_cloned("c"), None, "摊销淘汰到 3/4，次旧一并淘汰");
        assert_eq!(lru.get_cloned("i"), Some(9));
        assert_eq!(lru.get_cloned("h"), Some(7));
    }

    /// 字节预算同样触发淘汰（大图标不应把普通图标全部挤掉后仍超限）。
    #[test]
    fn lru_respects_byte_budget() {
        let mut lru: LruMap<u32> = LruMap::new(usize::MAX, 100);
        lru.insert("a".into(), 1, 40);
        lru.insert("b".into(), 2, 40);
        assert_eq!(lru.bytes, 80);
        lru.insert("c".into(), 3, 40);
        assert!(
            lru.bytes <= 75,
            "超预算后应淘汰到 3/4 目标以下，实际 {}",
            lru.bytes
        );
        assert_eq!(lru.get_cloned("c"), Some(3), "最新条目必须存活");
    }

    /// 同键替换按字节差量记账，避免重复键导致预算虚增。
    #[test]
    fn lru_replace_accounts_byte_delta() {
        let mut lru: LruMap<u32> = LruMap::new(10, usize::MAX);
        lru.insert("a".into(), 1, 40);
        lru.insert("a".into(), 2, 60);
        assert_eq!(lru.bytes, 60);
        assert_eq!(lru.map.len(), 1);
    }

    fn kind_key(request: &IconRequest) -> (bool, String) {
        match request_kind(request) {
            Kind::Type(key) => (true, key),
            Kind::Path(key) => (false, key),
        }
    }

    #[test]
    fn valid_icon_rejects_empty_or_truncated_buffers() {
        assert!(valid_icon(Some((Vec::new(), 0, 0))).is_none());
        assert!(valid_icon(Some((vec![0; 3], 1, 1))).is_none());
        assert!(valid_icon(Some((vec![0; 4], 1, 1))).is_none());
        assert!(valid_icon(Some((vec![0, 0, 0, 255], 1, 1))).is_some());
    }

    #[test]
    fn type_extensions_are_normalized() {
        let dotted = IconRequest::Type {
            extension: ".PDF".into(),
            is_dir: false,
        };
        let plain = IconRequest::Type {
            extension: "pdf".into(),
            is_dir: false,
        };
        assert_eq!(kind_key(&dotted), kind_key(&plain));
        assert_eq!(kind_key(&plain), (true, "pdf".into()));
    }

    #[test]
    fn virtual_entries_use_type_or_device_keys() {
        let folder = IconRequest::Type {
            extension: String::new(),
            is_dir: true,
        };
        let plain_file = IconRequest::Type {
            extension: String::new(),
            is_dir: false,
        };
        assert_eq!(kind_key(&folder), (true, DIR_KEY.into()));
        assert_eq!(kind_key(&plain_file), (true, FILE_KEY.into()));
        assert_ne!(kind_key(&folder), kind_key(&plain_file));
        assert_eq!(kind_key(&IconRequest::Device), (true, DEVICE_KEY.into()));
    }

    #[test]
    fn local_media_keeps_path_and_mtime_key() {
        let request = IconRequest::RealPath {
            path: r"C:\photos\sample.JPG".into(),
            is_dir: false,
            mtime: 42,
        };
        assert_eq!(
            kind_key(&request),
            (false, r"C:\photos\sample.JPG|42".into())
        );
    }

    #[test]
    fn shared_type_requests_are_distinguished_for_two_phase_fill() {
        // 扩展名/文件夹走共享缓存（首阶段先行），具体文件走路径缓存（后阶段跟进）
        assert!(request_is_shared_type(&IconRequest::Type {
            extension: "pdf".into(),
            is_dir: false,
        }));
        assert!(request_is_shared_type(&IconRequest::RealPath {
            path: r"C:\a\b.pdf".into(),
            is_dir: false,
            mtime: 1,
        }));
        assert!(!request_is_shared_type(&IconRequest::RealPath {
            path: r"C:\photos\sample.JPG".into(),
            is_dir: false,
            mtime: 1,
        }));
        assert!(request_is_shared_type(&IconRequest::Device));
    }

    /// 文件夹必须能通过公开入口拿到系统图标。
    /// 回归背景：图标来源设为「系统图标」时，文件夹若提取失败就会回退到内置
    /// 蓝色 Finder 矢量图，与资源管理器不一致。
    #[cfg(windows)]
    #[test]
    fn folder_icon_resolves_through_public_entry() {
        let dir = std::env::temp_dir();
        let req = IconRequest::RealPath {
            path: dir.to_string_lossy().to_string(),
            is_dir: true,
            mtime: 0,
        };
        assert_eq!(kind_key(&req), (true, DIR_KEY.into()), "文件夹应走共享类型缓存");
        let icon = load_cached_request(&req, 128).expect("文件夹应能取到系统图标");
        assert!(icon.w > 0 && icon.h > 0);
    }

    /// 诊断：后台线程中提取文件夹图标（复现 spawn_thumbnails 的运行环境）。
    /// 主线程能取到黄色图标、GUI 却渲染内置蓝色矢量，差异只可能来自线程环境
    /// （COM 初始化模式不同）或缓存中的旧图。
    #[cfg(windows)]
    #[test]
    fn diag_folder_icon_in_background_thread() {
        fn dominant(pixels: &[u8]) -> (u64, u64, u64, usize) {
            let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0usize);
            let mut i = 0;
            while i < pixels.len() {
                if pixels[i + 3] > 128 {
                    r += pixels[i] as u64;
                    g += pixels[i + 1] as u64;
                    b += pixels[i + 2] as u64;
                    n += 1;
                }
                i += 4;
            }
            if n == 0 {
                (0, 0, 0, 0)
            } else {
                (r / n as u64, g / n as u64, b / n as u64, n)
            }
        }
        // 清掉共享类型缓存，保证后台线程真的走到提取路径而非命中他测试写入的旧图
        clear_type_cache();
        let dir = std::env::temp_dir().to_string_lossy().to_string();
        let handle = std::thread::spawn(move || {
            let req = IconRequest::RealPath {
                path: dir,
                is_dir: true,
                mtime: 0,
            };
            load_cached_request(&req, 128).map(|ic| (dominant(&ic.pixels), ic.w, ic.h))
        });
        let got = handle.join().expect("后台线程不应 panic");
        println!("DIAG background folder={:?}", got);
        if let Some(((r, g, b, n), w, h)) = got {
            println!(
                "DIAG background size={}x{} pixels={} rgb=({},{},{}) is_yellowish={}",
                w,
                h,
                n,
                r,
                g,
                b,
                r > b && g > b
            );
        }
        assert!(got.is_some(), "后台线程必须能取到文件夹图标");
    }
}
