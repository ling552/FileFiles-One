// FileFiles One 入口：初始化主窗口、多标签页、绑定全部回调到真实文件系统
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod fs;
mod git;
mod preview_host;
mod ui_bridge;
mod update;

slint::include_modules!();

use app::{AppCore, ClipMode};
use fs::operations as ops;
use slint::ComponentHandle;
use slint::Model;
// 无边框窗口下访问底层 winit 窗口以实现自定义标题栏拖动
use slint::winit_030::WinitWindowAccessor;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn home_start_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn startup_path(setting: &str) -> PathBuf {
    match setting {
        "this-pc" => PathBuf::from(fs::virtualfs::THIS_PC_PATH),
        "quick" | "last" => home_start_path(),
        _ => home_start_path(),
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // UAC 提权子进程只执行白名单文件操作，不创建主窗口。
    if fs::elevated::handle_startup_args() {
        return Ok(());
    }
    // 安装程序卸载前调用：不创建窗口，只安全恢复仍由本应用持有的 Shell 关联。
    if std::env::args_os().any(|arg| arg == "--unregister-default-file-manager") {
        let _ = fs::default_app::set_default(false);
        return Ok(());
    }

    // 抑制 Slint 文本分词触发的 ICU4X "No segmentation model for language: ja"
    // 警告刷屏。
    log::set_max_level(log::LevelFilter::Error);

    let ui = MainWindow::new()?;

    // 启动目录：命令行参数优先（作为默认文件管理器被系统调起时传入目标目录），
    // 否则按用户设置进入，此电脑映射到虚拟驱动器根。
    // 用 args_os 避免非法 Unicode 路径（NTFS 允许未配对代理项）导致 panic。
    let startup_config = config::AppConfig::load();
    let arg_dir = std::env::args_os()
        .nth(1)
        .map(|os| {
            let s = os.to_string_lossy().into_owned();
            // 经典转义修复：shell 展开 "%1" 为 "D:\" 时，尾反斜杠+引号被
            // CommandLineToArgvW 解析成字面引号（参数变为 D:"）——去掉尾引号，
            // 纯盘符（"D:"）补回反斜杠成盘根
            let s = s.trim_end_matches('"').to_string();
            if s.len() == 2 && s.ends_with(':') {
                PathBuf::from(format!("{}\\", s))
            } else {
                PathBuf::from(s)
            }
        })
        .filter(|p| p.is_dir());
    let start = arg_dir.unwrap_or_else(|| startup_path(&startup_config.settings.startup_open));
    // 「默认文件管理器」自愈仅修复带本应用所有权标记的缺失项或旧安装路径；
    // 若用户后来改用其它文件管理器，不会静默夺回关联。
    let default_fm_state = if startup_config.settings.default_file_manager {
        fs::default_app::registration_state()
    } else {
        fs::default_app::RegistrationState::Disabled
    };
    let default_fm_repair_error = if startup_config.settings.default_file_manager
        && default_fm_state == fs::default_app::RegistrationState::Repairable
    {
        fs::default_app::set_default(true).err()
    } else {
        None
    };
    let core = Rc::new(RefCell::new(AppCore::new(start.clone())));
    core.borrow_mut().config = startup_config;

    // 注册侧栏图标后台加载完成后的重建入口（须在首次 build_sidebar 之前）
    ui_bridge::init_sidebar_warm(ui.as_weak());

    // 首次加载
    load_current(&ui, &core);
    // 右侧独立面板首次加载（双面板视图用）
    load_right(&ui, &core);
    let state = ui.global::<AppState>();
    {
        let c = core.borrow();
        state.set_nav_items(ui_bridge::build_sidebar(
            &start,
            &c.collapsed_sections,
            &c.config,
        ));
    }

    // 启动时把持久化的用户设置推送到 Theme 与 AppState
    push_settings(&ui, &core);
    if default_fm_state == fs::default_app::RegistrationState::External {
        // 其它程序已接管至少一个入口：安全恢复本应用仍持有的其余入口，再关闭配置开关。
        let _ = fs::default_app::set_default(false);
        state.set_set_default_fm(false);
        core.borrow_mut().config.settings.default_file_manager = false;
        core.borrow().config.save();
        state.set_status_text("默认文件管理器已由其它程序接管，已关闭本应用开关".into());
    } else if let Some(error) = default_fm_repair_error {
        state.set_status_text(format!("默认文件管理器自愈失败: {}", error).into());
    }

    // 启动时把持久化的栏宽/列宽推送到 AppState
    {
        let lay = &core.borrow().config.layout;
        state.set_sidebar_w(lay.sidebar_w);
        state.set_details_w(lay.details_w);
        state.set_col_modified_w(lay.col_modified);
        state.set_col_kind_w(lay.col_kind);
        state.set_col_size_w(lay.col_size);
        state.set_col_block_w(lay.col_block.max(216.0));
        // 列显示顺序：三个槽位互不重复才应用，避免损坏的配置造成两列重叠
        let (mo, ko, so) = (lay.col_modified_ord, lay.col_kind_ord, lay.col_size_ord);
        let mut slots = [mo, ko, so];
        slots.sort_unstable();
        if slots == [0, 1, 2] {
            state.set_col_modified_ord(mo);
            state.set_col_kind_ord(ko);
            state.set_col_size_ord(so);
        }
        state.set_dual_ratio(lay.dual_ratio.clamp(0.15, 0.85));
    }

    // 恢复上次关闭时的窗口位置与大小（物理像素；win_w<=0 表示首启，用默认值）。
    // 必须等 winit 窗口真正创建后用 winit 原生物理像素 API 应用（实测创建发生在
    // 事件循环启动后 80-250ms 之间）：过早经 Slint set_size 设置会在 scale_factor
    // 尚为 1.0 时被记成逻辑尺寸，窗口显示后按实际 DPI 再放大一次，每次重启复利膨胀。
    {
        let (x, y, w, h, maximized) = {
            let lay = &core.borrow().config.layout;
            (
                lay.win_x,
                lay.win_y,
                lay.win_w,
                lay.win_h,
                lay.win_maximized,
            )
        };
        restore_window_geometry(&ui, x, y, w, h, maximized, 20);
    }

    // 启动目录优先使用其已保存布局；未记录目录才使用全局默认视图。
    apply_folder_layout(&ui, &core);

    // 启动时推送已保存的网络位置列表（设置「云存储账号」页展示）
    ui_bridge::push_network_locations(&ui, &core.borrow());
    // 启动时推送自定义标签定义（工具栏「标记」下拉与侧栏同步）
    ui_bridge::push_custom_tags(&ui, &core.borrow());

    // —— 绑定全部回调 ——
    bind_navigation(&ui, &core);
    bind_selection(&ui, &core);
    bind_operations(&ui, &core);
    bind_new_menu(&ui, &core);
    bind_context_menu_ext(&ui, &core);
    bind_view_and_search(&ui, &core);
    bind_hash(&ui, &core);
    bind_tabs(&ui, &core);
    bind_window_chrome(&ui, &core);
    bind_layout(&ui, &core);
    bind_settings(&ui, &core);
    bind_right_pane(&ui, &core);

    // 当前目录实时监听：外部程序改动目录内容时自动软刷新（保留搜索与选中项）
    bind_watcher(&ui, &core);

    // 同名冲突对话框：复制 / 移动遇到目标已存在同名项时询问用户处置方式
    bind_conflict(&ui, &core);

    // 命令面板（Ctrl+P）：Rust 过滤命令 + 路径跳转
    bind_command_palette(&ui, &core);

    // 设备/驱动器热插拔定时轮询：插拔 U 盘/手机或挂载/卸载分区时自动刷新侧边栏与此电脑视图
    bind_device_polling(&ui, &core);

    // 后台预热侧栏缓存（特殊目录系统图标 + 快速访问枚举），完成后重建侧栏，
    // 避免 UI 线程在 build_sidebar 内同步做 COM 提取造成动画卡顿
    warm_sidebar_caches(&ui);

    // 应用更新：关于页 GitHub 链接 + 检查更新 / 带进度下载 / 启动安装
    bind_update(&ui, &core);

    // 文件名索引：重建（带进度）+ 后台索引开关的启动自动重建
    bind_index(&ui, &core);

    // 预热独立预览窗口：启动后空闲时创建（保持隐藏），把首次空格预览的
    // Slint 窗口/着色器初始化开销前移到启动期，视频/图片首开不再卡顿。
    warmup_preview_window(&ui);

    ui.run()
}

/// 启动 1.2s 后（避开主窗口首帧与设备枚举高峰）预创建预览窗口实例。
/// 失败静默：首次空格仍会即时创建。
fn warmup_preview_window(ui: &MainWindow) {
    let close_weak = ui.as_weak();
    let web_weak = ui.as_weak();
    let fs_weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(1200), move || {
        let _ = preview_host::ensure_window(
            move || {
                if let Some(ui) = close_weak.upgrade() {
                    ui.global::<AppState>().invoke_close_quicklook();
                }
            },
            move |on| {
                if let Some(ui) = web_weak.upgrade() {
                    ui.global::<AppState>().invoke_ql_set_web_mode(on);
                }
            },
            move || {
                if let Some(ui) = fs_weak.upgrade() {
                    ui.global::<AppState>().invoke_ql_toggle_video_fullscreen();
                }
            },
        );
    });
}

/// 后台预热侧栏缓存：特殊目录系统图标 + 快速访问枚举（均为阻塞 COM，
/// 不能在 UI 线程跑）。完成后回事件循环触发 devices-changed 重建侧栏，
/// 此时 build_sidebar 全部命中缓存，不再卡顿。
fn warm_sidebar_caches(ui: &MainWindow) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        #[cfg(windows)]
        {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            // 特殊目录系统图标（桌面/下载/文档/图片/音乐/视频）
            for dir in [
                dirs::desktop_dir(),
                dirs::download_dir(),
                dirs::document_dir(),
                dirs::picture_dir(),
                dirs::audio_dir(),
                dirs::video_dir(),
            ]
            .into_iter()
            .flatten()
            {
                let p = dir.to_string_lossy().trim_end_matches('\\').to_string();
                let _ = fs::thumbnail::special_dir_icon_cached(&p, 128);
            }
            // 快速访问枚举（填充 15s 缓存）
            let _ = fs::quickaccess::list();
        }
        // 回主线程重建侧栏（此时全部命中缓存）
        let _ = w.upgrade_in_event_loop(|ui| {
            ui.global::<AppState>().invoke_devices_changed();
        });
    });
}

/// 计算"设备 + 驱动器"拓扑签名。WPD、卷标/容量和回收站 API 可能被慢设备
/// 阻塞，必须只在后台线程调用。
fn device_topology_signature() -> String {
    let mut sig = String::new();
    for dev in fs::devices::list_devices() {
        sig.push_str(&dev.path);
        sig.push('\u{1}');
    }
    sig.push('|');
    for d in fs::disk::list_disks() {
        sig.push_str(&d.letter);
        sig.push(':');
        sig.push_str(&d.total.to_string());
        sig.push(';');
    }
    sig.push('|');
    sig.push(if fs::recyclebin::is_empty().unwrap_or(true) {
        '0'
    } else {
        '1'
    });
    sig
}

/// 后台轮询设备/驱动器拓扑，变化时经事件循环回主线程刷新。
///
/// 不用 UI 定时器轮询：定时器需事件循环已在运行才会被排程，而本绑定发生在
/// `ui.run()` 之前——首次枚举的结果会一直等到用户点击（产生输入事件唤醒事件
/// 循环）才被取走，表现为「刚打开时手机不显示，随便点一下才出来」。
/// 改由后台线程用 `upgrade_in_event_loop` 主动唤醒事件循环并回调，
/// 首次枚举一完成就立刻刷新。
fn bind_device_polling(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    // 处理器在主线程，可安全捕获 Rc<RefCell<AppCore>>
    let w = ui.as_weak();
    let c = core.clone();
    ui.global::<AppState>().on_devices_changed(move || {
        let Some(ui) = w.upgrade() else { return };
        let path = c.borrow().active_tab().history.current().clone();
        {
            let cc = c.borrow();
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    &path,
                    &cc.collapsed_sections,
                    &cc.config,
                ));
        }
        if path.to_string_lossy() == fs::virtualfs::THIS_PC_PATH {
            load_current(&ui, &c);
        }
        let r_at_this_pc = c.borrow().right_pane.history.current().to_string_lossy()
            == fs::virtualfs::THIS_PC_PATH;
        if r_at_this_pc {
            load_right(&ui, &c);
        }
    });

    let w = ui.as_weak();
    std::thread::spawn(move || {
        // 后台线程自行初始化 COM（MTA）。UI 线程由 winit 初始化为 STA，
        // 不能在此处或 list_devices_win 中以 MTA 污染 UI 线程。
        #[cfg(windows)]
        {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
        }
        let notify = |w: &slint::Weak<MainWindow>| {
            w.upgrade_in_event_loop(|ui| ui.global::<AppState>().invoke_devices_changed())
        };
        // 首次枚举：DEVICE_CACHE 此前为空，侧边栏与「此电脑」都还没有设备条目，
        // 无论签名如何都要推一次
        let mut last_sig = device_topology_signature();
        if notify(&w).is_err() {
            return;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let sig = device_topology_signature();
            if sig != last_sig {
                last_sig = sig;
                // 窗口已销毁：事件循环不复存在，退出轮询线程
                if notify(&w).is_err() {
                    break;
                }
            }
        }
    });
}

/// 绑定应用更新：关于页的 GitHub 链接、检查更新、带进度下载与安装启动。
/// 网络请求在后台线程执行（ureq 阻塞式），进度经 `invoke_from_event_loop`
/// 回主线程刷新 —— 与后台文件任务同一模式。
fn bind_update(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let state = ui.global::<AppState>();
    // 版本号与仓库地址推送到关于页
    state.set_app_version(update::CURRENT_VERSION.into());
    state.set_repo_url(update::REPO_URL.into());

    // 用系统默认浏览器打开链接（GitHub 仓库 / Issues）
    state.on_open_url(|url| {
        let _ = open::that(url.as_str());
    });

    // 检查结果与下载产物：用 Arc<Mutex> 存放——工作线程回填结果的
    // invoke_from_event_loop 闭包要求 Send，Rc<RefCell> 无法跨线程捕获
    let latest: Arc<Mutex<Option<update::ReleaseInfo>>> = Arc::new(Mutex::new(None));
    let installer: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));

    // —— 检查更新 ——
    {
        let w = ui.as_weak();
        let latest = latest.clone();
        state.on_check_update(move || {
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_update_state(1); // 检查中
            }
            let w = w.clone();
            let latest = latest.clone();
            std::thread::spawn(move || {
                let result = update::check_latest();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = w.upgrade() else { return };
                    let st = ui.global::<AppState>();
                    match result {
                        Ok(info) => {
                            if update::is_newer(&info.version, update::CURRENT_VERSION) {
                                st.set_update_latest_version(info.version.as_str().into());
                                st.set_update_notes(info.notes.as_str().into());
                                st.set_update_state(3); // 发现新版
                                *latest.lock().unwrap() = Some(info);
                            } else {
                                st.set_update_state(2); // 已是最新
                            }
                        }
                        Err(e) => {
                            st.set_update_error(e.into());
                            st.set_update_state(6); // 出错
                        }
                    }
                });
            });
        });
    }

    // —— 下载更新（带进度）——
    {
        let w = ui.as_weak();
        let latest = latest.clone();
        let installer = installer.clone();
        let cancel = cancel.clone();
        state.on_download_update(move || {
            let Some(info) = latest.lock().unwrap().clone() else {
                return;
            };
            cancel.store(false, Ordering::Relaxed);
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                st.set_update_progress(0.0);
                st.set_update_progress_text("准备下载…".into());
                st.set_update_state(4); // 下载中
            }
            let w = w.clone();
            let installer = installer.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let w_prog = w.clone();
                let result = update::download(&info, &cancel, move |done, total, speed| {
                    let frac = if total > 0 {
                        (done as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let text = format!(
                        "{} / {} · {}/s",
                        fs::metadata::human_size(done),
                        fs::metadata::human_size(total),
                        fs::metadata::human_size(speed as u64),
                    );
                    let w = w_prog.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let st = ui.global::<AppState>();
                            st.set_update_progress(frac);
                            st.set_update_progress_text(text.into());
                        }
                    });
                });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = w.upgrade() else { return };
                    let st = ui.global::<AppState>();
                    match result {
                        Ok(path) => {
                            *installer.lock().unwrap() = Some(path);
                            st.set_update_state(5); // 下载完成待安装
                        }
                        // 用户主动取消：回到「发现新版」可重新下载
                        Err(e) if e == "已取消" => st.set_update_state(3),
                        Err(e) => {
                            st.set_update_error(e.into());
                            st.set_update_state(6);
                        }
                    }
                });
            });
        });
    }

    // —— 取消下载 ——
    {
        let cancel = cancel.clone();
        state.on_cancel_download(move || {
            cancel.store(true, Ordering::Relaxed);
        });
    }

    // —— 安装并重启：启动安装程序（分离进程）后退出应用，避免 exe 被占用 ——
    {
        let w = ui.as_weak();
        let c = core.clone();
        state.on_install_update(move || {
            let path = installer.lock().unwrap().clone();
            let Some(path) = path else { return };
            match update::run_installer(&path) {
                Ok(()) => {
                    // 退出前保存窗口几何，安装重启后可恢复位置
                    if let Some(ui) = w.upgrade() {
                        save_window_geometry(&ui, &c);
                    }
                    let _ = slint::quit_event_loop();
                }
                Err(e) => {
                    if let Some(ui) = w.upgrade() {
                        let st = ui.global::<AppState>();
                        st.set_update_error(e.into());
                        st.set_update_state(6);
                    }
                }
            }
        });
    }
}

// ─── 文件名索引 ───

/// 绑定索引重建：设置页「重建索引」按钮 + 后台索引开关的启动自动重建
fn bind_index(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();
    // 启动时推送已有索引的概况（惰性加载磁盘索引文件）
    {
        let info = fs::index::summary();
        if !info.is_empty() {
            state.set_index_info(info.into());
        }
    }

    let w = ui.as_weak();
    let c = core.clone();
    state.on_rebuild_index(move || {
        if let Some(ui) = w.upgrade() {
            start_index_rebuild(&ui, &c);
        }
    });

    // 后台索引开启且尚无索引文件：启动时静默自动重建
    let auto = {
        let cc = core.borrow();
        cc.config.settings.background_index && !fs::index::exists()
    };
    if auto {
        start_index_rebuild(ui, core);
    }
}

/// 启动一次后台索引重建（已在重建中则忽略），进度回填到设置页进度条
fn start_index_rebuild(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    if fs::index::is_rebuilding() {
        return;
    }
    let scope = core.borrow().config.settings.index_location.clone();
    let st = ui.global::<AppState>();
    st.set_index_state(1);
    st.set_index_progress(0.0);
    st.set_index_progress_text("正在枚举目录…".into());

    let w = ui.as_weak();
    std::thread::spawn(move || {
        let w_prog = w.clone();
        let result = fs::index::rebuild(&scope, move |frac, count, current| {
            let text = if current.is_empty() {
                format!("已索引 {} 项", count)
            } else {
                format!("已索引 {} 项 · {}", count, current)
            };
            let w = w_prog.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    let st = ui.global::<AppState>();
                    st.set_index_progress(frac);
                    st.set_index_progress_text(text.into());
                }
            });
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let st = ui.global::<AppState>();
            st.set_index_state(0);
            match result {
                Ok(count) => {
                    st.set_index_info(format!("索引就绪：共 {} 项，深层搜索可用", count).into())
                }
                Err(e) => st.set_index_info(format!("重建失败：{}", e).into()),
            }
        });
    });
}

/// 绑定布局持久化：拖拽分隔条结束后把栏宽/列宽写回配置文件
fn bind_layout(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();
    let c = core.clone();
    state.on_save_layout(move |sidebar, details, col_mod, col_kind, col_size| {
        let mut core = c.borrow_mut();
        let lay = &mut core.config.layout;
        lay.sidebar_w = sidebar;
        lay.details_w = details;
        lay.col_modified = col_mod;
        lay.col_kind = col_kind;
        lay.col_size = col_size;
        core.config.save();
    });

    // 详细视图「固定块」宽度（名称列右缘拖出的分界）持久化
    let c = core.clone();
    state.on_save_col_block(move |block_w| {
        let mut core = c.borrow_mut();
        core.config.layout.col_block = block_w;
        core.config.save();
    });

    // 详细视图列显示顺序持久化（拖拽表头排序松手后）
    let c = core.clone();
    state.on_save_col_order(move |mo, ko, so| {
        let mut core = c.borrow_mut();
        let lay = &mut core.config.layout;
        lay.col_modified_ord = mo;
        lay.col_kind_ord = ko;
        lay.col_size_ord = so;
        core.config.save();
    });

    // 切换条目标签：打标签 / 取消，持久化后刷新（计数 + 当前标签视图）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_toggle_tag(move |idx, key| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let path = {
                let core = c.borrow();
                core.pane_entry_at(right, idx as usize)
                    .map(|e| e.path.clone())
            };
            if let Some(path) = path {
                {
                    let mut core = c.borrow_mut();
                    core.config.toggle_tag(&path, key.as_str());
                    core.config.save();
                }
                // 重新加载活动面板（刷新标签角标 / 若在标签视图则更新列表）+ 侧边栏计数
                reload_active_pane(&ui, &c);
            }
        }
    });

    // 顶部工具栏「标记」下拉：对全部选中项统一切换某标签。
    // 语义：若选中项全部已含该标签 → 全部去除；否则 → 全部添加。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_tag_selected(move |key| {
        if let Some(ui) = w.upgrade() {
            let key = key.to_string();
            // 按活动面板取选中项（双面板右侧活动时作用于右面板）
            let right = toolbar_routes_right(&ui);
            let paths: Vec<String> = {
                let core = c.borrow();
                core.pane_selected_paths(right)
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            };
            if paths.is_empty() {
                return;
            }
            {
                let mut core = c.borrow_mut();
                // 目标状态：仅当全部已含该标签时才去除，否则全部添加
                let all_tagged = paths.iter().all(|p| core.config.has_tag(p, &key));
                let target = !all_tagged;
                for p in &paths {
                    if core.config.has_tag(p, &key) != target {
                        core.config.toggle_tag(p, &key);
                    }
                }
                core.config.save();
            }
            reload_active_pane(&ui, &c);
            // 刷新「标记」下拉的对勾状态
            ui_bridge::update_selection_pane(&ui, &c.borrow(), right);
        }
    });

    // 添加自定义标签：新建定义并持久化，推送至「标记」下拉与侧栏
    let w = ui.as_weak();
    let c = core.clone();
    state.on_add_custom_tag(move |name, color| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                core.config.add_custom_tag(name.as_str(), color.as_str());
                core.config.save();
            }
            ui_bridge::push_custom_tags(&ui, &c.borrow());
            // 侧栏标签分区需重建（新增节点 + 计数）
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    &c.borrow().active_tab().history.current(),
                    &c.borrow().collapsed_sections,
                    &c.borrow().config,
                ));
            ui_bridge::update_selection_pane(&ui, &c.borrow(), toolbar_routes_right(&ui));
        }
    });

    // 删除自定义标签：移除定义并清理文件上的该标签
    let w = ui.as_weak();
    let c = core.clone();
    state.on_remove_custom_tag(move |id| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                core.config.remove_custom_tag(id.as_str());
                core.config.save();
            }
            ui_bridge::push_custom_tags(&ui, &c.borrow());
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    &c.borrow().active_tab().history.current(),
                    &c.borrow().collapsed_sections,
                    &c.borrow().config,
                ));
            reload_active_pane(&ui, &c);
        }
    });

    // 回收站还原：把 $R 移回原位置
    let w = ui.as_weak();
    let c = core.clone();
    state.on_restore_item(move |idx| {
        if let Some(ui) = w.upgrade() {
            let r_path = {
                let core = c.borrow();
                core.pane_entry_at(toolbar_routes_right(&ui), idx as usize)
                    .map(|e| e.path.clone())
            };
            if let Some(r_path) = r_path {
                match fs::recyclebin::restore(&r_path) {
                    Ok(_) => {
                        load_current(&ui, &c);
                        ui.global::<AppState>()
                            .set_status_text("已还原到原位置".into());
                    }
                    Err(e) => {
                        ui.global::<AppState>()
                            .set_status_text(format!("还原失败：{}", e).into());
                    }
                }
            }
        }
    });

    // 回收站：恢复选中项
    let w = ui.as_weak();
    let c = core.clone();
    state.on_restore_selected(move || {
        if let Some(ui) = w.upgrade() {
            let paths = c.borrow().selected_paths();
            let (mut ok, mut fail) = (0, 0);
            for p in &paths {
                match fs::recyclebin::restore(&p.to_string_lossy()) {
                    Ok(_) => ok += 1,
                    Err(_) => fail += 1,
                }
            }
            load_current(&ui, &c);
            ui.global::<AppState>()
                .set_status_text(format!("已恢复 {} 项，失败 {} 项", ok, fail).into());
        }
    });

    // 回收站：恢复全部项
    let w = ui.as_weak();
    let c = core.clone();
    state.on_restore_all(move || {
        if let Some(ui) = w.upgrade() {
            let paths: Vec<String> = {
                let core = c.borrow();
                core.active_tab()
                    .entries
                    .iter()
                    .map(|e| e.path.clone())
                    .collect()
            };
            let (mut ok, mut fail) = (0, 0);
            for p in &paths {
                match fs::recyclebin::restore(p) {
                    Ok(_) => ok += 1,
                    Err(_) => fail += 1,
                }
            }
            load_current(&ui, &c);
            ui.global::<AppState>()
                .set_status_text(format!("已恢复全部：{} 项，失败 {} 项", ok, fail).into());
        }
    });

    // 回收站：彻底删除选中项（不可逆）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_delete_permanent_selected(move || {
        if let Some(ui) = w.upgrade() {
            let paths = c.borrow().selected_paths();
            if paths.is_empty() {
                return;
            }
            // 后台任务彻底删除（进度卡片反馈，不阻塞 UI）
            c.borrow_mut().task_queue.push_back(fs::tasks::Job {
                kind: fs::tasks::TaskKind::DeletePermanent,
                srcs: paths,
                dst: PathBuf::new(),
            });
            start_next_job(&ui, &c);
        }
    });

    // 回收站：彻底删除全部（清空回收站，不可逆）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_delete_permanent_all(move || {
        if let Some(ui) = w.upgrade() {
            let paths: Vec<PathBuf> = {
                let core = c.borrow();
                core.active_tab()
                    .entries
                    .iter()
                    .map(|e| PathBuf::from(&e.path))
                    .collect()
            };
            if paths.is_empty() {
                return;
            }
            // 后台任务清空回收站（进度卡片反馈，不阻塞 UI）
            c.borrow_mut().task_queue.push_back(fs::tasks::Job {
                kind: fs::tasks::TaskKind::DeletePermanent,
                srcs: paths,
                dst: PathBuf::new(),
            });
            start_next_job(&ui, &c);
        }
    });
}

