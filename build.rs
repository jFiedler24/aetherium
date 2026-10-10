// Embeds icons/aetherium.ico into the .exe so it shows up in Explorer, the
// taskbar, and Alt-Tab; gpui's `windows-manifest` feature only covers the
// DPI-awareness manifest, not the icon resource.
//
// Also refreshes the Open Very Fast Trace requirements report on every
// build: scans the `[impl->req~…~1]` tags in src/ and the markdown specs in
// docs/requirements/, then writes target/requirements_report.html. Set
// OVFT_STRICT=1 to fail the build when traceability defects appear (CI uses
// `cargo ovft --check` instead).

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/aetherium.ico");
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {err}");
        }
    }

    trace_requirements();
}

fn trace_requirements() {
    use ovft_core::{Config, Tracer};

    // Rerun when sources or specs change so the report stays fresh.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=docs/requirements");
    println!("cargo:rerun-if-changed=.ovft.toml");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    // .ovft.toml (source/spec dirs, artifact types) with sensible defaults.
    let config = Config::find_and_load_config(std::path::Path::new(&manifest))
        .unwrap_or_else(Config::default);

    let tracer = Tracer::new(config);
    let Ok(result) = tracer.trace() else {
        println!("cargo:warning=ovft: tracing failed; report skipped");
        return;
    };

    let report = format!("{manifest}/target/requirements_report.html");
    if let Err(err) = tracer.generate_html_report(&result, std::path::Path::new(&report)) {
        println!("cargo:warning=ovft: could not write {report}: {err}");
    }

    if result.is_success {
        println!(
            "cargo:warning=ovft: {} items traced, full coverage ✓ ({report})",
            result.total_items
        );
    } else {
        println!(
            "cargo:warning=ovft: {} defect(s) in {} traced items ({report})",
            result.defect_count, result.total_items
        );
        for defect in &result.defects {
            println!("cargo:warning=ovft:   {defect:?}");
        }
        if std::env::var_os("OVFT_STRICT").is_some() {
            panic!("ovft: requirements traceability has defects");
        }
    }
}
