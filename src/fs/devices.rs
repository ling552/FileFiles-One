//! 便携设备枚举与浏览：WPD（Windows Portable Devices）COM 接口
//! - list_devices：枚举手机/平板/相机（IPortableDeviceManager），自动排除第三方命名空间扩展
//! - list_content：进入设备内部，枚举某对象下的子对象（文件夹/文件），实现"像资源管理器一样浏览"
//! - copy_to_temp：把设备上的文件复制到临时目录，供系统默认程序打开
//! - parent_path：取某对象的父对象虚拟路径，供"向上"导航
//! - 写操作：create_folder / create_file / rename / delete / pull_tree / push_tree / move_objects
//!   （新建、重命名、删除，以及设备↔电脑、设备内部的复制/移动）
//!
//! 虚拟路径编码：
//!   device://<deviceId>                 设备根（枚举 WPD_DEVICE_OBJECT_ID="DEVICE" 的子对象）
//!   device://<deviceId>\u{1}<objectId>  设备内部某对象（\u{1} SOH 作分隔，因对象 ID 可含任意字符）

use super::metadata::Entry;

/// 设备路径中 deviceId 与 objectId 的分隔符（SOH，对象 ID 不会包含该控制字符）
pub const SEP: char = '\u{1}';

/// 协议判定缓存：device_id → (是否 USB 大容量存储, 负缓存过期时间)。
/// 打开设备读协议有数十毫秒开销，而枚举随 1.5s 热插拔轮询在 UI 线程高频调用：
/// 成功判定永久缓存（同一设备协议不变，过期时间为 None）；读取失败（设备忙/
/// 锁屏拒绝）记短期负缓存（30s 内不重试），避免慢设备让界面每轮轮询都卡顿。
#[cfg(windows)]
static MSC_CACHE: std::sync::Mutex<
    Option<std::collections::HashMap<String, (bool, Option<std::time::Instant>)>>,
> = std::sync::Mutex::new(None);

/// 设备友好名缓存：device_id → 名称（来自最近一次枚举）。
/// 标签页标题 / 面包屑经 virtualfs::friendly_title 查询，避免显示原始设备 ID。
#[cfg(windows)]
static NAME_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
    std::sync::Mutex::new(None);

/// 最近一次完整枚举得到的设备快照。侧边栏重建只读此缓存，避免导航时同步访问 WPD。
#[cfg(windows)]
static DEVICE_CACHE: std::sync::Mutex<Vec<Entry>> = std::sync::Mutex::new(Vec::new());