/// 读取当前活跃标签页目录并推送到 UI
fn load_current(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    // 导航/刷新时清除可能残留的跨面板拖拽幽灵状态：
    // 拖拽启动后若因目录切换导致 InputOverlay 重建、pointer-up 丢失，幽灵会卡住。
    ui.global::<AppState>().set_pane_drag_active(false);
    // 同时退出行内重命名（两侧下标一并清）：editing 下标残留会禁用 InputOverlay，
    // 表现为「编辑框一直显示且界面无法点击」（如在新面板打开后旧编辑态未清）。
    ui.invoke_clear_editing();
    // 设置标签页不读取文件系统，仅推送标签与视图状态
    let is_settings = core.borrow().active_tab().kind == app::TabKind::Settings;
    if is_settings {
        {
            let c = core.borrow();
            ui_bridge::push_entries(ui, &c);
            ui_bridge::push_tabs(ui, &c);
        }
        // 设置页无目录内容，停止实时监听
        if let Some(w) = core.borrow_mut().watcher.as_mut() {
            w.clear();
        }
        return;
    }

    let path = core.borrow().active_tab().history.current().clone();
    let path_str = path.to_string_lossy().to_string();

    // 虚拟路径（标签 / 回收站 / 网络位置）走 provider 生成条目
    if fs::virtualfs::is_virtual(&path_str) {
        let entries = {
            let mut c = core.borrow_mut();
            fs::virtualfs::resolve(&path_str, &mut c.config).unwrap_or_default()
        };
        {
            let mut c = core.borrow_mut();
            let folders_first = c.config.settings.folders_first;
            let prev = selected_path_set(c.active_tab());
            let tab = c.active_tab_mut();
            tab.entries = entries;
            tab.folders_first = folders_first;
            tab.search.clear();
            tab.rebuild();
            restore_selection_by_path(tab, &prev);
        }
        {
            let c = core.borrow();
            ui_bridge::push_entries(ui, &c);
            ui_bridge::push_tabs(ui, &c);
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    &path,
                    &c.collapsed_sections,
                    &c.config,
                ));
        }
        // 虚拟路径（标签 / 回收站 / 网络位置）内容不由文件系统驱动，停止实时监听
        if let Some(w) = core.borrow_mut().watcher.as_mut() {
            w.clear();
        }
        return;
    }

    // 提前取出设置项，避免 match 表达式中的临时借用与内部 borrow_mut 冲突
    let (show_hidden, show_protected, folders_first) = {
        let c = core.borrow();
        (
            c.config.settings.show_hidden,
            c.config.settings.show_protected,
            c.config.settings.folders_first,
        )
    };
    match ops::read_dir(&path, show_hidden, show_protected) {
        Ok(entries) => {
            let mut c = core.borrow_mut();
            let prev = selected_path_set(c.active_tab());
            let tab = c.active_tab_mut();
            tab.entries = entries;
            tab.folders_first = folders_first;
            tab.search.clear();
            tab.rebuild();
            restore_selection_by_path(tab, &prev);
        }
        Err(e) => {
            let st = ui.global::<AppState>();
            st.set_status_text(format!("无法打开目录：{}", e).into());
            return;
        }
    }
    {
        let c = core.borrow();
        ui_bridge::push_entries(ui, &c);
        ui_bridge::push_tabs(ui, &c);
        ui.global::<AppState>()
            .set_nav_items(ui_bridge::build_sidebar(
                &path,
                &c.collapsed_sections,
                &c.config,
            ));
    }
    // 更新实时监听到新的当前目录（notify 后端非递归监听其直接子项变化）
    if let Some(w) = core.borrow_mut().watcher.as_mut() {
        w.watch(&path);
    }
}

/// 目录实时监听触发的「软刷新」：重读当前活跃标签目录并推送 UI，
/// 与 `load_current` 不同的是——保留当前搜索词，并按路径尽量恢复刷新前的选中项，
/// 避免外部程序（或后台任务）改动目录时打断用户正在进行的浏览 / 多选。
/// 仅处理普通文件标签页的真实目录；设置页与虚拟路径直接忽略。
fn reload_current_soft(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    if core.borrow().active_tab().kind != app::TabKind::Files {
        return;
    }
    let path = core.borrow().active_tab().history.current().clone();
    let path_str = path.to_string_lossy().to_string();
    if fs::virtualfs::is_virtual(&path_str) {
        return;
    }

    let (show_hidden, show_protected, folders_first) = {
        let c = core.borrow();
        (
            c.config.settings.show_hidden,
            c.config.settings.show_protected,
            c.config.settings.folders_first,
        )
    };
    let entries = match ops::read_dir(&path, show_hidden, show_protected) {
        Ok(e) => e,
        Err(_) => return, // 目录已被删除/移动等：留待用户主动导航，不打断当前视图
    };

    {
        let mut c = core.borrow_mut();
        // 记录刷新前的选中项路径，用于按路径恢复
        let prev: std::collections::HashSet<String> = c
            .active_tab()
            .selected_paths()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let tab = c.active_tab_mut();
        tab.entries = entries;
        tab.folders_first = folders_first;
        // 注意：不清空 tab.search，rebuild 会按现有搜索词重建 filtered
        tab.rebuild();
        // 按路径恢复选中（条目可能已重排，先收集下标再置位以避开借用冲突）
        if !prev.is_empty() {
            let to_select: Vec<usize> = (0..tab.filtered.len())
                .filter(|&fi| tab.entry_at(fi).is_some_and(|e| prev.contains(&e.path)))
                .collect();
            for fi in to_select {
                tab.selected[fi] = true;
            }
        }
    }

    let c = core.borrow();
    ui_bridge::push_entries(ui, &c);
    ui_bridge::push_tabs(ui, &c);
}

/// 右面板「软刷新」：与 reload_current_soft 同语义——重读目录并推送 UI，
/// 保留搜索词与按路径恢复选中，且不退出行内重命名。
/// 供 Shell 菜单新建后的延迟补刷使用：硬刷新会清掉刚建立的选中/重命名。
fn reload_right_soft(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let path = core.borrow().right_pane.history.current().clone();
    if fs::virtualfs::is_virtual(&path.to_string_lossy()) {
        return;
    }
    let (show_hidden, show_protected, folders_first) = {
        let c = core.borrow();
        (
            c.config.settings.show_hidden,
            c.config.settings.show_protected,
            c.config.settings.folders_first,
        )
    };
    let entries = match ops::read_dir(&path, show_hidden, show_protected) {
        Ok(e) => e,
        Err(_) => return,
    };
    {
        let mut c = core.borrow_mut();
        let prev = selected_path_set(&c.right_pane);
        let tab = &mut c.right_pane;
        tab.entries = entries;
        tab.folders_first = folders_first;
        tab.rebuild();
        restore_selection_by_path(tab, &prev);
    }
    ui_bridge::push_right(ui, &core.borrow());
    if toolbar_routes_right(ui) {
        ui_bridge::update_selection_pane(ui, &core.borrow(), true);
    }
}

/// 初始化当前目录实时监听：绑定 `auto-refresh` 回调到软刷新，创建 `DirWatcher`
/// 并注入 `AppCore`，随后立即监听启动目录。监听后端不可用时静默降级为手动 F5。
fn bind_watcher(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 监听线程经事件循环回主线程后，在此执行软刷新
    let w = ui.as_weak();
    let c = core.clone();
    state.on_auto_refresh(move || {
        if let Some(ui) = w.upgrade() {
            reload_current_soft(&ui, &c);
        }
    });

    // notify 事件（已防抖）→ 回主线程触发 auto-refresh 回调
    let w = ui.as_weak();
    let watcher = fs::watcher::DirWatcher::new(move || {
        let w = w.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().invoke_auto_refresh();
            }
        });
    });

    if let Ok(mut watcher) = watcher {
        // 立即监听启动目录（虚拟路径不监听）
        let cur = core.borrow().active_tab().history.current().clone();
        if !fs::virtualfs::is_virtual(&cur.to_string_lossy()) {
            watcher.watch(&cur);
        }
        core.borrow_mut().watcher = Some(watcher);
    }
}

/// 绑定同名冲突对话框的处置回调：用户选择后关闭对话框，并把决策经桥回送给
/// 正在阻塞等待的后台工作线程。
fn bind_conflict(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();
    let w = ui.as_weak();
    let bridge = core.borrow().conflict_bridge.clone();
    state.on_conflict_choose(move |decision, apply_all| {
        if let Some(ui) = w.upgrade() {
            ui.global::<AppState>().set_conflict_open(false);
        }
        let decision = match decision.as_str() {
            "overwrite" => fs::tasks::ConflictDecision::Overwrite,
            "rename" => fs::tasks::ConflictDecision::Rename,
            _ => fs::tasks::ConflictDecision::Skip,
        };
        if let Some(tx) = bridge.pending.lock().unwrap().take() {
            let _ = tx.send(fs::tasks::ConflictReply {
                decision,
                apply_all,
            });
        }
    });
}

/// 跨目录移动单个路径（同盘 rename；自动创建目标父目录）。
fn move_path(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::rename(from, to)
}

/// 执行撤销（逆操作），返回状态栏消息。
fn apply_undo(action: &app::UndoAction) -> String {
    use app::UndoAction::*;
    match action {
        Rename { orig, renamed } => {
            let name = orig
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match ops::rename(renamed, &name) {
                Ok(_) => format!("已撤销重命名（改回 {}）", name),
                Err(e) => format!("撤销失败：{}", e),
            }
        }
        Create { path } => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match fs::recyclebin::move_to_recycle_bin(&[path.clone()]) {
                Ok(_) => format!("已撤销新建（{} 移入回收站）", name),
                Err(e) => format!("撤销失败：{}", e),
            }
        }
        Move { pairs } => {
            let ok = pairs
                .iter()
                .filter(|(src, dst)| move_path(dst, src).is_ok())
                .count();
            format!("已撤销移动（移回 {} 项）", ok)
        }
        Delete { paths } => {
            let ok = paths
                .iter()
                .filter(|p| fs::recyclebin::restore_to_original(p).is_ok())
                .count();
            format!("已撤销删除（还原 {} 项）", ok)
        }
    }
}

/// 执行重做（正向操作），返回状态栏消息。
fn apply_redo(action: &app::UndoAction) -> String {
    use app::UndoAction::*;
    match action {
        Rename { orig, renamed } => {
            let name = renamed
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match ops::rename(orig, &name) {
                Ok(_) => format!("已重做重命名（{}）", name),
                Err(e) => format!("重做失败：{}", e),
            }
        }
        Create { path } => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match fs::recyclebin::restore_to_original(path) {
                Ok(_) => format!("已重做新建（{} 从回收站还原）", name),
                Err(e) => format!("重做失败：{}", e),
            }
        }
        Move { pairs } => {
            let ok = pairs
                .iter()
                .filter(|(src, dst)| move_path(src, dst).is_ok())
                .count();
            format!("已重做移动（{} 项）", ok)
        }
        Delete { paths } => match fs::recyclebin::move_to_recycle_bin(paths) {
            Ok(_) => format!("已重做删除（{} 项移入回收站）", paths.len()),
            Err(e) => format!("重做失败：{}", e),
        },
    }
}

/// 命令面板可用命令表：(图标, 标题, 描述, 快捷键, action)
fn palette_all_commands() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    vec![
        (
            "\u{E80A}",
            "切换到网格视图",
            "以图标网格显示文件",
            "",
            "view-grid",
        ),
        (
            "\u{EA37}",
            "切换到详细信息视图",
            "显示名称、日期、类型、大小列",
            "",
            "view-list",
        ),
        (
            "\u{F0E2}",
            "切换双面板",
            "开启/关闭左右两个独立文件面板",
            "F3",
            "toggle-dual",
        ),
        (
            "\u{E90D}",
            "切换详情面板",
            "显示/隐藏右侧详情面板",
            "",
            "toggle-details",
        ),
        (
            "\u{E72C}",
            "刷新当前目录",
            "重新读取文件列表",
            "F5",
            "refresh",
        ),
        (
            "\u{E74A}",
            "转到上一级目录",
            "返回父文件夹",
            "Backspace",
            "go-up",
        ),
        ("\u{E72B}", "后退", "导航历史后退", "", "go-back"),
        ("\u{E72A}", "前进", "导航历史前进", "", "go-forward"),
        (
            "\u{E8F4}",
            "新建文件夹",
            "在当前目录创建文件夹",
            "Ctrl+Shift+N",
            "new-folder",
        ),
        (
            "\u{E8A5}",
            "新建文件",
            "在当前目录创建文本文件",
            "",
            "new-file",
        ),
        ("\u{E713}", "打开设置", "打开应用设置页", "", "settings"),
        (
            "\u{E7B3}",
            "切换隐藏文件",
            "显示或隐藏隐藏项",
            "",
            "toggle-hidden",
        ),
        (
            "\u{E712}",
            "添加网络位置",
            "挂载 SMB 共享到盘符并浏览",
            "",
            "add-network-location",
        ),
        (
            "\u{EC50}",
            "打开此电脑",
            "查看全部驱动器与设备",
            "",
            "nav:this-pc://",
        ),
        (
            "\u{E74D}",
            "打开回收站",
            "查看与恢复已删除项目",
            "",
            "nav:recycle://",
        ),
        (
            "\u{E710}",
            "新建标签页",
            "按设置的默认位置打开新标签",
            "Ctrl+T",
            "new-tab",
        ),
        (
            "\u{E8C8}",
            "复制当前路径",
            "把当前目录完整路径复制到剪贴板",
            "",
            "copy-cwd",
        ),
    ]
}

/// Omnibar 路径补全：解析输入路径的父目录，列出以前缀开头的子项。
/// 输入以分隔符结尾时前缀为空（列全部）；父目录不可用时回退到活动面板当前目录。
fn compute_path_completions(input: &str, base: &Path) -> Vec<Crumb> {
    let p = Path::new(input);
    let (dir, prefix) = if input.ends_with('/') || input.ends_with('\\') {
        (p.to_path_buf(), String::new())
    } else {
        match p.parent() {
            Some(par) if !par.as_os_str().is_empty() => {
                let pre = p
                    .file_name()
                    .map(|f| f.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                (par.to_path_buf(), pre)
            }
            _ => (base.to_path_buf(), input.to_lowercase()),
        }
    };
    let mut comps: Vec<Crumb> = Vec::new();
    if dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                if name.to_lowercase().starts_with(&prefix) {
                    comps.push(Crumb {
                        name: name.into(),
                        path: ent.path().to_string_lossy().to_string().into(),
                    });
                }
            }
        }
    }
    comps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    comps.truncate(50);
    comps
}

/// 绑定命令面板：Rust 侧过滤命令 + 路径识别 + 执行分发（Ctrl+P 触发）。
fn bind_command_palette(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 查询：按标题/描述模糊过滤；输入为存在目录时置顶「跳转」命令
    let w = ui.as_weak();
    state.on_palette_query(move |query| {
        if let Some(ui) = w.upgrade() {
            let qt = query.trim().to_string();
            let ql = qt.to_lowercase();
            let mut rows: Vec<Command> = Vec::new();

            if !qt.is_empty() && Path::new(&qt).is_dir() {
                rows.push(Command {
                    icon: "\u{E8DA}".into(),
                    title: format!("跳转到 {}", qt).into(),
                    description: "打开该目录".into(),
                    shortcut: "Enter".into(),
                    action: format!("cd:{}", qt).into(),
                });
            }

            for (icon, title, desc, sc, action) in palette_all_commands() {
                if ql.is_empty()
                    || title.to_lowercase().contains(&ql)
                    || desc.to_lowercase().contains(&ql)
                {
                    rows.push(Command {
                        icon: icon.into(),
                        title: title.into(),
                        description: desc.into(),
                        shortcut: sc.into(),
                        action: action.into(),
                    });
                }
            }

            let st = ui.global::<AppState>();
            st.set_palette_commands(slint::ModelRc::new(slint::VecModel::from(rows)));
            st.set_palette_selected(0);
        }
    });

    // 执行：cd:/nav: 前缀跳转，其余分发到对应 AppState 回调
    let w = ui.as_weak();
    let c = core.clone();
    state.on_palette_run(move |action| {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            st.set_command_palette_open(false);
            let a = action.to_string();
            if let Some(path) = a.strip_prefix("cd:") {
                navigate_to(&ui, &c, PathBuf::from(path));
                return;
            }
            // nav: 前缀：虚拟路径（此电脑/回收站等）走通用导航回调
            if let Some(vpath) = a.strip_prefix("nav:") {
                st.invoke_navigate(vpath.into());
                return;
            }
            match a.as_str() {
                "view-grid" => st.invoke_set_view("grid".into()),
                "view-list" => st.invoke_set_view("list".into()),
                "toggle-dual" => st.invoke_toggle_dual(),
                "toggle-details" => st.invoke_toggle_details(),
                "refresh" => st.invoke_refresh(),
                "go-up" => st.invoke_go_up(),
                "go-back" => st.invoke_go_back(),
                "go-forward" => st.invoke_go_forward(),
                "new-folder" => st.invoke_new_folder(),
                "new-file" => st.invoke_new_file(),
                "new-tab" => st.invoke_new_tab(),
                "settings" => st.invoke_open_settings_tab(),
                "toggle-hidden" => {
                    let cur = st.get_set_show_hidden();
                    st.invoke_set_bool("show-hidden".into(), !cur);
                }
                "add-network-location" => {
                    st.set_netloc_dialog_open(true);
                }
                "copy-cwd" => {
                    let cwd = c
                        .borrow()
                        .active_tab()
                        .history
                        .current()
                        .to_string_lossy()
                        .to_string();
                    fs::clipboard::set_text(&cwd);
                    st.set_status_text("已复制当前路径".into());
                }
                _ => {}
            }
        }
    });
}

/// 从队列取出下一个任务并在工作线程执行；空闲时才启动，进度经事件循环回填 UI。
fn start_next_job(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    use std::sync::Arc;

    // 已有任务在跑则等其完成时再串联；否则取队首
    let job = {
        let mut c = core.borrow_mut();
        if c.task_control.is_some() {
            return;
        }
        match c.task_queue.pop_front() {
            Some(j) => j,
            None => return,
        }
    };

    let ctrl = Arc::new(fs::tasks::TaskControl::new());
    core.borrow_mut().task_control = Some(ctrl.clone());

    // 初始化进度卡片，避免首帧前的空白闪烁
    let op_label = job.kind.label();
    let dst_str = job.dst.to_string_lossy().to_string();
    let st = ui.global::<AppState>();
    st.set_task_active(true);
    st.set_task_paused(false);
    st.set_task_operation(op_label.into());
    st.set_task_current_file("准备中…".into());
    st.set_task_target(dst_str.into());
    st.set_task_completed(0);
    st.set_task_total(0);
    st.set_task_progress(0.0);
    st.set_task_speed("计算中…".into());
    st.set_task_eta("计算中…".into());

    let w_progress = ui.as_weak();
    let w_done = ui.as_weak();
    let w_ask = ui.as_weak();
    let bridge = core.borrow().conflict_bridge.clone();
    // 克隆任务信息供完成回调更新文件名索引（job 本体会被移入工作线程）
    let job_kind = job.kind;
    let job_srcs = job.srcs.clone();
    let job_dst = job.dst.clone();
    // 后台索引开关在启动时读取并捕获（Rc 非 Send，不能进事件循环闭包）；
    // 任务期间用户改设置属极小概率，偏差由下次全量重建修正。
    let bg_index = core.borrow().config.settings.background_index;
    std::thread::spawn(move || {
        // catch_unwind 包裹 run：任务执行或内部库 panic 时仍构造错误结果，保证
        // 下方 task-finished 一定触发、task_control 一定清理。否则一次 panic 会让
        // task_control 永久卡在 Some，start_next_job 对后续复制/剪切/粘贴静默 return，
        // 表现为「复制粘贴大部分情况无法使用」。
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fs::tasks::run(
                job,
                ctrl,
                move |p| {
                    let w = w_progress.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let st = ui.global::<AppState>();
                            st.set_task_operation(p.operation.into());
                            st.set_task_current_file(p.current_file.into());
                            st.set_task_target(p.target.into());
                            st.set_task_completed(p.completed);
                            st.set_task_total(p.total);
                            st.set_task_progress(p.fraction);
                            st.set_task_speed(p.speed.into());
                            st.set_task_eta(p.eta.into());
                        }
                    });
                },
                move |q| {
                    // 遇顶层同名冲突：把回复通道存入桥，请主线程弹窗，随后阻塞等待用户选择
                    let (tx, rx) = std::sync::mpsc::channel();
                    *bridge.pending.lock().unwrap() = Some(tx);
                    let w = w_ask.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let st = ui.global::<AppState>();
                            st.set_conflict_name(q.name.into());
                            st.set_conflict_operation(q.operation.into());
                            st.set_conflict_src_info(q.src_info.into());
                            st.set_conflict_dst_info(q.dst_info.into());
                            st.set_conflict_is_dir(q.is_dir);
                            st.set_conflict_apply_all(false);
                            st.set_conflict_open(true);
                        }
                    });
                    // 对话框被异常关闭 / 事件循环失效时默认跳过，保证工作线程不会永久阻塞
                    rx.recv().unwrap_or(fs::tasks::ConflictReply {
                        decision: fs::tasks::ConflictDecision::Skip,
                        apply_all: false,
                    })
                },
            )
        })) {
            Ok(r) => r,
            Err(_) => fs::tasks::TaskResult {
                ok: 0,
                skipped: 0,
                error: "任务执行异常（内部错误）".into(),
                cancelled: false,
                completed_paths: Vec::new(),
            },
        };

        // 完成：回主线程触发 task-finished（在那里访问 core 重载目录、串联下一项）
        let completed_paths = result.completed_paths.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w_done.upgrade() {
                let skip_note = if result.skipped > 0 {
                    format!("，跳过 {} 项", result.skipped)
                } else {
                    String::new()
                };
                let msg = if result.cancelled {
                    format!("已取消（完成 {} 项{}）", result.ok, skip_note)
                } else if result.error.is_empty() {
                    format!("已完成 {} 个项目{}", result.ok, skip_note)
                } else {
                    format!("操作部分失败：{}", result.error)
                };
                // 启用后台索引时按任务类型增量更新：复制->加入新项；移动->重命名（旧路径移除、新路径加入）。
                // 冲突重命名（" (2)"）的项索引会略有偏差，由 search 的 metadata 校验兜底，待下次全量重建修正。
                if bg_index && !result.cancelled {
                    for src in &job_srcs {
                        // 便携设备路径不纳入本地文件名索引（索引只覆盖本地文件系统）
                        if fs::devices::is_device_path(&src.to_string_lossy())
                            || fs::devices::is_device_path(&job_dst.to_string_lossy())
                        {
                            continue;
                        }
                        let Some(name) = src.file_name() else {
                            continue;
                        };
                        let dest = job_dst.join(name);
                        match job_kind {
                            fs::tasks::TaskKind::Copy => fs::index::add_path(&dest),
                            fs::tasks::TaskKind::Move => fs::index::rename_path(src, &dest),
                            _ => {}
                        }
                    }
                }
                ui.global::<AppState>()
                    .invoke_task_finished_with_paths(
                        result.ok,
                        msg.into(),
                        completed_paths
                            .iter()
                            .map(|p| p.to_string_lossy().to_string().into())
                            .collect::<Vec<slint::SharedString>>()
                            .as_slice()
                            .into(),
                    );
            }
        });
    });
}

/// 把「压缩选中项为归档」入队为后台任务（进度卡片显示速度/ETA，可暂停/取消）。
/// `fmt` 为输出格式："zip" / "7z" / "tar" / "targz"；
/// `idx` 为无选中时的回退目标项（-1 表示无）。
fn enqueue_compress(ui: &MainWindow, c: &Rc<RefCell<AppCore>>, idx: i32, fmt: &str) {
    let right = toolbar_routes_right(ui);
    let (items, dst_dir) = {
        let core = c.borrow();
        let mut paths = core.pane_selected_paths(right);
        if paths.is_empty() {
            if let Some(e) = core.pane_entry_at(right, idx as usize) {
                paths.push(PathBuf::from(&e.path));
            }
        }
        (paths, core.pane(right).history.current().clone())
    };
    if items.is_empty() {
        return;
    }
    let ext = match fmt {
        "7z" => "7z",
        "tar" => "tar",
        "targz" => "tar.gz",
        _ => "zip",
    };
    // 输出路径入队时即确定（重名自动避让），由后台任务流式写入。
    // 立即占位创建空文件：任务串行执行，若不占位，排队中的同名压缩任务
    // 在入队时看不到前一任务的产物，会解析到相同路径并互相覆盖/误删。
    let target =
        ops::resolve_conflict(dst_dir.join(format!("{}.{}", ops::archive_stem(&items), ext)));
    let _ = std::fs::File::create(&target);
    c.borrow_mut().task_queue.push_back(fs::tasks::Job {
        kind: fs::tasks::TaskKind::Compress,
        srcs: items,
        dst: target,
    });
    start_next_job(ui, c);
}

/// 在指定面板执行行内重命名提交并刷新该面板（记录撤销）
/// 在设备目录下为 `base` 找一个不重名的名称（「新建文件夹」/「新建文件夹 (2)」）。
/// 设备侧无文件系统，用 devices::child_named 查重。
fn unique_device_name(parent_vpath: &str, base: &str) -> String {
    if fs::devices::child_named(parent_vpath, base).is_none() {
        return base.to_string();
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{}", e)),
        _ => (base.to_string(), String::new()),
    };
    for n in 2..1000 {
        let candidate = format!("{} ({}){}", stem, n, ext);
        if fs::devices::child_named(parent_vpath, &candidate).is_none() {
            return candidate;
        }
    }
    for n in 1000..10_000 {
        let candidate = format!("{} (副本 {}){}", stem, n, ext);
        if fs::devices::child_named(parent_vpath, &candidate).is_none() {
            return candidate;
        }
    }
    let prefix = format!("{} (副本 {})", stem, std::process::id());
    for n in 1..10_000 {
        let candidate = format!(
            "{}{}{}",
            prefix,
            if n == 1 {
                String::new()
            } else {
                format!(" ({})", n)
            },
            ext
        );
        if fs::devices::child_named(parent_vpath, &candidate).is_none() {
            return candidate;
        }
    }
    // 设备目录极端拥挤时仍返回一个基于进程 ID 的名称；上面的查重覆盖正常范围。
    format!("{} (副本 {}){}", stem, std::process::id(), ext)
}

fn select_created_and_edit(ui: &MainWindow, c: &Rc<RefCell<AppCore>>, right: bool, created: &str) {
    // 归一化路径比较：新建项路径与过滤项路径可能因 \\?\ 长路径前缀、尾随分隔符、
    // 大小写差异（Windows 盘符/UNC）或 device:// 形式不一致而逐字匹配失败，
    // 导致新建后无法进入重命名。此处统一剥离前缀与尾分隔符后做大小写不敏感比较。
    let norm = |s: &str| -> String {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        s.trim_end_matches(['/', '\\']).to_string()
    };
    let new_idx = {
        let core = c.borrow();
        let tab = core.pane(right);
        let target = norm(created);
        tab.filtered
            .iter()
            .position(|&ei| norm(&tab.entries[ei].path).eq_ignore_ascii_case(&target))
    };
    let Some(i) = new_idx else { return };
    {
        let mut core = c.borrow_mut();
        let tab = core.pane_mut(right);
        tab.selected.fill(false);
        if i < tab.selected.len() {
            tab.selected[i] = true;
        }
    }
    if right {
        ui_bridge::refresh_right_selection(ui, &c.borrow());
        ui_bridge::update_selection_pane(ui, &c.borrow(), true);
        ui.invoke_set_editing_right(i as i32);
    } else {
        ui_bridge::refresh_selection(ui, &c.borrow());
        ui.invoke_set_editing(i as i32);
    }
}

/// 通用选中结果辅助函数：刷新目录后按路径选中操作完成的项目(不进入重命名)
fn select_completed_paths(ui: &MainWindow, c: &Rc<RefCell<AppCore>>, right: bool, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let norm = |s: &str| -> String {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        s.trim_end_matches(['/', '\\']).to_string()
    };
    let targets: Vec<String> = paths.iter().map(|p| norm(&p.to_string_lossy())).collect();
    let indices: Vec<usize> = {
        let core = c.borrow();
        let tab = core.pane(right);
        tab.filtered
            .iter()
            .enumerate()
            .filter_map(|(pos, &ei)| {
                let entry_path = norm(&tab.entries[ei].path);
                if targets.iter().any(|t| entry_path.eq_ignore_ascii_case(t)) {
                    Some(pos)
                } else {
                    None
                }
            })
            .collect()
    };
    if indices.is_empty() {
        return;
    }
    {
        let mut core = c.borrow_mut();
        let tab = core.pane_mut(right);
        tab.selected.fill(false);
        for &i in &indices {
            if i < tab.selected.len() {
                tab.selected[i] = true;
            }
        }
    }
    if right {
        ui_bridge::refresh_right_selection(ui, &c.borrow());
        ui_bridge::update_selection_pane(ui, &c.borrow(), true);
    } else {
        ui_bridge::refresh_selection(ui, &c.borrow());
        ui_bridge::update_selection_pane(ui, &c.borrow(), false);
    }
}

