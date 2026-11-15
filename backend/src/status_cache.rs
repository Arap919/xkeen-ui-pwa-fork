use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

struct Entry {
    value: Value,
    generation: usize,
    expires_at: Instant,
}

/// One bounded response, shared by all clients of one endpoint.
#[derive(Default)]
pub struct StatusCache {
    entry: Mutex<Option<Entry>>,
    generation: AtomicUsize,
}

impl StatusCache {
    pub async fn get_or_fetch<F, Fut>(&self, fetch: F) -> Value
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Value>,
    {
        // Holding the gate across the fetch coalesces misses. Cancellation
        // drops this guard, so another client can always retry the fetch.
        let mut entry = self.entry.lock().await;
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some(cached) = entry.as_ref()
            && cached.generation == generation
            && Instant::now() < cached.expires_at
        {
            return cached.value.clone();
        }
        let value = fetch().await;
        let ttl = if value.get("success").and_then(Value::as_bool) == Some(true) {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(500)
        };
        if self.generation.load(Ordering::SeqCst) == generation {
            *entry = Some(Entry {
                value: value.clone(),
                generation,
                expires_at: Instant::now() + ttl,
            });
        } else {
            *entry = None;
        }
        value
    }

    pub fn mutation(&self) -> MutationGuard<'_> {
        self.invalidate();
        MutationGuard(self)
    }

    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// Invalidate both before and after a mutation, including error/cancellation.
/// A read during a long restart must not remain cached after it completes.
pub struct MutationGuard<'a>(&'a StatusCache);

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Notify, oneshot};

    #[tokio::test(start_paused = true)]
    async fn success_expires_two_seconds_after_completion() {
        let cache = StatusCache::default();
        assert_eq!(
            cache
                .get_or_fetch(|| async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    json!({"success": true, "running": true})
                })
                .await["running"],
            true
        );
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert_eq!(
            cache
                .get_or_fetch(|| async { json!({"success": true, "running": false}) })
                .await["running"],
            true
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            cache
                .get_or_fetch(|| async { json!({"success": true, "running": false}) })
                .await["running"],
            false
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failures_replace_expired_success_and_expire_after_500ms() {
        let cache = StatusCache::default();
        cache
            .get_or_fetch(|| async { json!({"success": true, "cpuUsage": 7}) })
            .await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = json!({"success": false, "error": "offline"});
        assert_eq!(cache.get_or_fetch(|| async { error.clone() }).await, error);
        tokio::time::advance(Duration::from_millis(499)).await;
        assert_eq!(cache.get_or_fetch(|| async { json!({"success": true}) }).await, error);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            cache.get_or_fetch(|| async { json!({"success": true}) }).await,
            json!({"success": true})
        );
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch() {
        let cache = Arc::new(StatusCache::default());
        let release = Arc::new(Notify::new());
        let (started, wait_started) = oneshot::channel();
        let first = tokio::spawn({
            let cache = cache.clone();
            let release = release.clone();
            async move {
                cache
                    .get_or_fetch(|| async {
                        started.send(()).unwrap();
                        release.notified().await;
                        json!({"success": true, "sample": 1})
                    })
                    .await
            }
        });
        wait_started.await.unwrap();
        let second = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(|| async { json!({"success": true, "sample": 2}) })
                    .await
            }
        });
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(first.await.unwrap()["sample"], 1);
        assert_eq!(second.await.unwrap()["sample"], 1);
    }

    #[tokio::test]
    async fn invalidation_rejects_result_of_older_inflight_fetch() {
        let cache = Arc::new(StatusCache::default());
        let (started, wait_started) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();
        let first = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(|| async {
                        started.send(()).unwrap();
                        wait_release.await.unwrap();
                        json!({"success": true, "sample": "old"})
                    })
                    .await
            }
        });
        wait_started.await.unwrap();
        cache.invalidate();
        release.send(()).unwrap();
        first.await.unwrap();
        assert_eq!(
            cache
                .get_or_fetch(|| async { json!({"success": true, "sample": "new"}) })
                .await["sample"],
            "new"
        );
        cache.invalidate();
        assert_eq!(
            cache
                .get_or_fetch(|| async { json!({"success": true, "sample": "newer"}) })
                .await["sample"],
            "newer"
        );
    }

    #[tokio::test]
    async fn cancelled_fetch_does_not_block_the_next_client() {
        let cache = Arc::new(StatusCache::default());
        let (started, wait_started) = oneshot::channel();
        let first = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(|| async {
                        started.send(()).unwrap();
                        std::future::pending::<Value>().await
                    })
                    .await
            }
        });
        wait_started.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_fetch(|| async { json!({"success": true}) }),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"success": true}));
    }
}
