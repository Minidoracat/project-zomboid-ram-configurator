// SPDX-License-Identifier: GPL-3.0-only
fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/app.slint", config).expect("compile Slint interface");
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    winresource::WindowsResource::new()
        .set_icon("assets/app-icon.ico")
        .set("ProductName", "Project Zomboid RAM Configurator")
        .set("FileDescription", "Project Zomboid RAM Configurator")
        .set("OriginalFilename", "pz-ram-configurator.exe")
        .set("LegalCopyright", "Copyright (C) Minidoracat")
        .compile()
        .expect("compile Windows icon resources");
}
