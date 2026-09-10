; Inno Setup 脚本：RouteTool 安装包
; 构建方式：iscc /DMyAppVersion=x.y.z installer\RouteTool.iss
; （CI 中由 .github/workflows/release.yml 传入版本号；未传时回退到 0.1.0）
;
; 安装过程：
;   1. 复制 route-tool-core.exe / route-tool-ui.exe 到 {app}
;   2. 执行 route-tool-core.exe --install-service 注册 Windows 服务
;   3. 启动 RouteToolCore 服务
; 卸载过程：
;   1. 停止并删除服务
;   2. 删除程序文件（保留 %ProgramData%\RouteTool\ 下的配置与日志）

#ifndef MyAppVersion
#define MyAppVersion "0.1.0"
#endif

#define MyAppName "RouteTool"
#define MyAppPublisher "RouteTool"
#define MyAppExeName "route-tool-ui.exe"
#define CoreExeName "route-tool-core.exe"
#define ServiceName "RouteToolCore"
; 升级兼容：旧版本（v0.1.13 及以前）使用 bypass-ui.exe / bypass-core.exe，
; 在 uninstall 与 prepare-to-install 中清理掉，避免残留 exe 占用 lock 干扰升级。
#define LegacyUiExe "bypass-ui.exe"
#define LegacyCoreExe "bypass-core.exe"

