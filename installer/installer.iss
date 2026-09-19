; FileFiles One Windows 安装程序脚本(Inno Setup 6)
; CI 中通过 /DAppVersion=x.y.z 传入版本号;本地手动编译时使用下方默认值
#ifndef AppVersion
  #define AppVersion "0.5.7"
#endif

[Setup]
; 固定 AppId 保证升级安装时覆盖旧版本而非并存
AppId={{B7E4F3A2-9C51-4D8E-A6B0-3F2D1E5C8A47}
AppName=FileFiles One
AppVersion={#AppVersion}
AppPublisher=ling552
AppPublisherURL=https://github.com/ling552/FileFiles-One
AppSupportURL=https://github.com/ling552/FileFiles-One/issues
AppUpdatesURL=https://github.com/ling552/FileFiles-One/releases
DefaultDirName={autopf}\FileFiles One
DefaultGroupName=FileFiles One
UninstallDisplayIcon={app}\filefiles-one.exe
OutputBaseFilename=FileFiles-One-Setup-{#AppVersion}
OutputDir=Output
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10/11 x64：应用使用系统内置 API/WebView2；VC Runtime 已由 Cargo 静态链接。
MinVersion=10.0.17763
WizardStyle=modern
; 安装程序自身图标由根目录 icon.png 生成，需与应用图标同步更新
SetupIconFile=icon.ico
; 应用内更新场景:安装前自动关闭正在运行的 FileFiles One
CloseApplications=yes
; 默认装到 Program Files(需管理员);无管理员权限时允许降级装到用户目录
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\target\release\filefiles-one.exe"; DestDir: "{app}"; Flags: ignoreversion
; 离线恢复入口：应用无法启动时也可安全恢复仍由本应用持有的文件管理器关联。
Source: "Restore-DefaultFileManager.ps1"; DestDir: "{app}"; Flags: ignoreversion
; 内嵌 rclone（WebDAV 挂载为虚拟磁盘，挂载后图标与 D:/H: 等数据盘一致）：
; 构建机如已备好 thirdparty\rclone.exe 则一并打包到程序目录随装机落地；
; 若构建时缺失也不阻断安装——应用首次挂载 WebDAV 时会自动后台下载
; （ureq 直链 + zip 解压，全程 CREATE_NO_WINDOW 无终端窗口）到用户目录，
; 用户无需手动安装 rclone。
Source: "..\thirdparty\rclone.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; WinFsp（rclone mount Windows 必需的系统驱动）不在此捆绑：需管理员安装且涉及重启，
; 应用在挂载前会检测并指引到官网（https://winfsp.dev/rel/），缺失时自动回退原生 WebDAV 浏览。

[Icons]
Name: "{group}\FileFiles One"; Filename: "{app}\filefiles-one.exe"
Name: "{group}\恢复默认文件管理器"; Filename: "powershell.exe"; Parameters: "-NoProfile -File ""{app}\Restore-DefaultFileManager.ps1"""; WorkingDir: "{app}"; IconFilename: "{app}\filefiles-one.exe"
Name: "{group}\{cm:UninstallProgram,FileFiles One}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\FileFiles One"; Filename: "{app}\filefiles-one.exe"; Tasks: desktopicon

[Run]
; 安装完成后可勾选立即启动(应用内更新流程:安装结束直接回到新版本)
Filename: "{app}\filefiles-one.exe"; Description: "{cm:LaunchProgram,FileFiles One}"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
    Exec(ExpandConstant('{app}\filefiles-one.exe'),
      '--unregister-default-file-manager', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
