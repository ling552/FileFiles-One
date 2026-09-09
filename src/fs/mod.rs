//! 文件系统核心模块

pub mod clipboard;
pub mod disk;
// OLE 拖出：把选中文件拖拽到其他应用（DoDragDrop + Shell 数据对象）
pub mod drag_out;
// 受保护目录文件操作：权限被拒绝时按需请求 UAC
pub mod elevated;
pub mod hash;
// 分层语法高亮：曾用于空格预览多层染色。Slint 单元素不支持富文本、
// 多层叠加在混合中英文时基线漂移，预览已回退单层纯文本；模块保留备用
#[allow(dead_code)]
pub mod highlight;
pub mod video_preview;
// 文件名索引：后台重建（带进度）+ 深层搜索加速
pub mod index;
pub mod metadata;
pub mod network;
pub mod openwith;
pub mod operations;
// 空格键 Quick Look 预览内容计算（图片/文本/文件夹/其它归类）
pub mod preview;
// Office 高保真预览：调用本机 Office 转 PDF 后由 WebView2 渲染
pub mod office_preview;
// Quick Look 网页渲染视图：WebView2 渲染 HTML/PHP/Markdown（源码/渲染切换）
pub mod recyclebin;
pub mod web_preview;
// PE 文件 Authenticode 数字签名检测（字节解析，跨平台）
pub mod signature;
// 后台文件任务队列：复制 / 移动异步执行，带真实进度、暂停、取消
pub mod tasks;
// 目录实时监听：当前目录内容变化时自动刷新（基于 notify，无需手动 F5）
pub mod watcher;
// Windows 原生 Shell 右键菜单：IContextMenu + TrackPopupMenuEx（含第三方注册项）
pub mod shell_menu;
// 侧栏导航项专用轻量菜单，避免展示文件区第三方扩展项
pub mod sidebar_menu;
// Windows ShellNew 注册表枚举：读取系统右键"新建"菜单项
pub mod shell_new;
// 缩略图/系统图标统一提取：IShellItemImageFactory::GetImage（与资源管理器一致）
pub mod thumbnail;
// Windows 快速访问：Shell 命名空间枚举 + pintohome/unpinfromhome 动词
pub mod default_app;
pub mod devices;
pub mod quickaccess;
pub mod virtualfs;
// 云存储虚拟文件系统：FTP / WebDAV / SFTP
pub mod cloud;

use std::path::PathBuf;

/// 导航历史：维护后退/前进栈
pub struct NavigationHistory {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    current: PathBuf,
}

impl NavigationHistory {
    pub fn new(start: PathBuf) -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
            current: start,
        }
    }

    pub fn current(&self) -> &PathBuf {
        &self.current
    }

    /// 跳转到新路径（清空前进栈）
    pub fn navigate(&mut self, path: PathBuf) {
        if path == self.current {
            return;
        }
        self.back.push(self.current.clone());
        self.current = path;
        self.forward.clear();
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub fn go_back(&mut self) -> Option<&PathBuf> {
        if let Some(prev) = self.back.pop() {
            self.forward.push(self.current.clone());
            self.current = prev;
            Some(&self.current)
        } else {
            None
        }
    }

    pub fn go_forward(&mut self) -> Option<&PathBuf> {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.current.clone());
            self.current = next;
            Some(&self.current)
        } else {
            None
        }
    }
}