[Setup]
AppId={{7C1E4E8A-9B2F-4D6A-A5C3-1F0E2D3B4A5C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\RouteTool
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; 需要管理员权限安装服务
PrivilegesRequired=admin
OutputBaseFilename=RouteTool-Setup-{#MyAppVersion}
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
; ignoreversion: 总是覆盖（升级时替换旧 exe，不依赖版本比较）。
; restartreplace: 若目标文件正被占用（如服务仍在运行锁住 route-tool-core.exe），
;   标记为"重启后替换"而不是静默跳过，避免升级后仍跑旧二进制。
Source: "..\target\x86_64-pc-windows-msvc\release\{#CoreExeName}"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "..\target\x86_64-pc-windows-msvc\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion restartreplace

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Tasks]
; 开机自启改由应用内配置页管理（HKCU Run 键，值带 --tray 仅启动托盘），
; 不再在安装包里提供任务选项；旧安装写的 Run 键由 UI 启动时自愈/迁移。
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Run]
; 清理旧版本遗留服务（v0.1.3 及之前服务名为 BypassToolCore）：
; 应用改名后新服务叫 RouteToolCore，不清理的话升级会残留一个指向同一 exe 的僵尸服务。
Filename: "net"; Parameters: "stop BypassToolCore"; Flags: runhidden; Check: LegacyServiceExists()
Filename: "sc"; Parameters: "delete BypassToolCore"; Flags: runhidden; Check: LegacyServiceExists()
; 注册新服务（幂等：先停旧再装新）。
; 注意 net stop 不能带 runasoriginaluser：安装进程本就是提权运行的，
; 加了这个标志反而降权到原始用户令牌执行，停服务必然 Access Denied。
Filename: "net"; Parameters: "stop {#ServiceName}"; Flags: runhidden; Check: ServiceExists()
Filename: "{app}\{#CoreExeName}"; Parameters: "--uninstall-service"; Flags: runhidden; Check: ServiceExists()
Filename: "{app}\{#CoreExeName}"; Parameters: "--install-service"; Flags: runhidden; StatusMsg: "正在安装 RouteToolCore 服务..."
Filename: "sc"; Parameters: "start {#ServiceName}"; Flags: runhidden; StatusMsg: "正在启动服务..."
; UI 托盘（默认勾选立即运行）
; runasoriginaluser：安装包是管理员权限运行的，不加这个标志 UI 会以提权/管理员身份启动，
; 托盘图标可能落在另一个会话里，用户桌面上就看不到（还带着不必要的管理员权限）。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; 卸载顺序很关键：先杀 UI（否则托盘进程持有文件锁），再停服务，
; 服务停不掉就强杀 core 进程（否则 exe 被锁、文件删不掉 = 卸载不干净），
; 最后删除服务注册。
Filename: "taskkill"; Parameters: "/F /IM {#LegacyUiExe}"; Flags: runhidden; RunOnceId: "KillLegacyUI"
Filename: "taskkill"; Parameters: "/F /IM {#LegacyCoreExe}"; Flags: runhidden; RunOnceId: "KillLegacyCore"
Filename: "taskkill"; Parameters: "/F /IM {#MyAppExeName}"; Flags: runhidden; RunOnceId: "KillUI"
Filename: "sc"; Parameters: "stop {#ServiceName}"; Flags: runhidden; RunOnceId: "StopSvc"
Filename: "taskkill"; Parameters: "/F /IM {#CoreExeName}"; Flags: runhidden; RunOnceId: "KillCore"
Filename: "{app}\{#CoreExeName}"; Parameters: "--uninstall-service"; Flags: runhidden; RunOnceId: "DelSvc"
; 清理应用配置页写入的开机自启 Run 键值（/f 忽略不存在的情况）。
Filename: "reg"; Parameters: "delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v RouteToolUI /f"; Flags: runhidden; RunOnceId: "DelRunKey"

[UninstallDelete]
; 卸载时不清理 %ProgramData%\RouteTool（保留用户配置与日志）
; 开机自启 Run 键值在 [UninstallRun] 里用 reg delete 清理。

[Code]
// 检测服务是否已安装（用于幂等升级安装）。
function ServiceExists(): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec('sc', 'query {#ServiceName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode)
    and (ResultCode = 0);
end;

// 检测旧版本服务名（BypassToolCore）是否仍存在（v0.1.3 升级清理用）。
function LegacyServiceExists(): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec('sc', 'query BypassToolCore', '', SW_HIDE, ewWaitUntilTerminated, ResultCode)
    and (ResultCode = 0);
end;

// 检测服务是否处于 RUNNING 状态（区别于 ServiceExists 只判断"已注册"）。
// sc query 输出含 "RUNNING" 即运行中；把 stdout 重定向到临时文件再读取，
// 因为 Inno 的 Exec 无法直接捕获子进程 stdout。
function ServiceRunning(): Boolean;
var
  ResultCode: Integer;
  TmpFile: string;
  Lines: TArrayOfString;
  I: Integer;
begin
  Result := False;
  TmpFile := ExpandConstant('{tmp}\rt_svc_state.txt');
  DeleteFile(TmpFile);
  if not Exec('cmd.exe',
    '/C sc query {#ServiceName} > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Exit;
  if not LoadStringsFromFile(TmpFile, Lines) then
    Exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    if Pos('RUNNING', Uppercase(Lines[I])) > 0 then
    begin
      Result := True;
      Break;
    end;
  end;
end;

// 毫秒级等待（Inno 脚本内没有内置 Sleep，直接引入 kernel32 的）。
procedure WaitMs(Ms: Integer); external 'Sleep@kernel32.dll stdcall';

// 升级安装前先清场：正在运行的 route-tool-ui 会锁住 exe 导致复制文件失败，
// 服务没停就覆盖 route-tool-core 同理。这里杀进程 + 停服务，失败不中断安装
// （可能本来就没在运行）。sc stop 是异步的，等待服务真正停下来。
procedure PrepareToInstallCleanup();
var
  ResultCode: Integer;
  I: Integer;
begin
  Exec('taskkill', '/F /IM {#LegacyUiExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('taskkill', '/F /IM {#LegacyCoreExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('taskkill', '/F /IM {#MyAppExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  if ServiceExists() then
  begin
    Exec('sc', 'stop {#ServiceName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    // sc stop 返回不代表服务进程已退出，且进程退出前 route-tool-core.exe 仍被
    // 文件锁占用，直接复制会失败或被跳过。这里轮询等待服务真正停下
    // （最多 10 秒），而不是固定 sleep 2 秒，避免升级后仍残留旧二进制。
    for I := 0 to 9 do
    begin
      if not ServiceRunning() then
        Break;
      WaitMs(1000);
    end;
    // 兜底强杀（未在运行时 taskkill 只是返回错误，无副作用）。
    Exec('taskkill', '/F /IM {#CoreExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  PrepareToInstallCleanup();
  Result := '';
end;
