//! 系统剪贴板写入（仅 UI 侧使用）。
//!
//! 存在的理由：UI 里有若干「只读值被省略号截断」的位置（KeyValueRow），
//! 规范第 26/63 节要求提供 Tooltip / 复制 / 查看详情中的至少一种完整内容获取
//! 方式。这里只实现最小可用的「写文本到剪贴板」。
//!
//! 直接用 Win32 剪贴板 API，而不是引入 `arboard` 之类的额外依赖：
//! 需求只有单向写文本，依赖体积不划算。
//!
//! 全局内存的归属：`SetClipboardData` 成功后内存所有权移交系统，
//! 因此**不能**再调用 `GlobalFree`；失败时才由我们释放。

use anyhow::{bail, Context, Result};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// 把一段文本写入系统剪贴板（覆盖当前内容）。
///
/// 文本以 UTF-16 + 结尾 NUL 写入 `CF_UNICODETEXT`，这是 Windows 上
/// 记事本 / 浏览器 / 其它应用都能直接粘贴的通用格式。
pub fn set_text(text: &str) -> Result<()> {
    // UTF-16 编码 + 结尾 NUL（CF_UNICODETEXT 要求以 NUL 终止）。
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let byte_len = utf16.len() * std::mem::size_of::<u16>();

    // SAFETY: 以下均为标准剪贴板写入序列，参数取自官方文档：
    // OpenClipboard(None) → EmptyClipboard → GlobalAlloc → GlobalLock → 拷贝
    // → GlobalUnlock → SetClipboardData → CloseClipboard。
    unsafe {
        OpenClipboard(None).context("OpenClipboard 失败（剪贴板可能被其它进程占用）")?;

        // 从打开剪贴板这一刻起必须保证 CloseClipboard 被调用，
        // 因此用闭包承载主体逻辑，收尾统一放在闭包外。
        let result = (|| -> Result<()> {
            EmptyClipboard().context("EmptyClipboard 失败")?;

            // GlobalAlloc 返回 Result，失败时已带 Win32 错误码。
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len).context("GlobalAlloc 失败")?;

            let dst = GlobalLock(hmem);
            if dst.is_null() {
                // 加锁失败：内存仍归我们所有，必须释放。
                let _ = GlobalFree(Some(hmem));
                bail!("GlobalLock 失败");
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, dst as *mut u8, byte_len);
            let _ = GlobalUnlock(hmem);

            // SetClipboardData 成功后内存所有权移交系统：不能再 GlobalFree。
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0))).is_err() {
                let _ = GlobalFree(Some(hmem));
                bail!("SetClipboardData 失败");
            }
            Ok(())
        })();

        let _ = CloseClipboard();
        result
    }
}

/// 供内部使用的空 HGLOBAL 占位（避免未使用导入告警）。
#[allow(dead_code)]
fn _assert_handle_type(_: HGLOBAL) {}
