use std::path::Path;

// 前端产物在编译时嵌入。目录不存在时建一个空的，这样不装 Node 也能编译后端；
// 此时页面会提示先构建前端。
fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    if !dist.exists() {
        std::fs::create_dir_all(&dist).expect("无法创建 web/dist");
    }
    println!("cargo:rerun-if-changed={}", dist.display());
}
