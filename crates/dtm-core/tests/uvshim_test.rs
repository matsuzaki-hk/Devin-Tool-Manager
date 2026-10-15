#[test]
fn dbg_uv_shim() {
    let p = std::path::Path::new(r"C:\Users\avalo\.local\bin\serena.exe");
    eprintln!("bin_dir={:?}", dtm_core::resolve::uv_tool_bin_dir());
    eprintln!(
        "parent={:?}",
        p.parent().map(dtm_core::paths::normalize_path)
    );
    eprintln!(
        "bin_norm={:?}",
        dtm_core::resolve::uv_tool_bin_dir().map(|b| dtm_core::paths::normalize_path(&b))
    );
    eprintln!("is_shim={}", dtm_core::resolve::is_uv_tool_shim(p));
    eprintln!("shims={:?}", dtm_core::resolve::uv_tool_shim_packages());
    assert!(dtm_core::resolve::is_uv_tool_shim(p));
}
