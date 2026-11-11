//! Lists a path on Kimono: `ls "Space name" "Backups/kimono/C"`, signed in from KIMONO_TEST_*.
use std::sync::Arc;
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).unwrap();
    let client = kimono_core::Client::new(Arc::new(kimono_core::MemoryStore::default()));
    client.sign_in(&env("KIMONO_TEST_SERVER"), &env("KIMONO_TEST_USERNAME"), &env("KIMONO_TEST_PASSWORD")).await.unwrap();
    let space = client.spaces().await.unwrap().into_iter().find(|s| s.name == args[0]).expect("space");
    let mut at = space.root_id.clone();
    for part in args.get(1).map(|p| p.split('/').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()).unwrap_or_default() {
        at = client.folder(&at).await.unwrap().children.into_iter().find(|c| c.name == part).expect(&part).id;
    }
    fn walk<'a>(c: &'a kimono_core::Client, id: String, depth: usize) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            for n in c.folder(&id).await.unwrap().children {
                println!("{}{} {}{}", "  ".repeat(depth), n.name, if n.is_folder() { "/".to_string() } else { format!("{} bytes", n.size) }, if n.is_folder() { String::new() } else { format!(" v{}", n.content_version) });
                if n.is_folder() { walk(c, n.id, depth + 1).await; }
            }
        })
    }
    walk(&client, at, 0).await;
    client.sign_out().await;
}