pub fn cached_devices() -> Vec<Entry> {
    #[cfg(windows)]
    {
        DEVICE_CACHE.lock().map(|v| v.clone()).unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 查询设备友好名（最近一次枚举的缓存）。未知设备返回 None。
pub fn friendly_name(device_id: &str) -> Option<String> {
    #[cfg(windows)]
    {
        NAME_CACHE.lock().ok()?.as_ref()?.get(device_id).cloned()
    }
    #[cfg(not(windows))]
    {
        let _ = device_id;
        None
    }
}

pub fn list_devices() -> Vec<Entry> {
    #[cfg(windows)]
    {
        list_devices_win().unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 解析 device:// 虚拟路径，返回 (deviceId, objectId)。
/// 无 objectId 时默认设备根对象 "DEVICE"。返回 None 表示不是合法 device:// 路径。
pub fn parse(vpath: &str) -> Option<(String, String)> {
    let rest = vpath.strip_prefix("device://")?;
    match rest.split_once(SEP) {
        Some((dev, obj)) => Some((dev.to_string(), obj.to_string())),
        None => Some((rest.to_string(), "DEVICE".to_string())),
    }
}

/// 列出设备内某对象下的子项（文件夹/文件）。失败或非 device:// 路径返回空。
pub fn list_content(vpath: &str) -> Vec<Entry> {
    #[cfg(windows)]
    {
        list_content_win(vpath).unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        let _ = vpath;
        Vec::new()
    }
}

/// 把设备上的文件复制到临时目录，返回本地临时文件路径（供系统默认程序打开）。
pub fn copy_to_temp(vpath: &str) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        copy_to_temp_win(vpath).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = vpath;
        None
    }
}

/// 取某对象的父对象虚拟路径，供"向上"导航。已在设备根（objectId=="DEVICE"）时返回 None。
pub fn parent_path(vpath: &str) -> Option<String> {
    #[cfg(windows)]
    {
        parent_path_win(vpath)
    }
    #[cfg(not(windows))]
    {
        let _ = vpath;
        None
    }
}

#[cfg(windows)]
fn pwstr_read(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // WPD 返回的字符串按约定以 NUL 结尾；异常设备/驱动返回未终结缓冲时
    // 无界扫描会越过分配边界读。64K 字符上限兜底（远超真实设备名/对象名长度）
    const MAX_CHARS: usize = 64 * 1024;
    let mut len = 0usize;
    unsafe {
        while len < MAX_CHARS && *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len)).to_string()
    }
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn list_devices_win() -> windows::core::Result<Vec<Entry>> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::{IPortableDeviceManager, PortableDeviceManager};
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};

    // COM 由调用方所在线程负责初始化：
    // - 后台轮询线程：在入口处 CoInitializeEx(COINIT_MULTITHREADED)
    // - UI 线程：winit 的 OleInitialize 已将线程设为 STA
    // 此处不再自行初始化，否则在 UI 线程上用 MTA 会与 winit 的 STA 冲突（RPC_E_CHANGED_MODE）。
    let manager: IPortableDeviceManager =
        unsafe { CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)? };

    // WPD manager 在进程内缓存设备列表：不刷新的话，已拔出的设备会一直被枚举出来
    // （表现为「拔下设备后侧边栏依旧显示」），必须先显式刷新
    let _ = unsafe { manager.RefreshDeviceList() };

    let mut count = 0u32;
    unsafe { manager.GetDevices(std::ptr::null_mut(), &mut count)? };
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut ids: Vec<PWSTR> = vec![PWSTR(std::ptr::null_mut()); count as usize];
    unsafe { manager.GetDevices(ids.as_mut_ptr(), &mut count)? };

    let mut entries = Vec::new();
    for id in ids.iter().take(count as usize) {
        let id_str = pwstr_read(id.0);
        unsafe { CoTaskMemFree(Some(id.0.cast())) };

        // 排除 USB 大容量存储（协议 "MSC"）：它们已作为驱动器出现在「此电脑」，
        // WPD 影子设备的内容只是一个盘符对象，进入后等于绕道浏览本地磁盘
        if is_mass_storage(&id_str) {
            continue;
        }

        let id_wide = to_wide(&id_str);
        let pc_id = PCWSTR(id_wide.as_ptr());

        let mut name_len = 0u32;
        let _ = unsafe {
            manager.GetDeviceFriendlyName(pc_id, PWSTR(std::ptr::null_mut()), &mut name_len)
        };
        let name = if name_len > 0 {
            let mut buf: Vec<u16> = vec![0u16; name_len as usize];
            let ok = unsafe {
                manager.GetDeviceFriendlyName(pc_id, PWSTR(buf.as_mut_ptr()), &mut name_len)
            }
            .is_ok();
            if ok {
                pwstr_read(buf.as_ptr())
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        // 取不到友好名时退回通用名称（原始设备 ID 太长且无可读性，不再直接展示）
        let name = if name.trim().is_empty() {
            "便携设备".to_string()
        } else {
            name
        };

        // 写入友好名缓存，供标签页标题 / 面包屑查询
        if let Ok(mut cache) = NAME_CACHE.lock() {
            cache
                .get_or_insert_with(std::collections::HashMap::new)
                .insert(id_str.clone(), name.clone());
        }

        entries.push(Entry {
            name,
            path: format!("device://{}", id_str),
            is_dir: true,
            size_bytes: 0,
            modified_ts: 0,
            kind: "便携设备".to_string(),
            icon_label: String::new(),
            icon_class: "device".into(),
        });
    }

    if let Ok(mut cache) = DEVICE_CACHE.lock() {
        *cache = entries.clone();
    }
    Ok(entries)
}

/// 读取设备协议字符串（如 "MTP: 1.00" / "PTP: 1.00" / "MSC:"）。失败返回 None。
#[cfg(windows)]
fn read_protocol(device_id: &str) -> Option<String> {
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceKeyCollection, PortableDeviceKeyCollection, WPD_DEVICE_OBJECT_ID,
        WPD_DEVICE_PROTOCOL,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    let device = open_device(device_id).ok()?;
    let content = unsafe { device.Content() }.ok()?;
    let props = unsafe { content.Properties() }.ok()?;
    let keys: IPortableDeviceKeyCollection =
        unsafe { CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER) }
            .ok()?;
    unsafe { keys.Add(&WPD_DEVICE_PROTOCOL) }.ok()?;
    let vals = unsafe { props.GetValues(WPD_DEVICE_OBJECT_ID, &keys) }.ok()?;
    let s = get_string(&vals, &WPD_DEVICE_PROTOCOL);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 判定设备是否为 USB 大容量存储（协议以 "MSC" 开头）。
/// 成功判定永久缓存；读不到协议（设备忙/刚拔出）保守视为便携设备并记 30s 负缓存。
#[cfg(windows)]
fn is_mass_storage(device_id: &str) -> bool {
    if let Ok(cache) = MSC_CACHE.lock() {
        if let Some((v, expires)) = cache.as_ref().and_then(|m| m.get(device_id)) {
            match expires {
                None => return *v,
                Some(t) if std::time::Instant::now() < *t => return *v,
                Some(_) => {} // 负缓存过期，重新判定
            }
        }
    }
    let (msc, expires) = match read_protocol(device_id) {
        Some(p) => (p.trim_start().to_ascii_uppercase().starts_with("MSC"), None),
        None => (
            false,
            Some(std::time::Instant::now() + std::time::Duration::from_secs(30)),
        ),
    };
    if let Ok(mut cache) = MSC_CACHE.lock() {
        cache
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(device_id.to_string(), (msc, expires));
    }
    msc
}

/// 打开设备并返回 IPortableDevice。设置最小客户端信息（WPD_CLIENT_NAME）。
#[cfg(windows)]
fn open_device(
    device_id: &str,
) -> windows::core::Result<windows::Win32::Devices::PortableDevices::IPortableDevice> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        IPortableDevice, IPortableDeviceValues, PortableDevice, PortableDeviceValues,
        WPD_CLIENT_NAME,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    let client: IPortableDeviceValues =
        unsafe { CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)? };
    let app_name = to_wide("FileFiles One");
    unsafe { client.SetStringValue(&WPD_CLIENT_NAME, PCWSTR(app_name.as_ptr()))? };

    let device: IPortableDevice =
        unsafe { CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)? };
    let dev_wide = to_wide(device_id);
    unsafe { device.Open(PCWSTR(dev_wide.as_ptr()), &client)? };
    Ok(device)
}

