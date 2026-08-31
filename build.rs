fn main() {
    slint_build::compile("ui/main.slint").unwrap();
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "Fast Translation");
        res.set("ProductName", "Fast Translation");
        res.set("OriginalFilename", "fast_translation.exe");
        let _ = res.compile();
    }
}
