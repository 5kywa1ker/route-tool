
---

# 旁路由一键切换工具 - 技术需求文档（Final）

## 0. 项目一句话描述

Windows 11桌面工具，通过路由表叠加或网卡直改两种方式，一键切换"直连"与"走旁路由（局域网内的透明代理网关）"，具备旁路由异常自动侦测与回退能力。Core逻辑与UI分离，为跨平台迁移预留空间。

## 1. 总体架构

```
┌─────────────────────┐         Named Pipe           ┌──────────────────────────┐
│   bypass-ui.exe      │ ◄──────  JSON-RPC  ────────► │   bypass-core.exe          │
│  (托盘 + 设置窗口)     │      (本地IPC，双向)          │  (Windows Service运行)     │
│  普通用户权限          │   订阅状态推送(切换/异常/日志) │  SYSTEM权限                │
└─────────────────────┘                                └──────────────────────────┘
```

- 两个独立可执行文件，独立进程，通过IPC通信
- `bypass-core` 以Windows服务形式安装、开机自启、SYSTEM权限运行，独立完成健康检测与自动回退，**不依赖UI存活**
- `bypass-ui` 普通用户权限运行，只负责展示状态和下发指令，可随时关闭不影响Core工作
- 安装时执行一次 `bypass-core.exe --install-service` 完成服务注册（此为唯一一次需要管理员权限确认的时机）

## 2. 技术栈

| 类别 | 选型 |
|---|---|
| 语言 | Rust（stable channel） |
| Windows系统调用 | `windows-rs`（IP Helper API：`CreateIpForwardEntry2`/`DeleteIpForwardEntry2`/`GetIpForwardTable2`/`CreateUnicastIpAddressEntry`/`SetInterfaceDnsSettings`等） |
| 异步运行时 | `tokio` |
| 服务生命周期 | `windows-service` crate |
| 序列化 | `serde` + `serde_json` |
| IPC | Named Pipe（`tokio::net::windows::named_pipe`），自定义行分隔JSON-RPC协议 |
| UI托盘 | `tray-icon` + `tao` |
| UI窗口 | `slint`，不用WebView方案 |
| 通知 | `windows-rs` 调用 `Windows.UI.Notifications`（Toast） |
| 日志 | `tracing` + `tracing-appender`（按天滚动，保留7天） |
| 安装包 | Inno Setup |

## 3. Core 内部模块设计

### 3.1 模块划分（均为平台无关接口 + Windows具体实现分离）

```
core/
├── net_inspector/     // 网卡与网络状态探测
├── switch_engine/     // 切换策略（trait + 两种实现）
├── health_monitor/    // 健康检测状态机
├── state_store/       // 配置与运行状态持久化
├── service_host/      // Windows服务生命周期 + IPC server
└── ipc_protocol/       // JSON-RPC协议定义（请求/响应/事件）
```

### 3.2 关键接口定义（供实现时参考，方法签名避免泄露Windows专属概念）

```rust
// switch_engine
pub enum SwitchMode {
    RouteOverlay,
    AdapterReconfig,
}

pub struct BypassTarget {
    pub adapter_id: String,      // 平台无关的网卡标识
    pub bypass_ip: IpAddr,
    pub dns: Option<Vec<IpAddr>>, // 仅AdapterReconfig模式使用
}

#[async_trait]
pub trait SwitchStrategy: Send + Sync {
    async fn enable(&self, target: &BypassTarget) -> Result<SwitchHandle>;
    async fn disable(&self, handle: &SwitchHandle) -> Result<()>;
    async fn is_active(&self, handle: &SwitchHandle) -> Result<bool>;
    async fn reconcile_on_startup(&self) -> Result<ReconcileAction>; // 启动时一致性校验
}

// net_inspector
pub trait NetInspector: Send + Sync {
    async fn list_adapters(&self) -> Result<Vec<AdapterInfo>>;
    async fn current_route_state(&self, adapter_id: &str) -> Result<RouteState>;
    async fn ping(&self, ip: IpAddr, timeout_ms: u32) -> Result<bool>;
}

// health_monitor 事件
pub enum HealthEvent {
    ProbeFailed { consecutive_count: u32 },
    ProbeRecovered,
    AutoFallbackTriggered { reason: String, at: DateTime<Utc> },
}
```