fn rename_in_pane(
    ui: &MainWindow,
    c: &Rc<RefCell<AppCore>>,
    right: bool,
    idx: i32,
    new_name: &str,
) {
    let old = c
        .borrow()
        .pane_entry_at(right, idx as usize)
        .map(|e| PathBuf::from(&e.path));
    let trimmed = new_name.trim();
    // 重命名成功后的新路径：reload 后重新选中（资源管理器同款：重命名后保持选中）
    let mut renamed_to: Option<PathBuf> = None;
    // 名称未变（含清空或仅空白差异）：直接退出编辑，跳过文件系统重命名与全目录重载。
    // 点击别处退出重命名时按下层 pointer-event 会触发本提交，若每次都 reload 整个目录
    // （重读目录 + 重建模型 + 图标缓存查询）在大目录下明显卡顿；名称未变时无需任何副作用。
    let unchanged = trimmed.is_empty()
        || old
            .as_ref()
            .and_then(|p| {
                let s = p.to_string_lossy();
                if fs::devices::is_device_path(&s) {
                    fs::devices::object_info(&s).map(|(name, _, _)| name == trimmed)
                } else {
                    p.file_name().map(|n| n.to_string_lossy() == trimmed)
                }
            })
            .unwrap_or(true);
    if let Some(old) = old {
        // 便携设备对象：std::fs 不可用，改走 WPD 重命名
        let old_str = old.to_string_lossy().to_string();
        if !trimmed.is_empty() && !unchanged && fs::devices::is_device_path(&old_str) {
            match fs::devices::rename(&old_str, new_name) {
                Ok(()) => {}
                Err(e) => {
                    ui.global::<AppState>()
                        .set_status_text(format!("重命名失败：{}", e).into());
                }
            }
            ui.invoke_clear_editing();
            if right {
                load_right(ui, c);
            } else {
                load_current(ui, c);
            }
            return;
        }
        if !trimmed.is_empty() && !unchanged {
            match ops::rename(&old, new_name) {
                Ok(new_path) => {
                    c.borrow_mut().record_undo(app::UndoAction::Rename {
                        orig: old.clone(),
                        renamed: new_path.clone(),
                    });
                    // 启用后台索引时同步重命名（移除旧路径含子项、加入新路径）
                    if c.borrow().config.settings.background_index {
                        fs::index::rename_path(&old, &new_path);
                    }
                    renamed_to = Some(new_path);
                }
                Err(error) => {
                    let args = vec![old.as_os_str().to_os_string(), new_name.into()];
                    let elevated = fs::elevated::retry_if_permission_denied(
                        &error,
                        fs::elevated::ElevatedOp::Rename,
                        &args,
                    );
                    if elevated {
                        schedule_pane_reloads_for(ui, c, right, &[800, 2000]);
                    }
                    ui.global::<AppState>().set_status_text(
                        if elevated {
                            "已请求管理员权限重命名"
                        } else {
                            "重命名失败"
                        }
                        .into(),
                    );
                }
            }
        }
    }
    ui.invoke_clear_editing();
    // 名称未变则无需重载目录（否则反而引入卡顿）
    if !unchanged {
        if right {
            load_right(ui, c);
        } else {
            load_current(ui, c);
        }
        // 重命名后的条目按新路径重新选中，保持「一直选中直到取消」
        if let Some(p) = renamed_to {
            select_completed_paths(ui, c, right, &[p]);
        }
    }
}

/// 收集活动面板选中的可解压归档与当前目录
fn selected_archives(ui: &MainWindow, c: &Rc<RefCell<AppCore>>) -> (Vec<PathBuf>, PathBuf) {
    let right = toolbar_routes_right(ui);
    let core = c.borrow();
    let archives: Vec<PathBuf> = core
        .pane_selected_paths(right)
        .into_iter()
        .filter(|p| ops::is_zip_archive(p))
        .collect();
    (archives, core.pane(right).history.current().clone())
}

/// 公用工具栏导航是否应路由到右侧面板（双面板开启且活动面板为右侧）
fn toolbar_routes_right(ui: &MainWindow) -> bool {
    let state = ui.global::<AppState>();
    state.get_dual_pane() && state.get_active_pane() == "right"
}

/// 按活动面板刷新：双面板右侧活动时刷新右面板，否则刷新当前活动标签。
fn reload_active_pane(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    if toolbar_routes_right(ui) {
        load_right(ui, core);
    } else {
        load_current(ui, core);
    }
}

/// 在指定延迟点补刷活动面板。删除/系统菜单命令等 Shell 操作可能异步收尾，
/// 立即 reload 仍读到旧目录内容；延迟补刷保证视图最终与磁盘一致。
fn schedule_pane_reloads(ui: &MainWindow, core: &Rc<RefCell<AppCore>>, delays_ms: &[u64]) {
    for &delay in delays_ms {
        let w = ui.as_weak();
        let c = core.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            if let Some(ui) = w.upgrade() {
                reload_active_pane(&ui, &c);
            }
        });
    }
}

/// 归一化路径用于比较：剥离 `\\?\` 长路径前缀与尾随分隔符。
/// 与 select_created_and_edit / select_completed_paths 的比较口径一致。
fn norm_path_key(s: &str) -> String {
    let s = s.strip_prefix(r"\\?\").unwrap_or(s);
    s.trim_end_matches(['/', '\\']).to_string()
}

/// 刷新前选中路径快照（归一化键集合），供重建后按路径恢复选中。
fn selected_path_set(tab: &app::TabSession) -> std::collections::HashSet<String> {
    tab.selected_paths()
        .iter()
        .map(|p| norm_path_key(&p.to_string_lossy()))
        .collect()
}

/// 目录重载后按路径恢复选中：选中状态保持到用户主动取消为止，
/// 刷新/补刷/重命名提交等一切 reload 都不再偷选；
/// 导航到新目录时路径不匹配自然为空，保持原有导航语义。
fn restore_selection_by_path(tab: &mut app::TabSession, prev: &std::collections::HashSet<String>) {
    if prev.is_empty() {
        return;
    }
    for fi in 0..tab.filtered.len() {
        if tab
            .entry_at(fi)
            .is_some_and(|e| prev.contains(&norm_path_key(&e.path)))
        {
            tab.selected[fi] = true;
        }
    }
}

/// 给定面板当前目录的磁盘快照（归一化后的路径集合）。
/// 虚拟路径或读取失败返回 None——此时不做「新增项」比对。
fn snapshot_pane_dir(
    core: &Rc<RefCell<AppCore>>,
    right: bool,
) -> Option<std::collections::HashSet<String>> {
    let c = core.borrow();
    let dir = c.pane(right).history.current().clone();
    if fs::virtualfs::is_virtual(&dir.to_string_lossy()) {
        return None;
    }
    let (show_hidden, show_protected) = (
        c.config.settings.show_hidden,
        c.config.settings.show_protected,
    );
    drop(c);
    ops::read_dir(&dir, show_hidden, show_protected)
        .ok()
        .map(|entries| {
            entries
                .iter()
                .map(|e| norm_path_key(&e.path))
                .collect::<std::collections::HashSet<String>>()
        })
}

/// 刷新面板，并选中相对 `before` 快照新出现的条目。
///
/// 用于系统 Shell 右键菜单的「新建」：菜单命令由 Shell 自己执行，本程序既拿不到
/// 返回的新路径、也收不到通知，只能刷新后与操作前的目录快照比对，把新增项选中，
/// 与应用内「新增」菜单（select_created_and_edit）的行为对齐。
/// 返回是否已选中到新增项（true 时调用方可停止后续延迟比对）。
fn reload_and_select_new(
    ui: &MainWindow,
    core: &Rc<RefCell<AppCore>>,
    right: bool,
    before: &std::collections::HashSet<String>,
) -> bool {
    if right {
        load_right(ui, core);
    } else {
        load_current(ui, core);
    }
    let created: Vec<PathBuf> = {
        let c = core.borrow();
        let tab = c.pane(right);
        tab.filtered
            .iter()
            .filter_map(|&ei| tab.entries.get(ei))
            .filter(|e| !before.contains(&norm_path_key(&e.path)))
            .map(|e| PathBuf::from(&e.path))
            .collect()
    };
    if created.is_empty() {
        return false;
    }
    // 新建单项：与应用内「新增」一致，选中并直接进入行内重命名，
    // 用户可立刻输入名称。多项（粘贴/解压等）仅选中，不进入编辑。
    if created.len() == 1 {
        select_created_and_edit(ui, core, right, &created[0].to_string_lossy());
    } else {
        select_completed_paths(ui, core, right, &created);
    }
    true
}

/// 系统 Shell 菜单命令后的刷新序列：立即比对一次，并在给定延迟点重试。
/// Shell 命令（新建/粘贴/删除）异步收尾，立即读目录常常还看不到新项；
/// 一旦某次比对成功选中，后续重试只做普通刷新，避免把随后到达的其它变化
/// （如外部程序写入的文件）误当成本次新建项再次抢选。
fn schedule_reload_selecting_new(
    ui: &MainWindow,
    core: &Rc<RefCell<AppCore>>,
    right: bool,
    before: std::collections::HashSet<String>,
    delays_ms: &[u64],
) {
    let before = Rc::new(before);
    let done = Rc::new(std::cell::Cell::new(reload_and_select_new(
        ui, core, right, &before,
    )));
    for &delay in delays_ms {
        let w = ui.as_weak();
        let c = core.clone();
        let before = before.clone();
        let done = done.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            let Some(ui) = w.upgrade() else { return };
            if done.get() {
                // 已选中新项：后续补刷改走软刷新，保留选中状态与进行中的重命名
                if right {
                    reload_right_soft(&ui, &c);
                } else {
                    reload_current_soft(&ui, &c);
                }
                return;
            }
            done.set(reload_and_select_new(&ui, &c, right, &before));
        });
    }
}

/// 在指定延迟点补刷固定面板，避免异步操作期间用户切换活动面板后刷新错侧。
fn schedule_pane_reloads_for(
    ui: &MainWindow,
    core: &Rc<RefCell<AppCore>>,
    right: bool,
    delays_ms: &[u64],
) {
    for &delay in delays_ms {
        let w = ui.as_weak();
        let c = core.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            if let Some(ui) = w.upgrade() {
                if right {
                    load_right(&ui, &c);
                } else {
                    load_current(&ui, &c);
                }
            }
        });
    }
}

fn default_folder_layout(settings: &config::Settings) -> (&'static str, bool) {
    match settings.default_view.as_str() {
        "grid" => ("grid", settings.dual_pane_default),
        "dual" => ("list", true),
        _ => ("list", settings.dual_pane_default),
    }
}

fn folder_layout_for(config: &config::AppConfig, path: &Path) -> (&'static str, bool) {
    match config.folder_layout_normalized(&path.to_string_lossy()) {
        Some("grid") => ("grid", false),
        Some("dual-list") => ("list", true),
        Some("dual-grid") => ("grid", true),
        Some("list") => ("list", false),
        _ => default_folder_layout(&config.settings),
    }
}

/// 应用当前目录已保存的视图。双面板状态属于左侧活动目录；右面板仅共享其子视图。
/// 双面板已开启时：导航不退出双面板、不改变两侧视图（保持各面板当前/记录视图），
/// 仅由调用方重载被导航的面板——修复「双面板下进入文件夹自动退出/切换视图」。
fn apply_folder_layout(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    if core.borrow().active_tab().kind != app::TabKind::Files {
        return;
    }
    let st = ui.global::<AppState>();
    if st.get_dual_pane() {
        // 双面板会话内导航：保持双面板与两侧视图不变
        return;
    }
    let (mode, dual) = {
        let c = core.borrow();
        folder_layout_for(&c.config, c.active_tab().history.current())
    };
    st.set_view_mode(mode.into());
    st.set_dual_pane(dual);
    if dual {
        // 开启双面板时右面板视图与左对齐，避免右侧残留旧视图
        st.set_r_view_mode(st.get_view_mode());
        load_right(ui, core);
    } else {
        ui.invoke_clear_editing();
    }
}

fn save_current_folder_layout(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    if core.borrow().active_tab().kind != app::TabKind::Files {
        return;
    }
    let st = ui.global::<AppState>();
    let mode = match (st.get_dual_pane(), st.get_view_mode().as_str()) {
        (true, "grid") => "dual-grid",
        (true, _) => "dual-list",
        (false, "grid") => "grid",
        _ => "list",
    };
    let path = core
        .borrow()
        .active_tab()
        .history
        .current()
        .to_string_lossy()
        .to_string();
    let mut c = core.borrow_mut();
    c.config.set_folder_layout(&path, mode);
    c.config.save();
}

/// 在当前活跃标签页跳转到指定路径（支持虚拟路径 tag:// recycle:// network://）
fn navigate_to(ui: &MainWindow, core: &Rc<RefCell<AppCore>>, target: PathBuf) {
    let target_str = target.to_string_lossy().to_string();
    if !fs::virtualfs::is_virtual(&target_str) && !target.is_dir() {
        return;
    }
    core.borrow_mut().active_tab_mut().history.navigate(target);
    apply_folder_layout(ui, core);
    load_current(ui, core);
}

// ─── 双面板：右侧独立面板 ───

/// 读取右侧面板当前目录并推送到 UI 的 r-* 属性
fn load_right(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    // 与 load_current 同理：清除可能残留的跨面板拖拽幽灵（如"在新面板中打开"后），
    // 并退出行内重命名——r-entries 重建后残留的 r-editing-index 会把编辑框
    // 错挂到新列表同下标条目上
    ui.global::<AppState>().set_pane_drag_active(false);
    ui.invoke_clear_editing();
    let path = core.borrow().right_pane.history.current().clone();
    // 虚拟路径（this-pc:// 等）无法作为右面板目录读取——启动目录为「此电脑」时
    // 右面板与主面板同起点会落到虚拟路径，导致右面板空白并报错。回退到用户主目录。
    let path = if fs::virtualfs::is_virtual(&path.to_string_lossy()) {
        let home = home_start_path();
        core.borrow_mut().right_pane.history.navigate(home.clone());
        home
    } else {
        path
    };
    let (show_hidden, show_protected, folders_first) = {
        let c = core.borrow();
        (
            c.config.settings.show_hidden,
            c.config.settings.show_protected,
            c.config.settings.folders_first,
        )
    };
    match ops::read_dir(&path, show_hidden, show_protected) {
        Ok(entries) => {
            let mut c = core.borrow_mut();
            let prev = selected_path_set(&c.right_pane);
            let t = &mut c.right_pane;
            t.entries = entries;
            t.folders_first = folders_first;
            t.search.clear();
            t.rebuild();
            restore_selection_by_path(t, &prev);
        }
        Err(e) => {
            ui.global::<AppState>()
                .set_status_text(format!("右面板无法打开目录：{}", e).into());
            return;
        }
    }
    ui_bridge::push_right(ui, &core.borrow());
    // 导航/刷新已清空右面板选中：右面板为活动面板时同步 sel-* 全局状态
    // （否则 ActionBar 按钮可用性/详情栏残留导航前的选中信息）
    if toolbar_routes_right(ui) {
        ui_bridge::update_selection_pane(ui, &core.borrow(), true);
    }
}

/// 右侧面板跳转到指定目录（仅限真实目录）
fn navigate_right(ui: &MainWindow, core: &Rc<RefCell<AppCore>>, target: PathBuf) {
    if !target.is_dir() {
        return;
    }
    core.borrow_mut().right_pane.history.navigate(target);
    load_right(ui, core);
}

/// 绑定右侧面板的导航 / 选择 / 打开回调
fn bind_right_pane(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 切换活动面板（点击面板内容/空白时调用），并把 sel-* 全局选中信息
    // （详情栏 / 属性 / 解压按钮可见性）重新同步为新活动面板的选中项
    let w = ui.as_weak();
    let c = core.clone();
    state.on_set_active_pane(move |side| {
        if let Some(ui) = w.upgrade() {
            let right = side == "right";
            // 切换面板即结束任何进行中的行内重命名（点击另一面板 = 取消编辑，
            // 与资源管理器一致），避免编辑框跨面板切换残留
            ui.invoke_clear_editing();
            ui.global::<AppState>().set_active_pane(side);
            if ui.global::<AppState>().get_dual_pane() {
                ui_bridge::update_selection_pane(&ui, &c.borrow(), right);
            }
        }
    });

    // 地址栏提交 / 面包屑跳转
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_navigate(move |path| {
        if let Some(ui) = w.upgrade() {
            navigate_right(&ui, &c, PathBuf::from(path.as_str()));
        }
    });

    // 双击：文件夹进入、文件打开
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_open_entry(move |idx| {
        if let Some(ui) = w.upgrade() {
            let target = {
                let core = c.borrow();
                core.right_pane
                    .entry_at(idx as usize)
                    .map(|e| (e.is_dir, e.path.clone()))
            };
            if let Some((is_dir, path)) = target {
                if is_dir {
                    navigate_right(&ui, &c, PathBuf::from(path));
                } else {
                    open_with_cwd(&path);
                }
            }
        }
    });

    // 单击：单选高亮（就地更新模型，保持双击连续触发）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_select_entry(move |idx| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                let t = &mut core.right_pane;
                let i = idx as usize;
                if i < t.selected.len() {
                    for s in t.selected.iter_mut() {
                        *s = false;
                    }
                    t.selected[i] = true;
                    t.last_clicked = Some(i);
                }
            }
            ui_bridge::refresh_right_selection(&ui, &c.borrow());
            // 同步 sel-* 全局选中信息（详情栏 / 解压按钮可见性等）
            ui_bridge::update_selection_pane(&ui, &c.borrow(), true);
        }
    });

    // 右面板框选（支持多选）：语义同 on_box_select，作用于 right_pane
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_box_select(move |r0, r1, c0, c1, cols, additive| {
        if let Some(ui) = w.upgrade() {
            let changed = {
                let mut core = c.borrow_mut();
                let t = &mut core.right_pane;
                let n = t.selected.len() as i32;
                let old_selection = t.selected.clone();

                if !additive {
                    for s in t.selected.iter_mut() {
                        *s = false;
                    }
                }
                let cols = cols.max(1);
                let (lo_r, hi_r) = (r0.min(r1).max(0), r0.max(r1));
                let (lo_c, hi_c) = (c0.min(c1).max(0), c0.max(c1).min(cols - 1));
                let mut r = lo_r;
                while r <= hi_r {
                    let mut col = lo_c;
                    while col <= hi_c {
                        let idx = r * cols + col;
                        if idx >= 0 && idx < n {
                            t.selected[idx as usize] = true;
                        }
                        col += 1;
                    }
                    r += 1;
                }

                // 检测选择是否真正变化
                t.selected != old_selection
            };

            // 仅在选择真正变化时刷新 UI
            if changed {
                ui.global::<AppState>().set_active_pane("right".into());
                ui_bridge::refresh_right_selection(&ui, &c.borrow());
                ui_bridge::update_selection_pane(&ui, &c.borrow(), true);
            }
        }
    });

    // 右面板清空选择
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_clear_selection(move || {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                for s in core.right_pane.selected.iter_mut() {
                    *s = false;
                }
            }
            ui_bridge::refresh_right_selection(&ui, &c.borrow());
            ui_bridge::update_selection_pane(&ui, &c.borrow(), true);
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_go_back(move || {
        if let Some(ui) = w.upgrade() {
            let moved = c.borrow_mut().right_pane.history.go_back().is_some();
            if moved {
                load_right(&ui, &c);
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_go_forward(move || {
        if let Some(ui) = w.upgrade() {
            let moved = c.borrow_mut().right_pane.history.go_forward().is_some();
            if moved {
                load_right(&ui, &c);
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_go_up(move || {
        if let Some(ui) = w.upgrade() {
            let parent = c
                .borrow()
                .right_pane
                .history
                .current()
                .parent()
                .map(|p| p.to_path_buf());
            if let Some(p) = parent {
                navigate_right(&ui, &c, p);
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_refresh(move || {
        if let Some(ui) = w.upgrade() {
            load_right(&ui, &c);
        }
    });
}

/// 打开文件，并把被启动程序的工作目录设为文件所在目录。
/// 这样脚本/程序用相对路径读取同级文件（如 a.py 读取 config.json）才能命中，
/// 否则会继承 FileFiles One 自身的工作目录导致“找不到文件”。
#[cfg(windows)]
fn open_with_cwd(path: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // 文件所在目录作为工作目录（lpDirectory）
    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    // 转为以 NUL 结尾的 UTF-16
    let to_wide = |s: &OsStr| -> Vec<u16> { s.encode_wide().chain(std::iter::once(0)).collect() };
    let file_w = to_wide(OsStr::new(path));
    let dir_w = to_wide(parent.as_os_str());
    let op_w: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op_w.as_ptr(),
            file_w.as_ptr(),
            std::ptr::null(),
            dir_w.as_ptr(),
            SW_SHOWNORMAL,
        );
    }
}

/// 非 Windows 平台回退到默认打开方式（不强制设置工作目录）。
#[cfg(not(windows))]
fn open_with_cwd(path: &str) {
    let _ = open::that(path);
}

/// 求上一级路径：device:// 走 WPD 父对象逻辑，普通路径用 Path::parent。
fn parent_of(cur: &Path) -> Option<PathBuf> {
    let s = cur.to_string_lossy();
    if s.starts_with("device://") {
        return fs::devices::parent_path(&s).map(PathBuf::from);
    }
    cur.parent().map(|p| p.to_path_buf())
}

/// 双击便携设备文件：后台把文件复制到临时目录，完成后用系统默认程序打开。
/// 复制可能较慢（从手机传输），放后台避免界面卡顿，并通过状态栏反馈进度。
fn open_device_file(ui: &MainWindow, vpath: &str) {
    ui.global::<AppState>()
        .set_status_text("正在从设备复制文件…".into());
    let weak = ui.as_weak();
    let vpath = vpath.to_string();
    std::thread::spawn(move || {
        let result = fs::devices::copy_to_temp(&vpath);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Some(p) => {
                    ui.global::<AppState>()
                        .set_status_text("已打开设备文件".into());
                    let _ = open::that(&p);
                }
                None => {
                    ui.global::<AppState>()
                        .set_status_text("无法打开设备文件".into());
                }
            }
        });
    });
}

// ─── 导航 ───

fn bind_navigation(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    let w = ui.as_weak();
    let c = core.clone();
    state.on_navigate(move |path| {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                navigate_right(&ui, &c, PathBuf::from(path.as_str()));
            } else {
                navigate_to(&ui, &c, PathBuf::from(path.as_str()));
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_commit_path(move |path| {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                navigate_right(&ui, &c, PathBuf::from(path.as_str()));
            } else {
                navigate_to(&ui, &c, PathBuf::from(path.as_str()));
            }
        }
    });

    // Omnibar 路径补全 / > 命令模式：edit 模式输入变化时回填候选
    let w = ui.as_weak();
    let c = core.clone();
    state.on_request_path_completion(move |input| {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let t = input.to_string();
            if let Some(q) = t.strip_prefix('>') {
                // > 命令模式：复用命令面板过滤填充 palette-commands
                st.set_omni_cmd_mode(true);
                st.set_path_completions(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<Crumb>::new(),
                )));
                st.invoke_palette_query(q.trim_start().into());
                st.set_omni_cmd_selected(0);
            } else {
                st.set_omni_cmd_mode(false);
                let base = {
                    let core = c.borrow();
                    if toolbar_routes_right(&ui) {
                        core.right_pane.history.current().clone()
                    } else {
                        core.active_tab().history.current().clone()
                    }
                };
                let comps = compute_path_completions(&t, &base);
                st.set_path_completions(slint::ModelRc::new(slint::VecModel::from(comps)));
            }
        }
    });

    // 添加网络位置（SMB 挂载到空闲盘符）：读对话框输入 -> 挂载 -> 存配置 -> 导航
    let w = ui.as_weak();
    let c = core.clone();
    state.on_add_network_location(move || {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let name = st.get_netloc_name().to_string();
            let server = st.get_netloc_server().to_string();
            let user = st.get_netloc_user().to_string();
            let pass = st.get_netloc_pass().to_string();
            if server.is_empty() {
                st.set_status_text("请输入服务器地址（如 \\\\server\\share）".into());
                return;
            }
            let disp_name = if name.is_empty() {
                server.clone()
            } else {
                name
            };
            match crate::fs::network::mount_smb(&server, &user, &pass) {
                Some(drive) => {
                    {
                        let mut core = c.borrow_mut();
                        core.config
                            .network_locations
                            .push(crate::config::NetworkLocation {
                                name: disp_name.clone(),
                                server,
                                kind: "smb".into(),
                                drive: Some(drive.clone()),
                            });
                        core.config.save();
                    }
                    st.set_netloc_dialog_open(false);
                    st.set_netloc_name("".into());
                    st.set_netloc_server("".into());
                    st.set_netloc_user("".into());
                    st.set_netloc_pass("".into());
                    // 刷新设置「云存储账号」页的已保存列表
                    ui_bridge::push_network_locations(&ui, &c.borrow());
                    navigate_to(&ui, &c, PathBuf::from(drive));
                }
                None => {
                    st.set_status_text("挂载失败，请检查地址和凭据".into());
                }
            }
        }
    });

    // 移除网络位置：卸载盘符 + 删除配置
    let w = ui.as_weak();
    let c = core.clone();
    state.on_remove_network_location(move |name| {
        if let Some(ui) = w.upgrade() {
            let name = name.to_string();
            let mut removed: Option<crate::config::NetworkLocation> = None;
            {
                let mut core = c.borrow_mut();
                if let Some(pos) = core
                    .config
                    .network_locations
                    .iter()
                    .position(|l| l.name == name)
                {
                    removed = Some(core.config.network_locations.remove(pos));
                    core.config.save();
                }
            }
            if let Some(loc) = removed {
                if let Some(d) = loc.drive {
                    crate::fs::network::unmount_smb(&d);
                }
            }
            // 刷新设置「云存储账号」页的已保存列表；若正停留在网络位置视图则同步刷新
            ui_bridge::push_network_locations(&ui, &c.borrow());
            let at_network =
                c.borrow().active_tab().history.current().to_string_lossy() == "network://";
            if at_network {
                load_current(&ui, &c);
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_open_entry(move |idx| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let target = {
                let core = c.borrow();
                core.pane_entry_at(right, idx as usize)
                    .map(|e| (e.is_dir, e.path.clone()))
            };
            if let Some((is_dir, path)) = target {
                if is_dir {
                    if right {
                        navigate_right(&ui, &c, PathBuf::from(path));
                    } else {
                        navigate_to(&ui, &c, PathBuf::from(path));
                    }
                } else if path.starts_with("device://") {
                    open_device_file(&ui, &path);
                } else {
                    open_with_cwd(&path);
                }
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_go_back(move || {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                let moved = c.borrow_mut().right_pane.history.go_back().is_some();
                if moved {
                    load_right(&ui, &c);
                }
            } else {
                let moved = c.borrow_mut().active_tab_mut().history.go_back().is_some();
                if moved {
                    apply_folder_layout(&ui, &c);
                    load_current(&ui, &c);
                }
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_go_forward(move || {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                let moved = c.borrow_mut().right_pane.history.go_forward().is_some();
                if moved {
                    load_right(&ui, &c);
                }
            } else {
                let moved = c
                    .borrow_mut()
                    .active_tab_mut()
                    .history
                    .go_forward()
                    .is_some();
                if moved {
                    apply_folder_layout(&ui, &c);
                    load_current(&ui, &c);
                }
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_go_up(move || {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                let cur = c.borrow().right_pane.history.current().clone();
                if let Some(p) = parent_of(&cur) {
                    navigate_right(&ui, &c, p);
                }
            } else {
                let cur = c.borrow().active_tab().history.current().clone();
                if let Some(p) = parent_of(&cur) {
                    navigate_to(&ui, &c, p);
                }
            }
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_refresh(move || {
        if let Some(ui) = w.upgrade() {
            if toolbar_routes_right(&ui) {
                load_right(&ui, &c);
            } else {
                load_current(&ui, &c);
            }
        }
    });

    // 折叠 / 展开侧边栏分区
    let w = ui.as_weak();
    let c = core.clone();
    state.on_toggle_section(move |label| {
        if let Some(ui) = w.upgrade() {
            let path = {
                let mut core = c.borrow_mut();
                let key = label.to_string();
                if !core.collapsed_sections.remove(&key) {
                    core.collapsed_sections.insert(key);
                }
                core.active_tab().history.current().clone()
            };
            let c2 = c.borrow();
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    &path,
                    &c2.collapsed_sections,
                    &c2.config,
                ));
        }
    });
}

// ─── 选择 ───

fn bind_selection(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    let w = ui.as_weak();
    let c = core.clone();
    state.on_select_entry(move |idx, ctrl| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                let tab = core.active_tab_mut();
                let i = idx as usize;
                if i >= tab.selected.len() {
                    return;
                }
                if ctrl {
                    tab.selected[i] = !tab.selected[i];
                } else {
                    for s in tab.selected.iter_mut() {
                        *s = false;
                    }
                    tab.selected[i] = true;
                }
                tab.last_clicked = Some(i);
            }
            // 选中左侧条目时，双面板模式下激活左面板
            let state = ui.global::<AppState>();
            if state.get_dual_pane() {
                state.set_active_pane("left".into());
            }
            ui_bridge::refresh_selection(&ui, &c.borrow());
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_select_range(move |idx| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                let tab = core.active_tab_mut();
                let i = idx as usize;
                let anchor = tab.last_clicked.unwrap_or(i);
                let (lo, hi) = if anchor <= i {
                    (anchor, i)
                } else {
                    (i, anchor)
                };
                for (k, s) in tab.selected.iter_mut().enumerate() {
                    *s = k >= lo && k <= hi;
                }
            }
            ui_bridge::refresh_selection(&ui, &c.borrow());
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_select_all(move || {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                for s in core.active_tab_mut().selected.iter_mut() {
                    *s = true;
                }
            }
            ui_bridge::refresh_selection(&ui, &c.borrow());
        }
    });

    // 框选（活动标签/左面板）：把矩形子区 [r0..r1] × [c0..c1]（idx = r*cols+c）置为选中。
    // additive 为 false 时先清空既有选择（普通框选），为 true 时追加（Ctrl+框选）。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_box_select(move |r0, r1, c0, c1, cols, additive| {
        if let Some(ui) = w.upgrade() {
            let changed = {
                let mut core = c.borrow_mut();
                let tab = core.active_tab_mut();
                let n = tab.selected.len() as i32;
                let old_selection = tab.selected.clone();

                if !additive {
                    for s in tab.selected.iter_mut() {
                        *s = false;
                    }
                }
                let cols = cols.max(1);
                let (lo_r, hi_r) = (r0.min(r1).max(0), r0.max(r1));
                let (lo_c, hi_c) = (c0.min(c1).max(0), c0.max(c1).min(cols - 1));
                let mut r = lo_r;
                while r <= hi_r {
                    let mut col = lo_c;
                    while col <= hi_c {
                        let idx = r * cols + col;
                        if idx >= 0 && idx < n {
                            tab.selected[idx as usize] = true;
                        }
                        col += 1;
                    }
                    r += 1;
                }

                // 检测选择是否真正变化
                tab.selected != old_selection
            };

            // 仅在选择真正变化时刷新 UI
            if changed {
                let state = ui.global::<AppState>();
                if state.get_dual_pane() {
                    state.set_active_pane("left".into());
                }
                ui_bridge::refresh_selection(&ui, &c.borrow());
            }
        }
    });

    // 清空选择（点击空白处）：与 select-entry 一致，双面板下点击左面板空白
    // 也切换活动面板，使 sel-* 全局状态跟随「最近交互面板」
    let w = ui.as_weak();
    let c = core.clone();
    state.on_clear_selection(move || {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                for s in core.active_tab_mut().selected.iter_mut() {
                    *s = false;
                }
            }
            let state = ui.global::<AppState>();
            if state.get_dual_pane() {
                state.set_active_pane("left".into());
            }
            ui_bridge::refresh_selection(&ui, &c.borrow());
        }
    });

    // 计算文件夹总大小：后台递归统计选中文件夹内所有文件字节数，完成后回填 sel-size。
    // 大目录递归耗时，放工作线程避免阻塞 UI；回填时校验选中项未变，防止竞态串扰。
    let w = ui.as_weak();
    state.on_calculate_folder_size(move || {
        let ui = match w.upgrade() {
            Some(u) => u,
            None => return,
        };
        let state = ui.global::<AppState>();
        if !state.get_sel_is_dir() {
            return;
        }
        let path = state.get_sel_path().to_string();
        if path.is_empty() {
            return;
        }
        state.set_sel_size_calculating(true);
        let w2 = w.clone();
        std::thread::spawn(move || {
            let total = dir_total_size(std::path::Path::new(&path));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w2.upgrade() {
                    let state = ui.global::<AppState>();
                    // 选中项仍是同一文件夹时才回填（用户可能已切换）
                    if state.get_sel_path() == path.as_str()
                        && state.get_sel_is_dir()
                        && state.get_sel_size_calculating()
                    {
                        state.set_sel_size(
                            format!("{} ({} 字节)", fs::metadata::human_size(total), total).into(),
                        );
                        state.set_sel_size_calculating(false);
                    }
                }
            });
        });
    });
}

/// 递归统计目录下所有文件的总字节数（含子目录，忽略无权限/无法访问的项）。
/// 用显式栈避免深层目录递归溢出；符号链接目录不跟进，防止循环。
fn dir_total_size(path: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                // file_type 不跟随符号链接，避免环路
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        stack.push(entry.path());
                    } else if ft.is_file() {
                        if let Ok(m) = entry.metadata() {
                            total += m.len();
                        }
                    }
                }
            }
        }
    }
    total
}