/// 读取对象的字符串属性，自动释放 COM 字符串。
#[cfg(windows)]
fn get_string(
    vals: &windows::Win32::Devices::PortableDevices::IPortableDeviceValues,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> String {
    use windows::Win32::System::Com::CoTaskMemFree;
    unsafe {
        match vals.GetStringValue(key) {
            Ok(p) => {
                let s = pwstr_read(p.0);
                CoTaskMemFree(Some(p.0.cast()));
                s
            }
            Err(_) => String::new(),
        }
    }
}

#[cfg(windows)]
fn list_content_win(vpath: &str) -> windows::core::Result<Vec<Entry>> {
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceKeyCollection, PortableDeviceKeyCollection, WPD_CONTENT_TYPE_FOLDER,
        WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_DATE_MODIFIED,
        WPD_OBJECT_NAME, WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Variant::VT_DATE;

    let (device_id, parent_obj) = match parse(vpath) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };

    let device = open_device(&device_id)?;
    let content = unsafe { device.Content()? };
    let props = unsafe { content.Properties()? };

    // 只取需要的属性，减少跨进程往返
    let keys: IPortableDeviceKeyCollection =
        unsafe { CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        keys.Add(&WPD_OBJECT_NAME)?;
        keys.Add(&WPD_OBJECT_ORIGINAL_FILE_NAME)?;
        keys.Add(&WPD_OBJECT_CONTENT_TYPE)?;
        keys.Add(&WPD_OBJECT_SIZE)?;
        keys.Add(&WPD_OBJECT_DATE_MODIFIED)?;
    }

    let parent_wide = to_wide(&parent_obj);
    let enumr = unsafe { content.EnumObjects(0, PCWSTR(parent_wide.as_ptr()), None)? };

    let mut entries = Vec::new();
    let mut objids: [windows::core::PWSTR; 32] = [windows::core::PWSTR(std::ptr::null_mut()); 32];
    loop {
        let mut fetched = 0u32;
        let _hr = unsafe { enumr.Next(&mut objids, &mut fetched) };
        if fetched == 0 {
            break;
        }
        for slot in objids.iter().take(fetched as usize) {
            let oid_ptr = slot.0;
            let oid_str = pwstr_read(oid_ptr);
            let oid_wide = to_wide(&oid_str);

            if let Ok(vals) = unsafe { props.GetValues(PCWSTR(oid_wide.as_ptr()), &keys) } {
                let orig = get_string(&vals, &WPD_OBJECT_ORIGINAL_FILE_NAME);
                let name = if !orig.is_empty() {
                    orig
                } else {
                    get_string(&vals, &WPD_OBJECT_NAME)
                };
                let ctype =
                    unsafe { vals.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }.unwrap_or_default();
                let is_dir =
                    ctype == WPD_CONTENT_TYPE_FOLDER || ctype == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;
                let size = if is_dir {
                    0
                } else {
                    unsafe { vals.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE) }.unwrap_or(0)
                };
                // 修改日期：WPD 以 OLE DATE（自 1899-12-30 起的天数，本地墙钟时间）返回。
                // 先换算成"本地秒"，再按该时间点当时生效的时区规则（含夏令时）转 unix 秒
                // （显示层会再做一次 UTC→本地转换，不校正会偏移一个时区）。
                // 缺失时保持 0（UI 对 0 显示为空而非 1970）。
                let modified_ts = unsafe { vals.GetValue(&WPD_OBJECT_DATE_MODIFIED) }
                    .ok()
                    .and_then(|mut pv| {
                        let local_secs = unsafe {
                            let inner = &pv.Anonymous.Anonymous;
                            if inner.vt == VT_DATE {
                                // 25569 = 1899-12-30 与 1970-01-01 之间的天数
                                Some(((inner.Anonymous.date - 25569.0) * 86400.0) as i64)
                            } else {
                                None
                            }
                        };
                        // COM 契约：GetValue 返回的 PROPVARIANT 由调用方释放。
                        // VT_DATE 是标量本为 no-op，但不合规驱动可能返回分配型变体
                        unsafe {
                            let _ =
                                windows::Win32::System::Com::StructuredStorage::PropVariantClear(
                                    &mut pv,
                                );
                        }
                        let local_secs = local_secs?;
                        use chrono::TimeZone;
                        let naive = chrono::DateTime::from_timestamp(local_secs, 0)?.naive_utc();
                        chrono::Local
                            .from_local_datetime(&naive)
                            .earliest()
                            .map(|dt| dt.timestamp())
                    })
                    .filter(|&ts| ts > 0)
                    .unwrap_or(0);

                let (icon_class, icon_label, kind) =
                    super::metadata::classify(Path::new(&name), is_dir);
                entries.push(Entry {
                    name,
                    path: format!("device://{}{}{}", device_id, SEP, oid_str),
                    is_dir,
                    size_bytes: size,
                    modified_ts,
                    kind,
                    icon_label,
                    icon_class,
                });
            }

            unsafe { CoTaskMemFree(Some(oid_ptr.cast())) };
        }
    }

    Ok(entries)
}

#[cfg(windows)]
fn copy_to_temp_win(vpath: &str) -> Result<std::path::PathBuf, String> {
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    };

    // 本函数在后台线程被调用（open_device_file 的 spawn），必须先初始化本线程 COM；
    // 已初始化时返回 S_FALSE，忽略即可（与 thumbnail.rs 后台线程一致）
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }

    let (device_id, obj) = parse(vpath).ok_or("非 device:// 路径")?;
    if obj == "DEVICE" {
        return Err("设备根不可作为文件打开".to_string());
    }

    let s = session(&device_id)?;
    let name = object_info_in(&s, &device_id, &obj)
        .map(|(n, _, _)| n)
        .unwrap_or_default();
    let fname = sanitize_filename(&name, &obj);

    let dir = std::env::temp_dir().join("FileFiles One_mtp");
    let _ = std::fs::create_dir_all(&dir);
    // 独占创建（create_new，冲突时追加序号）：可预测的固定路径 + 截断式
    // File::create 会被同机低权限进程预置同名文件/junction 抢占（TOCTOU 内容替换）
    let (out_path, mut file) = create_exclusive(&dir, &fname)
        .map_err(|e| format!("创建临时文件失败: {e}"))?;

    let (stream, optimal) = open_read_stream(&s, &obj)?;
    drain_stream(&stream, optimal, &mut file, &mut NoSink)?;
    Ok(out_path)
}

