// Rebuild when built-in plugin files change (they are embedded with include_dir!,
// which cargo does not track on its own).
fn main() {
    println!("cargo:rerun-if-changed=plugins");
    if let Ok(entries) = std::fs::read_dir("plugins") {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
            if let Ok(files) = std::fs::read_dir(entry.path()) {
                for f in files.flatten() {
                    println!("cargo:rerun-if-changed={}", f.path().display());
                }
            }
        }
    }
}
