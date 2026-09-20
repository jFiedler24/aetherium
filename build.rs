// Embeds icons/aetherium.ico into the .exe so it shows up in Explorer, the
// taskbar, and Alt-Tab; gpui's `windows-manifest` feature only covers the
// DPI-awareness manifest, not the icon resource.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/aetherium.ico");
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {err}");
        }
    }
}