### 3.3 两种 SwitchStrategy 实现要点

**RouteOverlayStrategy**
- `enable`：调用 `CreateIpForwardEntry2` 添加 `0.0.0.0/0 via bypass_ip`，metric优于现有默认路由；返回的`SwitchHandle`记录该路由条目的唯一标识（接口索引+目标前缀），便于精确删除
- `disable`：调用 `DeleteIpForwardEntry2` 删除该条目
- `reconcile_on_startup`：读取state_store中"上次应处于的状态"，与`GetIpForwardTable2`实际结果比对，不一致则修正
- 不涉及DNS、不涉及网卡DHCP状态

**AdapterReconfigStrategy**
- `enable`前：先完整备份当前网卡状态（DHCP开关、静态IP/掩码/网关、DNS列表）到state_store，采用"临时文件写入+fsync+原子rename"方式落盘
- `enable`：将网卡切为静态配置，网关指向bypass_ip，DNS按`BypassTarget.dns`设置（为空则默认使用bypass_ip作为DNS）
- `disable`：读取备份状态完整恢复（若备份是DHCP则重新启用DHCP，若备份是静态则恢复原静态参数）
- `reconcile_on_startup`：检测当前网卡状态是否与备份记录的"预期变更后状态"一致，不一致（如异常关机后网卡处于中间态）则强制按备份恢复

### 3.4 健康检测逻辑

- 仅在`RouteOverlay`或`AdapterReconfig`处于enabled状态时运行
- 定时任务（默认间隔可配置，建议5-10秒）：
  1. ICMP ping `bypass_ip`
  2. 可选：通过当前路由发起一次到公共稳定地址的探测（判断链路是否真通）
- 连续失败达到阈值（默认3次，可配置）→ 触发`AutoFallbackTriggered`：
  1. 调用当前策略的`disable`
  2. 通过IPC向UI推送事件（用于Toast通知与状态刷新）
  3. 写日志
- 默认不自动重试恢复；提供开关"旁路由恢复后自动重新启用"（检测到`ProbeRecovered`且用户开启此项时，重新调用`enable`）

### 3.5 状态与配置持久化（state_store）

```rust
pub struct AppConfig {
    pub adapter_id: String,
    pub bypass_ip: IpAddr,
    pub switch_mode: SwitchMode,
    pub dns_override: Option<Vec<IpAddr>>,
    pub health_check_interval_secs: u32,
    pub failure_threshold: u32,
    pub auto_reenable_after_recovery: bool,
    pub notifications_enabled: bool,
}

pub struct RuntimeState {
    pub is_enabled: bool,
    pub current_mode: Option<SwitchMode>,
    pub pre_change_snapshot: Option<AdapterSnapshot>, // 仅AdapterReconfig模式需要
    pub last_updated: DateTime<Utc>,
}
```

