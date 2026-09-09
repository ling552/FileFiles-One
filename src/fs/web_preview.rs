//! Quick Look 网页渲染视图：WebView2（Edge 运行时）在主窗口内以原生子层渲染
//! HTML/PHP/Markdown，与预览浮层内容区对齐覆盖（与 video_preview 同模式）。
//!
//! 生命周期（仅 UI 线程）：首次进入渲染视图时异步创建 WebView2 环境与控制器
//! （回调经 UI 线程消息循环送达）；关闭预览只隐藏控制器（保留实例，再次打开
//! 秒开）；导航目标在创建完成前先挂起（pending），就绪后统一应用。
//!
//! Markdown 在 Rust 侧经 pulldown-cmark 转 HTML（GitHub 风格样式，跟随深浅主题）；
//! Markdown/PHP 写入临时 HTML 文件并注入 <base>，相对资源（图片等）按源目录解析。

use std::path::Path;

/// 渲染目标：源文件路径 + 深色主题标记。
/// html/htm 直接以 file:// 导航；md/php 生成临时 HTML 后导航。
pub struct WebContent {
    pub path: String,
    pub dark: bool,
}

/// 启动/更新渲染视图：`parent` 主窗口 HWND，`rect` 内容区物理像素矩形。
/// 返回 false 表示 WebView2 运行时不可用（调用方回退源码视图）。
#[cfg(windows)]
pub fn start(parent: isize, rect: (i32, i32, i32, i32), content: WebContent) -> bool {
    win_impl::start(parent, rect, content)
}

#[cfg(not(windows))]
pub fn start(_parent: isize, _rect: (i32, i32, i32, i32), _content: WebContent) -> bool {
    false
}

/// 隐藏渲染视图（保留 WebView2 实例，下次打开秒开；导航空白页停掉媒体播放）。
pub fn stop() {
    #[cfg(windows)]
    win_impl::stop();
}

/// 对齐子层到新的物理像素矩形（预留：窗口 resize/移动跟随，当前预览期间
/// 窗口几何不变化，与视频子窗口行为一致）。
#[allow(dead_code)]
#[cfg(windows)]
pub fn reposition(rect: (i32, i32, i32, i32)) {
    win_impl::reposition(rect);
}

#[allow(dead_code)]
#[cfg(not(windows))]
pub fn reposition(_rect: (i32, i32, i32, i32)) {}

/// Markdown → 完整 HTML 文档（GitHub 风格排版，深浅主题配色）
pub fn markdown_to_html(md: &str, dark: bool) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(md, opts);
    let mut body = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut body, parser);

    let (bg, fg, muted, border, code_bg, link) = if dark {
        ("#1e2227", "#d7dde3", "#9aa4ae", "#3a4149", "#2a3138", "#58a6ff")
    } else {
        ("#ffffff", "#24292f", "#57606a", "#d0d7de", "#f6f8fa", "#0969da")
    };
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>
  body {{ margin: 0; padding: 24px 32px; background: {bg}; color: {fg};
         font: 15px/1.65 -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif; }}
  h1, h2 {{ border-bottom: 1px solid {border}; padding-bottom: .3em; }}
  a {{ color: {link}; }}
  img {{ max-width: 100%; }}
  blockquote {{ margin: 0; padding: 0 1em; color: {muted}; border-left: .25em solid {border}; }}
  pre {{ background: {code_bg}; padding: 12px 16px; border-radius: 8px; overflow: auto; }}
  code {{ background: {code_bg}; padding: .15em .35em; border-radius: 5px;
          font-family: Consolas, "Courier New", monospace; font-size: 90%; }}
  pre code {{ padding: 0; background: transparent; }}
  table {{ border-collapse: collapse; }}
  th, td {{ border: 1px solid {border}; padding: 6px 13px; }}
  hr {{ border: 0; border-top: 1px solid {border}; }}
</style></head><body>{body}</body></html>"#
    )
}

