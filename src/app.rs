//! 应用状态：多标签页架构，每个标签页持有独立的导航历史、条目与选择状态

use crate::config::AppConfig;
use crate::fs::{metadata::Entry, NavigationHistory};
use std::path::PathBuf;

/// 剪贴板操作类型
#[derive(Clone, Copy, PartialEq)]
pub enum ClipMode {
    None,
    Copy,
    Cut,
}

/// 可撤销操作记录（Ctrl+Z 撤销 / Ctrl+Shift+Z 重做）。
/// 每个变体都同时描述正向与逆向，故可双向执行。
pub enum UndoAction {
    /// 重命名：orig 原完整路径、renamed 新完整路径（同目录）。
    /// 撤销 renamed→orig；重做 orig→renamed。
    Rename { orig: PathBuf, renamed: PathBuf },
    /// 新建：撤销=移入回收站，重做=从回收站按原路径还原。
    Create { path: PathBuf },
    /// 移动 / 剪切粘贴：(源完整路径, 目标完整路径) 列表。
    /// 撤销 dst→src；重做 src→dst。
    Move { pairs: Vec<(PathBuf, PathBuf)> },
    /// 删除（移入回收站）：被删除项的原完整路径列表。
    /// 撤销=从回收站还原；重做=再次移入回收站。
    Delete { paths: Vec<PathBuf> },
}

/// 标签页类型：普通文件浏览 / 设置页
#[derive(Clone, Copy, PartialEq)]
pub enum TabKind {
    Files,
    Settings,
}

/// 单个标签页的完整会话状态
pub struct TabSession {
    pub kind: TabKind,
    pub history: NavigationHistory,
    pub entries: Vec<Entry>,         // 当前目录全部条目（未过滤）
    pub filtered: Vec<usize>,        // 过滤后在 entries 中的索引（搜索用）
    pub selected: Vec<bool>,         // 与 filtered 等长的选中标记
    pub last_clicked: Option<usize>, // 用于 shift 范围选择（filtered 下标）
    pub search: String,
    pub sort_key: String,
    pub sort_asc: bool,
    /// 排序时文件夹是否始终优先（由设置 folders_first 驱动，load 时写入）
    pub folders_first: bool,
}

impl TabSession {
    pub fn new(start: PathBuf) -> Self {
        Self {
            kind: TabKind::Files,
            history: NavigationHistory::new(start),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: Vec::new(),
            last_clicked: None,
            search: String::new(),
            sort_key: "name".into(),
            sort_asc: true,
            folders_first: true,
        }
    }

    /// 创建设置标签页（不读取目录，仅用于承载设置界面）
    pub fn new_settings(start: PathBuf) -> Self {
        let mut s = Self::new(start);
        s.kind = TabKind::Settings;
        s
    }

    /// 排序并应用搜索过滤，重建 filtered/selected
    pub fn rebuild(&mut self) {
        let key = self.sort_key.clone();
        let asc = self.sort_asc;
        let folders_first = self.folders_first;
        // 「此电脑」视图：本机硬盘/移动硬盘/U盘等真实盘符在前，便携设备(手机等)在最后。
        // 若沿用按名称排序，卷标（如 "Data (D:)" < "Windows (C:)"）会导致 D 盘排在 C 盘前；
        // 若仅按路径字典序，device://（'d'）会插到 E:\ F:\ 等 U 盘前面。故先按是否便携设备分组。
        let is_this_pc =
            self.history.current().to_string_lossy() == crate::fs::virtualfs::THIS_PC_PATH;
        if is_this_pc {
            self.entries.sort_by(|a, b| {
                let a_dev = a.path.starts_with("device://");
                let b_dev = b.path.starts_with("device://");
                if a_dev != b_dev {
                    return a_dev.cmp(&b_dev); // 便携设备(true)排在后面
                }
                a.path.to_lowercase().cmp(&b.path.to_lowercase())
            });
        } else {
            self.entries.sort_by(|a, b| {
                if folders_first && a.is_dir != b.is_dir {
                    return b.is_dir.cmp(&a.is_dir); // 文件夹在前
                }
                let ord = match key.as_str() {
                    "size" => a.size_bytes.cmp(&b.size_bytes),
                    "modified" => a.modified_ts.cmp(&b.modified_ts),
                    "kind" => a.kind.cmp(&b.kind),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                };
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }

        let q = self.search.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();

        self.selected = vec![false; self.filtered.len()];
        self.last_clicked = None;
    }

    /// 取 filtered 下标对应的真实条目
    pub fn entry_at(&self, fi: usize) -> Option<&Entry> {
        self.filtered.get(fi).and_then(|&ei| self.entries.get(ei))
    }

    /// 当前所有选中项的真实路径
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .enumerate()
            .filter(|(_, &s)| s)
            .filter_map(|(fi, _)| self.entry_at(fi))
            .map(|e| PathBuf::from(&e.path))
            .collect()
    }

    pub fn first_selected(&self) -> Option<usize> {
        self.selected.iter().position(|&s| s)
    }

    /// 标签页标题（当前目录名或盘符；设置页固定为"设置"）
    pub fn title(&self) -> String {
        if self.kind == TabKind::Settings {
            return "设置".into();
        }
        let cur = self.history.current();
        let cur_str = cur.to_string_lossy();
        // 虚拟路径用友好标题：设备显示友好名而非原始 ID（swd#wpdbusenum...），
        // 此电脑/标签/回收站等同样取中文名
        if crate::fs::virtualfs::is_virtual(&cur_str) {
            return crate::fs::virtualfs::friendly_title(&cur_str);
        }
        cur.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cur.to_string_lossy().to_string())
    }
}

