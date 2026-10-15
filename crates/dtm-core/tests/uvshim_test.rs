#[test]
fn uv_shim_detection() {
    // uv bin_dir 直下の exe は shim、それ以外は非 shim (環境非依存)
    let Some(bin) = dtm_core::resolve::uv_tool_bin_dir() else {
        return;
    };
    let shim = bin.join("some-tool.exe");
    assert!(dtm_core::resolve::is_uv_tool_shim(&shim));
    assert!(!dtm_core::resolve::is_uv_tool_shim(std::path::Path::new(
        r"C:\other\some-tool.exe"
    )));
}
