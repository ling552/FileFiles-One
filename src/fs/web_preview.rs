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
    } else if ext == "docx" {
        // Word 文档：抽取正文转 HTML 渲染显示
        let text = super::preview::office_text(path)?;
        office_to_html(&text, content.dark)
    } else {
        // php 等：按静态 HTML 渲染（<?php ?> 段浏览器视作未知标签忽略）
        let raw = std::fs::read_to_string(path).ok()?;
        format!(r#"<base href="{}">{}"#, base, raw)
    };
    let tmp = std::env::temp_dir().join("filefiles-one_preview.html");
    std::fs::write(&tmp, html).ok()?;
    Some(file_url(&tmp.to_string_lossy()))
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

/// Windows 路径 → file:/// URL（反斜杠转正斜杠，空格等交由 WebView2 处理）
fn file_url(path: &str) -> String {
    format!("file:///{}", path.replace('\\', "/"))
}

#[cfg(windows)]
mod win_impl {
    use super::{url_for, WebContent};
    use std::cell::RefCell;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Controller,
    };
    use webview2_com::{
        CreateCoreWebView2ControllerCompletedHandler,
        CreateCoreWebView2EnvironmentCompletedHandler,
    };
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, RECT};

    struct WebState {
        controller: Option<ICoreWebView2Controller>,
        /// 环境/控制器异步创建期间挂起的导航目标
        pending: Option<((i32, i32, i32, i32), String)>,
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

    /// 应用矩形 + 导航 + 显示（控制器已就绪时）
    fn apply(controller: &ICoreWebView2Controller, rect: (i32, i32, i32, i32), url: &str) {
        unsafe {
            let _ = controller.SetBounds(to_rect(rect));
            if let Ok(webview) = controller.CoreWebView2() {
                let _ = webview.Navigate(PCWSTR(HSTRING::from(url).as_ptr()));
            }
            let _ = controller.SetIsVisible(true);
        }
    }

    pub fn start(parent: isize, rect: (i32, i32, i32, i32), content: WebContent) -> bool {
        let Some(url) = url_for(&content) else {
            return false;
        };
        let ready = STATE.with(|s| {
            let mut st = s.borrow_mut();
            if st.unavailable {
                return Some(false);
            }
            if let Some(controller) = &st.controller {
                apply(controller, rect, &url);
                return Some(true);
            }
            // 创建尚未完成：挂起导航目标，就绪后统一应用
            st.pending = Some((rect, url.clone()));
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
                                    // 应用挂起的导航（预览可能已关闭：pending 为 None 则只驻留隐藏）
                                    if let Some((rect, url)) = st.pending.take() {
                                        apply(&controller, rect, &url);
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
