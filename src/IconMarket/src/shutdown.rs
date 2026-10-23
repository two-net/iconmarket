//! Graceful shutdown (NFR-REL-05): stop taking new events, finish in-flight work for up
//! to 10 s, and leave the rest in the inbox for the next start.

use std::io;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tracing::warn;

pub const DRAIN_LIMIT: Duration = Duration::from_secs(10);

/// Resolves on SIGTERM or Ctrl-C.
pub async fn signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("cannot listen for Ctrl-C");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("cannot listen for SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Waits up to `limit` for the server to finish its in-flight requests and for the
/// background jobs on `tracker` to stop. Returns false if time ran out.
pub async fn drain(
    server: JoinHandle<io::Result<()>>,
    tracker: &TaskTracker,
    limit: Duration,
) -> bool {
    tracker.close();
    let finished = async {
        match server.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "server failed while shutting down"),
            Err(e) => warn!(error = %e, "server task failed while shutting down"),
        }
        tracker.wait().await;
    };
    tokio::time::timeout(limit, finished).await.is_ok()
}

#[cfg(test)]
mod tests {
    use tokio::time::{Instant, sleep};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waits_for_work_that_finishes_in_time() {
        let tracker = TaskTracker::new();
        tracker.spawn(sleep(Duration::from_secs(2)));
        let server = tokio::spawn(async {
            sleep(Duration::from_secs(3)).await;
            Ok(())
        });

        let start = Instant::now();
        assert!(drain(server, &tracker, DRAIN_LIMIT).await);
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_at_the_limit() {
        let tracker = TaskTracker::new();
        tracker.spawn(sleep(Duration::from_secs(60)));
        let server = tokio::spawn(async { Ok(()) });

        let start = Instant::now();
        assert!(!drain(server, &tracker, DRAIN_LIMIT).await);
        assert_eq!(start.elapsed(), DRAIN_LIMIT);
    }
}
