/// End-to-end tests for the backing-directory inotify watcher.
///
/// Each test mutates the backing directory directly (bypassing FUSE), waits for the
/// debounce window, and asserts cache validation side effects. Debounce is 150 ms.
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::BackingWatchHarness;

const DEBOUNCE_MS: u64 = 150;

async fn wait_for_replaced(
    harness: &BackingWatchHarness,
    rel: &Path,
    expected: &[u8],
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if harness.cache_mgr.is_cached(rel)
            && let Ok(bytes) = std::fs::read(harness.cache_mgr.cache_path(rel))
            && bytes == expected
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn backing_modify_replaces_cache_entry() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("show.mkv");

    std::fs::write(h.backing_path().join(&rel), b"original content padded____").unwrap();

    h.cache_io.submit_cache(rel.clone()).await;
    let cached = wait_for_replaced(
        &h,
        &rel,
        b"original content padded____",
        Duration::from_secs(5),
    )
    .await;
    assert!(cached, "file should be cached before mutation");

    // Overwrite the backing file — inotify fires IN_MODIFY on the root watch.
    std::fs::write(h.backing_path().join(&rel), b"modified content padded____").unwrap();

    h.wait_for_debounce().await;
    let replaced = wait_for_replaced(
        &h,
        &rel,
        b"modified content padded____",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        replaced,
        "cache entry should be replaced after backing modify"
    );
}

#[tokio::test]
async fn backing_delete_evicts_cache_entry() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("movie.mkv");

    std::fs::write(h.backing_path().join(&rel), b"some movie content goes here").unwrap();

    h.cache_io.submit_cache(rel.clone()).await;
    let cached = wait_for_replaced(
        &h,
        &rel,
        b"some movie content goes here",
        Duration::from_secs(5),
    )
    .await;
    assert!(cached, "file should be cached before deletion");

    std::fs::remove_file(h.backing_path().join(&rel)).unwrap();

    h.wait_for_debounce().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !h.cache_mgr.is_cached(&rel),
        "cache entry should be evicted after backing delete"
    );
}

#[tokio::test]
async fn backing_rename_replace_replaces_cache_entry() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("episode.mkv");
    let tmp = h.backing_path().join("episode.mkv.new");

    std::fs::write(h.backing_path().join(&rel), b"version one of content bytes").unwrap();

    h.cache_io.submit_cache(rel.clone()).await;
    let cached = wait_for_replaced(
        &h,
        &rel,
        b"version one of content bytes",
        Duration::from_secs(5),
    )
    .await;
    assert!(cached, "file should be cached before rename-replace");

    std::fs::write(&tmp, b"version two of content bytes").unwrap();
    std::fs::rename(&tmp, h.backing_path().join(&rel)).unwrap();

    h.wait_for_debounce().await;
    let replaced = wait_for_replaced(
        &h,
        &rel,
        b"version two of content bytes",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        replaced,
        "cache entry should be replaced after backing rename-replace"
    );
}

#[tokio::test]
async fn unrelated_files_not_evicted() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();

    let rel_a = PathBuf::from("ep01.mkv");
    let rel_b = PathBuf::from("ep02.mkv");

    std::fs::write(
        h.backing_path().join(&rel_a),
        b"episode one content padding_",
    )
    .unwrap();
    std::fs::write(
        h.backing_path().join(&rel_b),
        b"episode two content padding_",
    )
    .unwrap();

    h.cache_io.submit_cache(rel_a.clone()).await;
    h.cache_io.submit_cache(rel_b.clone()).await;
    let a_cached = wait_for_replaced(
        &h,
        &rel_a,
        b"episode one content padding_",
        Duration::from_secs(5),
    )
    .await;
    let b_cached = wait_for_replaced(
        &h,
        &rel_b,
        b"episode two content padding_",
        Duration::from_secs(5),
    )
    .await;
    assert!(a_cached && b_cached, "both files should be cached");

    std::fs::write(
        h.backing_path().join(&rel_a),
        b"episode one content changed_",
    )
    .unwrap();

    h.wait_for_debounce().await;
    let a_replaced = wait_for_replaced(
        &h,
        &rel_a,
        b"episode one content changed_",
        Duration::from_secs(5),
    )
    .await;

    assert!(a_replaced, "ep01 should be replaced");
    assert!(h.cache_mgr.is_cached(&rel_b), "ep02 should still be cached");
}

#[tokio::test]
async fn root_auto_seeded_subdir_requires_lookup() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let root_rel = PathBuf::from("rootfile.mkv");
    let subdir_rel = PathBuf::from("subdir/nested.mkv");

    std::fs::create_dir(h.backing_path().join("subdir")).unwrap();
    std::fs::write(
        h.backing_path().join(&root_rel),
        b"root content padded________",
    )
    .unwrap();
    std::fs::write(
        h.backing_path().join(&subdir_rel),
        b"subdir content padded______",
    )
    .unwrap();

    h.cache_io.submit_cache(root_rel.clone()).await;
    h.cache_io.submit_cache(subdir_rel.clone()).await;
    assert!(
        wait_for_replaced(
            &h,
            &root_rel,
            b"root content padded________",
            Duration::from_secs(5),
        )
        .await
    );
    assert!(
        wait_for_replaced(
            &h,
            &subdir_rel,
            b"subdir content padded______",
            Duration::from_secs(5),
        )
        .await
    );

    std::fs::write(
        h.backing_path().join(&root_rel),
        b"root content changed_______",
    )
    .unwrap();
    std::fs::write(
        h.backing_path().join(&subdir_rel),
        b"subdir content changed_____",
    )
    .unwrap();

    h.wait_for_debounce().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let root_replaced = wait_for_replaced(
        &h,
        &root_rel,
        b"root content changed_______",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        root_replaced,
        "root watch auto-seeded — replacement must fire"
    );
    assert!(
        h.cache_mgr.is_cached(&subdir_rel),
        "subdir watch not seeded — mutation must be invisible"
    );
}

