//! Help (FR-HELP-01, FR-HELP-03): "ช่วยเหลือ" lists the topics for the user's roles, and each
//! topic answers with built-in text and buttons for what to do next. Every user gets the same
//! eight topics, each told for their roles: a seller learns how to list products rather than
//! how to open a shop, say. Admins get one more, on their own menus.
//!
//! The text of a topic is its [`article`], which is also all the AI assistant may answer a
//! how-to question from (FR-AI-14): its `get_help` tool (#37) takes a topic's [`Topic::key`]
//! and returns the article for the user's roles.
//!
//! Help is open before onboarding (FR-UI-12) and to suspended users (FR-ADM-04). Reporting a
//! product or shop, which the "แจ้งปัญหา" topic explains, is [`report`](super::report).

#[cfg(test)]
mod tests;

use sea_orm::DbErr;

use super::DELETE_ACCOUNT;
use super::access::{self, authorize};
use super::onboarding;
use super::profile;
use super::roles::Roles;
use crate::ai::{Feature, Gateway};
use crate::config::{Config, PaymentMethod};
use crate::dispatcher::Turn;
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::line::{Action, Message};
use crate::notifications::templates::{HelpFacts, Templates, labels, topic_label};
use crate::thai::Label;

/// What the actions of the topics' buttons start with: `help.ordering`, `help.privacy`, …
/// "ช่วยเหลือ" itself is [`Item::Help`].
pub const PREFIX: &str = "help.";

/// A help topic (FR-HELP-01), in the order "ช่วยเหลือ" lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    /// "วิธีสั่งซื้อ"
    Ordering,
    /// "เปิดร้านและลงสินค้า": opening a shop, or for sellers, listing products
    Selling,
    /// "การชำระเงิน"
    Payment,
    /// "ข้อมูลของฉัน" (FR-ONB-05)
    MyData,
    /// "ความเป็นส่วนตัว": what the privacy notice says (NFR-PRIV-02)
    Privacy,
    /// "แจ้งปัญหา" (FR-HELP-02)
    Report,
    /// "ติดต่อผู้ดูแล" (FR-HELP-03)
    Contact,
    /// "ลบบัญชี" (FR-ONB-07)
    DeleteAccount,
    /// "งานผู้ดูแล": the admins' menus, for admins only
    Admin,
}

impl Topic {
    pub const ALL: [Self; 9] = [
        Self::Ordering,
        Self::Selling,
        Self::Payment,
        Self::MyData,
        Self::Privacy,
        Self::Report,
        Self::Contact,
        Self::DeleteAccount,
        Self::Admin,
    ];

    /// Its postback's action: [`PREFIX`] and its [`key`](Self::key).
    pub fn action(self) -> &'static str {
        match self {
            Self::Ordering => "help.ordering",
            Self::Selling => "help.selling",
            Self::Payment => "help.payment",
            Self::MyData => "help.my_data",
            Self::Privacy => "help.privacy",
            Self::Report => "help.report",
            Self::Contact => "help.contact",
            Self::DeleteAccount => "help.delete_account",
            Self::Admin => "help.admin",
        }
    }

    /// Its name, as the AI's `get_help` tool takes it (#37).
    pub fn key(self) -> &'static str {
        &self.action()[PREFIX.len()..]
    }

    /// The topic named `key`.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|topic| topic.key() == key)
    }

    pub fn postback(self) -> Postback {
        Postback::new(self.action())
    }

    /// Whether users with `roles` get it.
    pub fn is_for(self, roles: Roles) -> bool {
        self != Self::Admin || roles.admin
    }

    /// The topics for `roles`, in order.
    pub fn for_roles(roles: Roles) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |topic| topic.is_for(roles))
    }
}

/// "ช่วยเหลือ", typed or tapped: the topics for the user's roles.
pub async fn show(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Help).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let topics = Topic::for_roles(turn.actor.roles());
    Ok(vec![turn.templates.help(topics)])
}

