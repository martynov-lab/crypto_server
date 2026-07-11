//! Live smoke test for universe discovery. `cargo run -p universe --example discover`.
use domain::ALL_EXCHANGES;
use std::sync::Arc;
use universe::poller::{build_client, refresh_once};
use universe::UniverseStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = build_client()?;
    let store = Arc::new(UniverseStore::new());
    refresh_once(&client, &store, &ALL_EXCHANGES).await;

    let cat = store.catalog();
    println!("\n=== total distinct bases: {} ===", cat.len());
    for min in [2usize, 3, 4, 5, 6, 7, 8] {
        println!("listed on >= {min} venues: {}", store.screenable(min).len());
    }
    println!("\n=== top 15 by coverage ===");
    for (base, xs) in cat.iter().take(15) {
        let names: Vec<&str> = xs.iter().map(|x| x.as_str()).collect();
        println!("{base:<12} {} : {}", xs.len(), names.join(","));
    }
    Ok(())
}