/// 在 dir 下独占创建 fname（已存在则在扩展名前追加 .1/.2/… 序号避让），
/// 返回最终路径与写入句柄。
#[cfg(windows)]
fn create_exclusive(
    dir: &std::path::Path,
    fname: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    for i in 0u32.. {
        let name = if i == 0 {
            fname.to_string()
        } else {
            match fname.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{i}.{ext}"),
                _ => format!("{fname}.{i}"),
            }
        };
        let cand = dir.join(&name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cand)
        {
            Ok(f) => return Ok((cand, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("序号冲突循环必然在 u32 范围内找到可用文件名")
}

/// 清洗文件名中的非法字符；为空时用对象 ID 兜底。
#[cfg(windows)]
fn sanitize_filename(name: &str, fallback_obj: &str) -> String {
    let base = if name.trim().is_empty() {
        fallback_obj
    } else {
        name
    };
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        "mtp_file".to_string()
    } else {
        cleaned
    }
}

// ══════════════════════════════════════════════════════════════════════
//  写操作：新建 / 重命名 / 删除 / 传输（设备↔电脑、设备内部）
//
//  WPD 的写入全部走 IPortableDeviceContent：
//    - CreateObjectWithPropertiesOnly  建文件夹（无数据流的对象）
//    - CreateObjectWithPropertiesAndData + IStream::Commit  建文件并写入内容
//    - Delete（WITH_RECURSION）        删除对象及其子树
//    - Move / Copy                     设备内部搬移（可选命令，部分设备不支持）
//    - IPortableDeviceProperties::SetValues  改名
//
//  这些函数会打开设备（数十毫秒），因此以「一次操作一个会话」的粒度复用句柄；
//  调用线程必须已初始化 COM（UI 线程由 winit 置为 STA，后台任务线程自行 MTA）。
// ══════════════════════════════════════════════════════════════════════

/// 传输进度与取消回调。由调用方（后台任务）实现，设备侧只负责按块上报。
pub trait Sink {
    /// 开始传输一个文件：名称与总字节数（未知时为 0）
    fn begin_file(&mut self, name: &str, size: u64);
    /// 已传输 `bytes` 字节。返回 false 表示请求取消，传输会尽快中止。
    fn advance(&mut self, bytes: u64) -> bool;
    /// 当前文件传输结束（成功落地）
    fn end_file(&mut self) {}
}

/// 不关心进度的场景（新建空文件等）使用的空实现
pub struct NoSink;
impl Sink for NoSink {
    fn begin_file(&mut self, _name: &str, _size: u64) {}
    fn advance(&mut self, _bytes: u64) -> bool {
        true
    }
}

/// 判断路径是否位于便携设备内（含设备根）
pub fn is_device_path(path: &str) -> bool {
    path.starts_with("device://")
}

/// 取设备 ID（不含对象部分）；非 device:// 路径返回 None
pub fn device_id_of(vpath: &str) -> Option<String> {
    parse(vpath).map(|(d, _)| d)
}

/// 拼接子对象虚拟路径
pub fn child_vpath(device_id: &str, object_id: &str) -> String {
    format!("device://{}{}{}", device_id, SEP, object_id)
}

macro_rules! device_op {
    ($body:expr, $fallback:expr) => {{
        #[cfg(windows)]
        {
            $body
        }
        #[cfg(not(windows))]
        {
            $fallback
        }
    }};
}

/// 在设备目录下新建文件夹，成功返回新对象的虚拟路径
pub fn create_folder(parent_vpath: &str, name: &str) -> Result<String, String> {
    device_op!(
        create_object_win(parent_vpath, name, None, &mut NoSink),
        {
            let _ = (parent_vpath, name);
            Err("当前平台不支持便携设备".to_string())
        }
    )
}

/// 在设备目录下新建空文件，成功返回新对象的虚拟路径
pub fn create_file(parent_vpath: &str, name: &str) -> Result<String, String> {
    device_op!(
        create_object_win(parent_vpath, name, Some(std::path::Path::new("")), &mut NoSink),
        {
            let _ = (parent_vpath, name);
            Err("当前平台不支持便携设备".to_string())
        }
    )
}

/// 重命名设备上的对象
pub fn rename(vpath: &str, new_name: &str) -> Result<(), String> {
    device_op!(rename_win(vpath, new_name), {
        let _ = (vpath, new_name);
        Err("当前平台不支持便携设备".to_string())
    })
}

/// 删除设备上的对象（含子树）。所有 vpath 必须属于同一设备。
pub fn delete(vpaths: &[String]) -> Result<(), String> {
    device_op!(delete_win(vpaths), {
        let _ = vpaths;
        Err("当前平台不支持便携设备".to_string())
    })
}

/// 查找父目录下的同名子项，返回 (虚拟路径, 是否文件夹, 大小)
pub fn child_named(parent_vpath: &str, name: &str) -> Option<(String, bool, u64)> {
    let target = name.trim();
    list_content(parent_vpath)
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(target))
        .map(|e| (e.path, e.is_dir, e.size_bytes))
}

/// 设备 → 本地：把对象（文件或整个文件夹）复制到本地目录下。
/// `as_name` 为 Some 时改用该名称落地（冲突时「保留两者」用）。
pub fn pull_tree(
    vpath: &str,
    dst_dir: &std::path::Path,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    device_op!(pull_tree_win(vpath, dst_dir, as_name, sink), {
        let _ = (vpath, dst_dir, as_name, sink);
        Err("当前平台不支持便携设备".to_string())
    })
}

/// 本地 → 设备：把本地文件或目录复制到设备目录下。
/// `as_name` 为 Some 时改用该名称写入（冲突时「保留两者」用）。
pub fn push_tree(
    src: &std::path::Path,
    parent_vpath: &str,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    device_op!(push_tree_win(src, parent_vpath, as_name, sink), {
        let _ = (src, parent_vpath, as_name, sink);
        Err("当前平台不支持便携设备".to_string())
    })
}

/// 递归统计设备上某对象的文件数与总字节（用于任务进度的分母）。
/// 需要逐层枚举，大目录会慢，只在任务开始前调用一次。
pub fn tree_totals(vpath: &str) -> (i32, u64) {
    let mut files = 0i32;
    let mut bytes = 0u64;
    match object_info(vpath) {
        Some((_, false, size)) => (1, size),
        Some((_, true, _)) | None => {
            tree_children_totals(vpath, &mut files, &mut bytes);
            (files, bytes)
        }
    }
}

/// 目录条目已经携带类型和大小，只对子目录继续枚举；避免为每个文件重新 Open 设备。
fn tree_children_totals(vpath: &str, files: &mut i32, bytes: &mut u64) {
    for entry in list_content(vpath) {
        if entry.is_dir {
            tree_children_totals(&entry.path, files, bytes);
        } else {
            *files += 1;
            *bytes += entry.size_bytes;
        }
    }
}

/// 设备内部移动（同一设备）。设备不支持 Move 命令时返回 Err，由调用方回退为复制+删除。
pub fn move_objects(vpaths: &[String], dst_parent_vpath: &str) -> Result<(), String> {
    device_op!(move_objects_win(vpaths, dst_parent_vpath), {
        let _ = (vpaths, dst_parent_vpath);
        Err("当前平台不支持便携设备".to_string())
    })
}

/// 读取对象的名称与是否文件夹；对象不存在返回 None
pub fn object_info(vpath: &str) -> Option<(String, bool, u64)> {
    device_op!(object_info_win(vpath), {
        let _ = vpath;
        None
    })
}

/// 设备会话：一次操作内复用设备句柄，避免反复 Open 的开销。
#[cfg(windows)]
struct Session {
    /// 持有以维持 COM 生命周期：content/props 由它派生，提前释放会失效
    _device: windows::Win32::Devices::PortableDevices::IPortableDevice,
    content: windows::Win32::Devices::PortableDevices::IPortableDeviceContent,
    props: windows::Win32::Devices::PortableDevices::IPortableDeviceProperties,
}

#[cfg(windows)]
fn session(device_id: &str) -> Result<Session, String> {
    let device = open_device(device_id).map_err(|e| format!("无法打开设备：{}", e.message()))?;
    let content = unsafe { device.Content() }.map_err(|e| format!("读取设备内容失败：{}", e.message()))?;
    let props = unsafe { content.Properties() }
        .map_err(|e| format!("读取设备属性失败：{}", e.message()))?;
    Ok(Session {
        _device: device,
        content,
        props,
    })
}

/// 构造一组待写入的对象属性
#[cfg(windows)]
fn new_values(
) -> Result<windows::Win32::Devices::PortableDevices::IPortableDeviceValues, String> {
    use windows::Win32::Devices::PortableDevices::PortableDeviceValues;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    unsafe { CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("创建属性集失败：{}", e.message()))
}

/// 新建对象：`local_src` 为 None 建文件夹；为 Some 建文件并写入该本地文件内容
/// （空路径 "" 表示建 0 字节文件）。成功返回新对象的虚拟路径。
#[cfg(windows)]
fn create_object_win(
    parent_vpath: &str,
    name: &str,
    local_src: Option<&std::path::Path>,
    sink: &mut dyn Sink,
) -> Result<String, String> {
    let (device_id, parent_obj) = parse(parent_vpath).ok_or("不是便携设备路径")?;
    let s = session(&device_id)?;
    let oid = create_object_in(&s, &parent_obj, name, local_src, sink)?;
    Ok(child_vpath(&device_id, &oid))
}

/// 在已打开的会话中新建对象，返回新对象 ID。
#[cfg(windows)]
fn create_object_in(
    s: &Session,
    parent_obj: &str,
    name: &str,
    local_src: Option<&std::path::Path>,
    sink: &mut dyn Sink,
) -> Result<String, String> {
    use std::io::Read;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::{
        WPD_CONTENT_TYPE_FOLDER, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_FORMAT,
        WPD_OBJECT_FORMAT_UNSPECIFIED, WPD_OBJECT_NAME, WPD_OBJECT_ORIGINAL_FILE_NAME,
        WPD_OBJECT_PARENT_ID, WPD_OBJECT_SIZE,
    };
    use windows::Win32::System::Com::{CoTaskMemFree, IStream, STGC_DEFAULT};

    let name = name.trim();
    if name.is_empty() {
        return Err("名称不能为空".to_string());
    }

    let values = new_values()?;
    let parent_w = to_wide(parent_obj);
    let name_w = to_wide(name);
    unsafe {
        values
            .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(parent_w.as_ptr()))
            .map_err(|e| format!("设置父对象失败：{}", e.message()))?;
        values
            .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(name_w.as_ptr()))
            .map_err(|e| format!("设置名称失败：{}", e.message()))?;
        // 文件名属性：多数 MTP 设备以 ORIGINAL_FILE_NAME 作为文件系统名，缺失会导致
        // 新建对象在设备上不可见/无扩展名，故与 NAME 一并写入
        let _ = values.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(name_w.as_ptr()));
    }

    let Some(src) = local_src else {
        // —— 文件夹 ——
        unsafe {
            values
                .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_FOLDER)
                .map_err(|e| format!("设置对象类型失败：{}", e.message()))?;
        }
        let mut oid = PWSTR(std::ptr::null_mut());
        unsafe {
            s.content
                .CreateObjectWithPropertiesOnly(&values, &mut oid)
                .map_err(|e| format!("在设备上新建文件夹失败：{}", e.message()))?;
        }
        let id = pwstr_read(oid.0);
        unsafe { CoTaskMemFree(Some(oid.0.cast())) };
        return Ok(id);
    };

    // —— 文件 ——
    // 空路径约定为「新建 0 字节文件」，不读取本地内容
    let empty = src.as_os_str().is_empty();
    let size = if empty {
        0u64
    } else {
        std::fs::metadata(src)
            .map_err(|e| format!("读取源文件失败：{}", e))?
            .len()
    };
    unsafe {
        values
            .SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, size)
            .map_err(|e| format!("设置大小失败：{}", e.message()))?;
        let _ = values.SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_UNSPECIFIED);
    }

    let mut stream: Option<IStream> = None;
    let mut optimal = 0u32;
    let mut cookie = PWSTR(std::ptr::null_mut());
    unsafe {
        s.content
            .CreateObjectWithPropertiesAndData(&values, &mut stream, &mut optimal, &mut cookie)
            .map_err(|e| format!("在设备上新建文件失败：{}", e.message()))?;
        if !cookie.0.is_null() {
            CoTaskMemFree(Some(cookie.0.cast()));
        }
    }
    let stream = stream.ok_or("设备未返回写入流")?;

    sink.begin_file(name, size);
    if !empty && size > 0 {
        let mut file = std::fs::File::open(src).map_err(|e| format!("打开源文件失败：{}", e))?;
        let cap = if optimal == 0 { 256 * 1024 } else { optimal as usize };
        let mut buf = vec![0u8; cap];
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("读取源文件失败：{}", e))?;
            if n == 0 {
                break;
            }
            let mut written = 0u32;
            let hr = unsafe {
                stream.Write(buf.as_ptr().cast(), n as u32, Some(&mut written))
            };
            if hr.is_err() {
                // 中途失败：流未 Commit，设备会丢弃这个半成品对象
                return Err(format!("写入设备失败：0x{:08X}", hr.0));
            }
            if written as usize != n {
                return Err(format!("写入设备不完整：已写 {}，应写 {}", written, n));
            }
            if !sink.advance(n as u64) {
                return Err("已取消".to_string());
            }
        }
    }
    unsafe {
        stream
            .Commit(STGC_DEFAULT)
            .map_err(|e| format!("提交设备文件失败：{}", e.message()))?;
    }
    sink.end_file();

    // 提交后经 IPortableDeviceDataStream 取回真实对象 ID（用于后续递归/删除）
    let oid = unsafe {
        use windows::core::Interface;
        use windows::Win32::Devices::PortableDevices::IPortableDeviceDataStream;
        match stream.cast::<IPortableDeviceDataStream>() {
            Ok(ds) => match ds.GetObjectID() {
                Ok(p) => {
                    let id = pwstr_read(p.0);
                    CoTaskMemFree(Some(p.0.cast()));
                    id
                }
                Err(e) => return Err(format!("获取对象 ID 失败：{}", e.message())),
            },
            Err(e) => return Err(format!("获取数据流接口失败：{}", e.message())),
        }
    };
    if oid.is_empty() {
        return Err("设备未返回新对象 ID".to_string());
    }
    Ok(oid)
}