/// A tap on "ช่วยเหลือ", or on one of its topics. A topic the user no longer gets, say one of
/// a role they lost, shows the topics they do.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    ai: &Gateway,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    let actor = &turn.actor;
    let topic = postback
        .action()
        .strip_prefix(PREFIX)
        .and_then(Topic::from_key)
        .filter(|topic| topic.is_for(actor.roles()));
    let Some(topic) = topic else {
        return show(turn).await;
    };
    if let Err(denied) = authorize(turn.db, actor, access::Action::Help).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let roles = actor.roles();
    let article = article(turn.templates, config, ai, roles, topic);
    let next = next_steps(roles, actor.shop.is_some(), config, topic);
    Ok(vec![turn.templates.help_topic(article, next)])
}

/// The text of `topic` for a user with `roles` (FR-HELP-01). The AI assistant answers how-to
/// questions from this alone (FR-AI-14), so it is what the topic shows in the chat.
pub fn article(
    templates: &Templates,
    config: &Config,
    ai: &Gateway,
    roles: Roles,
    topic: Topic,
) -> String {
    let methods = &config.payment.methods;
    let facts = HelpFacts {
        seller: roles.seller,
        admin: roles.admin,
        assistant: ai.is_enabled(Feature::Assistant),
        listing: ai.is_enabled(Feature::Listing),
        require_approval: config.shop.require_approval,
        pending_minutes: config.order.pending_timeout_minutes,
        cash: methods.contains(&PaymentMethod::Cash),
        promptpay: methods.contains(&PaymentMethod::PromptPay),
        status_pushes: config.notification.order_status_to_buyer,
        admin_alerts: config.notification.admin_alerts,
        admin_contact: config.village.admin_contact.as_deref(),
        privacy: onboarding::privacy_facts(config),
    };
    templates.help_article(topic, &facts)
}

/// The buttons for what to do about `topic` (FR-UI-03), for a user with `roles` who has a shop
/// if `has_shop`, approved or not.
fn next_steps(roles: Roles, has_shop: bool, config: &Config, topic: Topic) -> Vec<Action> {
    let button = |postback: Postback, label: &Label| postback.button(label.clone());
    let item = |item: Item, label: &Label| button(item.postback(), label);
    let seller = roles.seller;
    match topic {
        Topic::Ordering => vec![
            item(Item::Buy, &labels::BUY),
            item(Item::Search, &labels::SEARCH),
            item(Item::Cart, &labels::CART),
        ],
        Topic::Selling if seller => vec![
            item(Item::AddProduct, &labels::ADD_PRODUCT),
            item(Item::MyProducts, &labels::MY_PRODUCTS),
            item(Item::ShopOrders, &labels::SHOP_ORDERS),
        ],
        Topic::Selling if has_shop => vec![item(Item::MyShop, &labels::MY_SHOP)],
        Topic::Selling => vec![item(Item::MyShop, &labels::OPEN_SHOP)],
        Topic::Payment if seller => vec![
            item(Item::MyOrders, &labels::MY_ORDERS),
            item(Item::ShopOrders, &labels::SHOP_ORDERS),
        ],
        Topic::Payment => vec![item(Item::MyOrders, &labels::MY_ORDERS)],
        Topic::MyData => vec![button(Postback::new(profile::SHOW), &labels::MY_DATA)],
        Topic::Privacy => match &config.privacy.notice_url {
            Some(url) => vec![Action::Uri {
                label: labels::FULL_NOTICE.clone(),
                uri: url.as_str().to_owned(),
            }],
            None => Vec::new(),
        },
        Topic::Report => {
            let contact = Topic::Contact;
            vec![button(contact.postback(), topic_label(contact))]
        }
        Topic::Contact => Vec::new(),
        Topic::DeleteAccount => vec![button(
            Postback::new(DELETE_ACCOUNT),
            &labels::DELETE_ACCOUNT,
        )],
        Topic::Admin => vec![
            item(Item::ReviewQueue, &labels::REVIEW_QUEUE),
            item(Item::ApproveShops, &labels::APPROVE_SHOPS),
        ],
    }
}