// ─── 文件操作 ───

fn bind_operations(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 复制（双面板下取活动面板的选中项：右侧活动 → right_pane，否则活动标签）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_copy_selected(move || {
        if let Some(ui) = w.upgrade() {
            let paths = {
                let mut core = c.borrow_mut();
                core.clipboard = if toolbar_routes_right(&ui) {
                    core.right_pane.selected_paths()
                } else {
                    core.selected_paths()
                };
                core.clip_mode = ClipMode::Copy;
                core.clipboard.clone()
            };
            let has_clips = !paths.is_empty();
            // 同步写入系统剪贴板（CF_HDROP）：跨文件夹/盘/标签及资源管理器互通。
            // 便携设备路径（device://）非真实文件系统路径，写入系统剪贴板对资源管理器
            // 无意义且会污染其粘贴行为，故仅写入应用内部剪贴板。
            if has_clips {
                let local_paths: Vec<_> = paths
                    .iter()
                    .filter(|p| !fs::devices::is_device_path(&p.to_string_lossy()))
                    .cloned()
                    .collect();
                if !local_paths.is_empty() {
                    fs::clipboard::set_files(&local_paths, false);
                }
            }
            ui.global::<AppState>().set_can_paste(has_clips);
        }
    });

    // 剪切（同上，按活动面板取选中项）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_cut_selected(move || {
        if let Some(ui) = w.upgrade() {
            let paths = {
                let mut core = c.borrow_mut();
                core.clipboard = if toolbar_routes_right(&ui) {
                    core.right_pane.selected_paths()
                } else {
                    core.selected_paths()
                };
                core.clip_mode = ClipMode::Cut;
                core.clipboard.clone()
            };
            let has_clips = !paths.is_empty();
            if has_clips {
                let local_paths: Vec<_> = paths
                    .iter()
                    .filter(|p| !fs::devices::is_device_path(&p.to_string_lossy()))
                    .cloned()
                    .collect();
                if !local_paths.is_empty() {
                    fs::clipboard::set_files(&local_paths, true);
                }
            }
            ui.global::<AppState>().set_can_paste(has_clips);
        }
    });

    // 粘贴：入队为后台任务（复制 / 移动），由工作线程异步执行并上报真实进度
    let w = ui.as_weak();
    let c = core.clone();
    state.on_paste_here(move || {
        if let Some(ui) = w.upgrade() {
            // 目标目录取活动面板（右侧活动 → right_pane 当前目录，否则活动标签）
            let routes_right = toolbar_routes_right(&ui);
            // 优先读系统剪贴板（CF_HDROP，可与资源管理器互通）；无文件则回退内部剪贴板
            let (clips, is_cut, dst) = {
                let core = c.borrow();
                let dst = if routes_right {
                    core.right_pane.history.current().clone()
                } else {
                    core.active_tab().history.current().clone()
                };
                let internal_has_device = core
                    .clipboard
                    .iter()
                    .any(|p| fs::devices::is_device_path(&p.to_string_lossy()));
                if internal_has_device && core.clip_mode != ClipMode::None {
                    // 设备路径无法进入 CF_HDROP；混合复制时系统剪贴板只含本地子集，
                    // 此时必须优先内部剪贴板，不能漏掉设备项。
                    (core.clipboard.clone(), core.clip_mode == ClipMode::Cut, dst)
                } else if let Some((sys_paths, sys_cut)) = fs::clipboard::get_files() {
                    (sys_paths, sys_cut, dst)
                } else if core.clip_mode != ClipMode::None && !core.clipboard.is_empty() {
                    (core.clipboard.clone(), core.clip_mode == ClipMode::Cut, dst)
                } else {
                    (Vec::new(), false, dst)
                }
            };
            if clips.is_empty() {
                ui.global::<AppState>()
                    .set_status_text("剪贴板无内容可粘贴".into());
                return;
            }
            let kind = if is_cut {
                fs::tasks::TaskKind::Move
            } else {
                fs::tasks::TaskKind::Copy
            };
            {
                let mut core = c.borrow_mut();
                // 剪切=移动：仅本地文件系统任务记录传统路径撤销；设备操作不可经 std::fs 撤销。
                if is_cut
                    && !clips
                        .iter()
                        .any(|p| fs::devices::is_device_path(&p.to_string_lossy()))
                    && !fs::devices::is_device_path(&dst.to_string_lossy())
                {
                    let pairs: Vec<(PathBuf, PathBuf)> = clips
                        .iter()
                        .filter_map(|src| src.file_name().map(|n| (src.clone(), dst.join(n))))
                        .collect();
                    core.record_undo(app::UndoAction::Move { pairs });
                }
                core.task_queue.push_back(fs::tasks::Job {
                    kind,
                    srcs: clips,
                    dst,
                });
                // 剪切粘贴后清空剪贴板，避免重复移动
                if is_cut {
                    core.clipboard.clear();
                    core.clip_mode = ClipMode::None;
                }
            }
            if is_cut {
                // 清空系统剪贴板中的文件数据，避免重复移动
                fs::clipboard::clear_files();
                ui.global::<AppState>().set_can_paste(false);
            }
            start_next_job(&ui, &c);
        }
    });

    // 跨面板拖放：把源面板选中项复制（默认）/ 移动（Ctrl）到另一面板当前目录
    let w = ui.as_weak();
    let c = core.clone();
    state.on_pane_drop(move |source, ctrl| {
        if let Some(ui) = w.upgrade() {
            let src_is_right = source == "right";
            let (srcs, dst) = {
                let core = c.borrow();
                let srcs = if src_is_right {
                    core.right_pane.selected_paths()
                } else {
                    core.selected_paths()
                };
                // 目标为另一面板的当前目录
                let dst = if src_is_right {
                    core.active_tab().history.current().clone()
                } else {
                    core.right_pane.history.current().clone()
                };
                (srcs, dst)
            };
            // 源为空或目标非可写入目录时忽略。
            // 便携设备目录（device://）不是真实文件系统路径，is_dir() 为 false，
            // 但它是合法的写入目标，需单独放行。
            let dst_ok = dst.is_dir() || fs::devices::is_device_path(&dst.to_string_lossy());
            if srcs.is_empty() || !dst_ok {
                return;
            }
            let kind = if ctrl {
                fs::tasks::TaskKind::Move
            } else {
                fs::tasks::TaskKind::Copy
            };
            c.borrow_mut()
                .task_queue
                .push_back(fs::tasks::Job { kind, srcs, dst });
            start_next_job(&ui, &c);
        }
    });

    // 任务暂停 / 继续切换
    let w = ui.as_weak();
    let c = core.clone();
    state.on_task_pause(move || {
        if let Some(ui) = w.upgrade() {
            let paused = {
                let core = c.borrow();
                core.task_control.as_ref().map(|ct| ct.toggle_pause())
            };
            if let Some(p) = paused {
                ui.global::<AppState>().set_task_paused(p);
            }
        }
    });

    // 取消当前任务并清空后续排队
    let c = core.clone();
    state.on_task_cancel(move || {
        let mut core = c.borrow_mut();
        if let Some(ct) = &core.task_control {
            ct.cancel();
        }
        core.task_queue.clear();
    });

    // 任务完成内部回调：存储完成路径并触发外部 task-finished
    let w2 = ui.as_weak();
    let c2 = core.clone();
    state.on_task_finished_with_paths(move |ok, msg, paths| {
        if let Some(ui) = w2.upgrade() {
            // 存储完成路径到 pending_select，供 task-finished 刷新后定位
            let path_list: Vec<PathBuf> = paths
                .iter()
                .map(|s| PathBuf::from(s.as_str()))
                .collect();
            c2.borrow_mut().pending_select = path_list;
            // 触发外部 task-finished 回调
            ui.global::<AppState>().invoke_task_finished(ok, msg);
        }
    });

    // 任务完成（工作线程经事件循环回调）：刷新目录、串联下一项或收起卡片
    let w = ui.as_weak();
    let c = core.clone();
    state.on_task_finished(move |_ok, msg| {
        if let Some(ui) = w.upgrade() {
            c.borrow_mut().task_control = None;
            let right_pane = ui.global::<AppState>().get_dual_pane()
                && ui.global::<AppState>().get_active_pane().as_str() == "right";
            load_current(&ui, &c);
            // 双面板时右侧面板也可能是任务的源或目标，一并刷新
            if ui.global::<AppState>().get_dual_pane() {
                load_right(&ui, &c);
            }
            // 刷新完成后，选中 pending_select 中的路径
            let paths = {
                let mut core = c.borrow_mut();
                std::mem::take(&mut core.pending_select)
            };
            if !paths.is_empty() {
                select_completed_paths(&ui, &c, right_pane, &paths);
            }
            let st = ui.global::<AppState>();
            st.set_status_text(msg);
            let has_more = !c.borrow().task_queue.is_empty();
            if has_more {
                start_next_job(&ui, &c);
            } else {
                st.set_task_active(false);
            }
        }
    });

    // 删除
    let w = ui.as_weak();
    let c = core.clone();
    state.on_delete_selected(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            // 「此电脑」视图中选中的是驱动器/设备，禁止删除
            let at_this_pc = c.borrow().pane(right).history.current().to_string_lossy()
                == fs::virtualfs::THIS_PC_PATH;
            if at_this_pc {
                ui.global::<AppState>()
                    .set_status_text("此电脑中的驱动器与设备无法删除".into());
                return;
            }
            let paths = c.borrow().pane_selected_paths(right);
            // 防御：过滤驱动器根路径（如 C:\），避免任何入口误删整盘
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| {
                    let s = p.to_string_lossy();
                    !(s.len() == 3 && s.as_bytes()[1] == b':')
                })
                .collect();
            if paths.is_empty() {
                return;
            }
            // 便携设备对象不能进入回收站，和本地对象分组分别处理。
            // 混合选择时不能把本地路径传给 WPD，也不能漏掉本地项目。
            let device_paths: Vec<String> = paths
                .iter()
                .filter(|p| fs::devices::is_device_path(&p.to_string_lossy()))
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let device_msg = if device_paths.is_empty() {
                None
            } else {
                Some(match fs::devices::delete(&device_paths) {
                    Ok(()) => format!("已从设备删除 {} 个项目", device_paths.len()),
                    Err(e) => format!("设备删除失败：{}", e),
                })
            };
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !fs::devices::is_device_path(&p.to_string_lossy()))
                .collect();
            if paths.is_empty() {
                if let Some(msg) = device_msg {
                    reload_active_pane(&ui, &c);
                    schedule_pane_reloads(&ui, &c, &[600, 2000]);
                    ui.global::<AppState>().set_status_text(msg.into());
                }
                return;
            }
            // 记录撤销 + 索引移除（入队时即记，删除在后台执行）
            c.borrow_mut()
                .record_undo(app::UndoAction::Delete { paths: paths.clone() });
            if c.borrow().config.settings.background_index {
                for p in &paths {
                    fs::index::remove_path(p);
                }
            }
            // 后台任务执行删除：进度卡片与粘贴/复制一致，可暂停/取消，
            // UI 线程不再被 Shell 删除阻塞（修复大文件夹删除无反馈/卡死）
            c.borrow_mut().task_queue.push_back(fs::tasks::Job {
                kind: fs::tasks::TaskKind::Delete,
                srcs: paths,
                dst: PathBuf::new(),
            });
            if let Some(msg) = device_msg {
                ui.global::<AppState>().set_status_text(msg.into());
            }
            start_next_job(&ui, &c);
        }
    });

    // 重命名提交：回调自带面板语义（rename-entry=左 / r-rename-entry=右），
    // 不按提交瞬间的 active-pane 路由——编辑中途点击另一面板不会改错对象
    let w = ui.as_weak();
    let c = core.clone();
    state.on_rename_entry(move |idx, new_name| {
        if let Some(ui) = w.upgrade() {
            rename_in_pane(&ui, &c, false, idx, new_name.as_str());
        }
    });
    let w = ui.as_weak();
    let c = core.clone();
    state.on_r_rename_entry(move |idx, new_name| {
        if let Some(ui) = w.upgrade() {
            rename_in_pane(&ui, &c, true, idx, new_name.as_str());
        }
    });

    // 新建文件夹
    let w = ui.as_weak();
    let c = core.clone();
    state.on_new_folder(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let dst = c.borrow().pane(right).history.current().clone();
            // 便携设备目录：std::fs 不可用，改走 WPD 新建
            let dst_str = dst.to_string_lossy().to_string();
            if fs::devices::is_device_path(&dst_str) {
                let name = unique_device_name(&dst_str, "新建文件夹");
                match fs::devices::create_folder(&dst_str, &name) {
                    Ok(path) => {
                        ui.global::<AppState>()
                            .set_status_text("已在设备上新建文件夹".into());
                        reload_active_pane(&ui, &c);
                        select_created_and_edit(&ui, &c, right, &path);
                        return;
                    }
                    Err(e) => {
                        ui.global::<AppState>()
                            .set_status_text(format!("新建失败：{}", e).into());
                    }
                }
                reload_active_pane(&ui, &c);
                return;
            }
            match ops::new_folder(&dst, "新建文件夹") {
                Ok(path) => {
                    c.borrow_mut()
                        .record_undo(app::UndoAction::Create { path: path.clone() });
                    if c.borrow().config.settings.background_index {
                        fs::index::add_path(&path);
                    }
                    reload_active_pane(&ui, &c);
                    select_created_and_edit(&ui, &c, right, &path.to_string_lossy());
                    return;
                }
                Err(error) => {
                    let args = vec![dst.as_os_str().to_os_string(), "新建文件夹".into()];
                    let elevated = fs::elevated::retry_if_permission_denied(
                        &error,
                        fs::elevated::ElevatedOp::CreateDir,
                        &args,
                    );
                    ui.global::<AppState>().set_status_text(
                        if elevated {
                            "已请求管理员权限创建文件夹"
                        } else {
                            "创建文件夹失败"
                        }
                        .into(),
                    );
                }
            }
            reload_active_pane(&ui, &c);
            schedule_pane_reloads(&ui, &c, &[800, 2000]);
        }
    });

    // 新建文件
    let w = ui.as_weak();
    let c = core.clone();
    state.on_new_file(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let dst = c.borrow().pane(right).history.current().clone();
            // 便携设备目录：std::fs 不可用，改走 WPD 新建
            let dst_str = dst.to_string_lossy().to_string();
            if fs::devices::is_device_path(&dst_str) {
                let name = unique_device_name(&dst_str, "新建文本文档.txt");
                match fs::devices::create_file(&dst_str, &name) {
                    Ok(path) => {
                        ui.global::<AppState>()
                            .set_status_text("已在设备上新建文件".into());
                        reload_active_pane(&ui, &c);
                        select_created_and_edit(&ui, &c, right, &path);
                        return;
                    }
                    Err(e) => {
                        ui.global::<AppState>()
                            .set_status_text(format!("新建失败：{}", e).into());
                    }
                }
                reload_active_pane(&ui, &c);
                return;
            }
            match ops::new_file(&dst, "新建文本文档.txt") {
                Ok(path) => {
                    c.borrow_mut()
                        .record_undo(app::UndoAction::Create { path: path.clone() });
                    if c.borrow().config.settings.background_index {
                        fs::index::add_path(&path);
                    }
                    reload_active_pane(&ui, &c);
                    select_created_and_edit(&ui, &c, right, &path.to_string_lossy());
                    return;
                }
                Err(error) => {
                    let args = vec![dst.as_os_str().to_os_string(), "新建文本文档.txt".into()];
                    let elevated = fs::elevated::retry_if_permission_denied(
                        &error,
                        fs::elevated::ElevatedOp::CreateFile,
                        &args,
                    );
                    ui.global::<AppState>().set_status_text(
                        if elevated {
                            "已请求管理员权限创建文件"
                        } else {
                            "创建文件失败"
                        }
                        .into(),
                    );
                }
            }
            reload_active_pane(&ui, &c);
            schedule_pane_reloads(&ui, &c, &[800, 2000]);
        }
    });

    // 撤销最近一次可逆操作（Ctrl+Z）：弹撤销栈执行逆操作，压入重做栈
    let w = ui.as_weak();
    let c = core.clone();
    state.on_undo(move || {
        if let Some(ui) = w.upgrade() {
            let action = c.borrow_mut().undo_stack.pop();
            let Some(action) = action else {
                ui.global::<AppState>()
                    .set_status_text("没有可撤销的操作".into());
                return;
            };
            let msg = apply_undo(&action);
            c.borrow_mut().redo_stack.push(action);
            load_current(&ui, &c);
            ui.global::<AppState>().set_status_text(msg.into());
        }
    });

    // 重做最近一次被撤销的操作（Ctrl+Shift+Z）：弹重做栈执行正向，压回撤销栈
    let w = ui.as_weak();
    let c = core.clone();
    state.on_redo(move || {
        if let Some(ui) = w.upgrade() {
            let action = c.borrow_mut().redo_stack.pop();
            let Some(action) = action else {
                ui.global::<AppState>()
                    .set_status_text("没有可重做的操作".into());
                return;
            };
            let msg = apply_redo(&action);
            c.borrow_mut().undo_stack.push(action);
            load_current(&ui, &c);
            ui.global::<AppState>().set_status_text(msg.into());
        }
    });

    // 计算重命名时应选中的主名长度（字节偏移）。
    // set-editing / set-editing-right 在 Slint 侧调用 name-select-len(name)，由此闭包实现。
    // 系统资源管理器语义：file.txt→选 file；.gitignore→全选；无扩展名→全选。
    state.on_name_select_len(move |name: slint::SharedString| {
        let s = name.as_str();
        match s.rfind('.') {
            Some(0) => s.len() as i32,  // 点开头：隐藏文件，全选
            Some(p) => p as i32,        // 常规：选主名不含扩展名
            None => s.len() as i32,     // 无扩展名：全选
        }
    });

    // F2 请求重命名当前选中项（按活动面板路由到对应的行内编辑）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_request_rename(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let idx = c.borrow().pane(right).first_selected();
            if let Some(i) = idx {
                if right {
                    ui.invoke_set_editing_right(i as i32);
                } else {
                    ui.invoke_set_editing(i as i32);
                }
            }
        }
    });

    // 「选中后单击重命名」防抖：单击已选中项先挂起，延迟提交。
    // 双击（打开）会在延迟窗口内调用 cancel-click-rename 取消挂起，
    // 从而避免双击打开时先闪一下重命名输入框（单击与双击在 pointer-up 阶段无法区分，
    // 必须延迟到能判定「没有第二次点击」时再进入重命名）。
    bind_click_rename(ui, core);

    // 打开属性（数据源按活动面板：右面板活动时显示右面板选中项）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_open_properties(move |idx| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            {
                let mut core = c.borrow_mut();
                let tab = core.pane_mut(right);
                let i = idx as usize;
                if i < tab.selected.len() {
                    for s in tab.selected.iter_mut() {
                        *s = false;
                    }
                    tab.selected[i] = true;
                }
            }
            ui_bridge::update_selection_pane(&ui, &c.borrow(), right);
            // 慢属性仅在用户明确打开对话框时读取，不拖慢普通选中操作。
            {
                let core = c.borrow();
                let tab = core.pane(right);
                if let Some(entry) = tab
                    .selected
                    .iter()
                    .enumerate()
                    .find(|(_, &s)| s)
                    .and_then(|(fi, _)| tab.entry_at(fi))
                {
                    let path = Path::new(&entry.path);
                    let state = ui.global::<AppState>();
                    ui_bridge::fill_properties(&state, path, entry.is_dir);
                    ui_bridge::fill_details(&state, path);
                }
            }
            let st = ui.global::<AppState>();
            let empty = vec![
                HashResult {
                    algo: "MD5".into(),
                    value: "".into(),
                },
                HashResult {
                    algo: "SHA-1".into(),
                    value: "".into(),
                },
                HashResult {
                    algo: "SHA-256".into(),
                    value: "".into(),
                },
                HashResult {
                    algo: "SHA-512".into(),
                    value: "".into(),
                },
            ];
            st.set_hashes(slint::ModelRc::new(slint::VecModel::from(empty)));
            ui.set_props_open(true);
        }
    });
}

/// 为"新增"菜单条目推导内置矢量图标类别（与文件列表分类一致）
fn shell_new_icon_class(ext: &str) -> (String, String) {
    if ext.is_empty() {
        return ("folder".into(), "F".into());
    }
    // 借用文件分类逻辑：构造一个仅含扩展名的虚拟路径
    let fake = std::path::PathBuf::from(format!("x{}", ext));
    let (class, label, _kind) = fs::metadata::classify(&fake, false);
    (class, label)
}

// ─── "新增"菜单：枚举系统注册表 ShellNew 项，下拉选择后创建并进入重命名 ───

/// 「选中后单击重命名」防抖定时器绑定（见 input_overlay.slint 的 request-click-rename）。
fn bind_click_rename(ui: &MainWindow, _core: &Rc<RefCell<AppCore>>) {
    use std::cell::Cell;
    use std::time::Duration;

    // 单次挂起重命名的代次：每次请求/取消都自增，旧定时器回调比对代次判定是否过期。
    let gen = Rc::new(Cell::new(0u64));
    let pending: Rc<RefCell<Option<(String, i32)>>> = Rc::new(RefCell::new(None));
    // 复用单个 SingleShot 定时器：新请求会重置计时，旧回调自然作废
    let timer = Rc::new(slint::Timer::default());

    let state = ui.global::<AppState>();
    let g = gen.clone();
    let p = pending.clone();
    let t = timer.clone();
    let w = ui.as_weak();
    state.on_request_click_rename(move |pane, idx| {
        let ng = g.get().wrapping_add(1);
        g.set(ng);
        *p.borrow_mut() = Some((pane.to_string(), idx));
        let p2 = p.clone();
        let g2 = g.clone();
        let w2 = w.clone();
        // 使用 Windows 当前双击时间并额外加宽 300ms 识别窗口：
        // 保证慢速双击/误触不会在打开的同时抢入重命名状态。
        #[cfg(windows)]
        let delay_ms = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() as u64 + 300
        };
        #[cfg(not(windows))]
        let delay_ms = 800;
        t.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(delay_ms),
            move || {
                // 代次不匹配=已被取消或被更新的请求取代
                if g2.get() != ng {
                    return;
                }
                if let Some((pane, idx)) = p2.borrow_mut().take() {
                    if let Some(ui) = w2.upgrade() {
                        if pane == "right" {
                            ui.invoke_set_editing_right(idx);
                        } else {
                            ui.invoke_set_editing(idx);
                        }
                    }
                }
            },
        );
    });

    let state = ui.global::<AppState>();
    let g = gen.clone();
    let p = pending.clone();
    state.on_cancel_click_rename(move || {
        g.set(g.get().wrapping_add(1));
        *p.borrow_mut() = None;
    });
}

