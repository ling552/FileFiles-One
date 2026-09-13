//! 无窗口子进程：安装后任何操作都不弹出终端窗口。
//!
//! Windows 上 `std::process::Command` 默认会为控制台子系统程序
//! （powershell.exe / rclone.exe / cmd.exe 等）创建一个可见的控制台窗口，
//! 表现为“操作时闪出一个黑框”。本模块统一提供带 `CREATE_NO_WINDOW`
//! 标志的构造入口，所有后台子进程（Office 转 PDF 的 PowerShell、
//! 应用更新的安装程序、rclone 挂载/列表等）必须经由此处创建，
//! 确保安装后的 release 版（windows_subsystem = windows）在任何操作下
//! 都不打开终端窗口。

use std::process::Command;

/// 创建默认隐藏控制台窗口的子进程命令。
/// 非 Windows 平台直接返回普通 Command（无窗口概念）。
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000：不为子进程创建控制台窗口，
        // 后台运行时不再闪出黑框；配合 stdin/stdout/stderr 重定向到 null 使用。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_command_builds_without_window_flag_side_effects() {
        // 仅验证构造不 panic，且程序名正确透传（不实际 spawn）
        let cmd = hidden_command("explorer.exe");
        assert_eq!(cmd.get_program().to_string_lossy(), "explorer.exe");
    }
}
