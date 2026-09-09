//! 独立预览窗口宿主：创建/复用 PreviewWindow 实例，并维护归档树展开状态。
//!
//! 预览从主窗口浮层改为真实窗口后，内容不再随主窗口布局走，因此：
//! 1. Slint 的 global 是「每根组件一份」，PreviewWindow 用自己的 PreviewState 与
//!    Theme，需在每次打开时从主窗口镜像主题并推送内容；
//! 2. 视频（Media Foundation）与网页（WebView2）原生子窗口改挂到本窗口，
//!    矩形由 main.rs 的 preview_content_rect_phys 按本窗口客户区计算；
//! 3. 归档树的展开集合按「归档路径 + 节点全路径」保存在本模块，
//!    点击目录行只重算可见节点，不重复解压读取归档。
//!
//! 窗口实例创建后常驻（隐藏而非销毁），避免反复按空格时重建窗口造成闪烁。

use crate::fs::metadata;
use crate::fs::preview::ArchiveTreeNode;
use crate::{ArchiveNode, MainWindow, PreviewState, PreviewWindow, Theme};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// 归档树的会话状态：完整树 + 已展开目录集合
#[derive(Default)]
struct ArchiveView {
    /// 当前预览的归档文件路径（切换文件时重置展开状态）
    source: String,
    /// 完整树（深度优先顺序，含所有层级）
    nodes: Vec<ArchiveTreeNode>,
    /// 已展开目录的归档内全路径
    expanded: HashSet<String>,
}

thread_local! {
    /// 预览窗口实例（首次打开时创建后常驻）。Slint 组件句柄不实现 Clone，
    /// 这里保存强引用，对外统一经 as_weak().upgrade() 交出临时句柄。
    static WINDOW: RefCell<Option<PreviewWindow>> = const { RefCell::new(None) };
    /// 归档树状态
    static ARCHIVE: RefCell<ArchiveView> = RefCell::new(ArchiveView::default());
}

/// 取已创建的预览窗口（未创建返回 None）
pub fn window() -> Option<PreviewWindow> {
    WINDOW.with(|w| w.borrow().as_ref().and_then(|win| win.as_weak().upgrade()))
}

/// 创建（或复用）预览窗口。`on_close` 在用户按 Esc/空格或点窗口 X 时调用，
/// `on_web_mode` 在切换渲染/源码视图时调用，均由 main.rs 注入以复用其原生逻辑。
pub fn ensure_window(
    on_close: impl Fn() + 'static,
    on_web_mode: impl Fn(bool) + 'static,
    on_fullscreen: impl Fn() + 'static,
) -> Result<PreviewWindow, slint::PlatformError> {
    if let Some(w) = window() {
        return Ok(w);
    }
    let win = PreviewWindow::new()?;
    let st = win.global::<PreviewState>();

    // 归档目录行点击：切换展开态并重算可见节点
    let weak = win.as_weak();
    st.on_toggle_folder(move |idx| {
        if let Some(win) = weak.upgrade() {
            toggle_folder(&win, idx);
        }
    });

    let close_cb = Rc::new(on_close);
    // 头部/键盘触发的关闭
    let c = close_cb.clone();
    st.on_close_preview(move || c());
    // 点窗口标题栏 X：Slint 默认会隐藏窗口，这里同步走一遍关闭清理
    let c = close_cb.clone();
    win.window().on_close_requested(move || {
        c();
        slint::CloseRequestResponse::HideWindow
    });

    st.on_set_web_mode(on_web_mode);
    st.on_toggle_fullscreen(on_fullscreen);

    let handle = win.as_weak();
    WINDOW.with(|w| *w.borrow_mut() = Some(win));
    // 强引用已存入 thread_local，这里返回一个等价的临时句柄
    handle.upgrade().ok_or(slint::PlatformError::NoPlatform)
}

/// 把主窗口的主题控制项镜像到预览窗口（两者各有一份 Theme global）
pub fn sync_theme(main: &MainWindow, win: &PreviewWindow) {
    let src = main.global::<Theme>();
    let dst = win.global::<Theme>();
    dst.set_theme_mode(src.get_theme_mode());
    dst.set_accent_key(src.get_accent_key());
    dst.set_accent_custom(src.get_accent_custom());
    dst.set_compact(src.get_compact());
    // 预览窗口不做半透明：原生视频/WebView2 子窗口无法与 DWM 亚克力正确合成，
    // 半透明会让画面区域出现穿透与拖影。
    dst.set_translucent(false);
}

