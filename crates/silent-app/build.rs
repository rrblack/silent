fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("compile ui/app.slint");

    // Icon and version details for Explorer and the taskbar.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/silent.ico")
            .set("ProductName", "Silent")
            .set("FileDescription", "Silent");
        res.compile().expect("compile Windows resources");
    }
}
