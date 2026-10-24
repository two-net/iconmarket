//! Notifications (SRS §2.1): message templates, reply and push sending, and quota-aware
//! batching (#15). Rich menus are provisioned here at startup (#17).

pub mod templates;

use sea_orm::DatabaseConnection;
use tracing::debug;

use crate::line;
use crate::users::roles;

/// Creates or updates the rich menus for each role (FR-UI-11).
pub async fn provision_rich_menus(_line: &line::Client) -> anyhow::Result<()> {
    debug!("rich-menu provisioning not implemented yet (#17)");
    Ok(())
}

/// Links the users whose roles changed to the rich menu for their roles (FR-UI-10): all of
/// them, or only the user `only`. A user stays outdated until relinked, so a failed run is made
/// up for by the next one. Called at startup, and after a change of roles is committed.
pub async fn relink_rich_menus(
    db: &DatabaseConnection,
    _line: &line::Client,
    only: Option<i64>,
) -> anyhow::Result<()> {
    let outdated = roles::outdated_menus(db, only).await?;
    debug!(
        users = outdated.len(),
        "rich-menu relinking not implemented yet (#17)"
    );
    Ok(())
}