/// 把主窗口 AppState 中已填好的 ql-* 内容推送到预览窗口的 PreviewState。
/// 归档类型（kind==5）额外读取树并按展开状态生成可见节点。
pub fn push_content(main: &MainWindow, win: &PreviewWindow, path: &str) {
    let src = main.global::<crate::AppState>();
    let dst = win.global::<PreviewState>();
    let kind = src.get_ql_kind();
    dst.set_kind(kind);
    dst.set_loading(src.get_ql_loading());
    dst.set_name(src.get_ql_name());
    dst.set_subtitle(src.get_ql_subtitle());
    dst.set_icon_class(src.get_ql_icon_class());
    dst.set_thumb(src.get_ql_thumb());
    dst.set_has_thumb(src.get_ql_has_thumb());
    dst.set_preview_image(src.get_ql_image());
    dst.set_has_image(src.get_ql_has_image());
    dst.set_img_w(src.get_ql_img_w());
    dst.set_img_h(src.get_ql_img_h());
    dst.set_text_content(src.get_ql_text());
    dst.set_code_kw(src.get_ql_code_kw());
    dst.set_code_str(src.get_ql_code_str());
    dst.set_code_cmt(src.get_ql_code_cmt());
    dst.set_info(src.get_ql_info());
    dst.set_can_render(src.get_ql_can_render());
    dst.set_web_mode(src.get_ql_web_mode());
    dst.set_office_doc(src.get_ql_office_doc());
    dst.set_office_pending(src.get_ql_office_pending());
    dst.set_video_fullscreen(src.get_ql_video_fullscreen());

    if kind == 5 {
        load_archive(win, path);
    } else {
        ARCHIVE.with(|a| *a.borrow_mut() = ArchiveView::default());
        dst.set_archive_nodes(ModelRc::new(VecModel::from(Vec::<ArchiveNode>::new())));
    }
}

/// 更新副标题（视频分辨率就绪、文件夹统计完成等）
pub fn set_subtitle(win: &PreviewWindow, text: &str) {
    win.global::<PreviewState>().set_subtitle(text.into());
}

/// 更新加载态（图片解码 / 视频媒体源解析完成后置 false，收起加载动画）
pub fn set_loading(win: &PreviewWindow, loading: bool) {
    win.global::<PreviewState>().set_loading(loading);
}

/// 图片位图后台解码完成后回填（尺寸在打开前已知，此处只换像素）
pub fn set_image(win: &PreviewWindow, image: slint::Image) {
    let st = win.global::<PreviewState>();
    st.set_preview_image(image);
    st.set_has_image(true);
}

/// 更新信息文本（文件夹递归统计完成后回填）
pub fn set_info(win: &PreviewWindow, text: &str) {
    win.global::<PreviewState>().set_info(text.into());
}

/// 读取归档并按「全部折叠」初始状态生成可见节点。
/// 读取失败时退回信息态（kind=0）并把错误显示在信息区。
fn load_archive(win: &PreviewWindow, path: &str) {
    let st = win.global::<PreviewState>();
    match crate::fs::preview::archive_tree(std::path::Path::new(path)) {
        Ok(nodes) => {
            let (dirs, files, total) = crate::fs::preview::archive_summary(&nodes);
            // 副标题补充归档内统计，替代旧文本清单的表头
            let base = st.get_subtitle().to_string();
            st.set_subtitle(
                format!(
                    "{}{}{} 个文件夹 · {} 个文件 · 解压后 {}",
                    base,
                    if base.is_empty() { "" } else { " · " },
                    dirs,
                    files,
                    metadata::human_size(total)
                )
                .into(),
            );
            ARCHIVE.with(|a| {
                let mut a = a.borrow_mut();
                a.source = path.to_string();
                a.nodes = nodes;
                a.expanded.clear();
            });
            refresh_archive_model(win);
        }
        Err(e) => {
            ARCHIVE.with(|a| *a.borrow_mut() = ArchiveView::default());
            st.set_archive_nodes(ModelRc::new(VecModel::from(Vec::<ArchiveNode>::new())));
            st.set_kind(0);
            st.set_info(format!("无法读取归档：{}", e).into());
        }
    }
}

/// 切换第 idx 个可见节点的展开态。idx 是「可见列表」下标，
/// 需先映射回完整树的 full_path 再改集合，然后重算可见列表。
fn toggle_folder(win: &PreviewWindow, idx: i32) {
    let st = win.global::<PreviewState>();
    let model = st.get_archive_nodes();
    let Some(row) = model.row_data(idx.max(0) as usize) else {
        return;
    };
    let key = row.full_path.to_string();
    ARCHIVE.with(|a| {
        let mut a = a.borrow_mut();
        if !a.expanded.remove(&key) {
            a.expanded.insert(key);
        }
    });
    refresh_archive_model(win);
}

/// 按当前展开集合过滤完整树，生成可见节点模型。
/// 判定规则：节点可见 ⇔ 其所有祖先目录都在展开集合中。
fn refresh_archive_model(win: &PreviewWindow) {
    let rows = ARCHIVE.with(|a| {
        let a = a.borrow();
        visible_nodes(&a.nodes, &a.expanded)
    });
    win.global::<PreviewState>()
        .set_archive_nodes(ModelRc::new(VecModel::from(rows)));
}