fn bind_new_menu(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 启动时枚举一次系统"新建"模板（注册表项在会话内基本不变），共享给创建回调使用
    let items = Rc::new(fs::shell_new::enumerate());

    // 推送到 UI 下拉菜单：先填内置矢量图标（保证绝不出现空白/白板）
    let entries: Vec<ShellNewEntry> = items
        .iter()
        .map(|it| {
            let (icon_class, icon_label) = shell_new_icon_class(&it.ext);
            ShellNewEntry {
                name: it.name.clone().into(),
                icon_class: icon_class.into(),
                icon_label: icon_label.into(),
                thumb: slint::Image::default(),
                has_thumb: false,
            }
        })
        .collect();
    state.set_new_menu_items(slint::ModelRc::new(slint::VecModel::from(entries)));

    // 图标来源 = "系统图标"：后台逐个按文件类型提取系统图标，回填到菜单模型，
    // 覆盖内置矢量图。失败的条目保持内置矢量图（不会出现空白）。
    let system_icons = core.borrow().config.settings.icon_source == "system";
    if system_icons {
        let exts: Vec<String> = items.iter().map(|it| it.ext.clone()).collect();
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            for (row, ext) in exts.into_iter().enumerate() {
                let Some((pixels, w, h)) =
                    fs::thumbnail::extract_type_icon(&ext, ext.is_empty(), 32)
                else {
                    continue;
                };
                let weak2 = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak2.upgrade() else { return };
                    let model = ui.global::<AppState>().get_new_menu_items();
                    if let Some(mut entry) = model.row_data(row) {
                        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                        buf.make_mut_bytes().copy_from_slice(&pixels);
                        entry.thumb = slint::Image::from_rgba8(buf);
                        entry.has_thumb = true;
                        model.set_row_data(row, entry);
                    }
                });
            }
        });
    }

    // 创建选中的"新建"项：在当前目录创建文件/文件夹，刷新后选中并进入行内重命名
    let w = ui.as_weak();
    let c = core.clone();
    let items_for_cb = items.clone();
    state.on_create_new_item(move |index| {
        let Some(ui) = w.upgrade() else { return };
        let Some(item) = items_for_cb.get(index as usize) else {
            return;
        };

        // 按活动面板路由：双面板右侧活动时在右面板当前目录新建
        let right = toolbar_routes_right(&ui);
        let dst = c.borrow().pane(right).history.current().clone();
        // 虚拟位置（回收站 / 标签 / 网络）无法新建实体文件
        if fs::virtualfs::is_virtual(&dst.to_string_lossy()) {
            ui.global::<AppState>()
                .set_status_text("当前位置无法新建项目".into());
            return;
        }

        let created = match fs::shell_new::create_item(&dst, item) {
            Ok(path) => path,
            Err(e) => {
                ui.global::<AppState>()
                    .set_status_text(format!("新建失败：{}", e).into());
                return;
            }
        };
        // 启用后台索引时增量加入
        if c.borrow().config.settings.background_index {
            fs::index::add_path(&created);
        }
        c.borrow_mut().record_undo(app::UndoAction::Create {
            path: created.clone(),
        });

        reload_active_pane(&ui, &c);

        // 刷新后的列表中定位新建项，并统一更新选择/详情状态后进入编辑。
        let created_str = created.to_string_lossy().to_string();
        select_created_and_edit(&ui, &c, right, &created_str);
        ui.global::<AppState>()
            .set_status_text(format!("已新建「{}」", item.name).into());
    });
}

// ─── 右键菜单扩展操作：新标签页打开 / 新面板打开 / 压缩 ZIP / 系统原生菜单 ───

fn bind_context_menu_ext(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 在新标签页中打开：文件夹进入该目录；文件进入其父目录
    let w = ui.as_weak();
    let c = core.clone();
    state.on_open_in_new_tab(move |idx| {
        if let Some(ui) = w.upgrade() {
            let target = {
                let core = c.borrow();
                core.pane_entry_at(toolbar_routes_right(&ui), idx as usize)
                    .map(|e| (e.is_dir, e.path.clone()))
            };
            if let Some((is_dir, path)) = target {
                // 标签栏已满时拒绝在新标签页打开，避免挤出窗口按钮
                if tabs_full(&ui, &c) {
                    ui.global::<AppState>()
                        .set_status_text("标签栏已满，请先关闭部分标签页".into());
                    return;
                }
                let dir = if is_dir {
                    PathBuf::from(&path)
                } else {
                    Path::new(&path)
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from(&path))
                };
                c.borrow_mut().new_tab(dir);
                load_current(&ui, &c);
            }
        }
    });

    // 在新面板中打开：进入该文件夹并切换到双面板视图
    let w = ui.as_weak();
    let c = core.clone();
    state.on_open_in_new_panel(move |idx| {
        if let Some(ui) = w.upgrade() {
            let target = {
                let core = c.borrow();
                core.pane_entry_at(toolbar_routes_right(&ui), idx as usize)
                    .map(|e| (e.is_dir, e.path.clone()))
            };
            if let Some((is_dir, path)) = target {
                let dir = if is_dir {
                    PathBuf::from(&path)
                } else {
                    Path::new(&path)
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from(&path))
                };
                // 在右侧独立面板打开该目录，并开启双面板（与布局正交）
                navigate_right(&ui, &c, dir);
                ui.global::<AppState>().set_dual_pane(true);
            }
        }
    });

    // 压缩为 ZIP：压缩选中项（若无选中则压缩目标项）到当前目录，后台任务执行
    let w = ui.as_weak();
    let c = core.clone();
    state.on_compress_selected(move |idx| {
        if let Some(ui) = w.upgrade() {
            enqueue_compress(&ui, &c, idx, "zip");
        }
    });

    // 压缩为指定格式（ActionBar「压缩」下拉：zip / 7z / tar / targz）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_compress_selected_fmt(move |idx, fmt| {
        if let Some(ui) = w.upgrade() {
            enqueue_compress(&ui, &c, idx, fmt.as_str());
        }
    });

    // 解压选中归档到以归档名命名的子文件夹：逐归档入队一个后台任务
    // （进度/速度/ETA、暂停/取消、同名冲突询问均由任务系统提供）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_extract_selected(move || {
        if let Some(ui) = w.upgrade() {
            let (archives, dst) = selected_archives(&ui, &c);
            if archives.is_empty() {
                return;
            }
            {
                let mut core = c.borrow_mut();
                for archive in archives {
                    let stem = archive
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "解压".to_string());
                    // 目标子文件夹以归档名命名，重名自动加序号；入队时即创建目录，
                    // 使同名归档的后续任务能避让到不同序号
                    let target = ops::resolve_conflict(dst.join(&stem));
                    let _ = std::fs::create_dir_all(&target);
                    core.task_queue.push_back(fs::tasks::Job {
                        kind: fs::tasks::TaskKind::Extract,
                        srcs: vec![archive],
                        dst: target,
                    });
                }
            }
            start_next_job(&ui, &c);
        }
    });

    // 解压选中归档到当前文件夹（内容直接落在当前目录，同名走冲突询问）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_extract_selected_here(move || {
        if let Some(ui) = w.upgrade() {
            let (archives, dst) = selected_archives(&ui, &c);
            if archives.is_empty() {
                return;
            }
            {
                let mut core = c.borrow_mut();
                for archive in archives {
                    core.task_queue.push_back(fs::tasks::Job {
                        kind: fs::tasks::TaskKind::Extract,
                        srcs: vec![archive],
                        dst: dst.clone(),
                    });
                }
            }
            start_next_job(&ui, &c);
        }
    });

    // 弹出 Windows 原生 Shell 右键菜单（含第三方注册项）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_show_system_menu(move |idx, mx, my| {
        if let Some(ui) = w.upgrade() {
            // 收集作用对象：优先用当前选中项（多选时菜单作用于全部，与资源管理器一致）；
            // 若右键的目标项不在选中集合中，则回退为仅该目标项。
            let right = toolbar_routes_right(&ui);
            let paths: Vec<String> = {
                let core = c.borrow();
                let tab = core.pane(right);
                let target_selected = tab.selected.get(idx as usize).copied().unwrap_or(false);
                if target_selected {
                    core.pane_selected_paths(right)
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect()
                } else {
                    core.pane_entry_at(right, idx as usize)
                        .map(|e| vec![e.path.clone()])
                        .unwrap_or_default()
                }
            };
            if paths.is_empty() {
                return;
            }
            // 弹出原生菜单（阻塞至用户选择/关闭）；若执行了命令（删除/重命名/粘贴/
            // 打开方式改默认应用等），清空图标缓存并刷新活动面板：
            // 文件类型关联可能已变化，旧缓存图标必须失效才能实时反映新默认应用。
            let invoked = show_system_context_menu(&ui, &paths, mx, my);
            if invoked {
                fs::thumbnail::clear_all_caches();
                reload_active_pane(&ui, &c);
                // Shell 命令（删除/粘贴等）异步执行：延迟补刷确保结果反映到视图
                schedule_pane_reloads(&ui, &c, &[600, 2000]);
            }
        }
    });

    // 弹出目录「背景」系统右键菜单（空白处：查看/排序/新建/粘贴等）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_show_system_background_menu(move |mx, my| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let dir = {
                let core = c.borrow();
                core.pane(right)
                    .history
                    .current()
                    .to_string_lossy()
                    .to_string()
            };
            // 虚拟路径（此电脑/回收站/标签等）没有对应的 Shell 目录背景菜单
            if fs::virtualfs::is_virtual(&dir) {
                return;
            }
            // 弹出前留一份目录快照：系统菜单的「新建」由 Shell 自己执行，不回传新
            // 路径，只能靠菜单前后的目录差集找出新建项，并将其选中 + 进入重命名，
            // 与应用内「新增」菜单一致（此前仅刷新，用户看不到新建的文件/文件夹）。
            let before = snapshot_pane_dir(&c, right);
            let invoked = show_system_background_menu(&ui, &dir, mx, my);
            if invoked {
                // 背景菜单命令（新建/粘贴等）可能异步收尾，故立即比对 + 延迟重试
                match before {
                    Some(before) => {
                        schedule_reload_selecting_new(&ui, &c, right, before, &[600, 2000])
                    }
                    None => {
                        reload_active_pane(&ui, &c);
                        schedule_pane_reloads(&ui, &c, &[600, 2000]);
                    }
                }
            }
        }
    });

    // 侧栏使用导航语义菜单，不复用文件对象的完整 Shell 菜单。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_show_sidebar_menu(move |path, mx, my| {
        if let Some(ui) = w.upgrade() {
            show_sidebar_menu(&ui, &c, path.as_str(), mx, my);
        }
    });

    // 按路径弹出系统右键菜单（侧栏之外的兼容入口）。
    // 虚拟 scheme 先映射为 Shell 命名空间解析名（::{CLSID}），无对应物的静默跳过。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_show_system_menu_path(move |path, mx, my| {
        if let Some(ui) = w.upgrade() {
            let path = path.to_string();
            let target = if fs::virtualfs::is_virtual(&path) {
                match path.as_str() {
                    // 此电脑：可得「管理/映射网络驱动器/属性」等菜单
                    "this-pc://" => "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}".to_string(),
                    // 回收站：可得「清空回收站/属性」
                    "recycle://" => "::{645FF040-5081-101B-9F08-00AA002F954E}".to_string(),
                    // 网络：Shell 网络命名空间
                    "network://" => "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}".to_string(),
                    // 标签/便携设备等应用内概念无系统菜单对应物：静默跳过
                    _ => return,
                }
            } else {
                if path.is_empty() {
                    return;
                }
                path
            };
            let invoked = show_system_context_menu(&ui, &[target], mx, my);
            if invoked {
                // 菜单命令可能改变侧栏内容（取消固定/弹出设备/重命名卷标/清空回收站），
                // 也可能删除了当前面板所在目录：重建侧栏 + 刷新 + 延迟补刷
                fs::thumbnail::clear_all_caches();
                reload_active_pane(&ui, &c);
                {
                    let c2 = c.borrow();
                    ui.global::<AppState>()
                        .set_nav_items(ui_bridge::build_sidebar(
                            c2.active_tab().history.current(),
                            &c2.collapsed_sections,
                            &c2.config,
                        ));
                }
                schedule_pane_reloads(&ui, &c, &[600, 2000]);
            }
        }
    });

    // OLE 拖出：把选中文件拖到其他应用（模态执行 DoDragDrop）。
    // 借用纪律与 on_show_system_menu 一致：先收集路径、释放借用，再进模态循环。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_drag_out(move |pane, idx| {
        if let Some(ui) = w.upgrade() {
            let right = pane.as_str() == "right";
            let paths: Vec<String> = {
                let core = c.borrow();
                let tab = core.pane(right);
                let target_selected = tab.selected.get(idx as usize).copied().unwrap_or(false);
                let raw: Vec<String> = if target_selected {
                    core.pane_selected_paths(right)
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect()
                } else {
                    core.pane_entry_at(right, idx as usize)
                        .map(|e| vec![e.path.clone()])
                        .unwrap_or_default()
                };
                // 过滤虚拟条目（标签/回收站视图等），仅真实文件系统路径可拖出
                raw.into_iter()
                    .filter(|p| !fs::virtualfs::is_virtual(p) && Path::new(p).exists())
                    .collect()
            };
            let st = ui.global::<AppState>();
            if paths.is_empty() {
                st.set_pane_drag_active(false);
                return;
            }
            // 模态 OLE 拖拽（阻塞至放下/取消）；期间左键 up 被 OLE 吃掉
            let effect = fs::drag_out::run(&paths);
            // 合成一次窗口外的左键释放，复位 Slint 指针抓取/按下状态
            let _ = ui
                .window()
                .try_dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(-10.0, -10.0),
                    button: slint::platform::PointerEventButton::Left,
                });
            st.set_pane_drag_active(false);
            // 目标应用执行「移动」时源文件将消失：刷新 + 延迟补刷（Shell 异步收尾）
            if effect == 2 {
                reload_active_pane(&ui, &c);
                schedule_pane_reloads(&ui, &c, &[600, 2000]);
            }
        }
    });

    // 单面板内部拖拽放到文件夹上：询问确认后后台移动
    let w = ui.as_weak();
    let c = core.clone();
    state.on_request_move_onto(move |pane, src_idx, dst_idx| {
        if let Some(ui) = w.upgrade() {
            let right = pane.as_str() == "right";
            let (srcs, dst) = {
                let core = c.borrow();
                let tab = core.pane(right);
                let dst = match tab.entry_at(dst_idx as usize) {
                    Some(e) if e.is_dir => PathBuf::from(&e.path),
                    _ => return,
                };
                let target_selected = tab.selected.get(src_idx as usize).copied().unwrap_or(false);
                let srcs: Vec<PathBuf> = if target_selected {
                    core.pane_selected_paths(right)
                } else {
                    tab.entry_at(src_idx as usize)
                        .map(|e| vec![PathBuf::from(&e.path)])
                        .unwrap_or_default()
                };
                (srcs, dst)
            };
            if srcs.is_empty() || srcs.iter().any(|s| s == &dst) {
                return;
            }
            // 询问是否移动，确认后才执行
            #[cfg(windows)]
            {
                use windows::Win32::UI::WindowsAndMessaging::{
                    MessageBoxW, MB_ICONQUESTION, MB_YESNO, IDYES,
                };
                let text = windows::core::HSTRING::from(&format!(
                    "将 {} 个项目移动到「{}」？",
                    srcs.len(),
                    dst.display()
                ));
                let cap = windows::core::HSTRING::from("移动");
                let ans = unsafe { MessageBoxW(None, &text, &cap, MB_YESNO | MB_ICONQUESTION) };
                if ans != IDYES {
                    return;
                }
            }
            {
                let mut core = c.borrow_mut();
                if !fs::devices::is_device_path(&dst.to_string_lossy()) {
                    let pairs: Vec<(PathBuf, PathBuf)> = srcs
                        .iter()
                        .filter_map(|s| s.file_name().map(|n| (s.clone(), dst.join(n))))
                        .collect();
                    core.record_undo(app::UndoAction::Move { pairs });
                }
                core.task_queue.push_back(fs::tasks::Job {
                    kind: fs::tasks::TaskKind::Move,
                    srcs,
                    dst,
                });
            }
            start_next_job(&ui, &c);
        }
    });

    // 固定到快速访问：调用系统 pintohome 动词写入真实快速访问，并刷新侧边栏
    let w = ui.as_weak();
    let c = core.clone();
    state.on_pin_to_quick_access(move |idx| {
        if let Some(ui) = w.upgrade() {
            let target = {
                let core = c.borrow();
                core.pane_entry_at(toolbar_routes_right(&ui), idx as usize)
                    .map(|e| (e.is_dir, e.path.clone()))
            };
            let Some((is_dir, path)) = target else { return };
            if !is_dir {
                return; // 仅文件夹可固定
            }
            let ok = fs::quickaccess::pin(&path);
            let c2 = c.borrow();
            // 重建侧边栏，使新固定项立即出现在「快速访问」
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    c2.active_tab().history.current(),
                    &c2.collapsed_sections,
                    &c2.config,
                ));
            ui.global::<AppState>().set_status_text(
                if ok {
                    "已固定到快速访问"
                } else {
                    "固定到快速访问失败"
                }
                .into(),
            );
        }
    });
}

/// 弹出侧栏专用菜单并执行导航操作。
#[cfg(windows)]
fn show_sidebar_menu(ui: &MainWindow, core: &Rc<RefCell<AppCore>>, path: &str, mx: f32, my: f32) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if path.is_empty() {
        return;
    }
    let real_dir = !fs::virtualfs::is_virtual(path) && Path::new(path).is_dir();
    let can_unpin = real_dir
        && fs::quickaccess::list()
            .iter()
            .any(|item| item.path.eq_ignore_ascii_case(path));
    let mut command = None;
    ui.window().with_winit_window(|winit_window| {
        let Ok(origin) = winit_window.inner_position() else {
            return;
        };
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let scale = winit_window.scale_factor() as f32;
        command = fs::sidebar_menu::show(
            isize::from(handle.hwnd),
            origin.x + (mx * scale).round() as i32,
            origin.y + (my * scale).round() as i32,
            real_dir,
            can_unpin,
        );
    });

    use fs::sidebar_menu::SidebarCommand;
    match command {
        Some(SidebarCommand::Open) => navigate_to(ui, core, PathBuf::from(path)),
        Some(SidebarCommand::OpenNewTab) => {
            // 标签栏已满时拒绝，避免挤出窗口按钮
            if tabs_full(ui, core) {
                ui.global::<AppState>()
                    .set_status_text("标签栏已满，请先关闭部分标签页".into());
                return;
            }
            core.borrow_mut().new_tab(PathBuf::from(path));
            load_current(ui, core);
        }
        Some(SidebarCommand::OpenNewPanel) => {
            navigate_right(ui, core, PathBuf::from(path));
            ui.global::<AppState>().set_dual_pane(true);
        }
        Some(SidebarCommand::Unpin) => {
            let ok = fs::quickaccess::unpin(path);
            let c = core.borrow();
            ui.global::<AppState>()
                .set_nav_items(ui_bridge::build_sidebar(
                    c.active_tab().history.current(),
                    &c.collapsed_sections,
                    &c.config,
                ));
            ui.global::<AppState>().set_status_text(
                if ok {
                    "已从快速访问取消固定"
                } else {
                    "取消固定失败"
                }
                .into(),
            );
        }
        _ => {}
    }
}

#[cfg(not(windows))]
fn show_sidebar_menu(
    _ui: &MainWindow,
    _core: &Rc<RefCell<AppCore>>,
    _path: &str,
    _mx: f32,
    _my: f32,
) {
}

/// 把窗口内逻辑坐标 (mx,my) 换算为屏幕物理坐标，并弹出系统原生右键菜单。
/// 返回是否执行了某条命令（供调用方决定是否刷新视图）。
#[cfg(windows)]
fn show_system_context_menu(ui: &MainWindow, paths: &[String], mx: f32, my: f32) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let paths = paths.to_vec();
    let mut invoked = false;
    ui.window().with_winit_window(|winit_window| {
        // 窗口左上角屏幕物理坐标
        let origin = match winit_window.inner_position() {
            Ok(p) => p,
            Err(_) => return,
        };
        // 逻辑像素 → 物理像素
        let scale = winit_window.scale_factor() as f32;
        let screen_x = origin.x + (mx * scale).round() as i32;
        let screen_y = origin.y + (my * scale).round() as i32;

        // 取 HWND（isize 形式传入 shell_menu）
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd_isize = isize::from(h.hwnd);
            invoked = fs::shell_menu::show(&paths, hwnd_isize, screen_x, screen_y);
        }
    });
    invoked
}

#[cfg(not(windows))]
fn show_system_context_menu(_ui: &MainWindow, _paths: &[String], _mx: f32, _my: f32) -> bool {
    false
}

/// 把窗口内逻辑坐标换算为屏幕物理坐标，弹出目录背景系统菜单（空白处右键）。
#[cfg(windows)]
fn show_system_background_menu(ui: &MainWindow, dir: &str, mx: f32, my: f32) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let dir = dir.to_string();
    let mut invoked = false;
    ui.window().with_winit_window(|winit_window| {
        let origin = match winit_window.inner_position() {
            Ok(p) => p,
            Err(_) => return,
        };
        let scale = winit_window.scale_factor() as f32;
        let screen_x = origin.x + (mx * scale).round() as i32;
        let screen_y = origin.y + (my * scale).round() as i32;
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd_isize = isize::from(h.hwnd);
            invoked = fs::shell_menu::show_background(&dir, hwnd_isize, screen_x, screen_y);
        }
    });
    invoked
}

#[cfg(not(windows))]
fn show_system_background_menu(_ui: &MainWindow, _dir: &str, _mx: f32, _my: f32) -> bool {
    false
}

// ─── 用户设置：启动推送 + setter 接线 ───

// 配置内部规范值 ↔ UI 中文显示值 的互转
fn lang_disp(c: &str) -> &'static str {
    match c {
        "en" => "English",
        _ => "简体中文",
    }
}
fn lang_canon(d: &str) -> &'static str {
    match d {
        "English" => "en",
        _ => "zh-CN",
    }
}
fn startup_disp(c: &str) -> &'static str {
    match c {
        "quick" => "快速访问",
        "this-pc" => "此电脑",
        _ => "上次的标签页",
    }
}
fn startup_canon(d: &str) -> &'static str {
    match d {
        "快速访问" => "quick",
        "此电脑" => "this-pc",
        _ => "last",
    }
}
fn icon_disp(c: &str) -> &'static str {
    match c {
        "builtin" => "内置图标",
        _ => "系统图标",
    }
}
fn icon_canon(d: &str) -> &'static str {
    match d {
        "内置图标" => "builtin",
        _ => "system",
    }
}
fn view_disp(c: &str) -> &'static str {
    match c {
        "grid" => "网格",
        "dual" => "双面板",
        _ => "详细信息",
    }
}
fn view_canon(d: &str) -> &'static str {
    match d {
        "网格" => "grid",
        "双面板" => "dual",
        _ => "list",
    }
}
fn sort_disp(c: &str) -> &'static str {
    match c {
        "size" => "大小",
        "modified" => "修改日期",
        "kind" => "类型",
        _ => "名称",
    }
}
fn sort_canon(d: &str) -> &'static str {
    match d {
        "大小" => "size",
        "修改日期" => "modified",
        "类型" => "kind",
        _ => "name",
    }
}
fn newtab_disp(c: &str) -> &'static str {
    match c {
        "this-pc" => "此电脑",
        "last" => "上次目录",
        _ => "快速访问",
    }
}
fn newtab_canon(d: &str) -> &'static str {
    match d {
        "此电脑" => "this-pc",
        "上次目录" => "last",
        _ => "quick",
    }
}
fn split_disp(c: &str) -> &'static str {
    match c {
        "40" => "40% / 60%",
        "60" => "60% / 40%",
        _ => "50% / 50%",
    }
}
fn split_canon(d: &str) -> &'static str {
    match d {
        "40% / 60%" => "40",
        "60% / 40%" => "60",
        _ => "50",
    }
}
fn index_disp(c: &str) -> &'static str {
    match c {
        "all" => "全部磁盘",
        "custom" => "自定义",
        _ => "用户目录",
    }
}
fn index_canon(d: &str) -> &'static str {
    match d {
        "全部磁盘" => "all",
        "自定义" => "custom",
        _ => "user",
    }
}

