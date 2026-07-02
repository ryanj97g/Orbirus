fn main() {
    // Embed the program icon (assets/orbirus.ico, resource id 1 — the tray
    // icon loads it from there too).
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/orbirus.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon resource compile failed: {e}");
    }
    println!("cargo:rerun-if-changed=assets/orbirus.ico");
}
