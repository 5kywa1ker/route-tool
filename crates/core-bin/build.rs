// 图标嵌入逻辑（ui-bin / core-bin 两个 bin 共用）：见 ../../build/embed_icon.rs
include!("../../build/embed_icon.rs");

fn main() {
    embed_app_icon();
}
