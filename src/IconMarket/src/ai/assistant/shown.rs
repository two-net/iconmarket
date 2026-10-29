//! What the assistant shows under its reply (FR-AI-02): the cards of what its tools found or
//! proposed, built from the database as the menus build them, so every price, stock figure,
//! total, order number and status the user sees is the database's, never the AI's (FR-AI-04).
//!
//! Products are one list, each card once, a proposal's "ใส่ 2 แผง" in place of its
//! "ใส่ตะกร้า" (Appendix A.1); shops another; then the rest as each tool rendered it. A flow a
//! tool started comes last, its question with its cancel button. LINE takes 5 messages a reply
//! (FR-LINE-11): the assistant's text first, then as many cards as fit before the flow's.

use sea_orm::DbErr;
use serde_json::{Value, json};

use crate::catalog::browse;
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::line::limits::{MESSAGES_PER_REQUEST, QUICK_REPLY_ITEMS};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{PRODUCTS_PER_PAGE, labels};
use crate::persistence::entity::{product, shop};

#[derive(Default)]
pub struct Shown {
    /// In the order first shown, each with its proposal's button, if one was made
    products: Vec<(product::Model, shop::Model, Option<Action>)>,
    shops: Vec<shop::Model>,
    /// The rest, as rendered, in order
    messages: Vec<Message>,
    /// For the last message's Quick Replies: categories, or where else to look
    choices: Vec<Action>,
    /// The messages of a flow a tool started
    flow: Option<Vec<Message>>,
    /// What was shown, by ID and name, for the conversation's memory
    remembered: Vec<Value>,
}

impl Shown {
    /// `product` of `shop`, on its card in the list.
    pub fn product(&mut self, product: product::Model, shop: shop::Model) {
        if self
            .products
            .iter()
            .any(|(shown, ..)| shown.id == product.id)
        {
            return;
        }
        self.remember(json!({ "product": product.id.to_string(), "name": product.name }));
        self.products.push((product, shop, None));
    }

    /// `product` of `shop` on its card in the list, with `proposal` in place of "ใส่ตะกร้า".
    pub fn proposal(&mut self, product: product::Model, shop: shop::Model, proposal: Action) {
        let id = product.id;
        self.product(product, shop);
        if let Some((.., shown)) = self.products.iter_mut().find(|(shown, ..)| shown.id == id) {
            *shown = Some(proposal);
        }
    }

    /// `shop`, on its card in the list of shops.
    pub fn shop(&mut self, shop: shop::Model) {
        if self.shops.iter().any(|shown| shown.id == shop.id) {
            return;
        }
        self.remember(json!({ "shop": shop.id.to_string(), "name": shop.name }));
        self.shops.push(shop);
    }

    /// `messages`, after what is shown so far.
    pub fn messages(&mut self, messages: impl IntoIterator<Item = Message>) {
        self.messages.extend(messages);
    }

    /// `choices`, as the Quick Replies of the reply.
    pub fn choices(&mut self, choices: Vec<Action>) {
        self.choices = choices;
    }

    /// The `messages` of a flow a tool started, which come last.
    pub fn flow(&mut self, messages: Vec<Message>) {
        self.flow = Some(messages);
    }

    /// Whether a tool started a flow: the reply ends with its question, so no other tool runs.
    pub fn started(&self) -> bool {
        self.flow.is_some()
    }

    /// Whether anything is shown at all.
    pub fn is_empty(&self) -> bool {
        self.products.is_empty()
            && self.shops.is_empty()
            && self.messages.is_empty()
            && self.choices.is_empty()
            && self.flow.is_none()
    }

    /// Keeps `shown`, by ID and name, in the conversation's memory.
    pub fn remember(&mut self, shown: Value) {
        self.remembered.push(shown);
    }

    /// What was shown, for the conversation's memory.
    pub fn memory(&self) -> Value {
        Value::Array(self.remembered.clone())
    }

    /// The reply: `text` first, if any, then the cards, as many as LINE takes.
    pub async fn render(
        self,
        turn: &Turn<'_>,
        config: &Config,
        text: Option<Message>,
    ) -> Result<Vec<Message>, DbErr> {
        let mut messages: Vec<Message> = text.into_iter().collect();
        if !self.products.is_empty() {
            let mut products = self.products;
            products.truncate(PRODUCTS_PER_PAGE);
            messages.push(browse::found(turn, config, &products).await?);
        }
        if !self.shops.is_empty() {
            let mut shops = self.shops;
            shops.truncate(PRODUCTS_PER_PAGE);
            messages.push(browse::found_shops(turn, config, &shops).await?);
        }
        messages.extend(self.messages);
        let flow = self.flow.unwrap_or_default();
        messages.truncate(MESSAGES_PER_REQUEST.saturating_sub(flow.len()));
        if !flow.is_empty() {
            // Its question is last, with its cancel button (FR-UI-04)
            messages.extend(flow);
            return Ok(messages);
        }
        // Somewhere to go on from (FR-UI-03)
        if let Some(last) = messages.pop() {
            let last = if !self.choices.is_empty() {
                let mut choices = self.choices;
                choices.truncate(QUICK_REPLY_ITEMS - 1);
                choices.push(Action::message(labels::MENU.clone(), labels::MENU.as_str()));
                last.with_quick_reply(QuickReply::new(choices))
            } else if last.quick_reply.is_none() {
                last.with_quick_reply(menus())
            } else {
                last
            };
            messages.push(last);
        }
        Ok(messages)
    }
}

/// The main menu and help, as if typed.
fn menus() -> QuickReply {
    QuickReply::new(
        [&labels::MENU, &labels::HELP].map(|label| Action::message(label.clone(), label.as_str())),
    )
}
