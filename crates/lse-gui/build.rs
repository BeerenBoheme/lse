//! 把 Blueprint 源编译为 .ui XML（需要 blueprint-compiler，sudo apt install blueprint-compiler）。
fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = ["window"];
    for f in files {
        let input = manifest.join("ui").join(format!("{f}.blp"));
        let output = std::path::Path::new(&out_dir).join(format!("{f}.ui"));
        let status = std::process::Command::new("blueprint-compiler")
            .arg("compile")
            .arg(&input)
            .arg("--output")
            .arg(&output)
            .status()
            .unwrap_or_else(|e| {
                panic!(
                    "无法运行 blueprint-compiler（{e}）。\n\
                     请先安装：sudo apt install -y blueprint-compiler"
                )
            });
        assert!(
            status.success(),
            "blueprint-compiler 编译失败: {}",
            input.display()
        );
        println!("cargo:rerun-if-changed={}", input.display());
    }
}
