//! OLE 拖入：接收其他应用拖来的文件（微信图片、资源管理器、浏览器等），
//! 放下后复制到当前打开的目录（复制语义，源文件不动）。
//!
//! winit 创建窗口时已注册自己的 IDropTarget（产生 WindowEvent::DroppedFile，
//! Slint 后端并不消费该事件），而每个窗口同时只允许注册一个拖放目标：
//! install 先 RevokeDragDrop 撤销 winit 的目标，再注册本模块的 IDropTarget。
//!
//! 数据格式：优先 CF_HDROP（微信 / 资源管理器等提供真实文件路径，交宿主走
//! 任务管线复制，带进度与同名冲突询问）；回退 FileGroupDescriptorW +
//! FileContents（浏览器拖图片等虚拟文件源，无磁盘路径，直接写入目标目录并
//! 自动避让重名）。仅当来源允许复制效果时接受拖放（尊重源语义）。
//!
//! 已知限制：虚拟文件路径在 Drop 回调内于 UI 线程同步整文件写盘（OLE 的
//! IStream 须在 STA 线程消费，编组到后台线程成本高），数百 MB 以上的大
//! 文件期间 UI 与源应用会短暂无响应；真实文件（CF_HDROP）路径走后台任务
//! 管线，无此问题。
//!
//! 线程前提：winit 已对 UI 线程 OleInitialize（STA），OLE 把回调投递到注册
//! 窗口所在线程；宿主钩子经 thread_local 存取，Drop 内同步执行（与右键
//! 菜单模态循环同模式，可安全借用 UI 线程状态）。

use std::path::PathBuf;

/// 宿主钩子：拖入落地时由 IDropTarget 回调（均在 UI 线程）。
pub struct DropHost {
    /// 返回接收拖入文件的目标目录；None = 当前目录不接受拖入（虚拟目录等）
    pub dst: Box<dyn Fn() -> Option<PathBuf>>,
    /// 把真实文件路径交给宿主入队复制任务（带进度 UI 与冲突询问）
    pub enqueue_copy: Box<dyn Fn(Vec<PathBuf>, PathBuf)>,
    /// 虚拟文件已直接写入目标目录：参数为 (成功数, 失败数)
    pub virtual_done: Box<dyn Fn(usize, usize)>,
}

#[cfg(windows)]
thread_local! {
    static HOST: std::cell::RefCell<Option<DropHost>> = const { std::cell::RefCell::new(None) };
}

/// 在指定窗口注册拖放目标。须等窗口真正创建后调用（HWND 有效）。
#[cfg(windows)]
pub fn install(hwnd: isize, host: DropHost) -> bool {
    HOST.with(|c| *c.borrow_mut() = Some(host));
    win_impl::register(hwnd)
}

#[cfg(not(windows))]
pub fn install(_hwnd: isize, _host: DropHost) -> bool {
    false
}

