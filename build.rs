fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/phosphor.ico");

        // Embed version-info so the signed exe, UAC prompt, and
        // Add/Remove Programs "Publisher" are coherent with the code-signing
        // certificate subject. Version fields come from Cargo at build time.
        let version = env!("CARGO_PKG_VERSION");
        res.set("CompanyName", "Marcin Spoczynski");
        res.set("ProductName", "Phosphor");
        res.set(
            "FileDescription",
            "Phosphor — a SID player for USBSID-Pico / Ultimate 64",
        );
        res.set("ProductVersion", version);
        res.set("FileVersion", version);
        res.set("LegalCopyright", "Copyright © Marcin Spoczynski");
        res.set("OriginalFilename", "phosphor.exe");

        res.compile().expect("Failed to compile Windows resources");
    }
}
