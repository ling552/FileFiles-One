//! 内存守护：后台周期监控进程内存，按「空闲 / 最小化 / 超阈值」自动瘦身。
//!
//! 瘦身分两层：
//! 1. 清空进程内缓存（图标/缩略图缓存、Slint 图像共享缓存）——真正归还
//!    分配器内存（提交字节下降）；
//! 2. `SetProcessWorkingSetSize(-1,-1)`（EmptyWorkingSet 语义）——把仍驻留
//!    物理内存的页面换出到待备列表，任务管理器中的内存列立即下降。
//!
//! 策略（每次监控 tick 最多执行一次瘦身）：
//! - 窗口最小化：用户看不见，图标可放心重建（限频）；
//! - 用户空闲（无键鼠输入）超过阈值：空闲状态内存压到最低，回来时
//!   图标后台异步重取，无感；
//! - 工作集/提交内存超过上限：无论是否空闲都收缩，防止浏览大量
//!   图片/视频后常驻内存持续增长（限频，避免反复触发）。
//!
//! 清理内容均为可再生的派生数据：图标像素、Slint 图像句柄；预览临时
//! 文件不清（磁盘缓存），目录数据与配置不动。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 监控轮询间隔
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// 用户空闲多久后触发瘦身（空闲状态内存最低的核心策略）
const IDLE_TRIM_AFTER_MS: u32 = 3 * 60_000;
/// 空闲瘦身的两次间隔下限（避免长时间空闲反复换页抖动）
const IDLE_TRIM_EVERY: Duration = Duration::from_secs(3 * 60);
/// 工作集阈值：超过即收缩（无论是否空闲）。
/// 必须高于活跃使用的自然水位（实测旧版活跃工作集约 730MB、提交约 650MB），
/// 否则正常浏览期间会反复触发收缩，图标反复重取、页面频繁换入换出，
/// 反而拖慢使用。空闲/最小化策略不受此阈值影响，仍保证空闲内存最低。
const WORKING_SET_HIGH: u64 = 700 * 1024 * 1024;
/// 提交内存阈值：超过即收缩（留出活跃使用余量）
const PRIVATE_HIGH: u64 = 900 * 1024 * 1024;
/// 超阈值瘦身的限频间隔
const PRESSURE_TRIM_EVERY: Duration = Duration::from_secs(90);
/// 最小化瘦身的限频间隔
const MINIMIZED_TRIM_EVERY: Duration = Duration::from_secs(60);

/// 上次瘦身时刻（Unix 秒），供诊断与外部查询
static LAST_TRIM_SECS: AtomicU64 = AtomicU64::new(0);

/// 主窗口 HWND（0 = 未知），用于 IsIconic 判断最小化
static MAIN_HWND: AtomicU64 = AtomicU64::new(0);

/// 启动内存守护：捕获主窗口 HWND 后，在独立线程进入监控循环。
/// 在窗口创建完成后（bind_window_chrome）调用一次即可。
pub fn start(ui: &crate::MainWindow) {
    let hwnd = capture_hwnd(ui);
    MAIN_HWND.store(hwnd, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("memguard".into())
        .spawn(monitor_loop)
        .ok();
}

/// 延迟重试捕获 HWND：winit 窗口在事件循环启动后 80–250ms 才创建，
/// 绑定时立即捕获多半拿不到（返回 0），须在 UI 线程按延迟重试补获。
pub fn capture_hwnd_with_retry(ui: &crate::MainWindow, delays_ms: &[u64]) {
    use slint::ComponentHandle;
    for &delay in delays_ms {
        let w = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(ui) = w.upgrade() {
                if MAIN_HWND.load(Ordering::SeqCst) == 0 {
                    let h = capture_hwnd(&ui);
                    MAIN_HWND.store(h, Ordering::SeqCst);
                }
            }
        });
    }
}

/// 经 winit 取主窗口 HWND（原始句柄，仅用于 IsIconic 探测）。
fn capture_hwnd(ui: &crate::MainWindow) -> u64 {
    #[cfg(windows)]
    {
        use slint::ComponentHandle;
        use slint::winit_030::WinitWindowAccessor;
        ui.window()
            .with_winit_window(|w| {
                use raw_window_handle::HasWindowHandle;
                match w.window_handle() {
                    Ok(h) => match h.as_raw() {
                        raw_window_handle::RawWindowHandle::Win32(w32) => {
                            w32.hwnd.get() as u64
                        }
                        _ => 0,
                    },
                    Err(_) => 0,
                }
            })
            .unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        let _ = ui;
        0
    }
}

