//! Against a real Kimono, when KIMONO_TEST_SERVER, KIMONO_TEST_USERNAME and KIMONO_TEST_PASSWORD
//! are set: signs in the way the app does, then uploads a small tree and reads it back.

use std::sync::Arc;

use kimono_core::bulk::{BulkUpload, Options};
use kimono_core::scan::{scan, ScanProgress};
use kimono_core::{Client, MemoryStore, SecretStore, SignInNext};
use rand::RngCore;

#[tokio::test(flavor = "multi_thread")]
async fn signs_in_and_uploads_to_a_real_kimono() {
    let (Ok(server), Ok(username), Ok(password)) = (std::env::var("KIMONO_TEST_SERVER"), std::env::var("KIMONO_TEST_USERNAME"), std::env::var("KIMONO_TEST_PASSWORD")) else {
        eprintln!("skipped: no Kimono to test against");
        return;
    };
    let store = Arc::new(MemoryStore::default());
    let client = Client::new(store.clone());
    assert_eq!(client.sign_in(&server, &username, &password).await.unwrap(), SignInNext::Done);
    assert!(store.get("kimono.refresh").is_some());
    assert!(client.me().await.unwrap().username.eq_ignore_ascii_case(&username));

    let space = client.spaces().await.unwrap().into_iter().find(|s| s.can_edit()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join(format!("kimono-live-{}", rand::random::<u32>()));
    std::fs::create_dir_all(local.join("inner")).unwrap();
    let mut bytes = vec![0u8; 3_000_000];
    rand::rng().fill_bytes(&mut bytes);
    std::fs::write(local.join("inner/data.bin"), &bytes).unwrap();
    std::fs::write(local.join("readme.txt"), "from the desktop app's tests").unwrap();

    let upload = BulkUpload::new(client.clone(), space.root_id.clone(), vec![scan(&local, &ScanProgress::default())], vec![], Options::default(), None);
    upload.run().await;
    assert!(upload.failures().is_empty(), "{:?}", upload.failures());

    let name = local.file_name().unwrap().to_string_lossy().to_string();
    let top = client.folder(&space.root_id).await.unwrap().children.into_iter().find(|n| n.name == name).unwrap();
    let inner = client.folder(&top.id).await.unwrap().children.into_iter().find(|n| n.name == "inner").unwrap();
    let file = client.folder(&inner.id).await.unwrap().children.into_iter().find(|n| n.name == "data.bin").unwrap();
    let got = client.content(&file.id, None).await.unwrap().bytes().await.unwrap();
    assert_eq!(got.as_ref(), bytes.as_slice());
    client.trash(&top.id).await.unwrap();
    client.sign_out().await;
}
