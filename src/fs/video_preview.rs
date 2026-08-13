//! Quick Look 视频内嵌播放：Media Foundation MFPlay 在宿主窗口的子窗口中
//! 渲染视频画面并输出音频，与预览浮层的内容区对齐覆盖。
//!
//! 生命周期由 UI 线程管理（thread_local）：打开视频预览时创建子窗口 + 播放器，
//! 关闭预览（空格/Esc/点击遮罩/切换文件）时停止播放并销毁子窗口。
//!
//! 性能：媒体源解析（容器嗅探/解码器加载）经 IMFPMediaPlayerCallback 异步进行
//! ——MFPCreateMediaPlayer 不传 URL 立即返回，CreateMediaItemFromURL(fSync=FALSE)
//! 后台解析，MEDIAITEM_CREATED → SetMediaItem → MEDIAITEM_SET → Play。
//! 旧实现同步传 URL 创建会阻塞 UI 线程直至媒体源就绪，大文件/机械盘上打开明显卡顿。
//! MFPlay 通过隐藏窗口把事件序列化回创建线程（UI 线程）的消息循环，回调内可安全
//! 调用播放器方法；分辨率就绪后经 `ready` 回调上报（预览卡片按视频宽高比自适应）。

/// 由编码帧尺寸与显示宽高比（DAR）推导「显示尺寸」。
/// `aspect` 是 MFPlay GetNativeVideoSize 第二个输出（display size），
/// 表示视频最终的显示比例；当它与编码帧方向不一致时，以高度为基准换算宽度。
pub(crate) fn display_size(native: (u32, u32), aspect: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = native;
    let (aw, ah) = aspect;
    if nw == 0 || nh == 0 {
        return (0, 0);
    }
    if aw == 0 || ah == 0 {
        return native;
    }
    let native_portrait = nw < nh;
    let aspect_portrait = aw < ah;
    let display_height = if native_portrait == aspect_portrait {
        nh as u64
    } else {
        nw as u64
    };
    let display_width = ((display_height * aw as u64) / ah as u64).max(1);
    (display_width as u32, display_height as u32)
}

/// 按像素宽高比（PAR）修正编码帧尺寸。
///
/// 与 `display_size` 的区别：`display_size` 的第二参数是 MFPlay 给出的
/// 「显示尺寸（像素）」，而 Shell 的 System.Video.HorizontalAspectRatio /
/// VerticalAspectRatio 是**单个像素**的宽高比，方形像素为 1:1。两者语义不同，
/// 把 1:1 当显示尺寸喂给 `display_size` 会被判成横屏并把宽度当高度基准，
/// 使竖屏视频塌成正方形（720×960 → 720×720）。
pub(crate) fn apply_par(native: (u32, u32), par: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = native;
    let (pw, ph) = par;
    if nw == 0 || nh == 0 {
        return (0, 0);
    }
    // 缺失或方形像素：编码尺寸即显示尺寸
    if pw == 0 || ph == 0 || pw == ph {
        return native;
    }
    // 非方形像素：宽度按 PAR 拉伸，高度不变
    let w = ((nw as u64 * pw as u64) / ph as u64).max(1);
    (w as u32, nh)
}

/// 按旋转角度换算显示尺寸：90/270 度需交换宽高。
/// Shell 属性处理器给的是「编码帧尺寸 + 旋转角」两个独立字段，手机竖拍视频
/// 常见 1920×1080 + 90°，必须交换后才是实际观感尺寸（与 MFPlay 上报的一致）。
pub(crate) fn apply_rotation(size: (u32, u32), rotation: u32) -> (u32, u32) {
    if rotation % 180 == 90 {
        (size.1, size.0)
    } else {
        size
    }
}

/// 打开播放器之前先读出视频显示尺寸（宽, 高），失败返回 None。
///
/// 目的：预览窗口在显示前就知道视频宽高比，一次性按正确比例开窗，
/// 不再「先横屏、拿到分辨率后再跳一次」。走 Shell 属性处理器（与资源管理器
/// 详细信息列同源，通常只读容器头部且有系统缓存），比开 Media Foundation
/// 媒体源快一个量级。UNC/虚拟路径跳过，避免网络往返卡住 UI 线程。
#[cfg(windows)]
pub fn probe_display_size(path: &str) -> Option<(u32, u32)> {
    win_impl::probe_display_size(path)
}

#[cfg(not(windows))]
pub fn probe_display_size(_path: &str) -> Option<(u32, u32)> {
    None
}

/// 原生控制条回调。控制条位于 MFPlay 子窗口上方，因此按钮事件由原生窗口
/// 转发到 Slint 状态回调，保持与非原生控件相同的播放、重播、静音和拖动行为。
pub struct ControlCallbacks {
    pub toggle_play: Box<dyn Fn() + Send>,
    pub replay: Box<dyn Fn() + Send>,
    pub mute: Box<dyn Fn() + Send>,
    pub seek: Box<dyn Fn(f32) + Send>,
    /// 关闭预览。兜底用：若焦点意外落到控制条上，空格/Esc 会被它的窗口过程
    /// 吞掉（两个 Slint 窗口都收不到），此时由控制条自己转发关闭。
    pub close: Box<dyn Fn() + Send>,
}