/// 解析 hex 颜色字符串（如 "#0078d4" 或 "#ff6600ff"）为 Slint Color。
/// 支持 #RGB / #RRGGBB / #RRGGBBAA 三种格式，失败返回 None。
fn parse_hex_color(hex: &str) -> Option<slint::Color> {
    let h = hex
        .strip_prefix('#')
        .or_else(|| hex.strip_prefix("0x"))
        .unwrap_or(hex);
    let (r, g, b, a) = match h.len() {
        3 => (
            u8::from_str_radix(&h[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&h[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&h[2..3].repeat(2), 16).ok()?,
            255u8,
        ),
        6 => (
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
            u8::from_str_radix(&h[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some(slint::Color::from_argb_u8(a, r, g, b))
}

/// Slint Color → hex 字符串 "#RRGGBB"
fn color_to_hex(c: slint::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// 启动时把持久化设置推送到 Theme（主题/半透明）与 AppState（其余项）
fn push_settings(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let c = core.borrow();
    let s = &c.config.settings;

    let theme = ui.global::<Theme>();
    theme.set_theme_mode(s.theme_mode.clone().into());
    theme.set_accent_key(s.accent.clone().into());
    // 自定义主题色：解析 hex 字符串回 Slint Color
    theme.set_accent_custom(
        parse_hex_color(&s.accent_custom).unwrap_or(slint::Color::from_rgb_u8(0x00, 0x78, 0xd4)),
    );
    theme.set_translucent(s.translucent);
    theme.set_opacity_level(s.opacity);
    theme.set_blur_level(s.blur);
    theme.set_compact(s.compact_mode);

    let st = ui.global::<AppState>();
    st.set_set_launch_startup(s.launch_on_startup);
    st.set_set_single_click(s.single_click_open);
    st.set_set_click_rename(s.click_to_rename);
    st.set_set_language(lang_disp(&s.language).into());
    st.set_set_startup_open(startup_disp(&s.startup_open).into());
    st.set_set_default_fm(s.default_file_manager);
    st.set_set_icon_source(icon_disp(&s.icon_source).into());
    st.set_set_show_hidden(s.show_hidden);
    st.set_set_show_ext(s.show_extensions);
    st.set_set_show_protected(s.show_protected);
    st.set_set_calc_size(s.calc_folder_size);
    st.set_set_folders_first(s.folders_first);
    st.set_set_default_view(view_disp(&s.default_view).into());
    st.set_set_default_sort(sort_disp(&s.default_sort).into());
    st.set_set_restore_tabs(s.restore_tabs);
    st.set_set_exit_last_tab(s.exit_on_last_tab);
    st.set_set_new_tab_loc(newtab_disp(&s.new_tab_location).into());
    st.set_set_show_details_default(s.show_details_default);
    st.set_set_dual_default(s.dual_pane_default);
    st.set_set_split_ratio(split_disp(&s.split_ratio).into());
    st.set_set_live_filter(s.live_filter);
    st.set_set_search_subfolders(s.search_subfolders);
    st.set_set_case_sensitive(s.case_sensitive);
    st.set_set_background_index(s.background_index);
    st.set_set_index_location(index_disp(&s.index_location).into());
    st.set_set_context_menu_system(s.context_menu_system);

    // 默认布局与详情面板显隐：仅启动时应用一次。
    // 布局（grid/list）与双面板正交：旧配置里 default_view 可能为 "dual"，
    // 此时回退为列表布局，双面板开关交由 dual_pane_default 决定。
    let layout = if s.default_view == "grid" {
        "grid"
    } else {
        "list"
    };
    st.set_view_mode(layout.into());
    st.set_dual_pane(s.dual_pane_default || s.default_view == "dual");
    st.set_show_details(s.show_details_default);
    // 图标缩放比例（Ctrl+滚轮），启动时回显
    st.set_icon_scale(s.icon_scale.clamp(0.7, 2.0));
}

/// 绑定设置项 setter：更新配置 → 持久化 → 回显（Theme/AppState）→ 必要时重载目录
fn bind_settings(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // —— 布尔设置 ——
    let w = ui.as_weak();
    let c = core.clone();
    state.on_set_bool(move |key, val| {
        if let Some(ui) = w.upgrade() {
            let mut reload = false;
            {
                let mut core = c.borrow_mut();
                let s = &mut core.config.settings;
                match key.as_str() {
                    "launch-startup" => s.launch_on_startup = val,
                    "single-click" => s.single_click_open = val,
                    "click-rename" => s.click_to_rename = val,
                    "default-fm" => s.default_file_manager = val,
                    "show-hidden" => {
                        s.show_hidden = val;
                        reload = true;
                    }
                    "show-ext" => s.show_extensions = val,
                    "show-protected" => {
                        s.show_protected = val;
                        reload = true;
                    }
                    "calc-size" => s.calc_folder_size = val,
                    "folders-first" => {
                        s.folders_first = val;
                        reload = true;
                    }
                    "restore-tabs" => s.restore_tabs = val,
                    "exit-last-tab" => s.exit_on_last_tab = val,
                    "show-details-default" => s.show_details_default = val,
                    "dual-default" => s.dual_pane_default = val,
                    "live-filter" => s.live_filter = val,
                    "search-subfolders" => s.search_subfolders = val,
                    "case-sensitive" => s.case_sensitive = val,
                    "background-index" => s.background_index = val,
                    "context-menu-system" => s.context_menu_system = val,
                    "translucent" => s.translucent = val,
                    "compact" => s.compact_mode = val,
                    _ => {}
                }
                core.config.save();
            }
            // 回显到 UI（Theme 或 AppState 属性），保证视觉与状态一致
            let theme = ui.global::<Theme>();
            let st = ui.global::<AppState>();
            match key.as_str() {
                "translucent" => {
                    theme.set_translucent(val);
                    // 同步开关窗口透明（边框延伸/圆角/标题栏处理）与真实亚克力磨砂（浓度随 blur-level 连续变化）
                    #[cfg(windows)]
                    apply_window_material(&ui);
                }
                "compact" => theme.set_compact(val),
                "launch-startup" => st.set_set_launch_startup(val),
                "single-click" => st.set_set_single_click(val),
                "click-rename" => st.set_set_click_rename(val),
                "default-fm" => {
                    // 应用/撤销注册表接管；失败时回滚开关并提示
                    match fs::default_app::set_default(val) {
                        Ok(_) => {
                            st.set_set_default_fm(val);
                            st.set_status_text(
                                if val {
                                    "已设为默认文件管理器 (双击文件夹与 Win+E 将打开本应用)"
                                } else {
                                    "已恢复系统资源管理器为默认"
                                }
                                .into(),
                            );
                        }
                        Err(e) => {
                            st.set_set_default_fm(!val);
                            c.borrow_mut().config.settings.default_file_manager = !val;
                            c.borrow().config.save();
                            st.set_status_text(format!("注册表操作失败: {}", e).into());
                        }
                    }
                }
                "show-hidden" => st.set_set_show_hidden(val),
                "show-ext" => st.set_set_show_ext(val),
                "show-protected" => st.set_set_show_protected(val),
                "calc-size" => st.set_set_calc_size(val),
                "folders-first" => st.set_set_folders_first(val),
                "restore-tabs" => st.set_set_restore_tabs(val),
                "exit-last-tab" => st.set_set_exit_last_tab(val),
                "show-details-default" => st.set_set_show_details_default(val),
                "dual-default" => {
                    st.set_set_dual_default(val);
                    // 即时生效：切换开关立刻进入/退出双面板（此前只回显，
                    // 用户会误以为设置无效）；下次启动由 push_settings 按配置接管
                    st.set_dual_pane(val);
                    if val {
                        load_right(&ui, &c);
                    } else {
                        // 关闭双面板时退出行内重命名，防 r-editing-index 残留锁死 InputOverlay
                        ui.invoke_clear_editing();
                    }
                }
                "live-filter" => st.set_set_live_filter(val),
                "search-subfolders" => st.set_set_search_subfolders(val),
                "case-sensitive" => st.set_set_case_sensitive(val),
                "background-index" => st.set_set_background_index(val),
                "context-menu-system" => st.set_set_context_menu_system(val),
                _ => {}
            }
            if reload {
                load_current(&ui, &c);
            }
        }
    });

    // —— 字符串设置（下拉选项 + 主题模式/主题色）——
    let w = ui.as_weak();
    let c = core.clone();
    state.on_set_string(move |key, val| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                let s = &mut core.config.settings;
                match key.as_str() {
                    "theme-mode" => s.theme_mode = val.to_string(),
                    "accent" => s.accent = val.to_string(),
                    "language" => s.language = lang_canon(val.as_str()).into(),
                    "startup-open" => s.startup_open = startup_canon(val.as_str()).into(),
                    "icon-source" => s.icon_source = icon_canon(val.as_str()).into(),
                    "default-view" => s.default_view = view_canon(val.as_str()).into(),
                    "default-sort" => s.default_sort = sort_canon(val.as_str()).into(),
                    "new-tab-loc" => s.new_tab_location = newtab_canon(val.as_str()).into(),
                    "split-ratio" => s.split_ratio = split_canon(val.as_str()).into(),
                    "index-location" => s.index_location = index_canon(val.as_str()).into(),
                    _ => {}
                }
                core.config.save();
            }
            let theme = ui.global::<Theme>();
            let st = ui.global::<AppState>();
            match key.as_str() {
                "theme-mode" => theme.set_theme_mode(val),
                "accent" => theme.set_accent_key(val),
                "language" => st.set_set_language(val),
                "startup-open" => st.set_set_startup_open(val),
                "icon-source" => st.set_set_icon_source(val),
                "default-view" => st.set_set_default_view(val),
                "default-sort" => st.set_set_default_sort(val),
                "new-tab-loc" => st.set_set_new_tab_loc(val),
                "split-ratio" => st.set_set_split_ratio(val),
                "index-location" => st.set_set_index_location(val),
                _ => {}
            }
            // 图标来源切换：重载当前目录以按新策略重新拉取系统图标/缩略图，
            // 并重建侧边栏导航模型——否则侧边栏图标（快速访问/驱动器等）要等下次导航才会更新
            if key.as_str() == "icon-source" {
                load_current(&ui, &c);
                let c2 = c.borrow();
                let path = c2.active_tab().history.current().clone();
                st.set_nav_items(ui_bridge::build_sidebar(
                    &path,
                    &c2.collapsed_sections,
                    &c2.config,
                ));
            }
            // 分隔比例预设：立即应用到双面板左侧占比并持久化
            if key.as_str() == "split-ratio" {
                let ratio = match c.borrow().config.settings.split_ratio.as_str() {
                    "40" => 0.4,
                    "60" => 0.6,
                    _ => 0.5,
                };
                st.set_dual_ratio(ratio);
                let mut core = c.borrow_mut();
                core.config.layout.dual_ratio = ratio;
                core.config.save();
            }
        }
    });

    // —— 数值设置（半透明不透明度 / 磨砂强度）——
    let w = ui.as_weak();
    let c = core.clone();
    state.on_set_number(move |key, val| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                match key.as_str() {
                    "opacity" => core.config.settings.opacity = val,
                    "blur" => core.config.settings.blur = val,
                    // 图标缩放（Ctrl+滚轮）：夹紧到 0.7..2.0 并持久化
                    "icon-scale" => core.config.settings.icon_scale = val.clamp(0.7, 2.0),
                    _ => {}
                }
                core.config.save();
            }
            let theme = ui.global::<Theme>();
            match key.as_str() {
                "opacity" => theme.set_opacity_level(val),
                "blur" => {
                    theme.set_blur_level(val);
                    // 模糊度连续映射到亚克力磨砂浓度（仅 Windows 生效），不再是二元开关
                    #[cfg(windows)]
                    apply_window_material(&ui);
                }
                _ => {}
            }
        }
    });

    // -- 取色板：点击自定义主题色色块时，打开 Windows 原生 ChooseColor 对话框 --
    let w = ui.as_weak();
    let c = core.clone();
    state.on_request_color_pick(move || {
        let Some(ui) = w.upgrade() else { return };
        let theme = ui.global::<Theme>();
        let current = theme.get_accent_custom();

        // 取主窗口 HWND 作为对话框父窗口
        let mut picked: Option<slint::Color> = None;
        ui.window().with_winit_window(|ww| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let Ok(handle) = ww.window_handle() else {
                return;
            };
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                let hwnd = isize::from(h.hwnd);
                picked = open_color_picker(current, hwnd);
            }
        });

        if let Some(color) = picked {
            // 更新 Theme
            theme.set_accent_custom(color);
            theme.set_accent_key("custom".into());
            // 持久化
            {
                let mut core = c.borrow_mut();
                core.config.settings.accent = "custom".into();
                core.config.settings.accent_custom = color_to_hex(color);
                core.config.save();
            }
        }
    });
}

// ─── 视图与搜索 ───

fn bind_view_and_search(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    let w = ui.as_weak();
    let c = core.clone();
    state.on_set_view(move |mode| {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            // 双面板下视图切换只作用于活动面板（左右独立），不统一切换
            if st.get_dual_pane() && st.get_active_pane().as_str() == "right" {
                st.set_r_view_mode(mode);
            } else {
                st.set_view_mode(mode);
            }
            save_current_folder_layout(&ui, &c);
        }
    });

    // 双面板开关：与布局正交，仅翻转 dual-pane 布尔；开启时刷新右侧面板内容
    let w = ui.as_weak();
    let c = core.clone();
    state.on_toggle_dual(move || {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let on = !st.get_dual_pane();
            st.set_dual_pane(on);
            if on {
                // 开启双面板时右面板视图初始与左一致
                st.set_r_view_mode(st.get_view_mode());
            }
            save_current_folder_layout(&ui, &c);
            if on {
                load_right(&ui, &c);
            } else {
                // 关闭双面板时退出行内重命名：RightPane 卸载后 Escape/Enter
                // 无法触达，残留的 r-editing-index 会永久禁用 InputOverlay
                ui.invoke_clear_editing();
            }
        }
    });

    // 交换左右面板：活动标签会话与右侧独立面板整体互换（含导航历史/排序/选中）
    let w = ui.as_weak();
    let c = core.clone();
    state.on_swap_panes(move || {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                // 设置标签页不参与交换（右面板无法承载设置界面）
                if core.active_tab().kind != app::TabKind::Files {
                    return;
                }
                let active = core.active;
                // 显式重借用出 &mut AppCore：RefMut 每次字段访问都经 DerefMut，
                // 直接对两个字段取 &mut 会被判成对整个结构的双重可变借用
                let core = &mut *core;
                std::mem::swap(&mut core.tabs[active], &mut core.right_pane);
            }
            load_current(&ui, &c);
            load_right(&ui, &c);
        }
    });

    // 双面板比例拖拽结束：持久化左侧占比
    let w = ui.as_weak();
    let c = core.clone();
    state.on_save_dual_ratio(move |ratio| {
        if let Some(_ui) = w.upgrade() {
            let mut core = c.borrow_mut();
            core.config.layout.dual_ratio = ratio.clamp(0.15, 0.85);
            core.config.save();
        }
    });

    // 打开「此电脑」属性（系统信息页）
    state.on_open_computer_properties(move || {
        let _ = std::process::Command::new("explorer.exe")
            .arg("ms-settings:about")
            .spawn();
    });

    let w = ui.as_weak();
    state.on_set_omni_mode(move |mode| {
        if let Some(ui) = w.upgrade() {
            ui.global::<AppState>().set_omni_mode(mode);
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_do_search(move |text| {
        if let Some(ui) = w.upgrade() {
            // 清空搜索：重新读取当前目录（深层搜索可能已把 entries 换成索引结果）
            if text.is_empty() {
                {
                    let mut core = c.borrow_mut();
                    core.active_tab_mut().search.clear();
                }
                ui.global::<AppState>().set_search_text(text);
                load_current(&ui, &c);
                return;
            }
            // 深层搜索：开启「搜索子文件夹」且当前为真实目录时，用文件名索引
            // 在当前目录范围内查找；索引未建立时回退到当前目录过滤
            let deep_dir = {
                let core = c.borrow();
                let tab = core.active_tab();
                let path = tab.history.current().clone();
                let is_virtual = fs::virtualfs::is_virtual(&path.to_string_lossy());
                if core.config.settings.search_subfolders && !is_virtual {
                    Some((path, core.config.settings.case_sensitive))
                } else {
                    None
                }
            };
            if let Some((dir, case_sensitive)) = deep_dir {
                if let Some(results) = fs::index::search(&dir, text.as_str(), case_sensitive, 1000)
                {
                    let n = results.len();
                    {
                        let mut core = c.borrow_mut();
                        let tab = core.active_tab_mut();
                        tab.entries = results;
                        tab.search = text.to_string();
                        tab.rebuild();
                    }
                    let st = ui.global::<AppState>();
                    st.set_search_text(text);
                    st.set_status_text(format!("深层搜索：含子文件夹共 {} 个匹配项", n).into());
                    ui_bridge::push_entries(&ui, &c.borrow());
                    return;
                }
                ui.global::<AppState>().set_status_text(
                    "尚未建立索引，已在当前目录过滤;可在 设置 > 搜索与索引 中重建索引".into(),
                );
            }
            {
                let mut core = c.borrow_mut();
                let tab = core.active_tab_mut();
                tab.search = text.to_string();
                tab.rebuild();
            }
            ui.global::<AppState>().set_search_text(text);
            ui_bridge::push_entries(&ui, &c.borrow());
        }
    });

    let w = ui.as_weak();
    state.on_toggle_details(move || {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            st.set_show_details(!st.get_show_details());
        }
    });

    let w = ui.as_weak();
    let c = core.clone();
    state.on_sort_by(move |key| {
        if let Some(ui) = w.upgrade() {
            {
                let mut core = c.borrow_mut();
                let tab = core.active_tab_mut();
                if tab.sort_key == key.as_str() {
                    tab.sort_asc = !tab.sort_asc;
                } else {
                    tab.sort_key = key.to_string();
                    tab.sort_asc = true;
                }
                tab.rebuild();
            }
            ui_bridge::push_entries(&ui, &c.borrow());
        }
    });
}

// ─── 哈希 ───

fn bind_hash(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();
    let c = core.clone();
    let w = ui.as_weak();
    state.on_compute_hash(move |algo| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let core = c.borrow();
            let tab = core.pane(right);
            if let Some(fi) = tab.first_selected() {
                if let Some(e) = tab.entry_at(fi) {
                    if !e.is_dir {
                        return ui_bridge::hash_to_shared(Path::new(&e.path), algo.as_str());
                    }
                }
            }
        }
        "请先选择一个文件".into()
    });

    // 哈希校验：按期望值长度自动识别算法并与选中文件比对，结果回填 AppState
    let c = core.clone();
    let w = ui.as_weak();
    state.on_verify_hash(move |expected| {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let st = ui.global::<AppState>();
            let (text, status) = {
                let core = c.borrow();
                let tab = core.pane(right);
                tab.first_selected()
                    .and_then(|fi| tab.entry_at(fi))
                    .filter(|e| !e.is_dir)
                    .map(|e| ui_bridge::verify_result(Path::new(&e.path), expected.as_str()))
                    .unwrap_or_else(|| ("请先选择一个文件".into(), 3))
            };
            st.set_verify_result(text);
            st.set_verify_status(status);
        }
    });

    // 复制文本到剪贴板（复制哈希值）
    state.on_copy_text(move |text| {
        fs::clipboard::set_text(text.as_str());
    });

    // 「打开方式 - 更改」：弹出系统「打开方式」对话框。
    // 对话框模态返回后用户可能已更改默认应用：文件类型关联图标与「打开方式」
    // 程序名都已变化，清空图标缓存并刷新视图，立即反映新默认应用。
    let c = core.clone();
    let w = ui.as_weak();
    state.on_open_with_dialog(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            let path = {
                let core = c.borrow();
                let tab = core.pane(right);
                tab.first_selected()
                    .and_then(|fi| tab.entry_at(fi))
                    .filter(|e| !e.is_dir)
                    .map(|e| e.path.clone())
            };
            if let Some(path) = path {
                show_open_with_dialog(&ui, &path);
                // open_with_dialog 内部已广播 SHCNE_ASSOCCHANGED 通知 Shell 刷新关联图标。
                // 立即清缓存+重载一次；再延迟补刷一次--Shell 处理关联广播有一定延迟，
                // 首次重提取可能仍取到旧图标，延迟重载确保新图标实时显示（无需重开文件夹）。
                fs::thumbnail::clear_all_caches();
                reload_active_pane(&ui, &c);
                let w2 = ui.as_weak();
                let c2 = c.clone();
                slint::Timer::single_shot(std::time::Duration::from_millis(450), move || {
                    if let Some(ui) = w2.upgrade() {
                        fs::thumbnail::clear_all_caches();
                        reload_active_pane(&ui, &c2);
                        // 详情栏「打开方式」程序名也随之刷新
                        ui_bridge::update_selection(&ui, &c2.borrow());
                    }
                });
            }
        }
    });

    // 空格键 Quick Look：填充预览内容并打开独立预览窗口（按活动面板取选中项）
    let c = core.clone();
    let w = ui.as_weak();
    state.on_open_quicklook(move || {
        if let Some(ui) = w.upgrade() {
            let right = toolbar_routes_right(&ui);
            if ui_bridge::fill_quicklook(&ui, &c.borrow(), right) {
                let preview_generation = next_preview_generation();
                let st = ui.global::<AppState>();
                st.set_quicklook_open(true);
                st.set_ql_video_fullscreen(false);
                let path = st.get_sel_path().to_string();
                // 先把内容推到独立窗口并显示，原生子窗口随后按其客户区定位
                if !show_preview_window(&ui, &path, preview_generation) {
                    st.set_quicklook_open(false);
                    return;
                }
                if st.get_ql_kind() == 4 {
                    // 视频：在预览内容区之上启动 Media Foundation 子窗口播放（含音频）
                    fs::web_preview::stop();
                    if !path.is_empty() {
                        start_video_preview(&ui, &path);
                    }
                } else if st.get_ql_can_render() && st.get_ql_web_mode() {
                    // Markdown/HTML/PHP：默认渲染视图（WebView2 子窗口覆盖内容区）
                    fs::video_preview::stop();
                    if !path.is_empty() {
                        start_web_preview(&ui, &path);
                    }
                } else {
                    fs::video_preview::stop();
                    fs::web_preview::stop();
                }
                if st.get_ql_kind() == 3 && !path.is_empty() {
                    let show_hidden = c.borrow().config.settings.show_hidden;
                    let show_protected = c.borrow().config.settings.show_protected;
                    let ql_path = path.clone();
                    let w_summary = ui.as_weak();
                    std::thread::spawn(move || {
                        let (dirs, files, size) = fs::preview::folder_summary(
                            Path::new(&ql_path),
                            show_hidden,
                            show_protected,
                        );
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = w_summary.upgrade() {
                                let state = ui.global::<AppState>();
                                if state.get_quicklook_open() && state.get_sel_path() == ql_path.as_str() {
                                    let info = format!(
                                        "包含 {} 个子文件夹、{} 个文件\n文件总大小 {}",
                                        dirs,
                                        files,
                                        fs::metadata::human_size(size)
                                    );
                                    state.set_ql_info(info.clone().into());
                                    // 同步到独立预览窗口（统计为后台线程回填）
                                    if let Some(pw) = preview_host::window() {
                                        preview_host::set_info(&pw, &info);
                                    }
                                }
                            }
                        });
                    });
                }
            }
        }
    });

    // 关闭 Quick Look：隐藏独立预览窗口，并停止可能进行中的视频播放与网页渲染
    let w_close = ui.as_weak();
    state.on_close_quicklook(move || {
        if let Some(ui) = w_close.upgrade() {
            fs::video_preview::stop();
            stop_video_timer_impl();
            fs::web_preview::stop();
            let st = ui.global::<AppState>();
            if st.get_ql_video_fullscreen() {
                set_quicklook_window_fullscreen(false);
            }
            st.set_ql_video_fullscreen(false);
            st.set_quicklook_open(false);
            #[cfg(windows)]
            clear_preview_window_icon();
            preview_host::hide();
        }
    });

    // 视频全屏：复用当前播放器，把独立预览窗口切换为当前显示器的无边框全屏。
    // 主窗口不参与，因此无需重新应用其无边框样式。
    let w_fs = ui.as_weak();
    state.on_ql_toggle_video_fullscreen(move || {
        if let Some(ui) = w_fs.upgrade() {
            let st = ui.global::<AppState>();
            if !st.get_quicklook_open() || st.get_ql_kind() != 4 {
                return;
            }
            let fullscreen = !st.get_ql_video_fullscreen();
            st.set_ql_video_fullscreen(fullscreen);
            // 同步按钮图标态到预览窗口
            if let Some(pw) = preview_host::window() {
                pw.global::<PreviewState>().set_video_fullscreen(fullscreen);
            }
            set_quicklook_window_fullscreen(fullscreen);
            schedule_video_repositions(&ui, &[40, 180]);
            // 原点已变，立即触发一次重定位（全屏切换后控制条需重新对齐）
            LAST_PREVIEW_ORIGIN.with(|c| c.set((i32::MIN, i32::MIN)));
            reposition_video_if_moved(&ui);
        }
    });

    // 播放 / 暂停切换
    let w_play = ui.as_weak();
    state.on_ql_video_toggle_play(move || {
        let paused = fs::video_preview::toggle_play();
        if let Some(ui) = w_play.upgrade() {
            ui.global::<AppState>().set_ql_video_paused(paused);
        }
    });

    // 重新播放：跳回开头并确保处于播放态
    let w_replay = ui.as_weak();
    state.on_ql_video_replay(move || {
        fs::video_preview::seek_100ns(0);
        if let Some(ui) = w_replay.upgrade() {
            let st = ui.global::<AppState>();
            st.set_ql_video_position(0);
            if st.get_ql_video_paused() {
                let paused = fs::video_preview::toggle_play();
                st.set_ql_video_paused(paused);
            }
        }
    });

    // 静音切换
    let w_mute = ui.as_weak();
    state.on_ql_video_toggle_mute(move || {
        let muted = !fs::video_preview::is_muted();
        fs::video_preview::set_muted(muted);
        if let Some(ui) = w_mute.upgrade() {
            ui.global::<AppState>().set_ql_video_muted(muted);
        }
    });

    // 进度条拖动 / 点击跳转：比例 0.0..1.0 → 100ns 位置
    let w = ui.as_weak();
    state.on_ql_video_seek(move |ratio| {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let dur = st.get_ql_video_duration();
            if dur <= 0 {
                return;
            }
            let r = ratio.clamp(0.0, 1.0);
            let sec = (dur as f64 * r as f64) as i64;
            // 秒 -> 100ns
            fs::video_preview::seek_100ns(sec * 10_000_000);
            st.set_ql_video_position(sec as i32);
        }
    });

    // 渲染/源码视图切换（Markdown/HTML/PHP）：启停 WebView2 子层。
    // 预览已是独立窗口，尺寸由用户/初始值决定，无需按视图重算卡片。
    let w = ui.as_weak();
    state.on_ql_set_web_mode(move |on| {
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            if !st.get_ql_can_render() || st.get_ql_web_mode() == on {
                return;
            }
            st.set_ql_web_mode(on);
            // 同步到预览窗口，让其切换渲染占位层与源码文本层
            if let Some(pw) = preview_host::window() {
                pw.global::<PreviewState>().set_web_mode(on);
            }
            if on {
                let path = st.get_sel_path().to_string();
                if !path.is_empty() {
                    start_web_preview(&ui, &path);
                }
            } else {
                fs::web_preview::stop();
            }
        }
    });
}

/// 预览窗口内容区的物理像素矩形（相对预览窗口客户区左上角）。
/// 与 preview_window.slint 布局约定一致：头部 60px、底部提示栏 38px，
/// 中间为内容区；原生视频/网页子窗口只覆盖内容区。
/// 视频按分辨率在内容区内等比居中，其余类型铺满内容区。
#[cfg(windows)]
fn preview_content_rect_phys(ui: &MainWindow) -> Option<(i32, i32, i32, i32)> {
    let pw = preview_host::window()?;
    let st = ui.global::<AppState>();
    let mut out = None;
    pw.window().with_winit_window(|winit_window| {
        let scale = winit_window.scale_factor() as f32;
        let size = winit_window.inner_size();
        let (win_w, win_h) = (size.width as f32, size.height as f32);
        let header = PREVIEW_HEADER_H * scale;
        let footer = PREVIEW_FOOTER_H * scale;
        let content_h = (win_h - header - footer).max(1.0);
        let mut x = 0.0_f32;
        let mut y = header;
        let mut width = win_w;
        let mut height = content_h;
        if st.get_ql_kind() == 4 {
            let vw = st.get_ql_img_w().max(0) as f32;
            let vh = st.get_ql_img_h().max(0) as f32;
            // 分辨率已知：等比适配并在内容区内居中，避免画面被拉伸。
            if vw > 0.0 && vh > 0.0 {
                let fit = (win_w / vw).min(content_h / vh);
                width = vw * fit;
                height = vh * fit;
                x += (win_w - width) / 2.0;
                y += (content_h - height) / 2.0;
            }
        }
        out = Some((x as i32, y as i32, width as i32, height as i32));
    });
    out
}

#[cfg(not(windows))]
fn preview_content_rect_phys(_ui: &MainWindow) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 预览窗口头部/底部高度（逻辑像素）——与 preview_window.slint 布局一致
const PREVIEW_HEADER_H: f32 = 60.0;
const PREVIEW_FOOTER_H: f32 = 38.0;

/// 切换预览窗口无边框全屏（视频全屏作用于预览窗口，不再影响主窗口）。
fn set_quicklook_window_fullscreen(fullscreen: bool) {
    if let Some(pw) = preview_host::window() {
        pw.window().with_winit_window(|window| {
            window.set_fullscreen(if fullscreen {
                Some(winit::window::Fullscreen::Borderless(
                    window.current_monitor(),
                ))
            } else {
                None
            });
        });
    }
}

/// 全屏切换后窗口尺寸异步更新，按固定延迟重对齐视频子窗口。
fn schedule_video_repositions(ui: &MainWindow, delays_ms: &[u64]) {
    for &delay in delays_ms {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<AppState>();
                if !st.get_quicklook_open() || st.get_ql_kind() != 4 {
                    return;
                }
                let rect = if st.get_ql_video_fullscreen() {
                    quicklook_fullscreen_rect_phys()
                } else {
                    preview_content_rect_phys(&ui)
                };
                if let Some(rect) = rect {
                    fs::video_preview::reposition(rect);
                }
            }
        });
    }
}

/// 预览窗口客户区全屏矩形（物理像素）。
#[cfg(windows)]
fn quicklook_fullscreen_rect_phys() -> Option<(i32, i32, i32, i32)> {
    let pw = preview_host::window()?;
    let mut out = None;
    pw.window().with_winit_window(|window| {
        let size = window.inner_size();
        // 全屏时播放器子窗口覆盖整个预览窗口客户区。MFPlay 会在该矩形内自行
        // 按视频比例留黑边；控制栏也以窗口矩形定位，始终贴住窗口最底部。
        out = Some((0, 0, size.width as i32, size.height as i32));
    });
    out
}

#[cfg(not(windows))]
fn quicklook_fullscreen_rect_phys() -> Option<(i32, i32, i32, i32)> {
    None
}

/// 取预览窗口 HWND（isize；窗口未就绪返回 0）
#[cfg(windows)]
fn preview_hwnd() -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Some(pw) = preview_host::window() else {
        return 0;
    };
    let mut hwnd_isize: isize = 0;
    pw.window().with_winit_window(|winit_window| {
        if let Ok(handle) = winit_window.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                hwnd_isize = isize::from(h.hwnd);
            }
        }
    });
    hwnd_isize
}

/// 创建/复用独立预览窗口，推送内容并显示。返回是否成功打开。
/// 窗口在显示前已按内容尺寸定型，显示后仅需等待图片/视频解码完成。
fn show_preview_window(ui: &MainWindow, path: &str, preview_generation: u64) -> bool {
    let close_weak = ui.as_weak();
    let web_weak = ui.as_weak();
    let fs_weak = ui.as_weak();
    let created = preview_host::ensure_window(
        move || {
            // 预览窗口内触发的关闭：走主窗口同一套清理逻辑
            if let Some(ui) = close_weak.upgrade() {
                ui.global::<AppState>().invoke_close_quicklook();
            }
        },
        move |on| {
            if let Some(ui) = web_weak.upgrade() {
                ui.global::<AppState>().invoke_ql_set_web_mode(on);
            }
        },
        move || {
            if let Some(ui) = fs_weak.upgrade() {
                ui.global::<AppState>().invoke_ql_toggle_video_fullscreen();
            }
        },
    );
    let pw = match created {
        Ok(pw) => pw,
        Err(e) => {
            eprintln!("[preview] 创建预览窗口失败：{e}");
            ui.global::<AppState>()
                .set_status_text(format!("无法打开预览窗口：{e}").into());
            return false;
        }
    };
    preview_host::sync_theme(ui, &pw);
    preview_host::push_content(ui, &pw, path);

    // 每次打开都按内容尺寸重算并居中（图片/视频已在 ui_bridge 中探测尺寸）
    let kind = ui.global::<AppState>().get_ql_kind();
    center_and_size_preview(ui, &pw, kind);

    // 标题栏图标：用文件自身的系统图标替代默认应用图标
    set_preview_window_icon(path);

    if pw.show().is_err() {
        return false;
    }

    // 安装 WM_MOVE 子类：拖动窗口时同步移动视频控制栏（避免拖动延迟）
    install_preview_move_handler();

    // 置顶到主窗口之上并取得焦点，使空格/Esc 直接作用于预览
    focus_preview_window(&pw);

    // 图片：后台解码位图（避免大图阻塞 UI 线程）
    if kind == 1 {
        decode_image_async(ui, &pw, path, preview_generation);
    } else if kind != 4 {
        // 文本/归档/文件夹/信息等内容已同步就绪，下一帧关闭加载动画。
        // 视频（kind==4）由 on_video_size_ready 关闭。
        // 加一帧延迟：让本帧的加载占位先绘制出来，再换内容，避免透明窗口闪现。
        let pw_weak = pw.as_weak();
        let ui_weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
            if let (Some(pw), Some(ui)) = (pw_weak.upgrade(), ui_weak.upgrade()) {
                if ui.global::<AppState>().get_quicklook_open() && preview_generation_is_current(preview_generation) {
                    preview_host::set_loading(&pw, false);
                    ui.global::<AppState>().set_ql_loading(false);
                }
            }
        });
    }

    true
}

