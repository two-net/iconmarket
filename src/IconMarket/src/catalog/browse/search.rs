//! "ค้นหา" (FR-SRCH-04): a buyer types what they look for, and gets the products whose name,
//! description, category or shop name contain it, as a list like any other (FR-SRCH-02).
//!
//! Thai is written without spaces between words, so a keyword matches anywhere inside a name,
//! not only whole words: *ครก* finds *ขนมครก*. Latin letters match whatever their case. A
//! keyword of several words, *ไข่ ป้าแดง*, finds the products that match every one of them,
//! each in any of the four. What the user types is matched as written, never as a pattern, and
//! never logged (NFR-PRIV-05).
//!
//! The AI assistant searches too (FR-SRCH-05): it makes of a request in plain Thai, such as
//! *อยากได้ผักสดส่งถึงบ้าน ไม่เกิน 50 บาท*, the keyword and other words for the same thing, any
//! of which may match in its place, plus a category, a highest price and shops that deliver.
//! What it finds comes from the database all the same.
//!
//! The keyword is asked for in a [flow](SearchFlow) of one step. A postback can't carry it
//! (NFR-SEC-03), so a list longer than a page [keeps](pending::keep) the search for its
//! "ดูเพิ่มเติม" to page through, until `session.pendingActionMinutes` have passed; after
//! that, or once `ยกเลิก` discards it, the button asks for the keyword again.

use sea_orm::prelude::Uuid;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::extension::postgres::PgExpr;
use sea_orm::{ColumnTrait, Condition, DbErr};
use serde::{Deserialize, Serialize};

use super::listing::Listing;
use super::{PAGE, page_of, products, suggestions};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::pending;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::Message;
use crate::notifications::templates::Browsed;
use crate::persistence::entity::{category, product, shop};
use crate::thai::Money;
use crate::users::access::{self, authorize};

/// The action of "ดูเพิ่มเติม" under a search's results: `pa` names the search kept, and
/// `page` the page to show
pub const MORE: &str = "browse.search";
const KEPT: &str = "pa";
/// What a search is kept as in `pending_actions.kind`. Keep it across releases, or the
/// "ดูเพิ่มเติม" buttons sent before an upgrade ask for the keyword again.
const KIND: &str = "search";

/// The longest keyword, in characters: as long as a product's name (FR-PROD-03)
pub const KEYWORD_CHARS: usize = 80;

/// A search (FR-SRCH-04): the keyword the user typed, or what the AI assistant makes of a
/// request in plain Thai (FR-SRCH-05). What the AI adds reads as nothing from a search kept
/// before it could, so its "ดูเพิ่มเติม" pages on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Search {
    /// As typed, with its spaces made single
    keyword: String,
    /// Other words for the same thing, each of which may match in the keyword's place
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    alternatives: Vec<String>,
    /// Only the products of the category with this ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category_id: Option<i64>,
    /// Only the products for at most this a unit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_price: Option<Money>,
    /// Only the products of shops that deliver
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    delivery_only: bool,
}

/// Why typed text is not a keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Empty,
    /// Longer than this many characters
    TooLong(usize),
}

impl Search {
    /// What the user typed as a search.
    pub fn parse(text: &str) -> Result<Self, Refused> {
        Ok(Self {
            keyword: keyword(text)?,
            alternatives: Vec::new(),
            category_id: None,
            max_price: None,
            delivery_only: false,
        })
    }

    /// A search for any of `keywords`, the first of which the reply names; one empty is left
    /// out, and so is one repeated (FR-SRCH-05).
    pub fn any_of<'a>(keywords: impl IntoIterator<Item = &'a str>) -> Result<Self, Refused> {
        let mut keywords = keywords.into_iter().filter_map(|text| keyword(text).ok());
        let mut search = Self::parse(&keywords.next().unwrap_or_default())?;
        for keyword in keywords {
            let known =
                keyword == search.keyword || search.alternatives.as_slice().contains(&keyword);
            if !known {
                search.alternatives.push(keyword);
            }
        }
        Ok(search)
    }

    /// Only what is in the category with ID `id`.
    pub fn in_category(self, id: i64) -> Self {
        Self {
            category_id: Some(id),
            ..self
        }
    }

    /// Only what costs at most `price` a unit.
    pub fn at_most(self, price: Money) -> Self {
        Self {
            max_price: Some(price),
            ..self
        }
    }

    /// Only what shops that deliver sell.
    pub fn delivered(self) -> Self {
        Self {
            delivery_only: true,
            ..self
        }
    }

    /// The keyword, as the reply names it.
    pub fn keyword(&self) -> &str {
        &self.keyword
    }

    /// What a product found matches: each word of the keyword, or of one of its alternatives,
    /// inside its name, description, category name or shop name, whatever the case; and the
    /// filters. For a select joining the product's shop and category.
    pub(super) fn condition(&self) -> Condition {
        let keywords = std::iter::once(&self.keyword).chain(&self.alternatives);
        let any = keywords.fold(Condition::any(), |any, keyword| any.add(matches(keyword)));
        let mut condition = Condition::all().add(any);
        if let Some(id) = self.category_id {
            condition = condition.add(product::Column::CategoryId.eq(id));
        }
        if let Some(price) = self.max_price {
            condition = condition.add(product::Column::Price.lte(price));
        }
        if self.delivery_only {
            condition = condition.add(shop::Column::OffersDelivery.eq(true));
        }
        condition
    }
}