#[cfg(windows)]
fn rename_win(vpath: &str, new_name: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        WPD_OBJECT_NAME, WPD_OBJECT_ORIGINAL_FILE_NAME,
    };

    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("名称不能为空".to_string());
    }
    let (device_id, obj) = parse(vpath).ok_or("不是便携设备路径")?;
    if obj == "DEVICE" {
        return Err("设备本身不可重命名".to_string());
    }
    let s = session(&device_id)?;
    let obj_w = to_wide(&obj);
    let name_w = to_wide(new_name);

    // 设备对 NAME / ORIGINAL_FILE_NAME 的可写性不一致（相机常只认 NAME，
    // 手机存储常只认 ORIGINAL_FILE_NAME）：逐个单独提交，任一成功即视为成功。
    let mut last_err = String::new();
    let mut ok = false;
    for key in [&WPD_OBJECT_ORIGINAL_FILE_NAME, &WPD_OBJECT_NAME] {
        let values = match new_values() {
            Ok(v) => v,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let set = unsafe { values.SetStringValue(key, PCWSTR(name_w.as_ptr())) };
        if let Err(e) = set {
            last_err = e.message().to_string();
            continue;
        }
        match unsafe { s.props.SetValues(PCWSTR(obj_w.as_ptr()), &values) } {
            Ok(_) => ok = true,
            Err(e) => last_err = e.message().to_string(),
        }
    }
    if ok {
        Ok(())
    } else {
        Err(format!("设备拒绝重命名：{}", last_err))
    }
}

