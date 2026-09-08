//! 单实例互斥：防止重复启动 bypass-ui 产生多个托盘图标。
//!
//! 首个实例持有命名互斥体 `Local\RouteToolUI-Mutex`，并创建命名事件
//! `Local\RouteToolUI-ShowEvent`。二次启动的进程检测到互斥体已存在时，
//! SetEvent 通知已有实例弹出设置窗口，然后自己退出。
//! 已有实例在 200ms 托盘轮询定时器里用 `poll_show_request()` 消费该事件。

use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    SYNCHRONIZATION_SYNCHRONIZE,
};

const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\RouteToolUI-Mutex");
const SHOW_EVENT_NAME: windows::core::PCWSTR = w!("Local\\RouteToolUI-ShowEvent");

/// 首个实例创建的“显示设置窗口”事件句柄（裸值存储；HANDLE 含裸指针非 Send/Sync）。
static SHOW_EVENT: OnceLock<usize> = OnceLock::new();

/// 单实例守卫：持有互斥体句柄，Drop 时释放（进程退出即释放）。
pub struct SingleInstanceGuard {
    _mutex: HANDLE,
}

/// 尝试成为唯一实例。
///
/// 返回 `Ok(Some(guard))`：本进程是首个实例。
/// 返回 `Ok(None)`：已有实例在运行，已通知其弹出设置窗口，调用方应立即退出。
pub fn acquire_or_notify() -> anyhow::Result<Option<SingleInstanceGuard>> {
    unsafe {
        // CreateMutexW 在互斥体已存在时仍返回有效句柄，需检查 GetLastError。
        let mutex = CreateMutexW(None, false, MUTEX_NAME)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 已有实例：通知它显示设置窗口。
            if let Ok(ev) = OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                false,
                SHOW_EVENT_NAME,
            ) {
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
            }
            return Ok(None);
        }

        // 首个实例：创建“显示设置窗口”事件供二次启动通知（自动重置）。
        let show_event = CreateEventW(None, false, false, SHOW_EVENT_NAME)?;
        let _ = SHOW_EVENT.set(show_event.0 as usize);
        Ok(Some(SingleInstanceGuard { _mutex: mutex }))
    }
}

/// 非阻塞检查“显示设置窗口”事件是否被触发（在 UI 事件循环定时器里轮询）。
///
/// 仅在首个实例内调用；事件不存在（理论上不会）时返回 false。
pub fn poll_show_request() -> bool {
    let Some(&raw) = SHOW_EVENT.get() else {
        return false;
    };
    unsafe { WaitForSingleObject(HANDLE(raw as *mut core::ffi::c_void), 0) == WAIT_OBJECT_0 }
}