/// 启动播放：`parent` 为主窗口 HWND（isize），`rect` 为子窗口在父窗口客户区内的
/// 物理像素位置 (x, y, w, h)，`path` 为视频文件完整路径。
/// `ready(视频宽, 视频高)` 在媒体项就绪、开始播放时回调（UI 线程消息循环内）。
/// 返回是否成功启动异步加载。
#[cfg(windows)]
pub fn start(
    parent: isize,
    rect: (i32, i32, i32, i32),
    path: &str,
    ready: Box<dyn Fn(u32, u32) + Send>,
    controls: ControlCallbacks,
) -> bool {
    win_impl::start(parent, rect, path, ready, controls)
}

#[cfg(not(windows))]
pub fn start(
    _parent: isize,
    _rect: (i32, i32, i32, i32),
    _path: &str,
    _ready: Box<dyn Fn(u32, u32) + Send>,
    _controls: ControlCallbacks,
) -> bool {
    false
}

/// 停止播放并销毁子窗口（未在播放时为空操作）。
pub fn stop() {
    #[cfg(windows)]
    win_impl::stop();
}

/// 移动/缩放播放子窗口到新的物理像素矩形（未在播放时为空操作）。
/// 预览卡片按视频原生宽高比自适应大小后，由 UI 线程调用对齐子窗口。
#[cfg(windows)]
pub fn reposition(rect: (i32, i32, i32, i32)) {
    win_impl::reposition(rect);
}

#[cfg(not(windows))]
pub fn reposition(_rect: (i32, i32, i32, i32)) {}

/// 仅按上次的客户区矩形重新贴合控制条到宿主窗口（未在播放时为空操作）。
///
/// 画面子窗口是宿主的真子窗口，宿主移动时由系统一起搬走；控制条是 WS_POPUP
/// owned 窗口，坐标在屏幕系，宿主移动后必须主动重算。拖动标题栏期间 Windows
/// 进入模态 move 循环、Slint 定时器停摆，故由宿主窗口过程在 WM_MOVE 内同步调用。
#[cfg(windows)]
pub fn sync_controls() {
    win_impl::sync_controls();
}

#[cfg(not(windows))]
pub fn sync_controls() {}

/// 媒体就绪后显示画面与控制条（加载期间保持隐藏，由 Slint 侧显示加载动画）。
pub fn reveal() {
    #[cfg(windows)]
    win_impl::reveal();
}

/// 设置原生半透明控制条的显示状态。控制条位于 MFPlay 视频子窗口上方。
pub fn set_controls_visible(visible: bool) {
    #[cfg(windows)]
    win_impl::set_controls_visible(visible);
}

/// 将 Rust/Slint 播放状态同步到原生控制条并请求重绘。
pub fn update_controls(position: i32, duration: i32, paused: bool, muted: bool) {
    #[cfg(windows)]
    win_impl::update_controls(position, duration, paused, muted);
}