/// 按内容类型计算预览窗口尺寸并居中到主窗口所在显示器，每次打开时调用。
/// 图片/视频按真实分辨率适配当前显示器可用区，其余类型用各自的经验值。
/// 窗口在显示前完成定位和定型，避免先显示再调整的闪烁。
fn center_and_size_preview(ui: &MainWindow, pw: &PreviewWindow, kind: i32) {
    let st = ui.global::<AppState>();
    let chrome = PREVIEW_HEADER_H + PREVIEW_FOOTER_H;

    // 获取主窗口所在显示器的工作区（物理像素）和 DPI
    #[cfg(windows)]
    let (work_w, work_h, work_x, work_y, dpi) = {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
        use windows::Win32::Foundation::HWND;

        let work_area = ui.window().with_winit_window(|w| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let handle = w.window_handle().ok()?;
            let hwnd = match handle.as_raw() {
                RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
                _ => return None,
            };

            unsafe {
                let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                    let work = mi.rcWork;
                    let scale = w.scale_factor() as f32;
                    Some((
                        (work.right - work.left) as f32,
                        (work.bottom - work.top) as f32,
                        work.left as f32,
                        work.top as f32,
                        (scale * 96.0) as u32,
                    ))
                } else {
                    None
                }
            }
        }).flatten();

        work_area.unwrap_or_else(|| {
            // 后备：使用 winit 提供的显示器信息
            ui.window().with_winit_window(|w| {
                w.current_monitor()
                    .map(|m| {
                        let size = m.size();
                        let pos = m.position();
                        let scale = w.scale_factor() as f32;
                        // 预留任务栏空间（通常在底部 48 逻辑像素）
                        (
                            size.width as f32,
                            (size.height as f32) - (48.0 * scale),
                            pos.x as f32,
                            pos.y as f32,
                            (scale * 96.0) as u32,
                        )
                    })
            }).flatten().unwrap_or((1920.0, 1080.0 - 48.0, 0.0, 0.0, 96))
        })
    };

    #[cfg(not(windows))]
    let (work_w, work_h, work_x, work_y, dpi) = {
        ui.window().with_winit_window(|w| {
            w.current_monitor()
                .map(|m| {
                    let size = m.size();
                    let pos = m.position();
                    let scale = w.scale_factor() as f32;
                    // 预留任务栏/停靠栏空间
                    (
                        size.width as f32,
                        (size.height as f32) - (48.0 * scale),
                        pos.x as f32,
                        pos.y as f32,
                        (scale * 96.0) as u32,
                    )
                })
        }).flatten().unwrap_or((1920.0, 1080.0 - 48.0, 0.0, 0.0, 96))
    };

    // 逻辑可用区：留出 160 逻辑像素边距，转换到目标 DPI
    let scale = dpi as f32 / 96.0;
    let max_w = ((work_w / scale) - 160.0).clamp(420.0, 2000.0);
    let max_h = ((work_h / scale) - 160.0).clamp(320.0, 1400.0);

    let (cw, ch) = match kind {
        // 图片：按原生分辨率适配（放不下等比缩小，小图不放大）
        1 => {
            let iw = st.get_ql_img_w().max(0) as f32;
            let ih = st.get_ql_img_h().max(0) as f32;
            if iw > 0.0 && ih > 0.0 {
                let fit = (max_w / iw).min((max_h - chrome) / ih).min(1.0);
                ((iw * fit).max(420.0), (ih * fit).max(280.0))
            } else {
                (860.0_f32.min(max_w), 560.0_f32.min(max_h - chrome))
            }
        }
        // 视频：按探测到的分辨率计算，失败时回退 16:9
        4 => {
            let vw = st.get_ql_img_w().max(0) as f32;
            let vh = st.get_ql_img_h().max(0) as f32;
            if vw > 0.0 && vh > 0.0 {
                let fit = (max_w / vw).min((max_h - chrome) / vh).min(1.0);
                ((vw * fit).max(420.0), (vh * fit).max(280.0))
            } else {
                (880.0_f32.min(max_w), 495.0_f32.min(max_h - chrome))
            }
        }
        // 归档树：偏高，便于展开层级后浏览
        5 => (760.0_f32.min(max_w), 620.0_f32.min(max_h - chrome)),
        // 文本/网页
        2 => (900.0_f32.min(max_w), 660.0_f32.min(max_h - chrome)),
        // 文件夹/信息：紧凑
        _ => (520.0_f32.min(max_w), 420.0_f32.min(max_h - chrome)),
    };

    let logical_size = slint::LogicalSize::new(cw.max(420.0), ch + chrome);
    pw.window().set_size(logical_size);

    // 居中：在工作区内居中显示，考虑 Chrome（标题栏 + 间隙）
    let work_w_logical = work_w / scale;
    let work_h_logical = work_h / scale;
    let centered_x = work_x / scale + (work_w_logical - logical_size.width) / 2.0;
    let centered_y = work_y / scale + (work_h_logical - logical_size.height) / 2.0;

    pw.window().set_position(slint::LogicalPosition::new(
        centered_x,
        centered_y,
    ));

    #[cfg(not(windows))]
    {
        // 非 Windows：Slint 的 set_position 在某些平台可能不可靠，至少先设尺寸
        let _ = (work_x, work_y, work_w, work_h, scale);
    }
}

thread_local! {
    static PREVIEW_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_preview_generation() -> u64 {
    PREVIEW_GENERATION.with(|generation| {
        let next = generation.get().wrapping_add(1);
        generation.set(next);
        next
    })
}

fn preview_generation_is_current(generation: u64) -> bool {
    PREVIEW_GENERATION.with(|current| current.get() == generation)
}

/// 图片后台解码：大图解码可能耗时数百毫秒，放后台线程避免卡住空格键。
/// 窗口已在显示前按真实尺寸定型，这里只回填像素数据。
fn decode_image_async(ui: &MainWindow, pw: &PreviewWindow, path: &str, preview_generation: u64) {
    let path = path.to_string();
    let pw_weak = pw.as_weak();
    let ui_weak = ui.as_weak();

    std::thread::spawn(move || {
        #[cfg(windows)]
        let icon_opt = crate::fs::thumbnail::extract(&path, crate::ui_bridge::QL_IMAGE_SIZE)
            .map(|(pixels, w, h)| crate::fs::thumbnail::IconPixels { pixels, w, h });

        #[cfg(not(windows))]
        let icon_opt: Option<crate::fs::thumbnail::IconPixels> = None;

        slint::invoke_from_event_loop(move || {
            let Some(pw) = pw_weak.upgrade() else { return };
            let Some(ui) = ui_weak.upgrade() else { return };
            if !preview_generation_is_current(preview_generation)
                || !ui.global::<AppState>().get_quicklook_open()
            {
                return;
            }

            preview_host::set_loading(&pw, false);
            ui.global::<AppState>().set_ql_loading(false);

            if let Some(icon) = icon_opt {
                let image = crate::ui_bridge::image_from(&icon);
                preview_host::set_image(&pw, image);
            }
        })
        .ok();
    });
}

/// 安装 WM_MOVE 和 WM_SIZE 子类：拖动/调整预览窗口时同步移动视频控制栏和画面
#[cfg(windows)]
fn install_preview_move_handler() {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{
        DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass,
    };
    use windows::Win32::UI::WindowsAndMessaging::{WM_MOVE, WM_NCDESTROY, WM_SIZE};

    let hwnd = preview_hwnd();
    if hwnd == 0 {
        return;
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _uid: usize,
        _data: usize,
    ) -> LRESULT {
        match msg {
            WM_MOVE => {
                // 先让 winit 处理（触发 Slint 的窗口移动事件）
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                // 同步移动控制栏到窗口新位置
                crate::fs::video_preview::sync_controls();
                result
            }
            WM_SIZE => {
                // 先让 winit 处理（触发 Slint 的窗口尺寸变化）
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                // 重新计算视频内容区矩形并重定位播放器子窗口
                if let Some(pw) = crate::preview_host::window() {
                    // 直接从预览窗口计算内容区物理矩形
                    let rect_opt = pw.window().with_winit_window(|window| {
                        let scale = window.scale_factor();
                        let size = window.inner_size();

                        // 计算内容区：去掉头部和底部边距
                        let chrome_h = ((PREVIEW_HEADER_H + PREVIEW_FOOTER_H) * scale as f32) as i32;
                        let content_h = (size.height as i32).saturating_sub(chrome_h).max(0);

                        Some((0, (PREVIEW_HEADER_H * scale as f32) as i32, size.width as i32, content_h))
                    });

                    if let Some(Some(rect)) = rect_opt {
                        crate::fs::video_preview::reposition(rect);
                    }
                }
                result
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), 0);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }

    unsafe {
        let _ = SetWindowSubclass(
            HWND(hwnd as *mut _),
            Some(subclass_proc),
            0, // subclass ID
            0, // ref data
        );
    }
}

#[cfg(not(windows))]
fn install_preview_move_handler() {}

#[cfg(not(windows))]
fn preview_hwnd() -> isize {
    0
}

/// 把预览窗口提到前台并聚焦（Windows 下用 SetForegroundWindow 确保键盘焦点）。
/// 关键：必须真正取得键盘焦点，否则空格/Esc 仍会送到主窗口而非预览窗口。
fn focus_preview_window(pw: &PreviewWindow) {
    pw.window().with_winit_window(|w| {
        w.focus_window();
    });
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, SetForegroundWindow, ShowWindow, SW_SHOWNORMAL,
        };
        use windows_sys::Win32::System::Threading::{
            AttachThreadInput, GetCurrentThreadId,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

        let hwnd = preview_hwnd() as HWND;
        if hwnd.is_null() {
            return;
        }
        unsafe {
            // 先确保窗口可见且未最小化
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            // AttachThreadInput 把当前前台窗口的线程与本线程输入队列挂接，
            // 这样 SetFocus 才能跨线程把焦点设到预览窗口。否则另一线程拥有
            // 前台焦点时，SetFocus 会静默失败——这是空格/Esc 关不上的根因。
            let mut fore_thread = 0u32;
            let _ = GetWindowThreadProcessId(hwnd, &mut fore_thread);
            let cur_thread = GetCurrentThreadId();
            let attached = if fore_thread != 0 && fore_thread != cur_thread {
                AttachThreadInput(cur_thread, fore_thread, 1)
            } else {
                0
            };
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);
            let _ = SetFocus(hwnd);
            if attached != 0 {
                AttachThreadInput(cur_thread, fore_thread, 0);
            }
        }
    }
}

// 预览窗口标题栏图标：上次设置的大/小 HICON，切换文件前销毁旧图标防泄漏。
thread_local! {
    static PREVIEW_ICON: std::cell::Cell<(isize, isize)> = const { std::cell::Cell::new((0, 0)) };
}

/// 用文件自身的系统图标设置预览窗口标题栏图标（替代默认应用图标）。
/// 虚拟路径/提取失败时保持默认图标不动。
#[cfg(windows)]
fn set_preview_window_icon(path: &str) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_SETICON, ICON_BIG, ICON_SMALL};

    let hwnd = preview_hwnd() as HWND;
    if hwnd.is_null() || path.is_empty() {
        return;
    }
    // 虚拟路径没有系统图标，跳过（保持默认）
    if path.starts_with("device://")
        || crate::fs::virtualfs::is_virtual(path)
    {
        clear_preview_window_icon();
        return;
    }
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut big: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let mut small: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let big_ok = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut big,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    let small_ok = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut small,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    };
    // 销毁上次设置的图标
    clear_preview_window_icon();
    let mut stored = (0isize, 0isize);
    if big_ok != 0 && !big.hIcon.is_null() {
        unsafe { SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big.hIcon as isize) };
        stored.0 = big.hIcon as isize;
    }
    if small_ok != 0 && !small.hIcon.is_null() {
        unsafe { SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small.hIcon as isize) };
        stored.1 = small.hIcon as isize;
    }
    PREVIEW_ICON.with(|c| c.set(stored));
}

#[cfg(windows)]
fn clear_preview_window_icon() {
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;
    let (big, small) = PREVIEW_ICON.with(|c| c.replace((0, 0)));
    if big != 0 {
        unsafe { DestroyIcon(big as *mut _) };
    }
    if small != 0 {
        unsafe { DestroyIcon(small as *mut _) };
    }
}

#[cfg(not(windows))]
fn set_preview_window_icon(_path: &str) {}

/// 取主窗口 HWND（isize；窗口未就绪返回 0）
#[cfg(windows)]
fn main_hwnd(ui: &MainWindow) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let mut hwnd_isize: isize = 0;
    ui.window().with_winit_window(|winit_window| {
        if let Ok(handle) = winit_window.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                hwnd_isize = isize::from(h.hwnd);
            }
        }
    });
    hwnd_isize
}

// 视频预览进度轮询定时器（UI 线程）：播放期间每 250ms 读取 MFPlay 位置刷新进度条。
// 句柄存于 thread_local，关闭预览时 stop 释放。
thread_local! {
    static VIDEO_TIMER: std::cell::RefCell<Option<slint::Timer>> =
        const { std::cell::RefCell::new(None) };
    /// 控制条自动隐藏所需的鼠标活动跟踪：上次光标位置（屏幕物理像素）与最后活动时间。
    static MOUSE_LAST_POS: std::cell::Cell<(i32, i32)> =
        const { std::cell::Cell::new((i32::MIN, i32::MIN)) };
    static MOUSE_LAST_ACTIVE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// 上次重定位视频子窗口时预览窗口的屏幕原点（物理像素）。
    /// 预览窗口可被用户拖动；控制条是 WS_POPUP owned 窗口（不随 owner 移动），
    /// 因此检测到原点变化时需主动重定位画面与控制条。
    static LAST_PREVIEW_ORIGIN: std::cell::Cell<(i32, i32)> =
        const { std::cell::Cell::new((i32::MIN, i32::MIN)) };
}

/// 视频轮询中的鼠标活动检测：光标在当前预览卡片内移动就显示控制条；
/// 卡片外移动不会唤醒。静止满 5 秒后隐藏原生覆盖层。
#[cfg(windows)]
fn poll_video_controls_visibility(ui: &MainWindow) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let st = ui.global::<AppState>();
    if !st.get_quicklook_open() || st.get_ql_kind() != 4 {
        return;
    }
    // 唤醒范围按整个预览窗口客户区计算：横竖屏视频即使因等比适配留有边带，
    // 鼠标移到头部或边带也应视为该窗口内活动。
    let Some(pw) = preview_host::window() else {
        return;
    };
    let mut rect = None;
    pw.window().with_winit_window(|window| {
        let size = window.inner_size();
        rect = Some((0, 0, size.width as i32, size.height as i32));
    });
    let Some(rect) = rect else {
        return;
    };
    let hwnd = preview_hwnd() as windows_sys::Win32::Foundation::HWND;
    if hwnd.is_null() {
        return;
    }
    let mut origin = POINT {
        x: rect.0,
        y: rect.1,
    };
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        return;
    }
    let mut cursor = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return;
    }
    let inside = cursor.x >= origin.x
        && cursor.x < origin.x + rect.2
        && cursor.y >= origin.y
        && cursor.y < origin.y + rect.3;
    let now = std::time::Instant::now();
    let last = MOUSE_LAST_POS.with(|p| p.replace((cursor.x, cursor.y)));
    if inside && last != (cursor.x, cursor.y) {
        MOUSE_LAST_ACTIVE.with(|t| t.set(Some(now)));
        if !st.get_ql_controls_visible() {
            st.set_ql_controls_visible(true);
            fs::video_preview::set_controls_visible(true);
        }
        return;
    }
    if !st.get_ql_controls_visible() {
        return;
    }
    let idle = MOUSE_LAST_ACTIVE
        .with(|t| t.get())
        .map(|t0| now.duration_since(t0))
        .unwrap_or_default();
    if idle >= std::time::Duration::from_secs(5) {
        st.set_ql_controls_visible(false);
        fs::video_preview::set_controls_visible(false);
    }
}

#[cfg(not(windows))]
fn poll_video_controls_visibility(_ui: &MainWindow) {}

/// 预览窗口移动或缩放后重定位原生视频画面与控制条。
/// 控制条是 WS_POPUP owned 窗口，不随 owner 自动移动，需在此主动校正。
#[cfg(windows)]
fn reposition_video_if_moved(ui: &MainWindow) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    let st = ui.global::<AppState>();
    if !st.get_quicklook_open() || st.get_ql_kind() != 4 {
        return;
    }
    let Some(pw) = preview_host::window() else {
        return;
    };
    let hwnd = preview_hwnd() as windows_sys::Win32::Foundation::HWND;
    if hwnd.is_null() {
        return;
    }
    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        return;
    }
    let prev = LAST_PREVIEW_ORIGIN.with(|c| c.get());
    if prev == (origin.x, origin.y) {
        return;
    }
    LAST_PREVIEW_ORIGIN.with(|c| c.set((origin.x, origin.y)));
    if let Some(rect) = if st.get_ql_video_fullscreen() {
        quicklook_fullscreen_rect_phys()
    } else {
        preview_content_rect_phys(ui)
    } {
        fs::video_preview::reposition(rect);
    }
    // 让窗口自身在下面借用一次，避免「未使用」告警
    drop(pw);
}

#[cfg(not(windows))]
fn reposition_video_if_moved(_ui: &MainWindow) {}

fn start_video_timer(ui: &MainWindow) {
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(250),
        move || {
            if let Some(ui) = weak.upgrade() {
                let (cur, dur) = fs::video_preview::position();
                let st = ui.global::<AppState>();
                // 100ns -> 秒（Slint int 是 i32）
                st.set_ql_video_position((cur / 10_000_000) as i32);
                if dur > 0 {
                    st.set_ql_video_duration((dur / 10_000_000) as i32);
                }
                fs::video_preview::update_controls(
                    st.get_ql_video_position(),
                    st.get_ql_video_duration(),
                    st.get_ql_video_paused(),
                    st.get_ql_video_muted(),
                );
                // 预览窗口被拖动/缩放时，原生控制条（WS_POPUP owned 窗口）不会
                // 跟随 owner，这里在 250ms 轮询中检测原点变化并主动重定位。
                reposition_video_if_moved(&ui);
                // 预览卡片内鼠标活动 → 显示；静止 5 秒 → 隐藏
                poll_video_controls_visibility(&ui);
            }
        },
    );
    VIDEO_TIMER.with(|t| {
        *t.borrow_mut() = Some(timer);
    });
}

fn stop_video_timer_impl() {
    VIDEO_TIMER.with(|t| {
        *t.borrow_mut() = None;
    });
}

/// 启动视频预览：子窗口对齐内容区，媒体源异步加载（不阻塞 UI）；
/// 分辨率就绪后回调 on_video_size_ready 把卡片调整为视频宽高比。
#[cfg(windows)]
fn start_video_preview(ui: &MainWindow, path: &str) {
    let Some(rect) = preview_content_rect_phys(ui) else {
        return;
    };
    // 视频子窗口挂到独立预览窗口而不是主窗口
    let hwnd = preview_hwnd();
    if hwnd == 0 {
        return;
    }
    let wk = ui.as_weak();
    let play_weak = ui.as_weak();
    let replay_weak = ui.as_weak();
    let mute_weak = ui.as_weak();
    let seek_weak = ui.as_weak();
    let close_weak = ui.as_weak();
    let ok = fs::video_preview::start(
        hwnd,
        rect,
        path,
        Box::new(move |vw, vh| {
            // MFPlay 回调线程 → UI 线程
            let _ = wk.upgrade_in_event_loop(move |ui| on_video_size_ready(&ui, vw, vh));
        }),
        fs::video_preview::ControlCallbacks {
            toggle_play: Box::new(move || {
                if let Some(ui) = play_weak.upgrade() {
                    let paused = fs::video_preview::toggle_play();
                    ui.global::<AppState>().set_ql_video_paused(paused);
                }
            }),
            replay: Box::new(move || {
                fs::video_preview::seek_100ns(0);
                if fs::video_preview::is_paused() {
                    let _ = fs::video_preview::toggle_play();
                }
                if let Some(ui) = replay_weak.upgrade() {
                    let st = ui.global::<AppState>();
                    st.set_ql_video_position(0);
                    st.set_ql_video_paused(false);
                }
            }),
            mute: Box::new(move || {
                let muted = !fs::video_preview::is_muted();
                fs::video_preview::set_muted(muted);
                if let Some(ui) = mute_weak.upgrade() {
                    ui.global::<AppState>().set_ql_video_muted(muted);
                }
            }),
            seek: Box::new(move |ratio| {
                if let Some(ui) = seek_weak.upgrade() {
                    let st = ui.global::<AppState>();
                    let duration = st.get_ql_video_duration();
                    if duration > 0 {
                        let second = (duration as f32 * ratio.clamp(0.0, 1.0)) as i64;
                        fs::video_preview::seek_100ns(second * 10_000_000);
                        st.set_ql_video_position(second as i32);
                    }
                }
            }),
            close: Box::new(move || {
                if let Some(ui) = close_weak.upgrade() {
                    ui.global::<AppState>().invoke_close_quicklook();
                }
            }),
        },
    );
    if !ok {
        let st = ui.global::<AppState>();
        st.set_ql_loading(false);
        if let Some(pw) = preview_host::window() {
            preview_host::set_loading(&pw, false);
        }
        st.set_status_text("视频播放启动失败（编解码器不支持）".into());
    } else {
        // 复位进度并启动轮询定时器刷新进度条
        let st = ui.global::<AppState>();
        st.set_ql_video_position(0);
        st.set_ql_video_duration(0);
        st.set_ql_video_paused(false);
        st.set_ql_video_muted(false);
        // 打开预览时控制条默认显示，并重置鼠标活动计时（5 秒无操作后自动隐藏）
        st.set_ql_controls_visible(true);
        MOUSE_LAST_POS.with(|p| p.set((i32::MIN, i32::MIN)));
        MOUSE_LAST_ACTIVE.with(|t| t.set(Some(std::time::Instant::now())));
        start_video_timer(ui);
    }
}

#[cfg(not(windows))]
fn start_video_preview(_ui: &MainWindow, _path: &str) {}

/// 视频原生分辨率就绪：更新副标题并显示播放器（窗口尺寸已在打开前探测）
#[cfg(windows)]
fn on_video_size_ready(ui: &MainWindow, vw: u32, vh: u32) {
    let st = ui.global::<AppState>();
    // 预览可能已被关闭或切换到其它内容：忽略迟到的分辨率
    if !st.get_quicklook_open() || st.get_ql_kind() != 4 {
        return;
    }

    // 探测失败时才记录媒体真实分辨率（探测成功在 ui_bridge 已记录）。
    // 此时还需更新字幕：ui_bridge 探测失败时字幕只有「视频文件 · 30.5 MB」，
    // 现在媒体就绪拿到真实尺寸后补上分辨率前缀。
    if st.get_ql_img_w() == 0 {
        st.set_ql_img_w(vw as i32);
        st.set_ql_img_h(vh as i32);
        // 只有探测失败（宽度为 0）时才需要添加分辨率前缀
        let sub = st.get_ql_subtitle().to_string();
        let res = format!("{}×{}", vw, vh);
        if !sub.starts_with(&res) {
            let sub = format!("{} 像素 · {}", res, sub);
            st.set_ql_subtitle(sub.clone().into());
            if let Some(pw) = preview_host::window() {
                preview_host::set_subtitle(&pw, &sub);
            }
        }
    }

    // 媒体就绪：关闭加载动画，显示播放器和控制栏
    st.set_ql_loading(false);
    if let Some(pw) = preview_host::window() {
        preview_host::set_loading(&pw, false);
    }
    crate::fs::video_preview::reveal();
    if let Some(rect) = if st.get_ql_video_fullscreen() {
        quicklook_fullscreen_rect_phys()
    } else {
        preview_content_rect_phys(ui)
    } {
        fs::video_preview::reposition(rect);
    }
}

/// 启动网页渲染视图（Markdown/HTML/PHP）：WebView2 子层对齐内容区异步创建；
/// 运行时不可用时回退源码视图并提示。
#[cfg(windows)]
fn start_web_preview(ui: &MainWindow, path: &str) {
    let Some(rect) = preview_content_rect_phys(ui) else {
        return;
    };
    // WebView2 控制器挂到独立预览窗口
    let hwnd = preview_hwnd();
    if hwnd == 0 {
        return;
    }
    let dark = ui.global::<Theme>().get_dark();
    let ok = fs::web_preview::start(
        hwnd,
        rect,
        fs::web_preview::WebContent {
            path: path.to_string(),
            dark,
        },
    );
    if !ok {
        let st = ui.global::<AppState>();
        st.set_ql_web_mode(false);
        // 回退源码视图：同步到预览窗口，让其显示高亮文本层
        if let Some(pw) = preview_host::window() {
            pw.global::<PreviewState>().set_web_mode(false);
        }
        st.set_status_text("渲染视图不可用（需要 WebView2 运行时），已切换到源码视图".into());
    }
}

#[cfg(not(windows))]
fn start_web_preview(_ui: &MainWindow, _path: &str) {}

/// 弹出系统「打开方式」对话框（取宿主 HWND 后调用 SHOpenWithDialog）。
#[cfg(windows)]
fn show_open_with_dialog(ui: &MainWindow, path: &str) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let path = path.to_string();
    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd_isize = isize::from(h.hwnd);
            fs::openwith::open_with_dialog(&path, hwnd_isize);
        }
    });
}

#[cfg(not(windows))]
fn show_open_with_dialog(_ui: &MainWindow, _path: &str) {}

// ─── 标签页 ───

/// 标签栏是否已满：按当前窗口逻辑宽度与标签数，复用 Slint `tabs-full` 同公式判断。
/// 标签默认 216px、空间不足时可收缩至 112px；再放一个低于此宽度的标签会挤出窗口按钮。
/// Rust 各新建入口在新增前守卫，与 Slint 端「+」按钮隐藏 / Ctrl+T 拦截保持一致。
fn tabs_full(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) -> bool {
    let n = core.borrow().tabs.len();
    if n == 0 {
        return false;
    }
    let size = ui.window().size(); // 物理像素
    let scale = ui.window().scale_factor();
    let win_w = if scale > 0.0 {
        size.width as f32 / scale
    } else {
        size.width as f32
    };
    // 可用宽度 = 窗口宽 − 左留白8 − 品牌图标26 − 窗口按钮3×46=138
    let avail = win_w - 172.0;
    // 所需 = (N+1) 个最小宽度标签(112) + (N+1) 个间距(4) + 左内边距8 + 新建按钮26
    let need = (n + 1) as f32 * 116.0 + 34.0;
    need > avail
}

