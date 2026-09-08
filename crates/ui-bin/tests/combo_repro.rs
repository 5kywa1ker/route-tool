//! 回归测试：设置窗口网卡 ComboBox 的选择交互（bug：无法选择网卡）。
//!
//! 使用 MinimalSoftwareWindow 平台合成鼠标事件，验证：
//! 1. 点击 ComboBox 能打开下拉弹出层；
//! 2. 点击弹出层条目能触发 `adapter-changed` 回调；
//! 3. 双向绑定下 `adapter-index` 属性与选择保持同步。

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};

slint::slint! {
    import { ComboBox } from "std-widgets.slint";

    export component TestUi inherits Window {
        width: 200px;
        height: 300px;
        in property <[string]> adapter-names: [];
        in-out property <int> adapter-index: -1;
        out property <int> last-changed: -999;
        out property <int> changed-count: 0;
        out property <bool> combo-has-focus: combo.has-focus;
        callback adapter-changed(int);
        adapter-changed(i) => {
            root.last-changed = i;
            root.changed-count += 1;
        }
        combo := ComboBox {
            width: root.width;
            height: 40px;
            model: root.adapter-names;
            current-index <=> root.adapter-index;
            selected => { root.adapter-changed(self.current-index); }
        }
    }
}

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
}

struct TestPlatform;

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(WINDOW.with(|w| w.clone()))
    }
}

fn click(x: f32, y: f32) {
    WINDOW.with(|w| {
        w.dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: slint::LogicalPosition { x, y },
            button: slint::platform::PointerEventButton::Left,
        });
        w.dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: slint::LogicalPosition { x, y },
            button: slint::platform::PointerEventButton::Left,
        });
    });
}

/// fluent 风格几何：ComboBox 未显式定位时渲染在窗口垂直中部（实测 y≈130..169）。
/// popup y = combo.y - 4，VerticalLayout padding 4px，条目高 40px：
/// 第 i 项中心 ≈ 150 + i*40。
#[test]
fn combobox_selection_fires_callback_and_syncs_index() {
    slint::platform::set_platform(Box::new(TestPlatform)).ok();

    let ui = TestUi::new().unwrap();
    WINDOW.with(|w| w.set_size(slint::PhysicalSize::new(200, 300)));

    ui.set_adapter_names(slint::ModelRc::new(slint::VecModel::from(vec![
        slint::SharedString::from("eth0"),
        slint::SharedString::from("wlan0"),
        slint::SharedString::from("vpn0"),
    ])));
    ui.set_adapter_index(0);
    assert_eq!(ui.get_adapter_index(), 0);

    // 打开下拉
    click(100.0, 150.0);
    assert!(
        ui.get_combo_has_focus(),
        "点击 ComboBox 后弹出层未打开（has-focus=false）"
    );

    // 选择第 3 项（index 2）
    click(100.0, 230.0);
    assert_eq!(ui.get_changed_count(), 1, "adapter-changed 回调未被触发");
    assert_eq!(ui.get_last_changed(), 2, "回调携带的索引不正确");
    // 双向绑定：Rust 侧属性应与选择同步
    assert_eq!(
        ui.get_adapter_index(),
        2,
        "adapter-index 属性未随选择更新（双向绑定失效）"
    );
}
