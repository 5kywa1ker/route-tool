//! 系统托盘：状态图标 + 右键菜单。
//!
//! TrayIcon 只能在创建线程（UI 线程）上使用；跨线程更新通过
//! `SHARED_TRAY` 静态句柄 + `invoke_from_event_loop` 实现。

use std::sync::OnceLock;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 托盘菜单动作（poll_event 返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Toggle,
    OpenSettings,
    OpenLogs,
    Quit,
}

/// 托盘状态色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// 直连（灰）。
    Direct,
    /// 旁路由生效（绿）。
    Bypass,
    /// 异常已回退（红）。
    Fallback,
}

/// 系统托盘。
pub struct Tray {
    _tray: TrayIcon,
    toggle_id: tray_icon::menu::MenuId,
    settings_id: tray_icon::menu::MenuId,
    logs_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

/// 全局托盘句柄：Tray 创建时注册。
///
/// TrayIcon 非 Send/Sync（内部含 Rc），静态存放需要 wrapper。
/// 所有调用方必须通过 `invoke_from_event_loop` 在 UI 线程上访问。
pub struct StaticTray(pub Tray);

// Safety: TrayIcon 持有的 HWND/菜单句柄只在创建线程（UI 线程）上被触碰；
// 本程序保证所有对 StaticTray 的访问都发生在 UI 线程（invoke_from_event_loop）。
unsafe impl Send for StaticTray {}
unsafe impl Sync for StaticTray {}

impl StaticTray {
    pub fn poll_event(&self) -> Option<TrayAction> {
        self.0.poll_event()
    }

    pub fn update_state(&self, state: TrayState) {
        self.0.update_state(state)
    }

    pub fn hide_icon(&self) {
        self.0.hide_icon()
    }
}

pub static SHARED_TRAY: OnceLock<StaticTray> = OnceLock::new();

impl Tray {
    pub fn new() -> anyhow::Result<Self> {
        let toggle_item = MenuItem::new("启用旁路由", true, None);
        let settings_item = MenuItem::new("打开设置", true, None);
        let logs_item = MenuItem::new("查看日志", true, None);
        let quit_item = MenuItem::new("退出", true, None);

        let toggle_id = toggle_item.id().clone();
        let settings_id = settings_item.id().clone();
        let logs_id = logs_item.id().clone();
        let quit_id = quit_item.id().clone();

        let menu = Menu::new();
        menu.append_items(&[
            &toggle_item,
            &PredefinedMenuItem::separator(),
            &settings_item,
            &logs_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])?;

        let icon = make_icon(TrayState::Direct)?;

        let tray = TrayIconBuilder::new()
            .with_id("route-tool-tray")
            .with_icon(icon)
            .with_tooltip("RouteTool - 直连")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .build()?;

        Ok(Self {
            _tray: tray,
            toggle_id,
            settings_id,
            logs_id,
            quit_id,
        })
    }

    /// 轮询菜单事件（MenuEvent 是全局通道，在 UI 事件循环里调用）。
    pub fn poll_event(&self) -> Option<TrayAction> {
        let receiver = MenuEvent::receiver();
        if let Ok(ev) = receiver.try_recv() {
            if ev.id == self.toggle_id {
                return Some(TrayAction::Toggle);
            } else if ev.id == self.settings_id {
                return Some(TrayAction::OpenSettings);
            } else if ev.id == self.logs_id {
                return Some(TrayAction::OpenLogs);
            } else if ev.id == self.quit_id {
                return Some(TrayAction::Quit);
            }
        }
        None
    }

    /// 移除托盘图标（退出前调用）。进程退出后 Windows 不会立刻清理图标，
    /// 会残留“幽灵图标”直到鼠标划过，因此必须显式隐藏。
    pub fn hide_icon(&self) {
        let _ = self._tray.set_visible(false);
    }

    /// 更新托盘图标与提示文案（仅 UI 线程调用）。
    pub fn update_state(&self, state: TrayState) {
        let (icon, tip) = match state {
            TrayState::Direct => (make_icon(TrayState::Direct).ok(), "RouteTool - 直连"),
            TrayState::Bypass => (make_icon(TrayState::Bypass).ok(), "RouteTool - 旁路由生效"),
            TrayState::Fallback => (
                make_icon(TrayState::Fallback).ok(),
                "RouteTool - 异常已自动回退",
            ),
        };
        if let Some(icon) = icon {
            let _ = self._tray.set_icon(Some(icon));
        }
        let _ = self._tray.set_tooltip(Some(tip));
    }
}

/// 32x32 托盘图标的原始 RGBA 像素（由 `scripts/gen_icons.py` 生成）。
///
/// 直接内联原始像素，避免运行时再做 PNG/ICO 解码，也省掉一个外部文件依赖：
/// 托盘图标必须随 exe 一起存在，找不到就等于托盘空白。
const TRAY_ICON_PX: usize = 32;

fn tray_icon_bytes(state: TrayState) -> &'static [u8] {
    match state {
        TrayState::Direct => include_bytes!("../../../assets/tray_direct.rgba"),
        TrayState::Bypass => include_bytes!("../../../assets/tray_bypass.rgba"),
        TrayState::Fallback => include_bytes!("../../../assets/tray_fallback.rgba"),
    }
}

/// 生成 32x32 托盘图标（灰 / 绿 / 红三态）。
fn make_icon(state: TrayState) -> anyhow::Result<Icon> {
    let rgba = tray_icon_bytes(state).to_vec();
    Ok(Icon::from_rgba(
        rgba,
        TRAY_ICON_PX as u32,
        TRAY_ICON_PX as u32,
    )?)
}
