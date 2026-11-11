//! Times a scan: `cargo run --release -p kimono-core --example scan -- C:\`
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let progress = kimono_core::scan::ScanProgress::default();
    let start = std::time::Instant::now();
    let found = kimono_core::scan::scan(std::path::Path::new(&path), &progress);
    println!("{}: {} in {} files, {} folders, {} unreadable, {:.1}s",
        path, kimono_core::format::bytes(found.size), found.file_count, found.folder_count, found.unreadable, start.elapsed().as_secs_f64());
}
