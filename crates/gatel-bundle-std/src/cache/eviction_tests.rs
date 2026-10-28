use super::*;

fn entry(size: usize) -> Entry {
    Entry {
        status: 200,
        headers: http::HeaderMap::new(),
        body: Bytes::from(vec![42; size]),
        stored: Instant::now(),
        ttl: Duration::from_secs(600),
        age_on_arrival: Duration::ZERO,
    }
}

fn key(index: usize) -> Key {
    Key::new(
        &http::Method::GET,
        "http",
        "example.test",
        &format!("/{index:010}"),
    )
}

#[test]
fn a_large_insert_evicts_oldest_copies_in_bulk_and_preserves_accounting() {
    let cache = Store::new(10_000 * 400);
    for index in 0..10_000 {
        cache.put(key(index), String::new(), vec![], entry(1));
    }
    let held = cache.held_bytes();
    assert_eq!(cache.len(), 10_000);
    assert!(cache.get(&key(0), "").is_some());
    cache.put(key(10_000), String::new(), vec![], entry((held * 3 / 4) as usize));
    assert!(cache.held_bytes() <= cache.max_bytes);
    assert!(cache.evictions.load(Ordering::Relaxed) > 4_000);
    assert!(cache.get(&key(1), "").is_none());
    assert!(cache.get(&key(0), "").is_some());
    assert!(cache.get(&key(10_000), "").is_some());
    let contents = cache.contents.lock().unwrap();
    let charged: u64 = contents.held.values().flatten().map(|copy| copy.weight).sum();
    assert_eq!(cache.held_bytes(), charged);
    assert_eq!(contents.held.len(), contents.vary.len());
}

#[test]
fn bulk_eviction_handles_many_variants_of_one_key_without_orphaned_metadata() {
    let cache = Store::new(5_000 * 500);
    let resource = key(0);
    let vary = vec!["accept-language".into()];
    for index in 0..5_000 {
        cache.put(resource.clone(), format!("{index:08}"), vary.clone(), entry(1));
    }
    assert!(cache.get(&resource, "00000000").is_some());
    let held = cache.held_bytes();
    cache.put(key(1), String::new(), vec![], entry((held * 3 / 4) as usize));
    assert!(cache.get(&resource, "00000001").is_none());
    assert!(cache.get(&resource, "00000000").is_some());
    assert_eq!(cache.vary_for(&resource), Some(vary));
    cache.invalidate(&resource);
    assert!(cache.vary_for(&resource).is_none());
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.remembered_resources(), 1);
    assert!(cache.held_bytes() <= cache.max_bytes);
}

#[test]
fn concurrent_hits_remain_coherent_during_large_insertions() {
    let cache = Arc::new(Store::new(2_000 * 400));
    for index in 0..2_000 {
        cache.put(key(index), String::new(), vec![], entry(1));
    }
    let hit = cache.clone();
    let reader = std::thread::spawn(move || {
        for _ in 0..2_000 {
            if let Some(entry) = hit.get(&key(0), "") {
                assert_eq!(entry.body.as_ref(), &[42]);
            }
        }
    });
    for index in 2_000..2_020 {
        cache.put(key(index), String::new(), vec![], entry(100_000));
    }
    reader.join().unwrap();
    assert!(cache.held_bytes() <= cache.max_bytes);
    assert_eq!(cache.len(), cache.remembered_resources());
}