#[tokio::test]
async fn debounce_coalesces_burst_into_single_replacement() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("coalesce.mkv");

    std::fs::write(h.backing_path().join(&rel), b"initial content padded_____").unwrap();
    h.cache_io.submit_cache(rel.clone()).await;
    assert!(
        wait_for_replaced(
            &h,
            &rel,
            b"initial content padded_____",
            Duration::from_secs(5),
        )
        .await
    );
    for i in 0u8..5 {
        std::fs::write(h.backing_path().join(&rel), [b'a' + i; 28]).unwrap();
    }

    h.wait_for_debounce().await;
    let replaced = wait_for_replaced(&h, &rel, &[b'e'; 28], Duration::from_secs(5)).await;
    assert!(replaced, "burst should coalesce into replacement");

    // Re-cache and verify no residual window fires for the now-quiet path.
    h.cache_io.submit_cache(rel.clone()).await;
    assert!(wait_for_replaced(&h, &rel, &[b'e'; 28], Duration::from_secs(5)).await);

    h.wait_for_debounce().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        h.cache_mgr.is_cached(&rel),
        "no residual window — re-cached file must survive"
    );
}

#[tokio::test]
async fn disabled_watcher_does_not_evict() {
    let h = BackingWatchHarness::new_without_watcher(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("stable.mkv");

    std::fs::write(h.backing_path().join(&rel), b"content should stay cached_").unwrap();
    h.cache_io.submit_cache(rel.clone()).await;
    assert!(
        wait_for_replaced(
            &h,
            &rel,
            b"content should stay cached_",
            Duration::from_secs(5),
        )
        .await
    );

    std::fs::write(h.backing_path().join(&rel), b"content changed on backing_").unwrap();

    // Wait longer than a full debounce + grace period.
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 500)).await;

    assert!(
        h.cache_mgr.is_cached(&rel),
        "watcher disabled — cache entry must persist"
    );
}

#[tokio::test]
async fn attrib_change_keeps_cache_entry() {
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();
    let rel = PathBuf::from("attrib.mkv");

    std::fs::write(h.backing_path().join(&rel), b"content with changing perms").unwrap();
    h.cache_io.submit_cache(rel.clone()).await;
    assert!(
        wait_for_replaced(
            &h,
            &rel,
            b"content with changing perms",
            Duration::from_secs(5),
        )
        .await
    );

    // chmod fires IN_ATTRIB on the root watch.
    std::fs::set_permissions(
        h.backing_path().join(&rel),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    h.wait_for_debounce().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        h.cache_mgr.is_cached(&rel),
        "IN_ATTRIB should queue validation but keep unchanged content cached"
    );
}

#[tokio::test]
async fn lru_evicted_watch_goes_blind() {
    // max_dirs=64: root (auto-seeded) + dir_00 fills 2 slots.
    // Seeding dir_01..=dir_65 (65 dirs) overflows twice: root is evicted first,
    // then dir_00. After the loop dir_00's watch no longer exists.
    let h = BackingWatchHarness::new(DEBOUNCE_MS).unwrap();

    std::fs::create_dir(h.backing_path().join("dir_00")).unwrap();
    let rel = PathBuf::from("dir_00/watched.mkv");
    std::fs::write(h.backing_path().join(&rel), b"content in a subdir________").unwrap();

    // Seed dir_00's watch via a FUSE lookup.
    let _ = std::fs::read_dir(h.mount_path().join("dir_00"))
        .unwrap()
        .count();

    h.cache_io.submit_cache(rel.clone()).await;
    assert!(
        wait_for_replaced(
            &h,
            &rel,
            b"content in a subdir________",
            Duration::from_secs(5),
        )
        .await,
        "file should be cached while watch is active"
    );

    // Seed 65 more dirs, overflowing the LRU cap and evicting dir_00's watch.
    for i in 1..=65usize {
        let name = format!("dir_{i:02}");
        std::fs::create_dir(h.backing_path().join(&name)).unwrap();
        let _ = std::fs::read_dir(h.mount_path().join(&name))
            .unwrap()
            .count();
    }
    // Allow the watcher task to drain all Touch requests and any debounce windows
    // opened by the IN_CREATE events on the (now-evicted) root watch.
    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 300)).await;

    // Mutate the file — dir_00's watch is gone so no inotify event will fire.
    std::fs::write(h.backing_path().join(&rel), b"content changed while blind").unwrap();

    h.wait_for_debounce().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        h.cache_mgr.is_cached(&rel),
        "evicted watch is blind — cache entry must survive the backing change"
    );
}