fn bind_tabs(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let state = ui.global::<AppState>();

    // 新建标签页：起始位置遵循设置「新标签页默认位置」
    let w = ui.as_weak();
    let c = core.clone();
    state.on_new_tab(move || {
        if let Some(ui) = w.upgrade() {
            // 标签栏已满（剩余宽度不足以再放一个最小宽度标签）：拒绝新增并提示
            if tabs_full(&ui, &c) {
                ui.global::<AppState>()
                    .set_status_text("标签栏已满，请先关闭部分标签页".into());
                return;
            }
            let start = {
                let core = c.borrow();
                match core.config.settings.new_tab_location.as_str() {
                    "this-pc" => PathBuf::from(fs::virtualfs::THIS_PC_PATH),
                    // 上次目录 = 当前活动标签所在目录
                    "last" => core.active_tab().history.current().clone(),
                    // quick（快速访问）与未知值回退到用户主目录
                    _ => home_start_path(),
                }
            };
            c.borrow_mut().new_tab(start);
            load_current(&ui, &c);
        }
    });

    // 关闭标签页（按下标关闭）。仅剩最后一个标签页时：
    // 若设置「关闭最后标签页时退出」已开启，则与关闭按钮同路径退出应用。
    let w = ui.as_weak();
    let c = core.clone();
    state.on_close_tab_at(move |idx| {
        if let Some(ui) = w.upgrade() {
            let (is_last, exit_on_last) = {
                let core = c.borrow();
                (core.tab_count() <= 1, core.config.settings.exit_on_last_tab)
            };
            if is_last {
                if exit_on_last {
                    // 与自绘关闭按钮/系统关闭请求一致：先保存窗口几何再退出
                    save_window_geometry(&ui, &c);
                    let _ = slint::quit_event_loop();
                }
                return;
            }
            if c.borrow_mut().close_tab(idx as usize).is_some() {
                load_current(&ui, &c);
            }
        }
    });

    // 切换标签页
    let w = ui.as_weak();
    let c = core.clone();
    state.on_switch_tab(move |idx| {
        if let Some(ui) = w.upgrade() {
            c.borrow_mut().switch_tab(idx as usize);
            apply_folder_layout(&ui, &c);
            load_current(&ui, &c);
        }
    });

    // 拖动重排标签页
    let w = ui.as_weak();
    let c = core.clone();
    state.on_move_tab(move |from, to| {
        if let Some(ui) = w.upgrade() {
            c.borrow_mut().move_tab(from as usize, to as usize);
            apply_folder_layout(&ui, &c);
            load_current(&ui, &c);
        }
    });

    // 打开（或切换到）设置标签页
    let w = ui.as_weak();
    let c = core.clone();
    state.on_open_settings_tab(move || {
        if let Some(ui) = w.upgrade() {
            // 已存在设置页则直接切换（不新增）；否则标签栏已满时拒绝新建
            let has_settings = c
                .borrow()
                .tabs
                .iter()
                .any(|t| t.kind == app::TabKind::Settings);
            if !has_settings && tabs_full(&ui, &c) {
                ui.global::<AppState>()
                    .set_status_text("标签栏已满，请先关闭部分标签页".into());
                return;
            }
            c.borrow_mut().open_settings_tab();
            load_current(&ui, &c);
        }
    });
}

// ─── 窗口控制（无边框自绘标题栏）───

/// 等 winit 窗口就绪后恢复窗口几何（物理像素），未就绪则每 40ms 重试。
/// 用 winit 原生 API 而非 Slint window().set_size：后者在 scale_factor 未确定时
/// 会把物理值当逻辑值记录，显示后按 DPI 再放大导致尺寸每次重启膨胀。
fn restore_window_geometry(
    ui: &MainWindow,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    maximized: bool,
    retries_left: u32,
) {
    let mut applied = false;
    ui.window().with_winit_window(|winit_window| {
        // scale_factor 就绪即窗口已真正创建；此时物理像素语义稳定
        if w > 200 && h > 200 {
            let _ =
                winit_window.request_inner_size(winit::dpi::PhysicalSize::new(w as u32, h as u32));
            winit_window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        if maximized {
            winit_window.set_maximized(true);
        }
        applied = true;
    });
    if applied {
        if maximized {
            ui.set_window_maximized(true);
        }
        // 恢复几何（尺寸/位置/最大化）都会触发 WM_NCCALCSIZE / FRAMECHANGED，
        // DWM 借此重算非客户区并丢弃边框延伸与亚克力策略。本函数的重试可能落在
        // schedule_window_effects 的最后一次（800ms）之后，冷启动便表现为
        // 「磨砂全丢、窗口全透明」。故几何落地后补一轮窗口效果。
        #[cfg(windows)]
        schedule_window_effects(ui, &[0, 60, 240]);
        return;
    }
    // winit 窗口尚未创建（with_winit_window 未执行闭包）：稍后重试
    if retries_left > 0 {
        let w_ui = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(40), move || {
            if let Some(ui) = w_ui.upgrade() {
                restore_window_geometry(&ui, x, y, w, h, maximized, retries_left - 1);
            }
        });
    }
}

/// 把当前窗口位置/大小/最大化状态写回配置（应用关闭或安装更新退出前调用）。
/// 最大化时仅记录标志，不覆盖已保存的常规尺寸——还原后仍回到之前的大小。
fn save_window_geometry(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    let maximized = ui.window().is_maximized();
    let mut c = core.borrow_mut();
    let lay = &mut c.config.layout;
    lay.win_maximized = maximized;
    if !maximized {
        let pos = ui.window().position();
        let size = ui.window().size();
        // 过滤异常值（最小化中关闭等场景可能拿到 0 尺寸）
        if size.width > 200 && size.height > 200 {
            lay.win_x = pos.x;
            lay.win_y = pos.y;
            lay.win_w = size.width as i32;
            lay.win_h = size.height as i32;
        }
    }
    c.config.save();
}

#[cfg(windows)]
fn apply_window_material(ui: &MainWindow) {
    let theme = ui.global::<Theme>();
    let translucent = theme.get_translucent();
    apply_acrylic_backdrop(ui, translucent);
    apply_acrylic_blur_behind(ui, translucent, theme.get_blur_level());
}

#[cfg(windows)]
fn apply_current_window_effects(ui: &MainWindow) {
    // 原生边缘 resize hook（幂等）：winit 窗口可能晚于首个定时器创建，
    // 借助与 DWM 效果相同的重试序列，确保窗口就绪后完成安装
    install_native_resize(ui);
    // 无条件剥离 WS_CAPTION | WS_SYSMENU：winit 无边框窗口仍会带上它们，Windows 11 DWM
    // 据此自绘一套原生标题栏按钮（最小化/最大化/关闭），与本程序自绘按钮重叠成「两套」，
    // 并在延伸边框后于客户区顶部合成一条原生标题栏玻璃带（表现为窗口顶部莫名的半透明条）。
    //
    // 必须排在 DWM 效果之前：剥离样式要用 SetWindowPos(SWP_FRAMECHANGED) 通知系统重算
    // 非客户区，而这次重算会连带清掉 DwmExtendFrameIntoClientArea 的边框延伸与
    // SetWindowCompositionAttribute 的亚克力策略。此前顺序相反（先材质后剥离），
    // 冷启动的重试序列里最后一步总是 FRAMECHANGED，把刚设好的磨砂清成全透明，
    // 用户须手动重开一次模糊度/半透明才恢复。改为先剥离、后设材质，材质总是最后落地。
    strip_native_caption_buttons(ui);
    apply_window_round_corners(ui);
    apply_window_material(ui);
}

#[cfg(windows)]
fn schedule_window_effects(ui: &MainWindow, delays_ms: &[u64]) {
    for &delay in delays_ms {
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            if let Some(ui) = w.upgrade() {
                apply_current_window_effects(&ui);
            }
        });
    }
}

fn bind_window_chrome(ui: &MainWindow, core: &Rc<RefCell<AppCore>>) {
    // 最小化
    let w = ui.as_weak();
    ui.on_minimize_window(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    // 最大化 / 还原切换
    let w = ui.as_weak();
    ui.on_toggle_maximize(move || {
        if let Some(ui) = w.upgrade() {
            let next = !ui.window().is_maximized();
            ui.window().set_maximized(next);
            ui.set_window_maximized(next);
            // 最大化/还原会触发 FRAMECHANGED，DWM 可能重置非客户区与亚克力策略。
            #[cfg(windows)]
            schedule_window_effects(&ui, &[80]);
        }
    });

    // 关闭窗口：先保存窗口几何，再退出事件循环。
    let w = ui.as_weak();
    let c = core.clone();
    ui.on_close_window(move || {
        if let Some(ui) = w.upgrade() {
            save_window_geometry(&ui, &c);
            let _ = slint::quit_event_loop();
        }
    });

    // 系统关闭请求（Alt+F4、任务栏缩略图关闭等）同样保存几何，并真正退出事件循环。
    // 仅返回 HideWindow 只会隐藏窗口、进程仍在后台运行，用户会以为「没关掉」；
    // 保存后主动 quit_event_loop，与自绘关闭按钮走同一退出路径。
    let w = ui.as_weak();
    let c = core.clone();
    ui.window().on_close_requested(move || {
        if let Some(ui) = w.upgrade() {
            save_window_geometry(&ui, &c);
        }
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    // 拖动窗口：在标题栏空白处按下时调用 winit drag_window
    let w = ui.as_weak();
    ui.on_start_window_drag(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().with_winit_window(|winit_window| {
                let _ = winit_window.drag_window();
            });
        }
    });

    // Windows 11：窗口创建、首次显示和 DWM 首轮合成可能分阶段完成；有限重试并始终
    // 读取当前 Theme，确保冷启动恢复的模糊度不会被后续窗口样式更新覆盖。
    // 原生边缘 resize hook 也在该重试序列内幂等安装（apply_current_window_effects）。
    #[cfg(windows)]
    {
        // 图标是稳定窗口身份，仅设置一次；DWM 合成效果才需要有限重试。
        set_window_icon(ui);
        schedule_native_window_icon(ui, 20);
        schedule_window_effects(ui, &[60, 250, 800]);
    }
}

/// 开启/关闭窗口透明所需的「玻璃基座」：把 DWM 边框延伸到整个客户区，
/// 使透明像素后方允许合成层显示内容；真正的磨砂浓度由 `apply_acrylic_blur_behind`
/// 通过 SetWindowCompositionAttribute 连续控制（DWMWA_SYSTEMBACKDROP_TYPE 恒设为
/// DWMSBT_NONE，避免与之重复合成打架）。
#[cfg(windows)]
fn apply_acrylic_backdrop(ui: &MainWindow, translucent: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::Controls::MARGINS;

    // windows-sys 0.59 未导出该枚举常量，按官方数值硬编码
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMSBT_NONE: i32 = 1;

    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = isize::from(h.hwnd) as HWND;
            unsafe {
                // 关键：无边框窗口默认边框为 0，透明像素后方不会被合成。用 -1 把边框
                // 延伸到整个客户区（"sheet of glass"），DWM 才会在客户区透明像素后方
                // 合成 SetWindowCompositionAttribute 绘制的亚克力磨砂；关闭时归零，
                // 恢复纯色窗口。
                let inset: i32 = if translucent { -1 } else { 0 };
                let margins = MARGINS {
                    cxLeftWidth: inset,
                    cxRightWidth: inset,
                    cyTopHeight: inset,
                    cyBottomHeight: inset,
                };
                DwmExtendFrameIntoClientArea(hwnd, &margins);

                // 磨砂浓度改由 apply_acrylic_blur_behind 提供连续控制，这里固定关闭
                // DWM 自身的系统背景，避免两套亚克力合成叠加出异常观感。
                let backdrop: i32 = DWMSBT_NONE;
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &backdrop as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    });

    // 注意：这里不再回头调用 strip_native_caption_buttons。剥离样式必然伴随
    // SWP_FRAMECHANGED，会把上面刚设好的边框延伸连同随后的亚克力策略一起清掉。
    // 剥离已提前到 apply_current_window_effects 的第一步，且做了幂等短路，
    // WS_CAPTION | WS_SYSMENU 在窗口整个生命周期内保持剥离状态，无需在此重复。
}

/// 通过非公开 API `SetWindowCompositionAttribute`（user32.dll 导出，TranslucentTB、
/// 旧版 Windows Terminal 等均在用）驱动亚克力磨砂。
///
/// **重要限制**：这个 API 能调的只有 tint 颜色与 tint 的混合浓度（`GradientColor`
/// 的 alpha），实际的高斯模糊半径由 DWM 内部固定、系统层面不提供任何调节手段——
/// 这是 Windows 平台本身的限制，并非本程序未实现。此前版本把 alpha 从 24 线性拉
/// 到 220，且 tint 的 RGB 直接取了偏白的 `Theme.bg`（浅色下 `#edf3f9`），后果就是
/// 用户反馈的两个问题：一是 0 → 略大于0 时 accent_state 从
/// TRANSPARENTGRADIENT 直接切到 ACRYLICBLURBEHIND，观感是硬跳变而不是过渡；二是
/// alpha 越拖越高时，混合出来的颜色越来越接近纯白，看起来像"刷白漆"而不是磨砂
/// 变浓。
///
/// 现在的方案：既然模糊半径做不到连续，就不再假装连续，而是把 `blur_level`
/// （0..30，UI 侧滑块以 step=6 吸附）离散量化成 6 个真正有视觉区分度的档位，
/// 每一档手工调过 alpha 上限（最高约 58%，避免完全糊成一面白墙，让模糊后的
/// 背景内容仍隐约透出「磨砂感」而非「纯色填充」），且 tint 颜色改用更中性、更
/// 低亮度的灰调（不再是接近纯白的 `Theme.bg`），从源头减少"发白"观感。
/// 0 档为 ACCENT_DISABLED（完全清透玻璃，不叠加任何 tint），与「不透明度」滑块
/// 的语义保持解耦：不透明度只控制透多少底色，模糊度只控制磨砂浓不浓。
#[cfg(windows)]
fn apply_acrylic_blur_behind(ui: &MainWindow, translucent: bool, blur_level: f32) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;

    #[repr(C)]
    struct AccentPolicy {
        accent_state: u32,
        accent_flags: u32,
        gradient_color: u32,
        animation_id: u32,
    }
    #[repr(C)]
    struct WindowCompositionAttribData {
        attrib: u32,
        p_data: *mut core::ffi::c_void,
        data_size: usize,
    }

    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_DISABLED: u32 = 0;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

    type SetWindowCompositionAttributeFn =
        unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

    // SetWindowCompositionAttribute 是非公开 API：虽然 user32.dll 确有导出，
    // 但 Windows SDK 提供的 user32.lib 只收录公开符号，静态 #[link] 声明会在
    // 链接期报「无法解析的外部符号」。故改为运行时通过 GetProcAddress 动态取址
    // （TranslucentTB 等工具的标准做法），并用 OnceLock 缓存避免重复查找。
    fn resolve_set_window_composition_attribute() -> Option<SetWindowCompositionAttributeFn> {
        use std::sync::OnceLock;
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        static CACHED: OnceLock<usize> = OnceLock::new();
        let addr = *CACHED.get_or_init(|| unsafe {
            let module_name: Vec<u16> = "user32.dll\0".encode_utf16().collect();
            let module = GetModuleHandleW(module_name.as_ptr());
            if module.is_null() {
                return 0;
            }
            let proc_name = b"SetWindowCompositionAttribute\0";
            GetProcAddress(module, proc_name.as_ptr()).map_or(0, |f| f as usize)
        });
        if addr == 0 {
            None
        } else {
            // SAFETY: 地址来自 GetProcAddress 查得的同名导出函数，签名按官方逆向文档核对。
            Some(unsafe { std::mem::transmute::<usize, SetWindowCompositionAttributeFn>(addr) })
        }
    }

    let theme = ui.global::<Theme>();
    // 中性、偏暗灰调的 tint：刻意避开接近纯白/纯黑的取值，alpha 升高时混合结果
    // 趋向"雾蒙蒙的灰玻璃"而不是"刷白漆/刷黑漆"。
    let (r, g, b): (u32, u32, u32) = if theme.get_dark() {
        (33, 38, 48)
    } else {
        (205, 211, 219)
    };

    // 6 档阶梯（对应 UI 滑块 step=6 吸附到 0/6/12/18/24/30），每档 alpha 手工
    // 调过增量与上限，档与档之间要有肉眼可辨的差异，同时上限（148/255≈58%）
    // 留足透光度，避免糊成一面实色墙。
    const TIER_ALPHA: [u32; 6] = [0, 34, 62, 90, 118, 148];

    let (state, alpha) = if !translucent {
        (ACCENT_DISABLED, 0u32)
    } else {
        let tier = ((blur_level / 6.0).round() as i32).clamp(0, 5) as usize;
        if tier == 0 {
            // 完全清透：不叠加任何 tint，纯粹靠「不透明度」滑块控制底色透光。
            (ACCENT_DISABLED, 0u32)
        } else {
            (ACCENT_ENABLE_ACRYLICBLURBEHIND, TIER_ALPHA[tier])
        }
    };
    let gradient_color = (alpha << 24) | (b << 16) | (g << 8) | r;

    let Some(set_window_composition_attribute) = resolve_set_window_composition_attribute() else {
        return;
    };

    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = isize::from(h.hwnd) as HWND;
            let mut policy = AccentPolicy {
                accent_state: state,
                accent_flags: 0,
                gradient_color,
                animation_id: 0,
            };
            let mut data = WindowCompositionAttribData {
                attrib: WCA_ACCENT_POLICY,
                p_data: &mut policy as *mut _ as *mut core::ffi::c_void,
                data_size: std::mem::size_of::<AccentPolicy>(),
            };
            unsafe {
                set_window_composition_attribute(hwnd, &mut data);
            }
        }
    });
}

/// 剥离 WS_CAPTION 与 WS_SYSMENU，以消除 DWM 自绘的 Windows 11 原生标题栏按钮
/// （最小化/最大化/关闭），避免与本程序自绘按钮重叠成「两套」。保留
/// WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_THICKFRAME 不动，因此 Aero Snap、任务栏
/// 最小化动画和原生边缘缩放仍可用；自绘按钮走 winit，不依赖 WS_SYSMENU。
///
/// 需在任何会重算非客户区的操作之后调用：启用亚克力（延伸边框）、以及最大化/还原切换
/// （winit set_maximized 会触发 FRAMECHANGED，使原生按钮重现）。
#[cfg(windows)]
fn strip_native_caption_buttons(ui: &MainWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_SYSMENU,
    };

    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = isize::from(h.hwnd) as HWND;
            unsafe {
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                let stripped = style & !((WS_CAPTION | WS_SYSMENU) as isize);
                // 幂等：样式位已经是目标值时直接返回，不再触发 SWP_FRAMECHANGED。
                // 这次重算会清掉 DWM 的边框延伸与亚克力策略，重试序列/最大化补刷里
                // 每次都无条件发一遍，等于反复把刚设好的磨砂清掉。
                if stripped == style {
                    return;
                }
                SetWindowLongPtrW(hwnd, GWL_STYLE, stripped);
                // 通知系统样式变更并重算非客户区，使原生按钮立即消失
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
                );
            }
        }
    });
}

/// 通过 DWM 让无边框窗口呈现 Windows 11 圆角
#[cfg(windows)]
fn apply_window_round_corners(ui: &MainWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = isize::from(h.hwnd) as HWND;
            let pref: u32 = DWMWCP_ROUND as u32;
            unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                    &pref as *const u32 as *const core::ffi::c_void,
                    std::mem::size_of::<u32>() as u32,
                );
            }
        }
    });
}

/// WM_NCHITTEST 自定义窗口过程：让无边框窗口的边缘由 Windows 原生 resize 引擎处理。
///
/// winit 的 resize-border-width 在 Slint 无边框窗口上实现不稳定，拖拽边缘时会
/// 忽大忽小、剧烈抖动（尤其右上角）。改为替换窗口过程拦截 WM_NCHITTEST：指针
/// 落在窗口边缘 6 逻辑像素内时返回 HTLEFT/HTRIGHT/HTTOP/HTBOTTOM 等命中码，
/// 交由系统原生处理 resize（平滑无抖动）；其余消息交给原窗口过程 CallWindowProcW，
/// 保持 Slint 客户区（标题栏拖动、按钮、列表）的原有指针路由不变。
/// 原窗口过程指针（0 = 尚未安装）。用原子而非 static mut：窗口过程在 UI 线程
/// 被系统回调，安装也在 UI 线程，但原子读写避免 static_mut_refs 的未定义行为。
#[cfg(windows)]
static ORIGINAL_WNDPROC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(windows)]
unsafe extern "system" fn hit_test_wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GetWindowRect, IsZoomed, GWL_STYLE, HTBOTTOM,
        HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, STYLESTRUCT,
        WM_NCACTIVATE, WM_NCHITTEST, WM_STYLECHANGING, WS_CAPTION, WS_SYSMENU,
    };

    // winit 在退出 Borderless 全屏时会先恢复系统窗口样式，随后才交还事件循环；
    // 延迟调用 strip_native_caption_buttons 会留下约 0.5 秒的 Win7 非客户区按钮闪烁。
    // 在 WM_STYLECHANGING 同步清除目标样式，系统从未真正收到带标题栏的 style。
    if msg == WM_STYLECHANGING && wparam as i32 == GWL_STYLE {
        let style = lparam as *mut STYLESTRUCT;
        if !style.is_null() {
            (*style).styleNew &= !(WS_CAPTION | WS_SYSMENU);
        }
    }
    // 无边框窗口没有可绘制的非客户区；直接确认该消息，防止焦点状态变化时
    // DWM 临时绘制一圈原生活动边框。
    if msg == WM_NCACTIVATE {
        return 1;
    }

    // 最大化时关闭边缘 resize，避免最大化状态下拖边缘触发还原抖动
    if msg == WM_NCHITTEST && IsZoomed(hwnd) == 0 {
        // lParam 低字=x、高字=y（屏幕物理像素，均为有符号 short）
        let packed = lparam as u32;
        let sx = (packed & 0xffff) as i16 as i32;
        let sy = ((packed >> 16) & 0xffff) as i16 as i32;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rc) != 0 {
            let dpi = GetDpiForWindow(hwnd);
            // 6 逻辑像素的 resize 边缘，按窗口 DPI 换算为物理像素
            let border = if dpi > 0 {
                (6.0 * dpi as f32 / 96.0).round() as i32
            } else {
                6
            };
            let on_left = sx < rc.left + border;
            let on_right = sx >= rc.right - border;
            let on_top = sy < rc.top + border;
            let on_bottom = sy >= rc.bottom - border;
            if on_left && on_top {
                return HTTOPLEFT as isize;
            }
            if on_right && on_top {
                return HTTOPRIGHT as isize;
            }
            if on_left && on_bottom {
                return HTBOTTOMLEFT as isize;
            }
            if on_right && on_bottom {
                return HTBOTTOMRIGHT as isize;
            }
            if on_left {
                return HTLEFT as isize;
            }
            if on_right {
                return HTRIGHT as isize;
            }
            if on_top {
                return HTTOP as isize;
            }
            if on_bottom {
                return HTBOTTOM as isize;
            }
        }
    }
    let prev = ORIGINAL_WNDPROC.load(std::sync::atomic::Ordering::Relaxed);
    if prev != 0 {
        let proc: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC =
            std::mem::transmute::<usize, _>(prev);
        CallWindowProcW(proc, hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// 为窗口安装边缘 resize 窗口过程（替换原 WNDPROC，仅一次）。
/// 标题栏样式由 strip_native_caption_buttons() 统一、幂等移除；这里仅负责钩子，
/// 避免把窗口样式刷新绑定到“首次安装”条件，导致最大化或 DWM 重算后按钮重现。
#[cfg(windows)]
fn install_native_resize(ui: &MainWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};

    ui.window().with_winit_window(|winit_window| {
        let Ok(handle) = winit_window.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = isize::from(h.hwnd) as HWND;
            // 仅首次安装：原 WNDPROC 尚未保存时才替换，避免重复 hook
            if ORIGINAL_WNDPROC.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                let new_proc = hit_test_wndproc as *const () as usize as isize;
                let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc) };
                // 保存原窗口过程供 CallWindowProcW 回调；0 表示失败，回落 DefWindowProc
                ORIGINAL_WNDPROC.store(prev as usize, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
}

#[cfg(not(windows))]
fn install_native_resize(_ui: &MainWindow) {}

/// 打开 Windows 原生 ChooseColor 取色板对话框（模态）。
/// `initial` 为初始选中颜色，`hwnd` 为父窗口句柄。
/// 用户确认返回 Some(Color)，取消返回 None。
#[cfg(windows)]
fn open_color_picker(initial: slint::Color, hwnd: isize) -> Option<slint::Color> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
    };

    // 16 个自定义颜色槽（ChooseColor 需要，初始化为白）
    let mut cust_colors: [u32; 16] = [0xFFFFFFu32; 16];
    // COLORREF 格式: 0x00BBGGRR（与 RGBA 顺序相反）
    let initial_rgb: u32 =
        (initial.red() as u32) | ((initial.green() as u32) << 8) | ((initial.blue() as u32) << 16);

    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd as *mut core::ffi::c_void,
        hInstance: std::ptr::null_mut(),
        rgbResult: initial_rgb,
        lpCustColors: cust_colors.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        lCustData: 0,
        lpfnHook: None,
        lpTemplateName: std::ptr::null(),
    };

    unsafe {
        if ChooseColorW(&mut cc) != 0 {
            let r = (cc.rgbResult & 0xFF) as u8;
            let g = ((cc.rgbResult >> 8) & 0xFF) as u8;
            let b = ((cc.rgbResult >> 16) & 0xFF) as u8;
            Some(slint::Color::from_rgb_u8(r, g, b))
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
fn open_color_picker(_initial: slint::Color, _hwnd: isize) -> Option<slint::Color> {
    None
}

/// 用 exe 内嵌的多尺寸图标资源设置窗口的原生大/小图标。
///
/// winit 的 `set_window_icon` 只影响 winit 自己维护的图标，任务栏悬停缩略图右上角
/// 读的是窗口通过 `WM_SETICON` 记录的原生 HICON——不设置时 Windows 回退到通用 exe
/// 图标。资源 ID 1 即 build.rs 经 winres 嵌入的 icon.ico。
/// 用 `LR_SHARED` 加载：句柄由系统缓存管理，无需 DestroyIcon。
#[cfg(windows)]
fn set_native_window_icon(hwnd: isize) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED,
        SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    if hwnd == 0 {
        return;
    }
    let hwnd = hwnd as *mut core::ffi::c_void;
    // MAKEINTRESOURCEW(1)：按序号引用嵌入的图标资源
    let resource = 1u16 as *const u16;
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        for (which, cx, cy) in [
            (
                ICON_BIG,
                GetSystemMetrics(SM_CXICON),
                GetSystemMetrics(SM_CYICON),
            ),
            (
                ICON_SMALL,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
            ),
        ] {
            let icon = LoadImageW(hinst, resource, IMAGE_ICON, cx, cy, LR_SHARED);
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as usize, icon as isize);
            }
        }
    }
}

#[cfg(windows)]
fn set_window_icon(ui: &MainWindow) {
    const ICON_PNG: &[u8] = include_bytes!("../icon.png");
    let load = || -> Option<(Vec<u8>, u32, u32)> {
        let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
            png::ColorType::Rgb => buf[..info.buffer_size()]
                .chunks(3)
                .flat_map(|c| [c[0], c[1], c[2], 255u8])
                .collect(),
            _ => return None,
        };
        Some((rgba, info.width, info.height))
    };
    // 原生窗口图标（任务栏缩略图角标）与 winit 图标各自独立，两者都要设置。
    // HWND 可能晚于本次调用才创建，交给下方重试逻辑兜底。
    set_native_window_icon(main_hwnd(ui));

    let Some((rgba, w, h)) = load() else {
        return;
    };
    let Ok(icon) = winit::window::Icon::from_rgba(rgba, w, h) else {
        return;
    };
    ui.window().with_winit_window(move |winit_window| {
        winit_window.set_window_icon(Some(icon));
    });
}

/// 窗口创建时机不定（实测事件循环启动后 80-250ms），首次设置可能因 HWND 未就绪
/// 而落空；沿用 DWM 效果那套有限重试，直到拿到 HWND 为止。
#[cfg(windows)]
fn schedule_native_window_icon(ui: &MainWindow, retries_left: u32) {
    let hwnd = main_hwnd(ui);
    if hwnd != 0 {
        set_native_window_icon(hwnd);
        return;
    }
    if retries_left == 0 {
        return;
    }
    let w = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
        if let Some(ui) = w.upgrade() {
            schedule_native_window_icon(&ui, retries_left - 1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_this_pc_setting_resolves_to_virtual_root() {
        assert_eq!(startup_path("this-pc"), PathBuf::from("this-pc://"));
    }

    #[test]
    fn startup_quick_and_last_use_home_fallback() {
        let home = home_start_path();
        assert_eq!(startup_path("quick"), home);
        assert_eq!(startup_path("last"), home_start_path());
        assert_eq!(startup_path("unknown"), home_start_path());
    }
}
