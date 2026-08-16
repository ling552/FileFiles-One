//! Windows ShellNew 注册表枚举：读取系统右键"新建"菜单项
//! 扫描 HKEY_CLASSES_ROOT\\.{ext}\\ShellNew，提取模板信息供"新增"按钮使用。

use std::path::PathBuf;
use winreg::enums::*;
use winreg::RegKey;

/// 单个"新建"模板项
#[derive(Clone, Debug)]
pub struct ShellNewItem {
    /// 显示名称（从文件类型友好名称推导，如"文本文档"）
    pub name: String,
    /// 文件扩展名（含点，如".txt"）
    pub ext: String,
    /// 模板类型：FileName（复制模板文件）、NullFile（创建空文件）、Data（写入注册表字节）
    pub kind: ShellNewKind,
    /// FileName 模式：模板文件绝对路径（需展开环境变量）
    pub template_path: Option<String>,
    /// Data 模式：写入的初始字节内容（Base64 或直接二进制）
    pub data: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShellNewKind {
    /// 复制模板文件到目标（FileName 值指向模板路径）
    FileName,
    /// 创建空文件（NullFile 值存在即可）
    NullFile,
    /// 写入注册表 Data 值到新文件（较少见）
    Data,
}

/// 枚举系统注册表中所有 ShellNew 项，返回可用的"新建"模板列表。
/// 优先返回常用项（文本文档、文件夹、Office 文档等）。
pub fn enumerate() -> Vec<ShellNewItem> {
    let mut items = Vec::new();

    // 文件夹特殊项（非注册表，直接内置）
    items.push(ShellNewItem {
        name: "文件夹".to_string(),
        ext: String::new(),
        kind: ShellNewKind::NullFile,
        template_path: None,
        data: None,
    });

    // 遍历 HKEY_CLASSES_ROOT 的所有扩展名键
    let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
    let Ok(keys) = hkcr.enum_keys().collect::<Result<Vec<_>, _>>() else {
        return items;
    };

    for key_name in keys {
        // 仅处理扩展名键（以 . 开头）
        if !key_name.starts_with('.') {
            continue;
        }

        let Ok(ext_key) = hkcr.open_subkey(&key_name) else {
            continue;
        };

        // ShellNew 注册有三种位置，按优先级查找第一个命中的：
        //   1. .ext\ShellNew                （经典位置，如 .rtf）
        //   2. <默认ProgID>\ShellNew         （如 .docx → Word.Document.12\ShellNew）
        //   3. .ext\<子ProgID>\ShellNew      （部分软件注册在扩展名下的 ProgID 子键里）
        // 仅取一处，避免同一扩展名重复出现。
        let shell_new_key = find_shell_new(&hkcr, &ext_key);
        let Some(shell_new_key) = shell_new_key else {
            continue;
        };

        // 读取友好名称（从 .ext 的默认值读取 ProgID，再从 ProgID 读取友好名称）
        let friendly_name = get_friendly_name(&hkcr, &key_name)
            .unwrap_or_else(|| format!("{} 文件", key_name.trim_start_matches('.').to_uppercase()));

        // 检测 ShellNew 子键的值类型
        let kind = if shell_new_key.get_value::<String, _>("FileName").is_ok() {
            ShellNewKind::FileName
        } else if shell_new_key.get_value::<String, _>("NullFile").is_ok() {
            ShellNewKind::NullFile
        } else if shell_new_key.get_raw_value("Data").is_ok() {
            ShellNewKind::Data
        } else {
            continue; // 无有效 ShellNew 值，跳过
        };

        let template_path = if kind == ShellNewKind::FileName {
            shell_new_key
                .get_value::<String, _>("FileName")
                .ok()
                .map(|p| expand_env_string(&p))
        } else {
            None
        };

        let data = if kind == ShellNewKind::Data {
            shell_new_key.get_raw_value("Data").ok().map(|v| v.bytes)
        } else {
            None
        };

        items.push(ShellNewItem {
            name: friendly_name,
            ext: key_name.clone(),
            kind,
            template_path,
            data,
        });
    }

    // 同一扩展名可能在多处命中（极少），按扩展名去重，保留首个
    items.sort_by(|a, b| a.ext.cmp(&b.ext));
    items.dedup_by(|a, b| a.ext == b.ext);

    // 排序：常用项置顶（文本文档 .txt、Word .docx、Excel .xlsx 等），其余按名称排序
    items.sort_by(|a, b| {
        let a_priority = priority(&a.ext);
        let b_priority = priority(&b.ext);
        if a_priority != b_priority {
            a_priority.cmp(&b_priority)
        } else {
            a.name.cmp(&b.name)
        }
    });

    items
}

/// 在给定扩展名键下按三种位置查找 ShellNew 子键（见 enumerate 注释）。
fn find_shell_new(hkcr: &RegKey, ext_key: &RegKey) -> Option<RegKey> {
    // 1. .ext\ShellNew
    if let Ok(k) = ext_key.open_subkey("ShellNew") {
        return Some(k);
    }

    // 2. 默认 ProgID\ShellNew（如 Word.Document.12\ShellNew）
    if let Ok(prog_id) = ext_key.get_value::<String, _>("") {
        if !prog_id.is_empty() {
            if let Ok(prog_key) = hkcr.open_subkey(&prog_id) {
                if let Ok(k) = prog_key.open_subkey("ShellNew") {
                    return Some(k);
                }
            }
        }
    }

    // 3. .ext\<子ProgID>\ShellNew
    if let Ok(sub_names) = ext_key.enum_keys().collect::<Result<Vec<_>, _>>() {
        for sub in sub_names {
            // ShellNew/PersistentHandler/OpenWithProgids 等非 ProgID 子键直接跳过其自身的同名子键判断由 open 决定
            if let Ok(sub_key) = ext_key.open_subkey(&sub) {
                if let Ok(k) = sub_key.open_subkey("ShellNew") {
                    return Some(k);
                }
            }
        }
    }

    None
}

/// 常用扩展名优先级（越小越靠前）
fn priority(ext: &str) -> u8 {
    match ext {
        "" => 0, // 文件夹
        ".txt" => 1,
        ".docx" | ".doc" => 2,
        ".xlsx" | ".xls" => 3,
        ".pptx" | ".ppt" => 4,
        ".zip" => 5,
        ".jpg" | ".png" => 6,
        _ => 99,
    }
}

/// 从扩展名键推导友好名称（.ext 默认值 → ProgID → 友好名称）
fn get_friendly_name(hkcr: &RegKey, ext: &str) -> Option<String> {
    let ext_key = hkcr.open_subkey(ext).ok()?;
    let prog_id: String = ext_key.get_value("").ok()?;
    if prog_id.is_empty() {
        return None;
    }
    let prog_key = hkcr.open_subkey(&prog_id).ok()?;
    prog_key.get_value::<String, _>("").ok()
}

/// 展开环境变量字符串（如 %WINDIR%\notepad.exe）
fn expand_env_string(s: &str) -> String {
    // 简单实现：仅展开 %WINDIR% 和 %SYSTEMROOT%（完整实现需调用 ExpandEnvironmentStringsW）
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    s.replace("%WINDIR%", &windir)
        .replace("%windir%", &windir)
        .replace("%SYSTEMROOT%", &windir)
        .replace("%systemroot%", &windir)
}

/// 根据 ShellNewItem 在指定目录创建新文件，返回创建的文件路径。
/// 自动处理文件名冲突（添加 (2) (3) 后缀）。
pub fn create_item(parent: &std::path::Path, item: &ShellNewItem) -> std::io::Result<PathBuf> {
    if item.ext.is_empty() {
        // 文件夹
        return crate::fs::operations::new_folder(parent, "新建文件夹");
    }

    // 基础文件名（不含扩展名）
    let base_name = if item.name.contains("文本") || item.ext == ".txt" {
        "新建文本文档"
    } else if item.name.contains("Word") || item.ext.contains("doc") {
        "新建 Microsoft Word 文档"
    } else if item.name.contains("Excel") || item.ext.contains("xls") {
        "新建 Microsoft Excel 工作表"
    } else if item.name.contains("PowerPoint") || item.ext.contains("ppt") {
        "新建 Microsoft PowerPoint 演示文稿"
    } else {
        // 通用：以友好名称构造（如"新建 RTF 格式"），避免所有未知类型都叫"新建文件"
        return create_with_base(parent, &format!("新建 {}", item.name), item);
    };

    create_with_base(parent, base_name, item)
}

/// 按给定基础文件名（不含扩展名）创建 ShellNew 文件，处理重名去重与模板写入。
fn create_with_base(
    parent: &std::path::Path,
    base_name: &str,
    item: &ShellNewItem,
) -> std::io::Result<PathBuf> {
    // 以 create_new 独占创建并按序号避让：exists 检查与写入之间存在竞态，
    // 并发场景会覆盖竞态窗口内刚出现的同名文件
    let (mut file, target) = claim_new_file(parent, base_name, &item.ext)?;

    // 按模板类型填充内容（NullFile 保持空文件即可）
    use std::io::Write;
    match &item.kind {
        ShellNewKind::NullFile => {}
        ShellNewKind::FileName => {
            if let Some(template) = &item.template_path {
                let tpl = PathBuf::from(template);
                if tpl.exists() {
                    let mut src = std::fs::File::open(&tpl)?;
                    std::io::copy(&mut src, &mut file)?;
                }
                // 模板文件不存在，回退到空文件（create_new 已创建空文件）
            }
        }
        ShellNewKind::Data => {
            file.write_all(item.data.as_deref().unwrap_or(&[]))?;
        }
    }

    Ok(target)
}

/// 独占创建 `base_name + 序号 + ext` 文件，重名时从 2 起追加 " (n)" 避让。
/// 返回写入句柄与最终路径。
fn claim_new_file(
    parent: &std::path::Path,
    base_name: &str,
    ext: &str,
) -> std::io::Result<(std::fs::File, PathBuf)> {
    let mut final_name = format!("{base_name}{ext}");
    let mut n = 2;
    loop {
        let target = parent.join(&final_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(f) => return Ok((f, target)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if n > 999 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "文件名冲突过多",
                    ));
                }
                final_name = format!("{base_name} ({n}){ext}");
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}
