//! 虚拟路径机制：把标签 / 回收站 / 网络位置 / 此电脑统一为可在应用内浏览的虚拟目录。

use super::metadata::{classify, unix_ts, Entry};
use crate::config::AppConfig;
use std::path::Path;
use std::time::SystemTime;

pub const THIS_PC_PATH: &str = "this-pc://";

/// 标签 key 与中文标签的映射
pub const TAG_KEYS: [(&str, &str); 3] = [
    ("important", "重要"),
    ("archive", "待归档"),
    ("done", "已完成"),
];

/// 判断是否为虚拟路径（包含云存储 cloud://）
pub fn is_virtual(path: &str) -> bool {
    path == THIS_PC_PATH
        || path.starts_with("tag://")
        || path.starts_with("recycle://")
        || path.starts_with("network://")
        || path.starts_with("device://")
        || path.starts_with("cloud://")
}

/// 虚拟路径的友好标题（用于面包屑 / 标题栏）
pub fn friendly_title(path: &str) -> String {
    if path == THIS_PC_PATH {
        "此电脑".to_string()
    } else if let Some(key) = path.strip_prefix("tag://") {
        let label = TAG_KEYS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, l)| *l)
            .unwrap_or(key);
        format!("标签 · {}", label)
    } else if path == "recycle://" {
        "回收站".to_string()
    } else if path == "network://" {
        "网络位置".to_string()
    } else if let Some(rest) = path.strip_prefix("cloud://") {
        // cloud://kind/name[/sub] -> 显示账号名与子路径
        let mut parts = rest.splitn(3, '/');
        let _kind = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        let sub = parts.next().unwrap_or("");
        if sub.is_empty() {
            name.to_string()
        } else {
            format!("{} / {}", name, sub)
        }
    } else if let Some(rest) = path.strip_prefix("device://") {
        // 显示设备友好名（来自最近一次枚举的缓存）；设备内部层级同样显示设备名
        // （MTP 对象 ID 不可读）。缓存未命中（如重启后恢复标签）退回通用名称。
        let dev_id = rest.split(super::devices::SEP).next().unwrap_or(rest);
        super::devices::friendly_name(dev_id).unwrap_or_else(|| "便携设备".to_string())
    } else {
        path.to_string()
    }
}

/// 解析虚拟路径并生成条目列表。返回 None 表示不是虚拟路径。
pub fn resolve(path: &str, config: &mut AppConfig) -> Option<Vec<Entry>> {
    if path == THIS_PC_PATH {
        let mut entries = super::disk::disk_entries();
        entries.extend(super::devices::list_devices());
        // 云存储根：每个 FTP/WebDAV/SFTP 账号作为可进入的文件夹
        entries.extend(super::cloud::list_cloud_roots(config));
        Some(entries)
    } else if path.starts_with("cloud://") {
        Some(super::cloud::list_cloud_dir(path, config))
    } else if path.starts_with("device://") {
        // WPD/MTP 设备内部浏览：枚举该对象下的子文件夹/文件
        Some(super::devices::list_content(path))
    } else if let Some(key) = path.strip_prefix("tag://") {
        Some(tag_entries(key, config))
    } else if path == "recycle://" {
        Some(super::recyclebin::list_recycle_bin())
    } else if path == "network://" {
        let mut entries = super::network::list_network_drives();
        entries.extend(super::network::list_saved(&config.network_locations));
        // 云存储同样在网络位置中聚合显示
        entries.extend(super::cloud::list_cloud_roots(config));
        Some(entries)
    } else {
        None
    }
}

/// 标签视图：列出所有打了该标签的文件/文件夹；路径失效则顺带清理。
fn tag_entries(key: &str, config: &mut AppConfig) -> Vec<Entry> {
    let paths = config.paths_with_tag(key);
    let mut entries = Vec::new();
    let mut dead: Vec<String> = Vec::new();

    for p in paths {
        let path = Path::new(&p);
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                dead.push(p.clone());
                continue;
            }
        };
        let is_dir = meta.is_dir();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.clone());
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let (icon_class, icon_label, kind) = classify(path, is_dir);
        entries.push(Entry {
            name,
            path: p.clone(),
            is_dir,
            size_bytes: if is_dir { 0 } else { meta.len() },
            modified_ts: unix_ts(modified),
            kind,
            icon_label,
            icon_class,
        });
    }

    if !dead.is_empty() {
        for d in &dead {
            config.remove_path_tags(d);
        }
        config.save();
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_pc_path_is_virtual_and_has_friendly_title() {
        assert_eq!(THIS_PC_PATH, "this-pc://");
        assert!(is_virtual(THIS_PC_PATH));
        assert_eq!(friendly_title(THIS_PC_PATH), "此电脑");
    }
}
