; Inno Setup 脚本：Bypass Tool 安装包
; 构建方式：iscc /DMyAppVersion=x.y.z installer\BypassTool.iss
; （CI 中由 .github/workflows/release.yml 传入版本号；未传时回退到 0.1.0）
;
; 安装过程：
;   1. 复制 bypass-core.exe / bypass-ui.exe 到 {app}
;   2. 执行 bypass-core.exe --install-service 注册 Windows 服务
;   3. 启动 BypassToolCore 服务
; 卸载过程：
;   1. 停止并删除服务
;   2. 删除程序文件（保留 %ProgramData%\BypassTool\ 下的配置与日志）

#ifndef MyAppVersion
#define MyAppVersion "0.1.0"
#endif

#define MyAppName "Bypass Tool"
#define MyAppPublisher "BypassTool"
#define MyAppExeName "bypass-ui.exe"
#define CoreExeName "bypass-core.exe"
#define ServiceName "BypassToolCore"

[Setup]
AppId={{7C1E4E8A-9B2F-4D6A-A5C3-1F0E2D3B4A5C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\BypassTool
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; 需要管理员权限安装服务
PrivilegesRequired=admin
OutputBaseFilename=BypassTool-Setup-{#MyAppVersion}
Compression=lzma
SolidCompression=yes
WizardStyle=modern
; Windows 10/11 x64
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
; 安装包自身图标（向导标题栏 + 生成的 Setup exe 文件图标）
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}

[Languages]
; 注意：Inno Setup 6 官方安装包并不自带 ChineseSimplified.isl（内置语言里没有中文），
; 直接写 compiler:Languages\ChineseSimplified.isl 会导致 ISCC 报
; "Couldn't open include file ..." 并中断编译，进而 CI 找不到安装包产物。
; 因此这里把官方翻译文件随仓库一起提供，用相对于本脚本的路径引用（CI 环境无关）。
Name: "chinesesimplified"; MessagesFile: "Languages\ChineseSimplified.isl"

[Files]
Source: "..\target\x86_64-pc-windows-msvc\release\bypass-core.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\x86_64-pc-windows-msvc\release\bypass-ui.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "开机自动启动 bypass-ui 托盘"; GroupDescription: "其他选项："; Flags: unchecked

[Run]
; 注册服务（幂等：先停旧再装新）
Filename: "net"; Parameters: "stop {#ServiceName}"; Flags: runhidden runasoriginaluser; Check: ServiceExists()
Filename: "{app}\{#CoreExeName}"; Parameters: "--uninstall-service"; Flags: runhidden; Check: ServiceExists()
Filename: "{app}\{#CoreExeName}"; Parameters: "--install-service"; Flags: runhidden; StatusMsg: "正在安装 BypassToolCore 服务..."
Filename: "sc"; Parameters: "start {#ServiceName}"; Flags: runhidden; StatusMsg: "正在启动服务..."
; UI 托盘（默认勾选立即运行）
; runasoriginaluser：安装包是管理员权限运行的，不加这个标志 UI 会以提权/管理员身份启动，
; 托盘图标可能落在另一个会话里，用户桌面上就看不到（还带着不必要的管理员权限）。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "sc"; Parameters: "stop {#ServiceName}"; Flags: runhidden; RunOnceId: "StopSvc"
Filename: "{app}\{#CoreExeName}"; Parameters: "--uninstall-service"; Flags: runhidden; RunOnceId: "DelSvc"

[UninstallDelete]
; 卸载时不清理 %ProgramData%\BypassTool（保留用户配置与日志）

[Registry]
; 开机自启 bypass-ui（HKEY_CURRENT_USER，卸载时删除）
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "BypassToolUI"; ValueData: """{app}\{#MyAppExeName}"""; Flags: uninsdeletevalue; Tasks: autostart

[Code]
// 检测服务是否已安装（用于幂等升级安装）。
function ServiceExists(): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec('sc', 'query {#ServiceName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode)
    and (ResultCode = 0);
end;