/// 把一组对象 ID 装进 WPD 的 PROPVARIANT 集合（Delete / Move 的入参格式）
#[cfg(windows)]
fn objid_collection(
    ids: &[String],
) -> Result<windows::Win32::Devices::PortableDevices::IPortableDevicePropVariantCollection, String>
{
    use windows::Win32::Devices::PortableDevices::{
        IPortableDevicePropVariantCollection, PortableDevicePropVariantCollection,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Variant::VT_LPWSTR;

    let coll: IPortableDevicePropVariantCollection = unsafe {
        CoCreateInstance(
            &PortableDevicePropVariantCollection,
            None,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|e| format!("创建对象集合失败：{}", e.message()))?;

    for id in ids {
        // Add 内部会拷贝值，故这里的宽字符缓冲只需在调用期间存活
        let mut w = to_wide(id);
        let mut pv = PROPVARIANT::default();
        unsafe {
            let inner = &mut pv.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = windows::core::PWSTR(w.as_mut_ptr());
            coll.Add(&pv)
                .map_err(|e| format!("加入对象集合失败：{}", e.message()))?;
            // pwszVal 指向栈上缓冲而非 CoTaskMem，不能走 PropVariantClear，
            // 置零后由 Rust 自行释放 w
            let inner = &mut pv.Anonymous.Anonymous;
            inner.Anonymous.pwszVal = windows::core::PWSTR(std::ptr::null_mut());
            inner.vt = windows::Win32::System::Variant::VT_EMPTY;
        }
    }
    Ok(coll)
}

#[cfg(windows)]
fn delete_win(vpaths: &[String]) -> Result<(), String> {
    use windows::Win32::Devices::PortableDevices::PORTABLE_DEVICE_DELETE_WITH_RECURSION;

    if vpaths.is_empty() {
        return Ok(());
    }
    let (device_id, _) = parse(&vpaths[0]).ok_or("不是便携设备路径")?;
    let mut ids = Vec::with_capacity(vpaths.len());
    for v in vpaths {
        let (d, o) = parse(v).ok_or("不是便携设备路径")?;
        if d != device_id {
            return Err("不能一次删除多个设备上的项目".to_string());
        }
        if o == "DEVICE" {
            return Err("设备本身不可删除".to_string());
        }
        ids.push(o);
    }

    let s = session(&device_id)?;
    let coll = objid_collection(&ids)?;
    let mut results = None;
    unsafe {
        s.content
            .Delete(
                PORTABLE_DEVICE_DELETE_WITH_RECURSION.0 as u32,
                &coll,
                &mut results,
            )
            .map_err(|e| format!("设备删除失败：{}", e.message()))?;
    }
    Ok(())
}

#[cfg(windows)]
fn move_objects_win(vpaths: &[String], dst_parent_vpath: &str) -> Result<(), String> {
    if vpaths.is_empty() {
        return Ok(());
    }
    let (dst_dev, dst_obj) = parse(dst_parent_vpath).ok_or("目标不是便携设备路径")?;
    let mut ids = Vec::with_capacity(vpaths.len());
    for v in vpaths {
        let (d, o) = parse(v).ok_or("不是便携设备路径")?;
        if d != dst_dev {
            return Err("跨设备移动请使用复制后删除".to_string());
        }
        ids.push(o);
    }

    let s = session(&dst_dev)?;
    let coll = objid_collection(&ids)?;
    let dst_w = to_wide(&dst_obj);
    let mut results = None;
    unsafe {
        s.content
            .Move(&coll, windows::core::PCWSTR(dst_w.as_ptr()), &mut results)
            .map_err(|e| format!("设备移动失败：{}", e.message()))?;
    }
    Ok(())
}

#[cfg(windows)]
fn object_info_win(vpath: &str) -> Option<(String, bool, u64)> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceKeyCollection, PortableDeviceKeyCollection, WPD_CONTENT_TYPE_FOLDER,
        WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_NAME,
        WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    let (device_id, obj) = parse(vpath)?;
    if obj == "DEVICE" {
        return Some((friendly_name(&device_id).unwrap_or_default(), true, 0));
    }
    let s = session(&device_id).ok()?;
    let keys: IPortableDeviceKeyCollection =
        unsafe { CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER) }
            .ok()?;
    unsafe {
        keys.Add(&WPD_OBJECT_NAME).ok()?;
        keys.Add(&WPD_OBJECT_ORIGINAL_FILE_NAME).ok()?;
        keys.Add(&WPD_OBJECT_CONTENT_TYPE).ok()?;
        keys.Add(&WPD_OBJECT_SIZE).ok()?;
    }
    let obj_w = to_wide(&obj);
    let vals = unsafe { s.props.GetValues(PCWSTR(obj_w.as_ptr()), &keys) }.ok()?;
    let orig = get_string(&vals, &WPD_OBJECT_ORIGINAL_FILE_NAME);
    let name = if orig.is_empty() {
        get_string(&vals, &WPD_OBJECT_NAME)
    } else {
        orig
    };
    let ctype = unsafe { vals.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }.unwrap_or_default();
    let is_dir = ctype == WPD_CONTENT_TYPE_FOLDER || ctype == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;
    let size = if is_dir {
        0
    } else {
        unsafe { vals.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE) }.unwrap_or(0)
    };
    Some((name, is_dir, size))
}

