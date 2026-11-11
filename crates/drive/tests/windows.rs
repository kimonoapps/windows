//! The Explorer drive end to end, on a real Windows: a sync root registered against a fake
//! Kimono, then everything a person does in Explorer, checked on both sides.
#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use kimono_core::fake::{Fake, ROOT};
use kimono_drive::windows::{self as cf, Kind, Provider};
use kimono_drive::Engine;

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explorer_drive_end_to_end() {
    let fake = Fake::start().await;
    let docs = fake.add_folder(ROOT, "Docs");
    let hello = fake.add_file(&docs.id, "hello.txt", b"hello from kimono");
    let big: Vec<u8> = (0..(9 * 1024 * 1024 + 123)).map(|i| (i % 251) as u8).collect();
    fake.add_file(ROOT, "big.bin", &big);

    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("Kimono");
    let account = format!("test-{}", std::process::id());
    let icon = std::env::current_exe().unwrap();
    cf::register(&root, &account, &icon).expect("registering the sync root");

    let engine = Engine::new(fake.client(), root.clone(), base.path().join("state.json"));
    let provider = Provider::start(engine.clone(), tokio::runtime::Handle::current()).expect("connecting");
    engine.round().await.expect("first round");

    let space = root.join("Family");
    let hello_path = space.join("Docs").join("hello.txt");
    // Placeholders: there, the right size, nothing downloaded.
    assert!(hello_path.exists());
    assert!(matches!(cf::kind(&hello_path).unwrap(), Kind::Placeholder { in_sync: true, .. }));
    assert_eq!(std::fs::metadata(space.join("big.bin")).unwrap().len(), big.len() as u64);

    // Opening downloads.
    let reader = { let p = hello_path.clone(); tokio::task::spawn_blocking(move || read(&p)) };
    assert_eq!(reader.await.unwrap(), b"hello from kimono");
    let reader = { let p = space.join("big.bin"); tokio::task::spawn_blocking(move || read(&p)) };
    assert_eq!(reader.await.unwrap(), big);

    // A file made here goes up, and becomes a placeholder.
    std::fs::write(space.join("Docs").join("new.txt"), b"made on windows").unwrap();
    until("the new file on Kimono", || fake.find("Docs/new.txt").is_some_and(|i| i.bytes == b"made on windows")).await;
    until("the new file in sync", || matches!(cf::kind(&space.join("Docs/new.txt")), Ok(Kind::Placeholder { in_sync: true, .. }))).await;

    // A folder moved in with things inside goes up whole.
    let outside = base.path().join("Outside");
    std::fs::create_dir_all(outside.join("inner")).unwrap();
    std::fs::write(outside.join("inner").join("deep.txt"), b"deep").unwrap();
    std::fs::rename(&outside, space.join("Outside")).unwrap();
    until("the moved-in folder on Kimono", || fake.find("Outside/inner/deep.txt").is_some_and(|i| i.bytes == b"deep")).await;

    // Renaming here renames on Kimono.
    let renamer = { let (a, b) = (hello_path.clone(), space.join("Docs").join("greeting.txt")); tokio::task::spawn_blocking(move || std::fs::rename(a, b)) };
    renamer.await.unwrap().expect("renaming");
    assert_eq!(fake.find("Docs/greeting.txt").map(|i| i.id), Some(hello.id.clone()));

    // Editing here saves on Kimono.
    std::fs::write(space.join("Docs").join("greeting.txt"), b"edited on windows").unwrap();
    until("the edit on Kimono", || fake.find("Docs/greeting.txt").is_some_and(|i| i.bytes == b"edited on windows")).await;

    // Deleting here sends it to Kimono's trash.
    let deleter = { let p = space.join("Docs").join("new.txt"); tokio::task::spawn_blocking(move || std::fs::remove_file(p)) };
    deleter.await.unwrap().expect("deleting");
    assert!(fake.find("Docs/new.txt").is_none());

    // Changes elsewhere arrive: a rename, an edit, a new file.
    let other = fake.add_file(&docs.id, "from-phone.txt", b"from the phone");
    fake.edit(&hello.id, b"edited on the phone");
    engine.round().await.unwrap();
    assert!(space.join("Docs/from-phone.txt").exists());
    let reader = { let p = space.join("Docs/greeting.txt"); tokio::task::spawn_blocking(move || read(&p)) };
    assert_eq!(reader.await.unwrap(), b"edited on the phone");
    fake.remove(&other.id);
    engine.round().await.unwrap();
    assert!(!space.join("Docs/from-phone.txt").exists());

    provider.stop();
    cf::unregister(&account).ok();
}