/// 把源文件解析为可导航的 URL：
/// html/htm → 原文件 file:// URL；md/markdown → 转 HTML 写临时文件；
/// pdf → 原文件 file:// URL（Edge 原生 PDF 查看器：分页/缩放/搜索/打印完整保留）；
/// doc/docx/xls/xlsx/ppt/pptx → 优先导航本机 Office 转出的缓存 PDF
///   （与 Office 打开版式一致），无缓存时回退文本排版 HTML（后台转好后自动升级）；
/// php → 原文内容注入 <base> 后写临时文件（渲染其中的静态 HTML 部分）。
/// 返回 None 表示读取失败。
pub fn url_for(content: &WebContent) -> Option<String> {
    let path = Path::new(&content.path);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext == "html" || ext == "htm" {
        return Some(file_url(&content.path));
    }
    if ext == "pdf" {
        // PDF 高保真：直接用 Edge 原生查看器打开源文件，不经过文本抽取
        if std::fs::metadata(path).is_ok() {
            return Some(file_url(&content.path));
        }
        return None;
    }
    if super::office_preview::office_app_for_ext(&ext).is_some() {
        // Office 高保真：缓存命中则直接看 Office 排版的 PDF
        if let Some(pdf) = super::office_preview::cached_pdf_if_fresh(path) {
            return Some(file_url(&pdf.to_string_lossy()));
        }
        // 本机装有对应 Office：渲染区显示加载条（about:blank 占位），
        // 后台转换完成后 navigate() 升级到 PDF，不再先闪一下文本版。
        let installed = super::office_preview::office_app_for_ext(&ext)
            .map(|a| super::office_preview::is_office_installed(a))
            .unwrap_or(false);
        if installed {
            return Some("about:blank".to_string());
        }
        // 未安装 Office：回退文本排版（新旧版统一，无占位提示）
        let text = super::preview::document_text(path)
            .or_else(|_| {
                super::preview::office_text(path)
                    .ok_or_else(|| "无法读取文档".to_string())
            })
            .unwrap_or_else(|_| "文档内容暂时无法预览".to_string());
        let html = office_to_html(&text, content.dark);
        return Some(temp_html_url(&html, path));
    }
    // 相对资源基准：源文件所在目录
    let base = path
        .parent()
        .map(|p| file_url(&format!("{}\\", p.to_string_lossy())))
        .unwrap_or_default();
    let html = if ext == "md" || ext == "markdown" {
        let md = std::fs::read_to_string(path).ok()?;
        let doc = markdown_to_html(&md, content.dark);
        // <base> 注入 <head> 首部，相对图片链接按源目录解析
        doc.replacen(
            "<head>",
            &format!(r#"<head><base href="{}">"#, base),
            1,
        )
    } else {
        // php 等：按静态 HTML 渲染（<?php ?> 段浏览器视作未知标签忽略）
        let raw = std::fs::read_to_string(path).ok()?;
        format!(r#"<base href="{}">{}"#, base, raw)
    };
    Some(temp_html_url(&html, path))
}

/// 渲染 HTML 写临时文件并返回 file:// URL。
/// 文件名按源文件指纹（路径+大小+修改时间）唯一命名：固定单文件名会导致切文件
/// 时 WebView2 对相同 URL 拒绝重新导航，显示的仍是上一个文件的内容（串片）。
pub(crate) fn temp_html_url(html: &str, src: &Path) -> String {
    use sha2::{Digest, Sha256};
    let (len, mtime) = std::fs::metadata(src)
        .map(|m| {
            (
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    let mut h = Sha256::new();
    h.update(src.to_string_lossy().as_bytes());
    h.update(len.to_le_bytes());
    h.update(mtime.to_le_bytes());
    let hex = format!("{:x}", h.finalize());
    let tmp = std::env::temp_dir().join(format!(
        "filefiles-one_preview_{}_{}.html",
        std::process::id(),
        &hex[..16],
    ));
    let _ = std::fs::write(&tmp, html);
    cleanup_old_temp_html();
    file_url(&tmp.to_string_lossy())
}

/// 清理 1 天前的预览临时 HTML（best-effort，忽略全部错误）
fn cleanup_old_temp_html() {
    let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("filefiles-one_preview_") || !name.ends_with(".html") {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs() > 24 * 3600)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Office 待转换时是否应延迟显示 WebView（保持隐藏以露出 Slint 加载层）。
/// 渲染区 Slint 加载动画位于 WebView 子窗口之下：一旦导航（即使 about:blank）
/// 控制器即覆盖内容区，用户看到的是空白而非加载动画。故待转换期间只后台创建
/// 控制器、不显示；后台转好后 navigate() 再显示并导航到 PDF。
pub fn should_defer_show(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let Some(app) = super::office_preview::office_app_for_ext(&ext) else {
        return false;
    };
    if super::office_preview::cached_pdf_if_fresh(path).is_some() {
        return false;
    }
    super::office_preview::is_office_installed(app)
}

/// 把已创建的 WebView2 导航到新 URL（Office 后台转好 PDF 后升级渲染用）。
/// 控制器不存在时返回 false（调用方忽略即可，下次 start 会用缓存直达）。
pub fn navigate(url: &str) -> bool {
    #[cfg(windows)]
    {
        win_impl::navigate(url)
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        false
    }
}

/// Office 文档正文 → 排版 HTML（渲染视图）：pre-wrap 保留段落换行
pub fn office_to_html(text: &str, dark: bool) -> String {
    let mut body = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        match c {
            '&' => body.push_str("&amp;"),
            '<' => body.push_str("&lt;"),
            '>' => body.push_str("&gt;"),
            _ => body.push(c),
        }
    }
    let (bg, fg, muted) = if dark {
        ("#1e2227", "#d7dde3", "#9aa4ae")
    } else {
        ("#ffffff", "#24292f", "#57606a")
    };
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>
  body {{ margin: 0; padding: 24px 32px; background: {bg}; color: {fg};
         font: 15px/1.75 -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif; }}
  .doc {{ white-space: pre-wrap; max-width: 860px; margin: 0 auto; }}
  .meta {{ color: {muted}; font-size: 12px; margin-bottom: 16px; }}
</style></head><body><div class="doc">{body}</div></body></html>"#
    )
}

/// Windows 路径 → file:/// URL：反斜杠转正斜杠并做百分号编码
/// （`#`、`?`、`%`、空格、非 ASCII 等不编码会破坏 URL 语义，
/// 相对资源的 <base> 会被解析到错误目录）
/// pub(crate) 供 Office 后台转换完成后直接构造 PDF 导航 URL。
pub(crate) fn file_url(path: &str) -> String {
    fn encode_into(out: &mut String, s: &str) {
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'!'
                | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':'
                | b'/' | b'@' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::from("file:///");
    encode_into(&mut out, &path.replace('\\', "/"));
    out
}

/// html/htm 预览允许页面脚本（动态 HTML 渲染是功能需求）；
/// md/docx 生成的 HTML 与 php 静态渲染一律禁用脚本——这些文件来源不可信
/// （常见于网络下载），raw HTML（含 `<script>`、`<img onerror=...>`）会
/// 原样进入 WebView2，禁脚本即阻断预览触发脚本执行的整条链路。
fn allows_scripts(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    ext == "html" || ext == "htm"
}

#[cfg(windows)]
mod win_impl {
    use super::{allows_scripts, url_for, WebContent};
    use std::cell::RefCell;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    };
    use webview2_com::{
        take_pwstr, CreateCoreWebView2ControllerCompletedHandler,
        CreateCoreWebView2EnvironmentCompletedHandler, NavigationStartingEventHandler,
        NewWindowRequestedEventHandler,
    };
    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::System::WinRT::EventRegistrationToken;

    struct WebState {
        controller: Option<ICoreWebView2Controller>,
        /// 环境/控制器异步创建期间挂起的导航目标（矩形，URL，脚本开关，可见性）
        pending: Option<((i32, i32, i32, i32), String, bool, bool)>,
        /// 是否已在创建流程中（防重复发起）
        creating: bool,
        /// 运行时不可用（创建失败过，不再重试）
        unavailable: bool,
    }

    thread_local! {
        static STATE: RefCell<WebState> = RefCell::new(WebState {
            controller: None,
            pending: None,
            creating: false,
            unavailable: false,
        });
    }

    fn to_rect(r: (i32, i32, i32, i32)) -> RECT {
        RECT {
            left: r.0,
            top: r.1,
            right: r.0 + r.2,
            bottom: r.1 + r.3,
        }
    }

    /// 读取事件参数里的 Uri（CoTaskMem 内存由 take_pwstr 释放）
    fn args_uri(uri: impl FnOnce(&mut PWSTR) -> windows::core::Result<()>) -> String {
        let mut pw = PWSTR::null();
        if uri(&mut pw).is_ok() {
            take_pwstr(pw)
        } else {
            String::new()
        }
    }

    /// 注册导航守卫（控制器创建成功后一次）：
    /// - 远程 http(s) 导航一律取消并转交系统默认浏览器——预览视图保持
    ///   file:// 上下文，页面内跳转/重定向无法把预览变成应用内钓鱼页；
    /// - window.open 新窗口请求直接 Handled 不放行，远程目标转系统浏览器。
    fn install_navigation_guard(webview: &ICoreWebView2) {
        unsafe {
            let nav = NavigationStartingEventHandler::create(Box::new(|_sender, args| {
                if let Some(args) = args {
                    let uri = args_uri(|pw| args.Uri(pw));
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        let _ = args.SetCancel(true);
                        let _ = open::that(&uri);
                    }
                }
                Ok(())
            }));
            let _ = webview.add_NavigationStarting(
                &nav,
                &mut EventRegistrationToken::default(),
            );
            let win = NewWindowRequestedEventHandler::create(Box::new(|_sender, args| {
                if let Some(args) = args {
                    let uri = args_uri(|pw| args.Uri(pw));
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        let _ = open::that(&uri);
                    }
                    let _ = args.SetHandled(true);
                }
                Ok(())
            }));
            let _ = webview.add_NewWindowRequested(
                &win,
                &mut EventRegistrationToken::default(),
            );
        }
    }

    /// 应用矩形 + 导航 + 显示（控制器已就绪时）。
    /// `allow_scripts` 按源文件类型切换脚本执行（见 [`super::allows_scripts`]），
    /// 并始终关闭 DevTools（预览视图非调试场景）。
    /// `visible` 为假时导航后保持隐藏（Office 待转换：露出 Slint 加载层）。
    fn apply(
        controller: &ICoreWebView2Controller,
        rect: (i32, i32, i32, i32),
        url: &str,
        allow_scripts: bool,
        visible: bool,
    ) {
        unsafe {
            let _ = controller.SetBounds(to_rect(rect));
            if let Ok(webview) = controller.CoreWebView2() {
                if let Ok(settings) = webview.Settings() {
                    let _ = settings.SetIsScriptEnabled(allow_scripts);
                    let _ = settings.SetAreDevToolsEnabled(false);
                }
                let _ = webview.Navigate(PCWSTR(HSTRING::from(url).as_ptr()));
            }
            let _ = controller.SetIsVisible(visible);
        }
    }

    pub fn start(parent: isize, rect: (i32, i32, i32, i32), content: WebContent) -> bool {
        use std::path::Path;
        let Some(url) = url_for(&content) else {
            return false;
        };
        let allow_scripts = allows_scripts(&content.path);
        // Office 待转换：后台创建控制器但保持隐藏，露出 Slint 加载动画；
        // 转好后 navigate() 再显示。其它类型立即显示。
        let visible = !super::should_defer_show(Path::new(&content.path));
        let ready = STATE.with(|s| {
            let mut st = s.borrow_mut();
            if st.unavailable {
                return Some(false);
            }
            if let Some(controller) = &st.controller {
                apply(controller, rect, &url, allow_scripts, visible);
                return Some(true);
            }
            // 创建尚未完成：挂起导航目标，就绪后统一应用
            st.pending = Some((rect, url.clone(), allow_scripts, visible));
            if st.creating {
                return Some(true);
            }
            st.creating = true;
            None
        });
        if let Some(done) = ready {
            return done;
        }

        // 首次进入：异步创建环境 → 控制器（回调经 UI 线程消息循环送达）
        let user_data = dirs::data_local_dir()
            .map(|d| d.join("FileFiles One").join("WebView2"))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let parent_hwnd = HWND(parent as *mut core::ffi::c_void);
        let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |result, environment| {
                let Ok(()) = result else {
                    STATE.with(|s| {
                        let mut st = s.borrow_mut();
                        st.creating = false;
                        st.unavailable = true;
                    });
                    return Ok(());
                };
                let Some(environment) = environment else {
                    STATE.with(|s| {
                        let mut st = s.borrow_mut();
                        st.creating = false;
                        st.unavailable = true;
                    });
                    return Ok(());
                };
                let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |result, controller| {
                        STATE.with(|s| {
                            let mut st = s.borrow_mut();
                            st.creating = false;
                            match (result, controller) {
                                (Ok(()), Some(controller)) => {
                                    // 导航守卫先于首次导航注册
                                    unsafe {
                                        if let Ok(webview) = controller.CoreWebView2() {
                                            install_navigation_guard(&webview);
                                        }
                                    }
                                    // 应用挂起的导航（预览可能已关闭：pending 为 None 则只驻留隐藏）
                                    if let Some((rect, url, allow_scripts, visible)) =
                                        st.pending.take()
                                    {
                                        apply(&controller, rect, &url, allow_scripts, visible);
                                    } else {
                                        unsafe {
                                            let _ = controller.SetIsVisible(false);
                                        }
                                    }
                                    st.controller = Some(controller);
                                }
                                _ => st.unavailable = true,
                            }
                        });
                        Ok(())
                    },
                ));
                unsafe {
                    if environment
                        .CreateCoreWebView2Controller(parent_hwnd, &ctrl_handler)
                        .is_err()
                    {
                        STATE.with(|s| {
                            let mut st = s.borrow_mut();
                            st.creating = false;
                            st.unavailable = true;
                        });
                    }
                }
                Ok(())
            },
        ));
        unsafe {
            // 传入 --disable-logging：抑制 Chromium 浏览器进程启停时的无害 ERROR 日志
            // （如 "Failed to unregister class Chrome_WidgetWin_0. Error = 1412"），
            // 避免污染控制台输出；不影响页面功能。
            use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2EnvironmentOptions;
            use webview2_com::CoreWebView2EnvironmentOptions;
            let options: ICoreWebView2EnvironmentOptions =
                CoreWebView2EnvironmentOptions::default().into();
            let _ = options
                .SetAdditionalBrowserArguments(windows::core::w!("--disable-logging"));
            let hr = CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(HSTRING::from(user_data).as_ptr()),
                Some(&options),
                &env_handler,
            );
            if hr.is_err() {
                STATE.with(|s| {
                    let mut st = s.borrow_mut();
                    st.creating = false;
                    st.unavailable = true;
                    st.pending = None;
                });
                return false;
            }
        }
        true
    }

    pub fn stop() {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.pending = None;
            if let Some(controller) = &st.controller {
                unsafe {
                    // 导航到空白页停掉可能的媒体播放，再隐藏（实例保留，下次秒开）
                    if let Ok(webview) = controller.CoreWebView2() {
                        let _ = webview.Navigate(PCWSTR(HSTRING::from("about:blank").as_ptr()));
                    }
                    let _ = controller.SetIsVisible(false);
                }
            }
        });
    }

    /// 已有控制器时直接导航（Office PDF 转好后升级用）；无控制器返回 false。
    /// 若环境仍在创建中，则只替换挂起导航的 URL 并保留原矩形，避免零矩形覆盖。
    pub fn navigate(url: &str) -> bool {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            if st.unavailable {
                return false;
            }
            if let Some(controller) = &st.controller {
                unsafe {
                    if let Ok(webview) = controller.CoreWebView2() {
                        let _ = webview.Navigate(PCWSTR(HSTRING::from(url).as_ptr()));
                    }
                    let _ = controller.SetIsVisible(true);
                }
                return true;
            }
            // 创建中：保留原矩形，只换 URL 并确保就绪后可见
            // （升级内容一定是真实高保真版/回退文本，不再是占位空白）
            if let Some((rect, _, scripts, _)) = st.pending.take() {
                st.pending = Some((rect, url.to_string(), scripts, true));
                return true;
            }
            false
        })
    }

    pub fn reposition(rect: (i32, i32, i32, i32)) {
        STATE.with(|s| {
            if let Some(controller) = &s.borrow().controller {
                unsafe {
                    let _ = controller.SetBounds(to_rect(rect));
                }
            }
        });
    }
}