/// 打开设备对象的只读数据流，返回 (流, 建议缓冲大小)
#[cfg(windows)]
fn open_read_stream(
    s: &Session,
    obj: &str,
) -> Result<(windows::Win32::System::Com::IStream, u32), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::WPD_RESOURCE_DEFAULT;
    use windows::Win32::System::Com::{IStream, STGM_READ};

    let resources = unsafe { s.content.Transfer() }
        .map_err(|e| format!("设备不支持读取内容：{}", e.message()))?;
    let obj_w = to_wide(obj);
    let mut optimal = 0u32;
    let mut stream: Option<IStream> = None;
    unsafe {
        resources
            .GetStream(
                PCWSTR(obj_w.as_ptr()),
                &WPD_RESOURCE_DEFAULT,
                STGM_READ.0 as u32,
                &mut optimal,
                &mut stream,
            )
            .map_err(|e| format!("打开设备文件失败：{}", e.message()))?;
    }
    let stream = stream.ok_or("设备未返回读取流")?;
    Ok((stream, optimal))
}

/// 把设备流写入 `out`，按块上报进度。
#[cfg(windows)]
fn drain_stream(
    stream: &windows::Win32::System::Com::IStream,
    optimal: u32,
    out: &mut impl std::io::Write,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let cap = if optimal == 0 {
        256 * 1024
    } else {
        optimal as usize
    };
    let mut buf = vec![0u8; cap];
    loop {
        let mut read = 0u32;
        let hr = unsafe { stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read)) };
        if read > 0 {
            out.write_all(&buf[..read as usize])
                .map_err(|e| format!("写入失败：{}", e))?;
            if !sink.advance(read as u64) {
                return Err("已取消".to_string());
            }
        }
        // S_OK(0) 继续；S_FALSE(1) 表示已到末尾，读完本批后结束
        if read == 0 || hr.0 != 0 {
            break;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn pull_tree_win(
    vpath: &str,
    dst_dir: &std::path::Path,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let (device_id, obj) = parse(vpath).ok_or("不是便携设备路径")?;
    let s = session(&device_id)?;
    pull_in(&s, &device_id, &obj, dst_dir, as_name, sink)
}

/// 递归拉取：`obj` 为文件则写入 `dst_dir/名称`，为文件夹则在其下建同名目录后递归。
#[cfg(windows)]
fn pull_in(
    s: &Session,
    device_id: &str,
    obj: &str,
    dst_dir: &std::path::Path,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let (name, is_dir, size) =
        object_info_in(s, device_id, obj).ok_or("无法读取设备对象信息")?;
    let name = sanitize_filename(as_name.unwrap_or(&name), obj);
    let dest = dst_dir.join(&name);

    if is_dir {
        std::fs::create_dir_all(&dest).map_err(|e| format!("创建目录失败：{}", e))?;
        for child in list_content(&child_vpath(device_id, obj)) {
            let (_, cobj) = parse(&child.path).ok_or("设备路径解析失败")?;
            pull_in(s, device_id, &cobj, &dest, None, sink)?;
        }
        return Ok(());
    }

    sink.begin_file(&name, size);
    let (stream, optimal) = open_read_stream(s, obj)?;
    // 先写临时文件再改名：中途取消/失败不会在目标目录留下半截文件。
    // 加入进程 ID，避免同一目录中并发拉取/重试时临时文件互相覆盖。
    let tmp = dest.with_extension(format!(
        "{}.{}.fxpart",
        dest.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    let mut file =
        std::fs::File::create(&tmp).map_err(|e| format!("创建本地文件失败：{}", e))?;
    let res = drain_stream(&stream, optimal, &mut file, sink);
    drop(file);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("落盘失败：{}", e)
    })?;
    sink.end_file();
    Ok(())
}

/// 会话内读取对象信息（避免 object_info 每次重开设备）
#[cfg(windows)]
fn object_info_in(s: &Session, device_id: &str, obj: &str) -> Option<(String, bool, u64)> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceKeyCollection, PortableDeviceKeyCollection, WPD_CONTENT_TYPE_FOLDER,
        WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_NAME,
        WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    if obj == "DEVICE" {
        return Some((friendly_name(device_id).unwrap_or_default(), true, 0));
    }
    let keys: IPortableDeviceKeyCollection =
        unsafe { CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER) }
            .ok()?;
    unsafe {
        keys.Add(&WPD_OBJECT_NAME).ok()?;
        keys.Add(&WPD_OBJECT_ORIGINAL_FILE_NAME).ok()?;
        keys.Add(&WPD_OBJECT_CONTENT_TYPE).ok()?;
        keys.Add(&WPD_OBJECT_SIZE).ok()?;
    }
    let obj_w = to_wide(obj);
    let vals = unsafe { s.props.GetValues(PCWSTR(obj_w.as_ptr()), &keys) }.ok()?;
    let orig = get_string(&vals, &WPD_OBJECT_ORIGINAL_FILE_NAME);
    let name = if orig.is_empty() {
        get_string(&vals, &WPD_OBJECT_NAME)
    } else {
        orig
    };
    let ctype = unsafe { vals.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }.unwrap_or_default();
    let is_dir = ctype == WPD_CONTENT_TYPE_FOLDER || ctype == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;
    let size = if is_dir {
        0
    } else {
        unsafe { vals.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE) }.unwrap_or(0)
    };
    Some((name, is_dir, size))
}