/// 查询播放器是否处于暂停态。
pub fn is_paused() -> bool {
    #[cfg(windows)]
    {
        win_impl::is_paused()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 暂停 / 播放切换，返回切换后是否处于暂停态（true=已暂停）。未在播放时为空操作。
#[cfg(windows)]
pub fn toggle_play() -> bool {
    win_impl::toggle_play()
}

#[cfg(not(windows))]
pub fn toggle_play() -> bool {
    false
}

/// 当前播放位置与总时长（单位 100ns），未在播放返回 (0, 0)。
/// 供 UI 线程轮询刷新进度条。
#[cfg(windows)]
pub fn position() -> (i64, i64) {
    win_impl::position()
}

#[cfg(not(windows))]
pub fn position() -> (i64, i64) {
    (0, 0)
}

/// 跳转到指定 100ns 位置（未在播放时为空操作）。
pub fn seek_100ns(pos: i64) {
    #[cfg(windows)]
    win_impl::seek_100ns(pos);
}

/// 设置/查询静音（未在播放时设置为空操作、查询返回 false）。
pub fn set_muted(muted: bool) {
    #[cfg(windows)]
    win_impl::set_muted(muted);
}

pub fn is_muted() -> bool {
    #[cfg(windows)]
    {
        win_impl::is_muted()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
mod win_impl {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows::core::{implement, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
        EndPaint, FillRect, InvalidateRect, RoundRect, SelectObject, SetBkMode, SetTextColor,
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER,
        DT_SINGLELINE, DT_VCENTER, FW_NORMAL, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID,
        TRANSPARENT,
    };
    use windows::Win32::Media::MediaFoundation::{
        IMFPMediaPlayer, IMFPMediaPlayerCallback, IMFPMediaPlayerCallback_Impl,
        MFPCreateMediaPlayer, MFP_EVENT_HEADER, MFP_EVENT_TYPE_MEDIAITEM_CREATED,
        MFP_EVENT_TYPE_MEDIAITEM_SET, MFP_MEDIAITEM_CREATED_EVENT, MFP_MEDIAITEM_SET_EVENT,
        MFP_OPTION_NONE, MFP_POSITIONTYPE_100NS,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

    /// 空格 / Esc 虚拟键码。直接写常量而非引 Win32_UI_Input_KeyboardAndMouse：
    /// windows crate 未开该 feature（仅 windows-sys 开了），为两个常量加 feature 不值得。
    const VK_SPACE_CODE: usize = 0x20;
    const VK_ESCAPE_CODE: usize = 0x1B;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
        MoveWindow, RegisterClassW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOP, LWA_ALPHA,
        SW_SHOWNOACTIVATE, WM_KEYDOWN, WM_MOUSEWHEEL,
        MA_NOACTIVATE, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, WNDCLASSW,
        WINDOW_EX_STYLE, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WS_CHILD,
        WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    const CONTROL_H: i32 = 48;

    #[derive(Clone, Copy)]
    struct ControlState {
        position: i32,
        duration: i32,
        paused: bool,
        muted: bool,
        visible: bool,
        /// 音量百分比 0..100（滚轮调节时短暂显示在控制条时间区）
        volume: i32,
    }

    struct ControlWindowData {
        callbacks: super::ControlCallbacks,
        owner: isize,
        dragging: bool,
    }

    thread_local! {
        // (播放器, 视频子窗口句柄, 原生控制条句柄)：仅 UI 线程访问
        static ACTIVE: RefCell<Option<(IMFPMediaPlayer, isize, isize)>> = const { RefCell::new(None) };
        // 暂停态（仅 UI 线程）：start 时复位为 false，toggle_play 翻转并据此调 Pause/Play
        static PAUSED: Cell<bool> = const { Cell::new(false) };
        static CONTROL_STATE: Cell<ControlState> = const { Cell::new(ControlState {
            position: 0,
            duration: 0,
            paused: false,
            muted: false,
            visible: true,
            volume: 100,
        }) };
        /// 音量反馈显示截止时间：滚轮调节后 1.2s 内控制条时间区改显音量百分比。
        /// 控制条每 250ms 轮询重绘，到期后自然恢复时间显示，无需额外定时器。
        static VOLUME_FLASH_UNTIL: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
        /// 最近一次 reposition 的矩形（宿主客户区坐标，物理像素）。
        /// 宿主纯移动时尺寸不变，只需用它重算控制条的屏幕坐标。
        static LAST_RECT: Cell<(i32, i32, i32, i32)> = const { Cell::new((0, 0, 0, 0)) };
        /// 媒体是否已就绪（未就绪时画面与控制条保持隐藏，让位给加载动画）
        static REVEALED: Cell<bool> = const { Cell::new(false) };
    }

    /// 播放代次：每次 start/stop 自增。异步事件携带发起时代次（dwUserData），
    /// 回调内比对当前代次——快速切换视频时丢弃迟到的旧媒体项，防止画面串台。
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// MFPlay 事件回调：媒体项异步创建完成 → 装载；装载完成 → 播放 + 上报分辨率。
    /// MFPlay 把事件序列化回创建线程（UI 线程）的消息循环，方法内可直接调用播放器。
    #[implement(IMFPMediaPlayerCallback)]
    struct PlayerCallback {
        generation: u64,
        ready: Box<dyn Fn(u32, u32) + Send>,
    }

    impl IMFPMediaPlayerCallback_Impl for PlayerCallback_Impl {
        fn OnMediaPlayerEvent(&self, peventheader: *const MFP_EVENT_HEADER) {
            unsafe {
                if peventheader.is_null() {
                    return;
                }
                let header = &*peventheader;
                // 本回调所属播放已被停止/替换：忽略一切迟到事件
                if self.generation != GENERATION.load(Ordering::SeqCst) {
                    return;
                }
                let Some(player) = header.pMediaPlayer.as_ref() else {
                    return;
                };
                match header.eEventType {
                    t if t == MFP_EVENT_TYPE_MEDIAITEM_CREATED => {
                        if header.hrEvent.is_err() {
                            return;
                        }
                        let ev = &*(peventheader as *const MFP_MEDIAITEM_CREATED_EVENT);
                        // dwUserData 携带发起时代次，双重校验防串台
                        if ev.dwUserData as u64 != self.generation {
                            return;
                        }
                        if let Some(item) = ev.pMediaItem.as_ref() {
                            let _ = player.SetMediaItem(item);
                        }
                    }
                    t if t == MFP_EVENT_TYPE_MEDIAITEM_SET => {
                        if header.hrEvent.is_err() {
                            return;
                        }
                        let _ev = &*(peventheader as *const MFP_MEDIAITEM_SET_EVENT);
                        // 保持画面宽高比（信箱式留黑边）：分辨率就绪前子窗口按默认
                        // 16:9 布局，若不设此模式，竖屏视频会被拉伸变形
                        let _ = player.SetAspectRatioMode(
                            windows::Win32::Media::MediaFoundation::MFVideoARMode_PreservePicture.0
                                as u32,
                        );
                        let _ = player.Play();
                        // 优先使用 Media Foundation 计算后的显示宽高比尺寸。
                        // 它已包含非方形像素等修正，竖屏素材不能只用编码帧尺寸。
                        let mut native = SIZE::default();
                        let mut display = SIZE::default();
                        if player
                            .GetNativeVideoSize(Some(&mut native), Some(&mut display))
                            .is_ok()
                        {
                            let size = super::display_size(
                                (native.cx.max(0) as u32, native.cy.max(0) as u32),
                                (display.cx.max(0) as u32, display.cy.max(0) as u32),
                            );
                            if size.0 > 0 && size.1 > 0 {
                                (self.ready)(size.0, size.1);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn start(
        parent: isize,
        rect: (i32, i32, i32, i32),
        path: &str,
        ready: Box<dyn Fn(u32, u32) + Send>,
        controls: super::ControlCallbacks,
    ) -> bool {
        // 先停掉上一次播放（切换视频/重复打开）
        stop();
        // 新播放从播放态开始（PAUSED 复位）
        PAUSED.with(|p| p.set(false));
        REVEALED.with(|r| r.set(false));
        LAST_RECT.with(|c| c.set(rect));
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

        let class: Vec<u16> = "FileFilesOneVideoHost"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let url: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            // 自定义窗口类（而非系统 STATIC）：窗口过程需接收 WM_MOUSEWHEEL 调节音量，
            // STATIC 类会把滚轮消息直接丢弃。黑色背景笔刷替代 SS_BLACKRECT 免自绘底色。
            let mut wc = WNDCLASSW::default();
            wc.style = CS_HREDRAW | CS_VREDRAW;
            wc.lpfnWndProc = Some(video_wnd_proc);
            wc.hbrBackground = CreateSolidBrush(COLORREF(0));
            wc.lpszClassName = PCWSTR(class.as_ptr());
            let _ = RegisterClassW(&wc);
            // 不带 WS_VISIBLE：媒体就绪前保持隐藏，让 Slint 的加载动画可见
            // （原生子窗口会盖住同区域的 Slint 绘制，一开就显示等于黑屏等待）。
            let style = WS_CHILD | WS_CLIPSIBLINGS;
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                style,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                Some(HWND(parent as *mut core::ffi::c_void)),
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(_) => return false,
            };

            // 播放器本体创建很快（不传 URL 不解析媒体）；媒体源解析走异步回调
            let callback: IMFPMediaPlayerCallback = PlayerCallback { generation, ready }.into();
            let mut player: Option<IMFPMediaPlayer> = None;
            let created = MFPCreateMediaPlayer(
                PCWSTR::null(),
                false,
                MFP_OPTION_NONE,
                Some(&callback),
                Some(hwnd),
                Some(&mut player),
            );
            let Some(player) = created.ok().and(player) else {
                let _ = DestroyWindow(hwnd);
                return false;
            };
            // 异步创建媒体项（fSync=FALSE 立即返回）；dwUserData 携带代次
            if player
                .CreateMediaItemFromURL(PCWSTR(url.as_ptr()), false, generation as usize, None)
                .is_err()
            {
                let _ = player.Shutdown();
                let _ = DestroyWindow(hwnd);
                return false;
            }
            let controls_hwnd = create_controls_window(
                HWND(parent as *mut core::ffi::c_void),
                controls,
            );
            let Some(controls_hwnd) = controls_hwnd else {
                let _ = player.Shutdown();
                let _ = DestroyWindow(hwnd);
                return false;
            };
            position_controls(controls_hwnd, rect);
            ACTIVE.with(|a| *a.borrow_mut() = Some((player, hwnd.0 as isize, controls_hwnd.0 as isize)));
            CONTROL_STATE.with(|s| s.set(ControlState {
                position: 0,
                duration: 0,
                paused: false,
                muted: false,
                visible: true,
                volume: 100,
            }));
            VOLUME_FLASH_UNTIL.with(|t| t.set(None));
        }
        true
    }

    pub fn stop() {
        // 代次自增：在途的异步事件全部作废
        GENERATION.fetch_add(1, Ordering::SeqCst);
        REVEALED.with(|r| r.set(false));
        ACTIVE.with(|a| {
            if let Some((player, hwnd, controls_hwnd)) = a.borrow_mut().take() {
                unsafe {
                    let _ = player.Stop();
                    let _ = player.Shutdown();
                    let _ = DestroyWindow(HWND(controls_hwnd as *mut core::ffi::c_void));
                    let _ = DestroyWindow(HWND(hwnd as *mut core::ffi::c_void));
                }
            }
        });
    }

    /// 对齐子窗口到新矩形（卡片按视频宽高比自适应后调用）。
    /// MFPlay 不会自动感知宿主窗口尺寸变化：MoveWindow 后必须调用
    /// UpdateVideo() 通知播放器重算视频布局，否则画面仍按旧窗口
    /// 大小渲染（表现为切换视频后首次预览比例错误、直到重开才恢复）。
    pub fn reposition(rect: (i32, i32, i32, i32)) {
        LAST_RECT.with(|c| c.set(rect));
        ACTIVE.with(|a| {
            if let Some((player, hwnd, controls_hwnd)) = a.borrow().as_ref() {
                unsafe {
                    let _ = MoveWindow(
                        HWND(*hwnd as *mut core::ffi::c_void),
                        rect.0,
                        rect.1,
                        rect.2,
                        rect.3,
                        true,
                    );
                    position_controls(HWND(*controls_hwnd as *mut core::ffi::c_void), rect);
                    let _ = player.UpdateVideo();
                }
            }
        });
    }

    /// 只把控制条贴回宿主客户区底部（宿主移动时用；不动画面子窗口、不调播放器）
    pub fn sync_controls() {
        let rect = LAST_RECT.with(|c| c.get());
        if rect.2 <= 0 || rect.3 <= 0 {
            return;
        }
        ACTIVE.with(|a| {
            if let Some((_, _, controls_hwnd)) = a.borrow().as_ref() {
                position_controls(HWND(*controls_hwnd as *mut core::ffi::c_void), rect);
            }
        });
    }

    /// 媒体就绪：显示画面子窗口，并按可见性状态显示控制条
    pub fn reveal() {
        if REVEALED.with(|r| r.replace(true)) {
            return;
        }
        let controls_visible = CONTROL_STATE.with(|s| s.get().visible);
        ACTIVE.with(|a| {
            if let Some((_, hwnd, controls_hwnd)) = a.borrow().as_ref() {
                unsafe {
                    // 画面是真子窗口，SW_SHOW 不会激活
                    let _ = ShowWindow(HWND(*hwnd as *mut core::ffi::c_void), SW_SHOW);
                    if controls_visible {
                        // 控制条是 WS_POPUP 顶层窗口：必须用 SW_SHOWNOACTIVATE。
                        // WS_EX_NOACTIVATE 只拦「点击激活」，显式 SW_SHOW 仍会激活它
                        // 并夺走前台焦点，使空格/Esc 不再送到 Slint 预览窗口。
                        let _ = ShowWindow(
                            HWND(*controls_hwnd as *mut core::ffi::c_void),
                            SW_SHOWNOACTIVATE,
                        );
                    }
                }
            }
        });
    }

    /// System.Video.* 属性集的 FMTID（与 Shell 详细信息列同源）
    const PKEY_VIDEO_FMTID: windows::core::GUID =
        windows::core::GUID::from_u128(0x64440491_4c8b_11d1_8b70_080036b11a03);
    /// System.Video.FrameWidth / FrameHeight / HorizontalAspectRatio /
    /// VerticalAspectRatio / Orientation 的 PID
    const PID_FRAME_WIDTH: u32 = 3;
    const PID_FRAME_HEIGHT: u32 = 4;
    const PID_ASPECT_H: u32 = 42;
    const PID_ASPECT_V: u32 = 45;
    const PID_ORIENTATION: u32 = 99;

    /// 经 Shell 属性处理器读出视频显示尺寸。UNC/虚拟路径直接放弃（避免网络阻塞）。
    pub fn probe_display_size(path: &str) -> Option<(u32, u32)> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::PROPERTYKEY;
        use windows::Win32::UI::Shell::PropertiesSystem::{
            IPropertyStore, SHGetPropertyStoreFromParsingName, GPS_DEFAULT,
        };

        // 网络路径的属性处理器可能要拉取整段容器头，UI 线程上不可接受
        if path.is_empty() || path.starts_with("\\\\") || crate::fs::virtualfs::is_virtual(path) {
            return None;
        }
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let store: IPropertyStore = unsafe {
            SHGetPropertyStoreFromParsingName(PCWSTR(wide.as_ptr()), None, GPS_DEFAULT).ok()?
        };
        let read = |pid: u32| -> Option<u32> {
            let key = PROPERTYKEY {
                fmtid: PKEY_VIDEO_FMTID,
                pid,
            };
            let value = unsafe { store.GetValue(&key).ok()? };
            u32::try_from(&value).ok()
        };
        let w = read(PID_FRAME_WIDTH)?;
        let h = read(PID_FRAME_HEIGHT)?;
        if w == 0 || h == 0 {
            return None;
        }
        // Shell 的宽高比字段是「像素宽高比 PAR」，非方形像素才需拉伸宽度；
        // 之后再按旋转角交换宽高（手机竖拍常见 1920×1080 + 90°）。
        let par = match (read(PID_ASPECT_H), read(PID_ASPECT_V)) {
            (Some(aw), Some(ah)) if aw > 0 && ah > 0 => (aw, ah),
            _ => (0, 0),
        };
        let size = super::apply_par((w, h), par);
        let rotation = read(PID_ORIENTATION).unwrap_or(0);
        let size = super::apply_rotation(size, rotation);
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    fn control_class() -> Vec<u16> {
        "FileFilesOneVideoControls".encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 视频画面子窗口过程：滚轮调节音量（STATIC 类会丢弃滚轮消息，故用自定义类）。
    unsafe extern "system" fn video_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            // 滚轮调音量：WHEEL_DELTA=120/格，每格 5% 音量
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
                adjust_volume(delta * 0.05);
                LRESULT(0)
            }
            // 与控制条一致：点击画面不抢占焦点，空格/Esc 仍由 Slint 预览窗口处理
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// 滚轮音量调节：在当前音量上累加 delta（±0.05/格），夹紧 0..1；
    /// 静音状态下上调自动取消静音，保证调节即刻可听。
    /// 调节后 1.2s 内控制条时间区改显音量百分比作为反馈。
    fn adjust_volume(delta: f32) {
        ACTIVE.with(|a| {
            let Some((player, _, controls_hwnd)) = a.borrow().as_ref().cloned() else {
                return;
            };
            unsafe {
                let cur = player.GetVolume().unwrap_or(1.0);
                let muted = player.GetMute().unwrap_or_default().as_bool();
                let vol = (cur + delta).clamp(0.0, 1.0);
                if delta > 0.0 && muted {
                    let _ = player.SetMute(false);
                }
                let _ = player.SetVolume(vol);
                CONTROL_STATE.with(|s| {
                    let mut st = s.get();
                    st.volume = (vol * 100.0).round() as i32;
                    s.set(st);
                });
                VOLUME_FLASH_UNTIL.with(|t| {
                    t.set(Some(std::time::Instant::now() + std::time::Duration::from_millis(1200)))
                });
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                    Some(HWND(controls_hwnd as *mut core::ffi::c_void)),
                    None,
                    false,
                );
            }
        });
    }

    unsafe fn create_controls_window(
        parent: HWND,
        callbacks: super::ControlCallbacks,
    ) -> Option<HWND> {
        let class = control_class();
        let mut wc = WNDCLASSW::default();
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(control_wnd_proc);
        wc.lpszClassName = PCWSTR(class.as_ptr());
        let _ = RegisterClassW(&wc);
        let state = Box::new(ControlWindowData {
            callbacks,
            owner: parent.0 as isize,
            dragging: false,
        });
        let state_ptr = Box::into_raw(state);
        let created = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            None,
            Some(state_ptr as *const core::ffi::c_void),
        );
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(_) => {
                drop(Box::from_raw(state_ptr));
                return None;
            }
        };
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 205, LWA_ALPHA);
        Some(hwnd)
    }

    unsafe extern "system" fn control_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                LRESULT(1)
            }
            WM_NCDESTROY => {
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut ControlWindowData;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_PAINT => {
                paint_controls(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // 控制栏需要接收鼠标点击，但绝不能成为活动窗口或夺走键盘焦点。
            // 返回 MA_NOACTIVATE 后，空格键仍由主 Slint 窗口的 FocusScope 处理。
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONDOWN => {
                let x = mouse_x(lparam);
                let width = client_width(hwnd);
                let (track_left, track_right) = track_bounds(width);
                if x < 46 {
                    invoke_control(hwnd, 0, 0.0);
                } else if x < 82 {
                    invoke_control(hwnd, 1, 0.0);
                } else if x >= track_left && x <= track_right {
                    set_dragging(hwnd, true);
                    invoke_control(hwnd, 3, seek_ratio(x, track_left, track_right));
                } else if x >= width - 52 {
                    invoke_control(hwnd, 2, 0.0);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if is_dragging(hwnd) {
                    let x = mouse_x(lparam);
                    let (track_left, track_right) = track_bounds(client_width(hwnd));
                    invoke_control(hwnd, 3, seek_ratio(x, track_left, track_right));
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                set_dragging(hwnd, false);
                LRESULT(0)
            }
            // 控制条上滚轮同样调节音量（与画面区手感一致）
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
                adjust_volume(delta * 0.05);
                LRESULT(0)
            }
            // 兜底：焦点若意外落到控制条，空格/Esc 本会被 DefWindowProc 吞掉，
            // 这里转发到关闭回调，保证按键手感与预览窗口一致。
            WM_KEYDOWN if wparam.0 == VK_SPACE_CODE || wparam.0 == VK_ESCAPE_CODE => {
                invoke_control(hwnd, 4, 0.0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn paint_controls(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let background = CreateSolidBrush(COLORREF(0x00302a28));
        let _ = FillRect(hdc, &rc, background);
        let _ = DeleteObject(background.into());

        let state = CONTROL_STATE.with(|s| s.get());
        let width = rc.right - rc.left;
        let (track_left, track_right) = track_bounds(width);
        // 参考样图：整体略偏上居中，48px 控制条内轨道位于 24px 基线。
        let track_y = rc.bottom / 2;
        let ratio = if state.duration > 0 {
            (state.position as f32 / state.duration as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_x = track_left + ((track_right - track_left) as f32 * ratio) as i32;

        let bg = CreateSolidBrush(COLORREF(0x00d6d3d1));
        let old_bg_brush = SelectObject(hdc, bg.into());
        let bg_pen = CreatePen(PS_SOLID, 1, COLORREF(0x00d6d3d1));
        let old_bg_pen = SelectObject(hdc, bg_pen.into());
        let _ = RoundRect(hdc, track_left, track_y - 1, track_right, track_y + 2, 3, 3);
        let _ = SelectObject(hdc, old_bg_pen);
        let _ = DeleteObject(bg_pen.into());
        let _ = SelectObject(hdc, old_bg_brush);
        let _ = DeleteObject(bg.into());

        let white = CreateSolidBrush(COLORREF(0x00ffffff));
        let old_brush = SelectObject(hdc, white.into());
        let white_track_pen = CreatePen(PS_SOLID, 1, COLORREF(0x00ffffff));
        let old_track_pen = SelectObject(hdc, white_track_pen.into());
        let _ = RoundRect(hdc, track_left, track_y - 1, thumb_x, track_y + 2, 3, 3);
        // 样图采用空心圆形滑块：白色描边、内部透出控制栏背景。
        let _ = SelectObject(hdc, old_brush);
        let hollow = CreateSolidBrush(COLORREF(0x00302a28));
        let old_hollow = SelectObject(hdc, hollow.into());
        let _ = Ellipse(hdc, thumb_x - 8, track_y - 8, thumb_x + 9, track_y + 9);
        let _ = SelectObject(hdc, old_hollow);
        let _ = DeleteObject(hollow.into());
        let _ = SelectObject(hdc, old_track_pen);
        let _ = DeleteObject(white_track_pen.into());
        let _ = DeleteObject(white.into());

        // GDI 几何线条没有抗锯齿，细线重播/音量会呈现明显锯齿。改用 Windows
        // 自带 Segoe MDL2 Assets 媒体字形，与系统播放器同源并由字体栅格器抗锯齿。
        let font_name: Vec<u16> = "Segoe MDL2 Assets\0".encode_utf16().collect();
        let icon_font = CreateFontW(
            -20,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, icon_font.into());
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x00ffffff));
        draw_mdl2_icon(hdc, if state.paused { '\u{E768}' } else { '\u{E769}' }, 12, 0, 48, 48);
        draw_mdl2_icon(hdc, '\u{E72C}', 46, 0, 40, 48);
        draw_mdl2_icon(
            hdc,
            if state.muted { '\u{E74F}' } else { '\u{E767}' },
            width - 54,
            0,
            48,
            48,
        );
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(icon_font.into());
        // 滚轮调节后 1.2s 内时间区改显音量百分比（控制条 250ms 轮询重绘，到期自动恢复）
        let flashing = VOLUME_FLASH_UNTIL
            .with(|t| t.get())
            .is_some_and(|until| std::time::Instant::now() < until);
        let label = if flashing {
            format!("音量 {}%", state.volume)
        } else {
            let safe_pos = state.position.max(0);
            format!("{:02}:{:02}", safe_pos / 60, safe_pos % 60)
        };
        let mut time = label.encode_utf16().collect::<Vec<_>>();
        let mut timer = RECT {
            left: width - 126,
            top: 0,
            right: width - 58,
            bottom: rc.bottom,
        };
        let _ = DrawTextW(
            hdc,
            &mut time,
            &mut timer,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
        let _ = EndPaint(hwnd, &ps);
    }

    unsafe fn draw_mdl2_icon(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        glyph: char,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) {
        let mut text = [glyph as u16];
        let mut rect = RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
        let _ = DrawTextW(
            hdc,
            &mut text,
            &mut rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    fn control_data(hwnd: HWND) -> *mut ControlWindowData {
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ControlWindowData }
    }

    fn invoke_control(hwnd: HWND, action: u8, ratio: f32) {
        let ptr = control_data(hwnd);
        if ptr.is_null() {
            return;
        }
        unsafe {
            match action {
                0 => ((*ptr).callbacks.toggle_play)(),
                1 => ((*ptr).callbacks.replay)(),
                2 => ((*ptr).callbacks.mute)(),
                3 => ((*ptr).callbacks.seek)(ratio),
                4 => ((*ptr).callbacks.close)(),
                _ => {}
            }
            // 不在这里调用 SetActiveWindow/SetFocus：WM_MOUSEACTIVATE 已保证焦点从未离开
            // 主窗口，事后强制激活反而会让 DWM 重绘原生非客户区边框。
        }
    }

    fn set_dragging(hwnd: HWND, dragging: bool) {
        let ptr = control_data(hwnd);
        if !ptr.is_null() {
            unsafe { (*ptr).dragging = dragging; }
        }
    }

    fn is_dragging(hwnd: HWND) -> bool {
        let ptr = control_data(hwnd);
        !ptr.is_null() && unsafe { (*ptr).dragging }
    }

    fn mouse_x(lparam: LPARAM) -> i32 {
        (lparam.0 & 0xffff) as i16 as i32
    }

    fn client_width(hwnd: HWND) -> i32 {
        unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            (rc.right - rc.left).max(1)
        }
    }

    fn track_bounds(width: i32) -> (i32, i32) {
        // 参考图间距：左侧暂停/重播之后留 16px，右侧为时间与音量留足空间。
        (96, (width - 132).max(97))
    }

    fn seek_ratio(x: i32, left: i32, right: i32) -> f32 {
        ((x - left) as f32 / (right - left).max(1) as f32).clamp(0.0, 1.0)
    }

    fn position_controls(hwnd: HWND, rect: (i32, i32, i32, i32)) {
        unsafe {
            let owner = control_data(hwnd);
            let owner_hwnd = if owner.is_null() {
                None
            } else {
                Some(HWND((*owner).owner as *mut core::ffi::c_void))
            };
            if let Some(owner_hwnd) = owner_hwnd {
                let mut origin = windows::Win32::Foundation::POINT {
                    x: rect.0,
                    y: rect.1 + rect.3 - CONTROL_H,
                };
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(owner_hwnd, &mut origin);
                // 不带 SWP_SHOWWINDOW：显示与否统一由 reveal / set_controls_visible 决定，
                // 否则媒体加载期间的每次定位都会把控制条提前亮出来。
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    origin.x,
                    origin.y,
                    rect.2,
                    CONTROL_H,
                    SWP_NOACTIVATE,
                );
            }
        }
    }

    pub fn set_controls_visible(visible: bool) {
        CONTROL_STATE.with(|s| {
            let mut state = s.get();
            state.visible = visible;
            s.set(state);
        });
        // 媒体尚未就绪：只记状态，实际显示交给 reveal
        if !REVEALED.with(|r| r.get()) {
            return;
        }
        ACTIVE.with(|a| {
            if let Some((_, _, hwnd)) = a.borrow().as_ref() {
                unsafe {
                    let overlay = HWND(*hwnd as *mut core::ffi::c_void);
                    // 同 reveal：本函数由 250ms 鼠标轮询反复调用，用 SW_SHOW 会
                    // 在光标每次移入预览区时抢一次焦点，空格随之失效。
                    let _ = ShowWindow(
                        overlay,
                        if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
                    );
                }
            }
        });
    }

    pub fn update_controls(position: i32, duration: i32, paused: bool, muted: bool) {
        CONTROL_STATE.with(|s| {
            s.set(ControlState {
                position,
                duration,
                paused,
                muted,
                visible: s.get().visible,
                volume: s.get().volume,
            })
        });
        ACTIVE.with(|a| {
            if let Some((_, _, hwnd)) = a.borrow().as_ref() {
                unsafe {
                    let _ = InvalidateRect(
                        Some(HWND(*hwnd as *mut core::ffi::c_void)),
                        None,
                        false,
                    );
                }
            }
        });
    }

    pub fn is_paused() -> bool {
        PAUSED.with(|p| p.get())
    }

    /// 暂停 / 播放切换，返回切换后是否处于暂停态（true=已暂停）
    pub fn toggle_play() -> bool {
        ACTIVE.with(|a| {
            if let Some((player, _, _)) = a.borrow().as_ref() {
                let new_paused = !PAUSED.with(|p| p.get());
                unsafe {
                    if new_paused {
                        let _ = player.Pause();
                    } else {
                        let _ = player.Play();
                    }
                }
                PAUSED.with(|p| p.set(new_paused));
                new_paused
            } else {
                false
            }
        })
    }

    /// 当前播放位置与总时长（单位 100ns），未在播放返回 (0, 0)
    pub fn position() -> (i64, i64) {
        ACTIVE.with(|a| {
            if let Some((player, _, _)) = a.borrow().as_ref() {
                unsafe {
                    let cur = player
                        .GetPosition(&MFP_POSITIONTYPE_100NS)
                        .ok()
                        .map(|pv| pv_i64(&pv))
                        .unwrap_or(0);
                    let dur = player
                        .GetDuration(&MFP_POSITIONTYPE_100NS)
                        .ok()
                        .map(|pv| pv_i64(&pv))
                        .unwrap_or(0);
                    (cur, dur)
                }
            } else {
                (0, 0)
            }
        })
    }

    /// 跳转到指定 100ns 位置
    pub fn seek_100ns(pos: i64) {
        ACTIVE.with(|a| {
            if let Some((player, _, _)) = a.borrow().as_ref() {
                unsafe {
                    let mut pv = PROPVARIANT::default();
                    set_pv_i8(&mut pv, pos);
                    let _ = player.SetPosition(&MFP_POSITIONTYPE_100NS, &pv);
                }
            }
        });
    }

    /// 设置静音（IMFPMediaPlayer::SetMute）
    pub fn set_muted(muted: bool) {
        ACTIVE.with(|a| {
            if let Some((player, _, _)) = a.borrow().as_ref() {
                unsafe {
                    let _ = player.SetMute(muted);
                }
            }
        });
    }

    /// 当前是否静音（未在播放返回 false）
    pub fn is_muted() -> bool {
        ACTIVE.with(|a| {
            if let Some((player, _, _)) = a.borrow().as_ref() {
                unsafe { player.GetMute().unwrap_or_default().as_bool() }
            } else {
                false
            }
        })
    }

    /// 从 PROPVARIANT 偏移 8 字节处读 i64（VT_I8/VT_UI8 的值；vt 在偏移 0）
    unsafe fn pv_i64(pv: &PROPVARIANT) -> i64 {
        *((pv as *const PROPVARIANT as *const u8).add(8) as *const i64)
    }

    /// 构造 VT_I8 的 PROPVARIANT（vt=20 写偏移 0，值写偏移 8）
    unsafe fn set_pv_i8(pv: &mut PROPVARIANT, val: i64) {
        let p = pv as *mut PROPVARIANT as *mut u8;
        *(p as *mut u16) = 20; // VT_I8
        *((p.add(8)) as *mut i64) = val;
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_par, apply_rotation, display_size};

    /// 方形像素（Shell 对绝大多数视频报 1:1）必须原样保留编码尺寸。
    /// 回归用例：曾把 PAR 误当显示尺寸喂给 display_size，
    /// 使 720×960 竖屏塌成 720×720、852×480 横屏塌成 480×480。
    #[test]
    fn par_one_to_one_keeps_frame_size() {
        assert_eq!(apply_par((720, 960), (1, 1)), (720, 960));
        assert_eq!(apply_par((852, 480), (1, 1)), (852, 480));
        assert_eq!(apply_par((1920, 1080), (1, 1)), (1920, 1080));
    }

    /// PAR 字段缺失（读不到属性）时同样保留编码尺寸
    #[test]
    fn par_missing_keeps_frame_size() {
        assert_eq!(apply_par((720, 960), (0, 0)), (720, 960));
    }

    /// 非方形像素按 PAR 拉伸宽度，高度不变（PAL DV 720×576 PAR 64:45 → 1024×576）
    #[test]
    fn par_non_square_stretches_width_only() {
        assert_eq!(apply_par((720, 576), (64, 45)), (1024, 576));
    }

    /// 90/270 度旋转交换宽高，0/180 度保持原样
    #[test]
    fn rotation_swaps_only_on_quarter_turns() {
        assert_eq!(apply_rotation((1920, 1080), 0), (1920, 1080));
        assert_eq!(apply_rotation((1920, 1080), 90), (1080, 1920));
        assert_eq!(apply_rotation((1920, 1080), 180), (1920, 1080));
        assert_eq!(apply_rotation((1920, 1080), 270), (1080, 1920));
    }

    #[test]
    fn display_size_keeps_landscape_and_portrait() {
        assert_eq!(display_size((1920, 1080), (16, 9)), (1920, 1080));
        assert_eq!(display_size((1080, 1920), (9, 16)), (1080, 1920));
    }

    #[test]
    fn display_size_applies_rotation_aspect() {
        assert_eq!(display_size((1920, 1080), (9, 16)), (1080, 1920));
    }

    #[test]
    fn display_size_applies_non_square_pixels() {
        assert_eq!(display_size((720, 576), (16, 9)), (1024, 576));
    }
}
