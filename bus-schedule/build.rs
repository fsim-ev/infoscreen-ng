fn main() {
    let conf = slint_build::CompilerConfiguration::new()
    //    .with_include_paths(vec!["../../ui".into()])
        ;
    slint_build::compile_with_config("ui/app.slint", conf).unwrap();
}