#[cfg(windows)]
mod win_impl {
    use super::HOST;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use windows::core::{implement, PCWSTR};
    use windows::Win32::Foundation::{HGLOBAL, HWND, POINTL};
    use windows::Win32::System::Com::{
        DVASPECT_CONTENT, FORMATETC, IDataObject, IStream, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP,
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{DragQueryFileW, FILEDESCRIPTORW, HDROP};

    thread_local! {
        /// DragEnter 探测的「是否含文件」缓存：拖拽期间格式集合不变，
        /// DragOver 每次触发（高频）只需读缓存，不重复探测
        static ACCEPTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub fn register(hwnd: isize) -> bool {
        unsafe {
            let h = HWND(hwnd as *mut core::ffi::c_void);
            // winit 启动时已注册其拖放目标（其事件被 Slint 后端忽略）：
            // 必须先撤销才能注册本模块的目标；事件循环退出时 winit 统一 Revoke，
            // 撤到本模块的目标也无害
            let _ = RevokeDragDrop(h);
            let target: IDropTarget = DropTarget.into();
            RegisterDragDrop(h, &target).is_ok()
        }
    }

    /// 极简拖放目标：全程只提供「复制」效果，Drop 时解析文件并回调宿主
    #[implement(IDropTarget)]
    struct DropTarget;

    impl IDropTarget_Impl for DropTarget_Impl {
        fn DragEnter(
            &self,
            pdataobj: windows::core::Ref<'_, IDataObject>,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            // QueryGetData 只探测格式可用性，不会触发源的延迟渲染
            let accepts = unsafe { pdataobj.as_ref().map_or(false, |d| has_file_formats(d)) };
            ACCEPTS.set(accepts);
            unsafe {
                *pdweffect = if accepts {
                    *pdweffect & DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
            Ok(())
        }

        fn DragOver(
            &self,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            unsafe {
                *pdweffect = if ACCEPTS.get() {
                    *pdweffect & DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            ACCEPTS.set(false);
            Ok(())
        }

        fn Drop(
            &self,
            pdataobj: windows::core::Ref<'_, IDataObject>,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            ACCEPTS.set(false);
            unsafe { *pdweffect = DROPEFFECT_NONE };
            let Some(data) = pdataobj.as_ref() else {
                return Ok(());
            };
            // 目标目录在 Drop 时刻决定（拖拽途中用户可能切换了活动面板）
            let dst = HOST.with(|h| h.borrow().as_ref().and_then(|host| (host.dst)()));
            let Some(dst) = dst else {
                return Ok(());
            };
            unsafe {
                // 1) 真实文件路径：交给宿主任务管线（进度 / 同名冲突询问）
                let paths = extract_hdrop(data);
                if !paths.is_empty() {
                    HOST.with(|h| (h.borrow().as_ref().unwrap().enqueue_copy)(paths, dst));
                    return Ok(());
                }
                // 2) 虚拟文件（浏览器拖图等）：直接写入目标目录
                let (ok, fail) = extract_virtual_files(data, &dst);
                if ok + fail > 0 {
                    HOST.with(|h| (h.borrow().as_ref().unwrap().virtual_done)(ok, fail));
                }
            }
            Ok(())
        }
    }

    /// (FileGroupDescriptorW, FileContents) 剪贴板格式号（注册失败为 0，缓存一次）
    fn virtual_formats() -> (u16, u16) {
        static FMT: std::sync::OnceLock<(u16, u16)> = std::sync::OnceLock::new();
        *FMT.get_or_init(|| unsafe {
            (
                reg_format("FileGroupDescriptorW"),
                reg_format("FileContents"),
            )
        })
    }

    unsafe fn reg_format(name: &str) -> u16 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) as u16
    }

    /// 探测数据对象是否含可接收的文件格式（真实路径或虚拟文件）
    unsafe fn has_file_formats(data: &IDataObject) -> bool {
        let (fgd, _) = virtual_formats();
        let mut fmt = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        if data.QueryGetData(&fmt).is_ok() {
            return true;
        }
        if fgd != 0 {
            fmt.cfFormat = fgd;
            if data.QueryGetData(&fmt).is_ok() {
                return true;
            }
        }
        false
    }

    /// 从数据对象读取 CF_HDROP（真实文件路径列表）。无该格式返回空。
    unsafe fn extract_hdrop(data: &IDataObject) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let fmt = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let Ok(mut medium) = data.GetData(&fmt) else {
            return out;
        };
        // CF_HDROP 的 HGLOBAL 可直接作为 HDROP 交给 DragQueryFileW（同系统剪贴板用法）
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h: HGLOBAL = medium.u.hGlobal;
            if !h.0.is_null() {
                let count = DragQueryFileW(HDROP(h.0), 0xFFFF_FFFF, None);
                for i in 0..count {
                    let len = DragQueryFileW(HDROP(h.0), i, None) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(HDROP(h.0), i, Some(&mut buf));
                    if let Some(pos) = buf.iter().position(|&c| c == 0) {
                        buf.truncate(pos);
                    }
                    out.push(PathBuf::from(String::from_utf16_lossy(&buf)));
                }
            }
        }
        ReleaseStgMedium(&mut medium as *mut STGMEDIUM);
        out
    }

    /// 读取 FileGroupDescriptorW + FileContents（虚拟文件：源里没有真实磁盘路径），
    /// 逐项拉取 IStream 写入 `dst`（重名自动避让为「名称 (2)」）。返回 (成功, 失败)。
    unsafe fn extract_virtual_files(data: &IDataObject, dst: &Path) -> (usize, usize) {
        let (fmt_fgd, fmt_fc) = virtual_formats();
        if fmt_fgd == 0 || fmt_fc == 0 {
            return (0, 0);
        }
        let fmt = FORMATETC {
            cfFormat: fmt_fgd,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let Ok(mut medium) = data.GetData(&fmt) else {
            return (0, 0);
        };
        let mut ok = 0;
        let mut fail = 0;
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h: HGLOBAL = medium.u.hGlobal;
            let total = GlobalSize(h);
            let base = GlobalLock(h) as *const u8;
            if !base.is_null() && total >= 4 {
                let count = *(base as *const u32) as usize;
                let item = std::mem::size_of::<FILEDESCRIPTORW>();
                // 防御：源进程提供的块大小不可信，超界或数量异常整批放弃
                if count > 0 && count <= 4096 && total >= 4 + count * item {
                    for i in 0..count {
                        // FILEDESCRIPTORW 是 packed 结构：整块 read_unaligned 到
                        // 对齐的本地副本后再取字段（直接取字段引用是未对齐引用 UB）
                        let d: FILEDESCRIPTORW =
                            std::ptr::read_unaligned(base.add(4 + i * item) as *const FILEDESCRIPTORW);
                        let Some(name) = sanitize_name(d.cFileName) else {
                            continue;
                        };
                        let target = super::super::operations::resolve_conflict(dst.join(&name));
                        if copy_stream_to_file(data, fmt_fc, i as i32, &target) {
                            ok += 1;
                        } else {
                            fail += 1;
                        }
                    }
                }
                let _ = GlobalUnlock(h);
            }
        }
        ReleaseStgMedium(&mut medium as *mut STGMEDIUM);
        (ok, fail)
    }

    /// 按 lindex 拉取 FileContents 流写入 `target`。返回是否完整写入。
    unsafe fn copy_stream_to_file(
        data: &IDataObject,
        cf_filecontents: u16,
        index: i32,
        target: &Path,
    ) -> bool {
        let fmt = FORMATETC {
            cfFormat: cf_filecontents,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index,
            tymed: TYMED_ISTREAM.0 as u32,
        };
        let Ok(mut medium) = data.GetData(&fmt) else {
            return false;
        };
        let mut written = false;
        if medium.tymed == TYMED_ISTREAM.0 as u32 {
            // 先克隆接口（AddRef）再释放 STGMEDIUM，避免悬垂
            let inner: &Option<IStream> = &medium.u.pstm;
            let stream = inner.clone();
            if let Some(stream) = stream {
                written = (|| -> bool {
                    let Ok(file) = std::fs::File::create(target) else {
                        return false;
                    };
                    let mut out = std::io::BufWriter::new(file);
                    let mut buf = [0u8; 64 * 1024];
                    let ok = loop {
                        let mut got = 0u32;
                        let hr = stream.Read(
                            buf.as_mut_ptr() as *mut core::ffi::c_void,
                            buf.len() as u32,
                            Some(&mut got),
                        );
                        if hr.is_err() {
                            break false;
                        }
                        if got == 0 {
                            break true;
                        }
                        if out.write_all(&buf[..got as usize]).is_err() {
                            break false;
                        }
                    } && out.flush().is_ok();
                    if !ok {
                        // 失败时清理半截文件，避免残留不完整内容被误当成完整文件
                        let _ = std::fs::remove_file(target);
                    }
                    ok
                })();
            }
        }
        ReleaseStgMedium(&mut medium as *mut STGMEDIUM);
        written
    }

    /// 文件名消毒：只取路径末段（去掉源提供的分隔符与盘符，防写入任意路径），
    /// 「.」「..」与空名拒绝。返回 None 表示跳过该项。
    /// 参数按值取（FILEDESCRIPTORW 是 packed 结构，字段不可按引用访问）
    fn sanitize_name(wide: [u16; 260]) -> Option<String> {
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        let raw = String::from_utf16_lossy(&wide[..end]);
        let raw = raw.rsplit(['\\', '/', ':']).next().unwrap_or("").trim();
        if raw.is_empty() || raw == "." || raw == ".." {
            return None;
        }
        Some(raw.to_string())
    }
}
