# FileFiles One

现代化 Windows 桌面文件管理器,使用 **Rust + Slint** 实现。

[![Platform](https://img.shields.io/badge/platform-Windows-blue)](https://github.com/ling552/FileFiles-One/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](https://github.com/ling552/FileFiles-One/blob/main/LICENSE)

![ico](icon.webp)

## 功能特性

- 📁 单/双面板文件浏览,列表与网格视图,标签页
- 🎨 亚克力磨砂质感界面,深浅色主题,多主题色
- 🔍 实时过滤搜索、命令面板、路径补全
- 📦 ZIP / 7z / TAR / tar.gz 归档压缩与解压(后台任务 + 实时进度)
- 🖼️ 系统图标 / 缩略图 / 视频首帧、空格键 Quick Look 快速预览
- 🖱️ 拖拽与剪贴板互通:从微信/浏览器/资源管理器拖入文件即复制到当前目录,选中项可拖出发送到其他应用,复制/剪切的文件可在微信聊天等外部应用中直接粘贴
- 🔧 属性对话框:常规 / 安全(ACL)/ 详细信息(EXIF、音频标签)/ 数字签名
- 🌿 Git 集成:分支显示与文件状态徽章
- ♻️ 回收站、快速访问、网络位置、设备管理
- 🔄 内置更新:从 GitHub Releases 检查并下载更新
- ⌨️ 完整快捷键:复制/剪切/粘贴/撤销/重做/重命名/全选等

## 安装

从 [Releases](../../releases) 页面下载最新的 `FileFiles-One-Setup-x.y.z.exe` 安装程序。

## 从源码构建

需要 Rust 工具链(stable):

```bash
cargo build --release
```

产物位于 `target/release/filefiles-one.exe`。

## 恢复默认文件管理器

若卸载或应用无法启动后仍需恢复原来的文件夹关联，请从对应版本 Release 下载 `Restore-DefaultFileManager.ps1`，在 PowerShell 中执行：

```powershell
cd C:\Users\<your-name>\Downloads    #此处改为脚本下载地址
powershell -ExecutionPolicy Bypass -File .\Restore-DefaultFileManager.ps1
```

脚本仅在关联仍带有 `FileFilesOneOwner` 标记、且命令精确为 FileFiles One 的受管格式时恢复备份；已改用其它文件管理器或命令被修改的项会跳过，不会覆盖。

## 许可证

[MIT](LICENSE)
