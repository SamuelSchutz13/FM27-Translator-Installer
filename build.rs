// Embute o ícone no .exe (aparece no Explorer e na barra de tarefas).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new().set_icon("assets/icon.ico").compile().unwrap();
    }
}
