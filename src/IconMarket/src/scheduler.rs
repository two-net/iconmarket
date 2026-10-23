//! Scheduler (SRS §2.1, §4.12): in-process background jobs (FR-JOB-01). Built by #10.

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::debug;

/// Spawns the background jobs on `tracker`. Each job stops picking up work once `stop` is
/// cancelled; whatever it leaves is still in the database at the next start (NFR-REL-05).
pub fn start(_tracker: &TaskTracker, _stop: CancellationToken) {
    debug!("no background jobs yet (#10)");
}
