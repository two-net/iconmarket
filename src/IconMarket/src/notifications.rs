//! Notifications (SRS §2.1): message templates, reply and push sending, and quota-aware
//! batching (#15). Rich menus are provisioned here at startup (#17).

pub mod templates;

use tracing::debug;

/// Creates or updates the rich menus for each role (FR-UI-11).
pub async fn provision_rich_menus() -> anyhow::Result<()> {
    debug!("rich-menu provisioning not implemented yet (#17)");
    Ok(())
}