#[cfg(windows)]
fn push_tree_win(
    src: &std::path::Path,
    parent_vpath: &str,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let (device_id, parent_obj) = parse(parent_vpath).ok_or("目标不是便携设备路径")?;
    let s = session(&device_id)?;
    push_in(&s, &device_id, src, &parent_obj, as_name, sink)
}

/// 递归推送：目录先在设备上建同名文件夹（已存在则复用），文件直接写入。
#[cfg(windows)]
fn push_in(
    s: &Session,
    device_id: &str,
    src: &std::path::Path,
    parent_obj: &str,
    as_name: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let name = match as_name {
        Some(n) => n.to_string(),
        None => src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or("源路径没有名称")?,
    };

    if src.is_dir() {
        // 目标已有同名文件夹则合并进去，避免设备上堆出重复目录
        let parent_v = child_vpath(device_id, parent_obj);
        let existing = child_named(&parent_v, &name).and_then(|(v, is_dir, _)| {
            if is_dir {
                parse(&v).map(|(_, o)| o)
            } else {
                None
            }
        });
        let dir_obj = match existing {
            Some(o) => o,
            None => create_object_in(s, parent_obj, &name, None, sink)?,
        };
        let iter = std::fs::read_dir(src).map_err(|e| format!("读取源目录失败：{}", e))?;
        for entry in iter.flatten() {
            push_in(s, device_id, &entry.path(), &dir_obj, None, sink)?;
        }
        return Ok(());
    }

    // 覆盖同名文件时先用临时名称完整上传，再删除旧对象并把新对象改回原名。
    // 上传失败/取消不会碰旧文件，避免设备断开或空间不足导致不可恢复的数据丢失。
    let parent_v = child_vpath(device_id, parent_obj);
    if let Some((old_vpath, false, _)) = child_named(&parent_v, &name) {
        let temp_name = free_device_temp_name(&parent_v, &name);
        let new_obj = create_object_in(s, parent_obj, &temp_name, Some(src), sink)?;
        delete_win(&[old_vpath]).map_err(|e| format!("替换旧文件失败：{}", e))?;
        let new_vpath = child_vpath(device_id, &new_obj);
        rename_win(&new_vpath, &name).map_err(|e| {
            format!("文件已上传，但恢复原名称失败（当前名称：{}）：{}", temp_name, e)
        })?;
        return Ok(());
    }
    create_object_in(s, parent_obj, &name, Some(src), sink).map(|_| ())
}

/// 为安全覆盖生成设备目录内未占用的临时对象名。
#[cfg(windows)]
fn free_device_temp_name(parent_vpath: &str, name: &str) -> String {
    let base = format!("{}.filefiles-one-upload", name);
    if child_named(parent_vpath, &base).is_none() {
        return base;
    }
    for n in 2..10_000 {
        let candidate = format!("{}.{}", base, n);
        if child_named(parent_vpath, &candidate).is_none() {
            return candidate;
        }
    }
    format!("{}.{}", base, std::process::id())
}

#[cfg(windows)]
fn parent_path_win(vpath: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceKeyCollection, PortableDeviceKeyCollection, WPD_OBJECT_PARENT_ID,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    let (device_id, obj) = parse(vpath)?;
    if obj == "DEVICE" {
        return None; // 已在设备根
    }

    let device = open_device(&device_id).ok()?;
    let content = unsafe { device.Content().ok()? };
    let props = unsafe { content.Properties().ok()? };

    let keys: IPortableDeviceKeyCollection =
        unsafe { CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER).ok()? };
    unsafe { keys.Add(&WPD_OBJECT_PARENT_ID).ok()? };

    let obj_wide = to_wide(&obj);
    let vals = unsafe { props.GetValues(PCWSTR(obj_wide.as_ptr()), &keys).ok()? };
    let parent = get_string(&vals, &WPD_OBJECT_PARENT_ID);

    if parent.is_empty() || parent == "DEVICE" {
        Some(format!("device://{}", device_id))
    } else {
        Some(format!("device://{}{}{}", device_id, SEP, parent))
    }
}