/// 由完整树 + 展开集合计算可见行（纯函数，便于单测）
fn visible_nodes(nodes: &[ArchiveTreeNode], expanded: &HashSet<String>) -> Vec<ArchiveNode> {
    let mut out = Vec::new();
    // 折叠目录的路径前缀：其后代一律跳过。
    // 树是深度优先顺序，遇到折叠目录后，直到出现层级不深于它的节点为止都是其后代。
    let mut skip_below: Option<i32> = None;
    for n in nodes {
        if let Some(level) = skip_below {
            if n.level > level {
                continue;
            }
            skip_below = None;
        }
        let expanded_now = n.is_dir && expanded.contains(&n.full_path);
        out.push(ArchiveNode {
            name: SharedString::from(n.name.as_str()),
            full_path: SharedString::from(n.full_path.as_str()),
            size: SharedString::from(metadata::human_size(n.size)),
            is_dir: n.is_dir,
            level: n.level,
            expanded: expanded_now,
            has_children: n.has_children,
        });
        if n.is_dir && n.has_children && !expanded_now {
            skip_below = Some(n.level);
        }
    }
    out
}

/// 关闭预览窗口（隐藏并清理归档状态与大内存占用）。
/// PreviewWindow 常驻隐藏复用，不清内容的话大图位图与文本层会一直驻留。
pub fn hide() {
    if let Some(w) = window() {
        let st = w.global::<PreviewState>();
        st.set_loading(false);
        st.set_office_pending(false);
        st.set_has_image(false);
        st.set_preview_image(slint::Image::default());
        st.set_text_content("".into());
        st.set_code_kw("".into());
        st.set_code_str("".into());
        st.set_code_cmt("".into());
        st.set_has_thumb(false);
        st.set_thumb(slint::Image::default());
        let _ = w.hide();
    }
    ARCHIVE.with(|a| *a.borrow_mut() = ArchiveView::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, full: &str, level: i32, is_dir: bool, has_children: bool) -> ArchiveTreeNode {
        ArchiveTreeNode {
            name: name.to_string(),
            full_path: full.to_string(),
            size: 0,
            is_dir,
            level,
            has_children,
        }
    }

    /// 全部折叠时只剩根级节点
    #[test]
    fn collapsed_shows_root_level_only() {
        let nodes = vec![
            node("dir", "dir", 0, true, true),
            node("sub", "dir/sub", 1, true, true),
            node("deep.txt", "dir/sub/deep.txt", 2, false, false),
            node("inner.txt", "dir/inner.txt", 1, false, false),
            node("a.txt", "a.txt", 0, false, false),
        ];
        let vis = visible_nodes(&nodes, &HashSet::new());
        let names: Vec<String> = vis.iter().map(|n| n.name.to_string()).collect();
        assert_eq!(names, vec!["dir", "a.txt"]);
        assert!(!vis[0].expanded);
    }

    /// 展开一级目录只显示其直接子项，孙层仍隐藏
    #[test]
    fn expanding_one_level_reveals_direct_children() {
        let nodes = vec![
            node("dir", "dir", 0, true, true),
            node("sub", "dir/sub", 1, true, true),
            node("deep.txt", "dir/sub/deep.txt", 2, false, false),
            node("inner.txt", "dir/inner.txt", 1, false, false),
        ];
        let mut expanded = HashSet::new();
        expanded.insert("dir".to_string());
        let vis = visible_nodes(&nodes, &expanded);
        let names: Vec<String> = vis.iter().map(|n| n.name.to_string()).collect();
        assert_eq!(names, vec!["dir", "sub", "inner.txt"]);
        assert!(vis[0].expanded);
        assert!(!vis[1].expanded);

        // 再展开子目录，孙层随之可见
        expanded.insert("dir/sub".to_string());
        let vis = visible_nodes(&nodes, &expanded);
        let names: Vec<String> = vis.iter().map(|n| n.name.to_string()).collect();
        assert_eq!(names, vec!["dir", "sub", "deep.txt", "inner.txt"]);
    }

    /// 折叠父目录时，即使子目录仍在展开集合中也不应泄漏出来
    #[test]
    fn collapsed_parent_hides_expanded_child() {
        let nodes = vec![
            node("dir", "dir", 0, true, true),
            node("sub", "dir/sub", 1, true, true),
            node("deep.txt", "dir/sub/deep.txt", 2, false, false),
        ];
        let mut expanded = HashSet::new();
        expanded.insert("dir/sub".to_string());
        let vis = visible_nodes(&nodes, &expanded);
        let names: Vec<String> = vis.iter().map(|n| n.name.to_string()).collect();
        assert_eq!(names, vec!["dir"]);
    }

    /// 空目录不带箭头，也不影响后续同层节点可见性
    #[test]
    fn empty_dir_does_not_swallow_siblings() {
        let nodes = vec![
            node("empty", "empty", 0, true, false),
            node("b.txt", "b.txt", 0, false, false),
        ];
        let vis = visible_nodes(&nodes, &HashSet::new());
        let names: Vec<String> = vis.iter().map(|n| n.name.to_string()).collect();
        assert_eq!(names, vec!["empty", "b.txt"]);
        assert!(!vis[0].has_children);
    }
}