- 配置文件与运行状态文件分开存储（`config.json` / `runtime_state.json`），均落在 `%ProgramData%\BypassTool\`（服务级数据，不用用户目录，因为Core以SYSTEM权限运行）
- 所有写入操作走"临时文件+原子rename"

## 4. IPC 协议

Named Pipe，管道名如 `\\.\pipe\BypassToolCore`，ACL限制仅当前用户/管理员组可连接。

协议：行分隔JSON-RPC 2.0风格，请求/响应 + 服务端主动推送事件两种消息类型。

**方法列表（MVP需要实现）**：
- `GetStatus() -> RuntimeState`
- `GetConfig() -> AppConfig`
- `UpdateConfig(AppConfig) -> Result<()>`
- `EnableBypass() -> Result<()>`
- `DisableBypass() -> Result<()>`
- `TestConnectivity(ip: IpAddr) -> Result<bool>`（用于首次配置向导做连通性校验）
- `ListAdapters() -> Vec<AdapterInfo>`
- `SubscribeEvents()`（建立后Core持续推送`HealthEvent`/状态变化事件）

预留字段：`protocol_version`、`supported_modes`（能力协商，为跨平台/未来扩展模式留空间）。

## 5. UI 需求

### 5.1 首次配置向导
- 自动探测网卡列表、当前网关，供用户选择
- 用户输入旁路由IP
- 选择切换模式（路由叠加 / 网卡直改），附一句话说明两者区别，帮助用户判断
- 网卡直改模式下可选填DNS（留空默认用旁路由IP）
- 保存前调用`TestConnectivity`做ping校验并提示结果

### 5.2 托盘
- 图标状态：灰=直连，绿=旁路由生效，红=异常已自动回退
- 右键菜单：启用/禁用旁路由、打开设置、查看日志、退出
- 悬浮提示显示当前模式与状态

### 5.3 设置窗口
- 网卡、旁路由IP、切换模式（切换前需先禁用当前状态）
- 健康检测间隔、失败阈值
- 是否恢复后自动重新启用
- 通知开关、开机自启（Core服务层面已默认自启，此处主要是开关UI自身是否随系统启动）
- 日志查看入口（打开日志文件所在目录或内嵌简单查看器）

### 5.4 通知
- 自动回退触发时：Toast提示"旁路由异常，已自动切回直连"，附时间

## 6. 非功能需求

| 维度 | 目标 |
|---|---|
| Core空闲内存占用 | < 15MB |
| UI打开时内存占用 | < 30MB |
| CPU空闲占用 | 近似0，事件/定时器驱动，禁止忙轮询 |
| Core对UI存活的依赖 | 无，UI关闭/崩溃不影响Core正常工作 |
| IPC往返延迟 | < 10ms |
| 服务7x24稳定性 | 长期运行不崩溃、不内存泄漏 |
| 崩溃自愈 | Core重启后能通过`reconcile_on_startup`自动纠正状态，不留"脏路由"或"脏网卡配置" |

## 7. 异常场景处理清单（务必覆盖测试）

- Core服务异常退出后被Windows自动重启 → 状态一致性校验与修复
- 电脑睡眠/唤醒 → 重新校验路由/网卡状态是否仍符合预期
- 网卡禁用后重新启用、或切换WiFi/有线 → 检测并按需重建
- 旁路由设备重启期间健康检测应能正确识别"临时不可达"并按阈值触发回退，不能误判过快
- 用户在网卡直改模式启用期间手动改了网卡设置 → 至少要能在下次`disable`或`reconcile_on_startup`时发现不一致并给出日志提示（不强求自动纠正用户的手动改动）
- IPv6：MVP阶段仅处理IPv4默认路由/配置，README中需注明此限制

## 8. CI/CD（GitHub Actions）

- 触发：push tag（如 `v*.*.*`）
- Runner：`windows-latest`
- 步骤：
  1. checkout
  2. 安装Rust工具链（`dtolnay/rust-toolchain` 或官方action）
  3. `cargo build --release` 生成 `bypass-core.exe` 与 `bypass-ui.exe`
  4. 用 Inno Setup（`iscc`，可通过 chocolatey 或直接下载安装）编译安装包，安装脚本包含"运行`bypass-core.exe --install-service`"步骤
  5. `softprops/action-gh-release` 创建Release并上传安装包
- 版本号：从Git tag读取，注入到`Cargo.toml`版本与安装包版本信息中
- 本地开发不要求配置任何编译发布环境，全部产物由Actions产出

## 9. 项目目录结构建议

```
bypass-tool/
├── crates/
│   ├── core-lib/          # 平台无关的trait定义 + 状态机逻辑
│   ├── core-win/          # Windows专属实现（IP Helper调用等）
│   ├── core-bin/          # bypass-core.exe 入口（服务宿主+IPC server）
│   ├── ipc-protocol/      # 共享的JSON-RPC协议定义
│   └── ui-bin/            # bypass-ui.exe 入口（托盘+设置窗口）
├── installer/             # Inno Setup脚本
├── .github/workflows/
│   └── release.yml
└── README.md
```

## 10. 里程碑与验收标准

**Phase 1 - MVP骨架**
- Core作为Windows服务能正常安装/启动/停止
- IPC能跑通（UI能调用`GetStatus`拿到数据）
- RouteOverlay模式：手动配置后能一键enable/disable，路由表变化可通过`route print`验证
- 托盘图标能反映基本状态
- 验收：手动切换旁路由，实际流量走向随之改变，且过程无残留路由

**Phase 2 - 健康检测与回退**
- RouteOverlay模式下，人为断开旁路由设备，能在阈值时间内自动回退并收到Toast通知
- 日志正确记录切换与回退事件
- 验收：拔网线模拟旁路由离线，观察从检测到失败到自动回退到通知弹出的完整链路

**Phase 3 - 网卡直改模式**
- AdapterReconfigStrategy完整实现，含状态备份与恢复
- 首次配置向导支持模式选择
- 独立"恢复出厂网络设置"兜底小工具（不依赖Core/UI进程，单独可执行）
- 验收：反复enable/disable网卡直改模式20次以上，网卡最终状态与初始状态完全一致；模拟Core进程被强制kill后重启，能自动纠正到正确状态

**Phase 4 - 打磨与发布**
- GitHub Actions全流程跑通，从打tag到产出安装包全自动
- 安装包完成服务注册、开机自启配置
- 设置界面功能完整，多网络环境下手动切换配置可用
- 验收：全新Windows 11虚拟机上，从下载安装包到完成首次配置到日常使用全流程走通

## 11. 明确不做（本期范围外）

- IPv6路由/配置处理
- 跨平台（macOS/Linux）实现，仅要求架构层面trait抽象合理，不要求实际代码
- 代码签名（后续按需补，本期安装包会有"未知发布者"提示属预期）
- 多套配置自动按网络环境切换（识别SSID/网段自动匹配配置）
- 应用内自动更新

---

## 12. 实现状态（2026-09-08）

### 12.1 已完成

| 范围 | 说明 |
|---|---|
| workspace 结构 | 5 crates：`ipc-protocol` / `core-lib` / `core-win` / `core-bin` / `ui-bin` |
| IPC 协议 | 行分隔 JSON-RPC 2.0，管道 `\\.\pipe\BypassToolCore`（DACL 仅 SYSTEM/Administrators/Authenticated Users），支持 GetStatus / GetConfig / UpdateConfig / EnableBypass / DisableBypass / TestConnectivity / ListAdapters / SubscribeEvents（事件推送）；响应携带 `protocol_version` / `supported_modes` 预留协商字段 |
| RouteOverlay 策略 | `CreateIpForwardEntry2`/`DeleteIpForwardEntry2` 添加/删除 `0.0.0.0/1` + `128.0.0.0/1` 两条叠加路由（最长前缀匹配必然覆盖默认路由，不依赖 metric），`is_active` 校验两条 /1 路由 + 接口状态 |
| AdapterReconfig 策略 | netsh 静态 IP/网关/DNS 切换；启用前快照备份（`adapter_snapshot.json`），禁用时恢复（DHCP 或原静态参数）——恢复逻辑在策略 `disable` 内完成，手动禁用与健康检测自动回退共用同一路径 |
| 健康检测 | 定时 ICMP ping bypass_ip，连续失败达阈值 → 自动回退（调策略 disable）+ `AutoFallbackTriggered` 事件；可选"恢复后自动重新启用"；健康巡检中自动校验路由仍生效（睡眠唤醒/网卡切换自愈）；回退/重建等状态变化持久化到 `runtime_state.json`；世代计数器防止 disable 后在途探测复活路由 |
| 启动一致性校验 | `reconcile_on_startup`：按 `runtime_state.json` 预期状态修正实际状态（叠加模式校验/重建 /1 路由，直改模式校验网关/快照）；预期直连时清理残留叠加路由与孤儿快照；校验后恢复健康检测循环 |
| UpdateConfig 热生效 | 配置校验（网卡/IP/间隔/阈值）→ 落盘；启用中目标变更（网卡/IP/模式/DNS）自动重放切换，仅健康参数变化则重启监控循环 |
| Windows 服务 | `bypass-core --install-service` / `--uninstall-service`，SCM 管理自启，`--console` 调试模式；Stop/Shutdown 信号优雅停机（IPC server 退出后上报 Stopped） |
| 持久化 | `%ProgramData%\BypassTool\{config,runtime_state,adapter_snapshot}.json`，临时文件+fsync+原子 rename，损坏文件按默认值容错 |
| 日志 | tracing + tracing-appender 按天滚动（`%ProgramData%\BypassTool\logs\`；UI 日志在 `%TEMP%\BypassTool\`）；7 天保留期，core 启动时清理 + 每 24h 巡检，UI 启动时清理 |
| UI | 托盘（灰/绿/红三态图标 + 右键菜单 + 悬浮提示）、Slint 设置窗口、Toast 通知（回退时）、1s 状态轮询 + 断线自动重连（含配置/网卡列表重新同步）；网卡下拉框选择即落盘；关窗口隐藏到托盘不退出，托盘"打开设置"可唤出 |
| 安装包 | `installer/BypassTool.iss`（Inno Setup，含服务注册/启动/开机自启 UI 可选项） |
| CI/CD | `.github/workflows/release.yml`：push tag `v*` → 构建（`--target x86_64-pc-windows-msvc`，与安装包路径一致）→ 测试 → iscc 打包 → GitHub Release |
| 测试 | `cargo test --workspace`：协议序列化往返/能力字段、状态存储原子写/容错、健康检测状态机（降级/回退/自动重启用/Idle/过期世代）、日志保留清理等单测 |
| 代码质量 | `cargo clippy --workspace --all-targets` 零警告 |

### 12.2 已知限制（与 §11 对应）

- **仅支持 IPv4**：路由叠加只处理 `0.0.0.0/1` + `128.0.0.0/1`（AF_INET），网卡直改只设置 IPv4 静态地址与 IPv4 DNS；IPv6 路由与 DNS（含 RA/DHCPv6）不受管理。启用旁路由后 IPv6 流量仍按系统原路由行走，不会经旁路由。
- **AdapterReconfig 模式使用 netsh**：需求文档 §2 建议的 `CreateUnicastIpAddressEntry`/`SetInterfaceDnsSettings` 纯 API 方案，因 DHCP 开关与静态网关设置需额外操作 WMI/注册表，MVP 采用系统自带 netsh（SYSTEM 权限可用）；后续可替换 `core-win/src/netsh.rs`。
- 子网掩码快照恢复固定按 /24 处理（`adapter_snapshot.json` 中 `static_ipv4_mask` 预留了扩展位）。
- 首次配置向导目前为单窗口简化实现（网卡下拉框已接入 ListAdapters；DNS 自定义输入待补）。
- "恢复出厂网络设置"兜底小工具（Phase 3 验收项）尚未单独提供。

### 12.3 本地构建与运行

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
cargo build
# 调试运行 core（控制台模式，不需装服务）
.\target\debug\bypass-core.exe --console
# 另一终端运行 UI
.\target\debug\bypass-ui.exe
```

服务模式（管理员 PowerShell）：

```powershell
.\target\release\bypass-core.exe --install-service
net start BypassToolCore
```