/// `text` as a keyword: its spaces made single, and not too long.
fn keyword(text: &str) -> Result<String, Refused> {
    let keyword = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if keyword.is_empty() {
        Err(Refused::Empty)
    } else if keyword.chars().count() > KEYWORD_CHARS {
        Err(Refused::TooLong(KEYWORD_CHARS))
    } else {
        Ok(keyword)
    }
}

/// What a product matching `keyword` has: each of its words inside its name, description,
/// category name or shop name, whatever the case.
fn matches(keyword: &str) -> Condition {
    let mut words: Vec<&str> = Vec::new();
    for word in keyword.split(' ') {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words.into_iter().fold(Condition::all(), |all, word| {
        // Without `ESCAPE`, which sea-query puts inside `ILIKE (…)` where PostgreSQL refuses
        // it: PostgreSQL escapes with `\` anyway
        let pattern = || format!("%{}%", escaped(word));
        let any = Condition::any()
            .add(Expr::col((product::Entity, product::Column::Name)).ilike(pattern()))
            .add(Expr::col((product::Entity, product::Column::Description)).ilike(pattern()))
            .add(Expr::col((category::Entity, category::Column::Name)).ilike(pattern()))
            .add(Expr::col((shop::Entity, shop::Column::Name)).ilike(pattern()));
        all.add(any)
    })
}

/// `word` with `\`, `%` and `_` escaped by `\`, so that `LIKE` matches it as written.
pub(crate) fn escaped(word: &str) -> String {
    let mut escaped = String::with_capacity(word.len());
    for c in word.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The flow asking what to search for. It has no draft: the keyword is all it asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFlow {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Keyword,
}

impl Flow for SearchFlow {
    const NAME: &'static str = "search";
    type Step = Step;
}

/// A tap on "ค้นหา": asks what to search for.
pub async fn start(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Browse).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    ask(turn, turn.templates.ask_keyword()).await
}

/// The keyword, typed (FR-UI-12 step 3): the first page of what it finds. `None` for anything
/// but text, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    _session: Session<SearchFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let search = match Search::parse(text) {
        Ok(search) => search,
        Err(refused) => {
            let again = match refused {
                Refused::Empty => turn.templates.ask_keyword(),
                Refused::TooLong(max) => turn.templates.text_too_long(max),
            };
            let again = Answer::<SearchFlow>::Again(vec![again]);
            return flow::answered(turn, again).await.map(Some);
        }
    };
    let found = results(turn, config, &search, None, 0).await?;
    flow::answered(turn, Answer::<SearchFlow>::Done(found))
        .await
        .map(Some)
}

/// A tap on "ดูเพิ่มเติม" under a search's results: the page it asks for, while the search is
/// kept; otherwise the keyword is asked for again.
pub(super) async fn more(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    let kept = match postback.uuid(KEPT) {
        Some(id) => pending::kept::<Search>(turn, KIND, id)
            .await?
            .map(|search| (id, search)),
        None => None,
    };
    let Some((id, search)) = kept else {
        return ask(turn, turn.templates.search_expired()).await;
    };
    let page = postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok())
        .unwrap_or(0);
    results(turn, config, &search, Some(id), page).await
}

/// Starts the flow with `question`, in place of any flow in progress.
async fn ask(turn: &Turn<'_>, question: Message) -> Result<Vec<Message>, DbErr> {
    let session = Session {
        step: Step::Keyword,
        draft: SearchFlow::default(),
    };
    flow::start(turn, session, vec![question]).await
}

/// Page `page` of what `search` finds, kept as `kept` if it already is. Its "ดูเพิ่มเติม"
/// keeps it, if not yet kept. Nothing found, the reply says so and suggests where else to look
/// (FR-SRCH-08).
async fn results(
    turn: &Turn<'_>,
    config: &Config,
    search: &Search,
    kept: Option<Uuid>,
    page: usize,
) -> Result<Vec<Message>, DbErr> {
    let found = page_of(turn, Listing::Search(search), page).await?;
    let browsed = Browsed::Search(search.keyword());
    if found.items.is_empty() {
        let suggestions = suggestions(turn, browsed).await?;
        return Ok(vec![turn.templates.nothing_found(browsed, suggestions)]);
    }
    let more = match found.next {
        Some(next) => {
            let id = match kept {
                Some(id) => id,
                None => pending::keep(turn, KIND, search).await?,
            };
            let next = i64::try_from(next).expect("a few pages");
            Some(Postback::new(MORE).with_uuid(KEPT, id).with_int(PAGE, next))
        }
        None => None,
    };
    Ok(vec![
        products(turn, config, browsed, &found.items, more).await?,
    ])
}
