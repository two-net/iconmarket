//! Authorization (NFR-SEC-02, NFR-SEC-10): the one check, by role and ownership, of every
//! action a user asks for, however they asked: menu, typed keyword, postback or AI tool.
//!
//! The actor is loaded from the database for each event, never cached or taken from postback
//! data, so a suspension or a lost role applies to the very next tap, old buttons included.
//! Postbacks carry only opaque IDs (NFR-SEC-03), which are checked here against what the actor
//! owns. The conversation framework (#13) runs [`authorize`] before executing any postback, and
//! the AI assistant (#37) before every tool call, with the requesting user as the actor
//! (FR-AI-02).
//!
//! This decides who may act, not whether the action still makes sense: whether an order can
//! still be accepted, or a product is still for sale, is each flow's to check (FR-UI-13).

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use super::roles::Roles;
use crate::persistence::entity::{order, product, shop, user};

/// The user acting, and their shop if they have one.
#[derive(Clone, Debug, PartialEq)]
pub struct Actor {
    pub user: user::Model,
    pub shop: Option<shop::Model>,
}

impl Actor {
    /// User `user_id` as they are now; `None` if there is no such user or the account is
    /// deleted.
    pub async fn load(db: &impl ConnectionTrait, user_id: i64) -> Result<Option<Self>, DbErr> {
        let Some(user) = user::Entity::find_by_id(user_id).one(db).await? else {
            return Ok(None);
        };
        if user.status == user::Status::Deleted {
            return Ok(None);
        }
        let shop = shop::Entity::find()
            .filter(shop::Column::OwnerId.eq(user.id))
            .one(db)
            .await?;
        Ok(Some(Self { user, shop }))
    }

    pub fn roles(&self) -> Roles {
        Roles::of(&self.user, self.shop.as_ref())
    }

    fn is_active(&self) -> bool {
        self.user.status == user::Status::Active
    }

    /// Whether the actor may manage their shop: only an `APPROVED` one (FR-SHOP-10).
    fn manages_shop(&self) -> Result<(), Denied> {
        match self.shop.as_ref().map(|shop| shop.status) {
            Some(shop::Status::Approved) => Ok(()),
            Some(shop::Status::Suspended) => Err(Denied::ShopSuspended),
            Some(shop::Status::Pending | shop::Status::Rejected | shop::Status::Closed) | None => {
                Err(Denied::NotAllowed)
            }
        }
    }

    fn shop_id(&self) -> Option<i64> {
        self.shop.as_ref().map(|shop| shop.id)
    }
}

/// Something a user asks to do. Beyond the IDs of what it acts on, it carries nothing from the
/// request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    // Open to every user, suspended ones too (FR-ADM-04, NFR-PRIV-03)
    /// Help topics and the admins' contact (FR-HELP)
    Help,
    /// Categories, shops, products and search (FR-SRCH)
    Browse,
    /// Seeing and correcting one's personal data, and deleting the account (FR-ONB-05,
    /// FR-ONB-07)
    MyData,
    /// Seeing an order: its buyer, its shop's owner, or an admin who isn't suspended
    ViewOrder(i64),

    // Buyers: every user who isn't suspended
    /// The cart and checkout (FR-CART)
    Buy,
    /// Cancelling an order, confirming its receipt or sending its slip: its buyer (FR-ORD,
    /// FR-PAY)
    ActAsBuyer(i64),
    /// Reporting a product or shop (FR-HELP-02)
    Report,
    /// The AI assistant (FR-AI)
    UseAi,
    /// Applying to open a shop (FR-SHOP-01)
    ApplyForShop,

    // Sellers
    /// Opening, closing and setting up the shop, adding products and seeing its sales: the
    /// owner of an `APPROVED` shop (FR-SHOP, FR-PROD)
    ManageShop,
    /// Editing, restocking or deleting a product: the owner of its shop, while `APPROVED`
    ManageProduct(i64),
    /// Accepting, rejecting, progressing or cancelling an order, and confirming its payment:
    /// the owner of its shop. A suspended shop's orders continue (FR-SHOP-10).
    HandleOrder(i64),

    // Admins
    /// Approvals, the review queue, users, categories, statistics, announcements and
    /// cancelling orders (FR-ADM, FR-ORD-12)
    Administer,
}

impl Action {
    /// Whether a suspended user may still do it: they can't order, sell or use the AI, but
    /// can see their orders and get help (FR-ADM-04), and keep their data rights (NFR-PRIV-03).
    fn open_to_suspended(self) -> bool {
        matches!(
            self,
            Self::Help | Self::Browse | Self::MyData | Self::ViewOrder(_)
        )
    }
}

/// Why an action was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// The user is suspended (FR-ADM-04)
    Suspended,
    /// The user's shop is suspended (FR-SHOP-10)
    ShopSuspended,
    /// The user lacks the role, or what they act on isn't theirs
    NotAllowed,
    /// What they act on no longer exists
    NotFound,
}

/// Whether `actor` may do `action` now (NFR-SEC-02). Loads what the action acts on to check
/// who owns it.
pub async fn authorize(
    db: &impl ConnectionTrait,
    actor: &Actor,
    action: Action,
) -> Result<Result<(), Denied>, DbErr> {
    match actor.user.status {
        user::Status::Active => {}
        user::Status::Suspended if action.open_to_suspended() => {}
        user::Status::Suspended => return Ok(Err(Denied::Suspended)),
        user::Status::Deleted => return Ok(Err(Denied::NotAllowed)),
    }
    let user_id = actor.user.id;
    let decision = match action {
        Action::Help
        | Action::Browse
        | Action::MyData
        | Action::Buy
        | Action::Report
        | Action::UseAi
        | Action::ApplyForShop => Ok(()),
        Action::ViewOrder(id) => {
            order_check(db, id, |order| {
                order.buyer_id == user_id
                    || Some(order.shop_id) == actor.shop_id()
                    || (actor.user.is_admin && actor.is_active())
            })
            .await?
        }
        Action::ActAsBuyer(id) => order_check(db, id, |order| order.buyer_id == user_id).await?,
        Action::HandleOrder(id) => {
            order_check(db, id, |order| Some(order.shop_id) == actor.shop_id()).await?
        }
        Action::ManageShop => actor.manages_shop(),
        Action::ManageProduct(id) => match product::Entity::find_by_id(id).one(db).await? {
            None => Err(Denied::NotFound),
            Some(product) if Some(product.shop_id) == actor.shop_id() => actor.manages_shop(),
            Some(_) => Err(Denied::NotAllowed),
        },
        Action::Administer if actor.user.is_admin => Ok(()),
        Action::Administer => Err(Denied::NotAllowed),
    };
    Ok(decision)
}

/// Whether order `id` exists and `allowed` holds for it.
async fn order_check(
    db: &impl ConnectionTrait,
    id: i64,
    allowed: impl FnOnce(&order::Model) -> bool,
) -> Result<Result<(), Denied>, DbErr> {
    Ok(match order::Entity::find_by_id(id).one(db).await? {
        None => Err(Denied::NotFound),
        Some(order) if allowed(&order) => Ok(()),
        Some(_) => Err(Denied::NotAllowed),
    })
}