/// 应用核心状态：管理多个标签页、剪贴板（跨标签共享）
pub struct AppCore {
    pub tabs: Vec<TabSession>,
    pub active: usize,
    pub clipboard: Vec<PathBuf>,
    pub clip_mode: ClipMode,
    /// 已折叠的侧边栏分区标签集合
    pub collapsed_sections: std::collections::HashSet<String>,
    /// 持久化配置（布局尺寸 + 文件标签）
    pub config: AppConfig,
    /// 待执行的后台任务队列（复制 / 移动）
    pub task_queue: std::collections::VecDeque<crate::fs::tasks::Job>,
    /// 当前正在执行任务的控制句柄（暂停 / 取消）；None 表示空闲
    pub task_control: Option<std::sync::Arc<crate::fs::tasks::TaskControl>>,
    /// 双面板视图的右侧独立面板：自带导航历史 / 条目 / 选择，与活动标签互不影响
    pub right_pane: TabSession,
    /// 当前活跃标签目录的实时监听句柄；main() 初始化后注入，导航时更新监听路径
    pub watcher: Option<crate::fs::watcher::DirWatcher>,
    /// 同名冲突对话框桥：后台任务遇冲突时经此请求主线程弹窗并等待用户决策
    pub conflict_bridge: std::sync::Arc<crate::fs::tasks::ConflictBridge>,
    /// 撤销栈：最近的可逆操作（Ctrl+Z 弹栈执行逆操作）
    pub undo_stack: Vec<UndoAction>,
    /// 重做栈：被撤销的操作（Ctrl+Shift+Z 弹栈重放正向）
    pub redo_stack: Vec<UndoAction>,
    /// 待自动选中的完成路径（任务完成后存储，刷新目录后定位选中）
    pub pending_select: Vec<std::path::PathBuf>,
}

impl AppCore {
    pub fn new(start: PathBuf) -> Self {
        // 右侧面板默认也从主目录起步，后续独立导航
        let right_start = start.clone();
        Self {
            tabs: vec![TabSession::new(start)],
            active: 0,
            clipboard: Vec::new(),
            clip_mode: ClipMode::None,
            collapsed_sections: std::collections::HashSet::new(),
            config: AppConfig::load(),
            task_queue: std::collections::VecDeque::new(),
            task_control: None,
            right_pane: TabSession::new(right_start),
            watcher: None,
            conflict_bridge: std::sync::Arc::new(crate::fs::tasks::ConflictBridge::new()),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_select: Vec::new(),
        }
    }

    /// 记录一次新操作到撤销栈，并清空重做栈（标准编辑器语义：新操作使重做失效）。
    pub fn record_undo(&mut self, action: UndoAction) {
        self.undo_stack.push(action);
        self.redo_stack.clear();
    }

    // —— 按活动面板路由（双面板：right=true 作用于右面板，否则活动标签）——

    pub fn pane(&self, right: bool) -> &TabSession {
        if right {
            &self.right_pane
        } else {
            self.active_tab()
        }
    }

    pub fn pane_mut(&mut self, right: bool) -> &mut TabSession {
        if right {
            &mut self.right_pane
        } else {
            self.active_tab_mut()
        }
    }

    pub fn pane_selected_paths(&self, right: bool) -> Vec<PathBuf> {
        self.pane(right).selected_paths()
    }

    pub fn pane_entry_at(&self, right: bool, fi: usize) -> Option<&Entry> {
        self.pane(right).entry_at(fi)
    }

    // —— 标签页管理 ——

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_tab_title(&self) -> String {
        self.active_tab().title()
    }

    /// 创建新标签页（默认打开快速访问/主目录）
    pub fn new_tab(&mut self, path: PathBuf) -> usize {
        let idx = self.tabs.len();
        self.tabs.push(TabSession::new(path));
        self.active = idx;
        idx
    }

    /// 关闭标签页，返回新的活跃下标
    pub fn close_tab(&mut self, idx: usize) -> Option<usize> {
        if self.tabs.len() <= 1 {
            return None; // 不能关闭最后一个标签页
        }
        if idx >= self.tabs.len() {
            return None;
        }
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        if self.active > idx {
            self.active -= 1;
        }
        Some(self.active)
    }

    /// 切换到指定标签页
    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
        }
    }

    /// 打开设置标签页：若已存在则切换过去，否则新建一个并激活
    pub fn open_settings_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.kind == TabKind::Settings) {
            self.active = idx;
            return;
        }
        let start = self.active_tab().history.current().clone();
        let idx = self.tabs.len();
        self.tabs.push(TabSession::new_settings(start));
        self.active = idx;
    }

    /// 拖动重排：把 from 处的标签移动到 to 处，并让其保持为活动标签
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = to;
    }

    /// 下一个标签页（Ctrl+Tab 循环）
    pub fn next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// 上一个标签页（Ctrl+Shift+Tab）
    pub fn prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            if self.active == 0 {
                self.active = self.tabs.len() - 1;
            } else {
                self.active -= 1;
            }
        }
    }

    /// 获取活跃标签页的可变引用
    pub fn active_tab(&self) -> &TabSession {
        &self.tabs[self.active]
    }

    pub fn active_tab_mut(&mut self) -> &mut TabSession {
        &mut self.tabs[self.active]
    }

    // —— 委托给活跃标签页的方法（保持与原有调用兼容）——

    pub fn rebuild(&mut self) {
        self.active_tab_mut().rebuild();
    }

    pub fn entry_at(&self, fi: usize) -> Option<&Entry> {
        self.active_tab().entry_at(fi)
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.active_tab().selected_paths()
    }
}
