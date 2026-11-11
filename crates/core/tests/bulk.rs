use std::path::Path;
use std::sync::atomic::Ordering;

use kimono_core::bulk::{BulkUpload, Options};
use kimono_core::fake::{Fake, ROOT};
use kimono_core::scan::{scan, ScanProgress};
use kimono_core::tus::{self, Source};
use rand::RngCore;

fn write(root: &Path, relative: &str, size: usize) -> Vec<u8> {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = vec![0u8; size];
    rand::rng().fill_bytes(&mut bytes);
    std::fs::write(&path, &bytes).unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_a_whole_tree_and_skips_it_the_second_time() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path();
    let photo = write(local, "Photos/2024/a.jpg", 300_000);
    let empty = write(local, "Photos/2024/empty.txt", 0);
    let video = write(local, "Photos/clip.mov", 9 * 1024 * 1024);
    write(local, "Photos/Deep/er/still/note.md", 12);
    write(local, "Photos/desktop.ini", 10);
    std::fs::create_dir_all(local.join("Photos/Nothing here")).unwrap();
    let single = write(local, "loose.bin", 4096);

    let found = scan(&local.join("Photos"), &ScanProgress::default());
    assert_eq!(found.file_count, 4);
    assert_eq!(found.size, 300_000 + 9 * 1024 * 1024 + 12);

    let client = fake.client();
    let options = Options { small_lanes: 4, big_lanes: 2, ..Default::default() };
    let upload = BulkUpload::new(client.clone(), ROOT, vec![found], vec![local.join("loose.bin")], options, None);
    upload.run().await;

    assert!(upload.failures().is_empty(), "{:?}", upload.failures());
    assert_eq!(upload.stats.done_files.load(Ordering::Relaxed), 5);
    assert_eq!(upload.stats.done_bytes.load(Ordering::Relaxed) as u64, upload.stats.total_bytes.load(Ordering::Relaxed));
    assert_eq!(fake.find("Photos/2024/a.jpg").unwrap().bytes, photo);
    assert_eq!(fake.find("Photos/2024/empty.txt").unwrap().bytes, empty);
    assert_eq!(fake.find("Photos/clip.mov").unwrap().bytes, video);
    assert!(fake.find("Photos/Deep/er/still/note.md").is_some());
    assert!(fake.find("Photos/Nothing here").is_some());
    assert!(fake.find("Photos/desktop.ini").is_none());
    assert_eq!(fake.find("loose.bin").unwrap().bytes, single);
    let files = fake.count("file");

    let again = BulkUpload::new(client, ROOT, vec![scan(&local.join("Photos"), &ScanProgress::default())], vec![local.join("loose.bin")], Options::default(), None);
    again.run().await;
    assert_eq!(again.stats.skipped_files.load(Ordering::Relaxed), 5);
    assert_eq!(fake.count("file"), files);
}

#[tokio::test(flavor = "multi_thread")]
async fn carries_on_after_dropped_connections() {
    let fake = Fake::start().await;
    fake.drop_every.store(2, Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    let big = write(dir.path(), "Big/large.bin", tus::PIECE as usize * 2 + 12345);
    let small: Vec<_> = (0..20).map(|i| write(dir.path(), &format!("Big/many/{i}.txt"), 2000 + i)).collect();

    let upload = BulkUpload::new(fake.client(), ROOT, vec![scan(&dir.path().join("Big"), &ScanProgress::default())], vec![], Options::default(), None);
    upload.run().await;

    assert!(upload.failures().is_empty(), "{:?}", upload.failures());
    assert_eq!(fake.find("Big/large.bin").unwrap().bytes, big);
    for (i, bytes) in small.iter().enumerate() { assert_eq!(&fake.find(&format!("Big/many/{i}.txt")).unwrap().bytes, bytes); }
    assert_eq!(upload.stats.done_bytes.load(Ordering::Relaxed) as u64, upload.stats.total_bytes.load(Ordering::Relaxed));
}

#[tokio::test]
async fn replacing_keeps_an_edit_from_elsewhere_as_a_copy() {
    let fake = Fake::start().await;
    let file = fake.add_file(ROOT, "notes.txt", b"one");
    fake.edit(&file.id, b"two");
    let result = tus::send(&fake.client(), Source::Memory("mine".into()), 4, &tus::replace(&file.id, 1), None, None).await.unwrap();
    assert!(result.conflict);
    assert_eq!(fake.find("notes.txt").unwrap().bytes, b"two");
    assert_eq!(fake.find("notes (edited elsewhere).txt").unwrap().bytes, b"mine");
}

#[tokio::test]
async fn reads_ranges_and_the_change_feed() {
    let fake = Fake::start().await;
    let client = fake.client();
    let start = client.latest_cursor().await.unwrap();
    let folder = fake.add_folder(ROOT, "Docs");
    let file = fake.add_file(&folder.id, "a.txt", b"hello world");
    let changes = client.changes(start).await.unwrap();
    assert!(changes.changed.iter().any(|n| n.id == file.id && n.parent_id.as_deref() == Some(folder.id.as_str())));
    let response = client.content(&file.id, Some((6, Some(10)))).await.unwrap();
    assert_eq!(response.text().await.unwrap(), "world");
    assert_eq!(client.changes(start + 1000).await.unwrap_err().status, 410);
}