fn monitor_loop() {
    // 启动后允许尽早响应压力（空闲策略仍按自身间隔执行）
    let mut last_trim = Instant::now() - Duration::from_secs(120);
    let mut last_pressure = Instant::now();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let (ws, private) = match read_process_memory() {
            Some(v) => v,
            None => continue,
        };
        let now = Instant::now();
        let idle_ms = user_idle_ms();
        let idle = idle_ms >= IDLE_TRIM_AFTER_MS;
        let minimized = is_main_minimized();
        let over = ws > WORKING_SET_HIGH || private > PRIVATE_HIGH;

        // 空闲或最小化：限频内清缓存 + 收缩工作集；超阈值时不等空闲
        if idle && now.duration_since(last_trim) >= IDLE_TRIM_EVERY {
            trim();
            last_trim = now;
        } else if minimized && now.duration_since(last_trim) >= MINIMIZED_TRIM_EVERY {
            trim();
            last_trim = now;
        } else if over && now.duration_since(last_pressure) >= PRESSURE_TRIM_EVERY {
            trim();
            last_trim = now;
            last_pressure = now;
        }
    }
}

/// 执行一次瘦身：清空可再生的图标缓存，再收缩工作集。
/// 缓存清理由本线程直接完成（全局 Mutex 缓存）；Slint 图像缓存
/// 绑定 UI 线程 thread_local，经事件循环调度清空。
pub fn trim() {
    clear_derived_caches();
    trim_working_set();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LAST_TRIM_SECS.store(now, Ordering::Relaxed);
}

#[cfg(windows)]
fn clear_derived_caches() {
    // 1) 图标/缩略图像素缓存（全局 Mutex，任意线程可清）
    crate::fs::thumbnail::clear_all_caches();
    // 2) Slint 图像共享缓存：持有着像素 Arc，必须回 UI 线程清空才真正释放
    let _ = slint::invoke_from_event_loop(|| {
        crate::ui_bridge::clear_icon_image_cache();
    });
}

#[cfg(not(windows))]
fn clear_derived_caches() {
    crate::fs::thumbnail::clear_all_caches();
}

/// EmptyWorkingSet 语义收缩：把当前物理驻留页全部换出到待备列表。
/// 不终止线程、不损坏状态；被再次访问的页面按需换回，代价是一次软缺页。
#[cfg(windows)]
fn trim_working_set() {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe {
        let handle: HANDLE = GetCurrentProcess();
        // usize::MAX 即 -1：请求最小工作集（EmptyWorkingSet 等效）
        let _ = SetProcessWorkingSetSize(handle, usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
fn trim_working_set() {}

/// 读取当前进程内存：(工作集字节, 提交内存字节)。失败返回 None（跳过本轮）。
#[cfg(windows)]
fn read_process_memory() -> Option<(u64, u64)> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let handle: HANDLE = GetCurrentProcess();
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
            PrivateUsage: 0,
        };
        if GetProcessMemoryInfo(
            handle,
            &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut _,
            pmc.cb,
        ) != 0
        {
            Some((pmc.WorkingSetSize as u64, pmc.PrivateUsage as u64))
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
fn read_process_memory() -> Option<(u64, u64)> {
    None
}

/// 系统级用户空闲时长（毫秒）：任何键鼠输入都会重置，覆盖全系统输入，
/// 应用失焦但用户在操作其它程序时不误判空闲。
#[cfg(windows)]
fn user_idle_ms() -> u32 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info) != 0 {
            // GetTickCount 约 49.7 天回绕：saturating 减法兜底
            return GetTickCount().saturating_sub(info.dwTime);
        }
    }
    0
}

#[cfg(not(windows))]
fn user_idle_ms() -> u32 {
    0
}

#[cfg(windows)]
fn is_main_minimized() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
    let raw = MAIN_HWND.load(Ordering::SeqCst);
    if raw == 0 {
        return false;
    }
    // windows-sys 0.59 的 HWND 为指针类型；存储用 u64 便于跨线程原子传递
    let hwnd = raw as usize as *mut core::ffi::c_void;
    (unsafe { IsIconic(hwnd) }) != 0
}

#[cfg(not(windows))]
fn is_main_minimized() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阈值常量自洽，防止误改后失衡
    #[test]
    fn thresholds_are_sane() {
        assert!(WORKING_SET_HIGH > 64 * 1024 * 1024, "阈值过低会频繁抖动");
        assert!(PRIVATE_HIGH >= WORKING_SET_HIGH);
        assert!(IDLE_TRIM_AFTER_MS >= 60_000, "空闲判定过短会打断正常使用");
    }
}
