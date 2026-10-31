//! Virtual villagers, each doing what the real one does: reading the reply, finding the button
//! and tapping it, as `offered()` and `pressed()` do in the worker's tests. The next tap can't be
//! worked out in advance: a confirmation has a single-use ID, the Quick Replies depend on the
//! shop, and a seller hears of an order by push.
//!
//! A step that doesn't get what it expects fails the journey. A wrong tap gets a fast refusal,
//! which would otherwise pass for a fast reply.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::oneshot;

use crate::Random;
use crate::line::Line;
use crate::run::{Kind, Metrics};

/// How long a user waits for an answer before it counts as lost
const PATIENCE: Duration = Duration::from_secs(60);
/// A person reads the reply and thinks, then taps
const THINK_MS: (u64, u64) = (3_000, 8_000);

const SLOW_DOWN: &str = "ส่งข้อความเร็วเกินไป";

pub type Step = Result<Value, String>;

/// What every villager shares.
pub struct Market {
    pub http: reqwest::Client,
    pub app: String,
    pub secret: String,
    pub line: Arc<Line>,
    pub metrics: Metrics,
    pub categories: Vec<String>,
    pub buyers: Mutex<VecDeque<String>>,
    pub listers: Mutex<VecDeque<String>>,
    pub started: Instant,
    pub random: Mutex<Random>,
    /// Tells this run's events and new users from an earlier run's on the same database
    pub run: u64,
    /// Numbers for events and for users new to the OA
    pub numbers: AtomicU64,
    /// Product photos LINE has fetched, once each, as its CDN does
    pub fetched: Mutex<HashSet<String>>,
    pub orders_placed: AtomicU64,
}

impl Market {
    fn number(&self) -> u64 {
        self.numbers.fetch_add(1, Ordering::Relaxed)
    }

    /// The LINE user ID of a villager nobody has seen.
    pub fn new_user(&self) -> String {
        format!("U{:016x}{:016x}", self.run, self.number())
    }

    /// A category for buyers: any but the last, which takes what's listed during the run, or
    /// those new products, shown first, would draw nearly every order.
    fn category_to_browse(&self) -> String {
        let browsed = self.categories.len() - 1;
        self.categories[self.pick(browsed)].clone()
    }

    fn category_to_list_in(&self) -> String {
        self.categories[self.categories.len() - 1].clone()
    }

    fn think(&self) -> Duration {
        let ms = self.random.lock().unwrap().between(THINK_MS.0, THINK_MS.1);
        Duration::from_millis(ms)
    }

    fn pick(&self, choices: usize) -> usize {
        self.random.lock().unwrap().between(0, choices as u64) as usize
    }

    fn sign(&self, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("HMAC takes a key of any length");
        mac.update(body.as_bytes());
        STANDARD.encode(mac.finalize().into_bytes())
    }

    /// Fetches the product photos of `messages` LINE hasn't seen, as its CDN would.
    fn fetch_photos(self: &Arc<Self>, messages: &Value) {
        let text = messages.to_string();
        for (at, _) in text.match_indices("/img/p/") {
            let path: String = text[at..]
                .chars()
                .take_while(|&c| c != '"' && c != '\\')
                .collect();
            if !self.fetched.lock().unwrap().insert(path.clone()) {
                continue;
            }
            let market = Arc::clone(self);
            tokio::spawn(async move {
                let started = Instant::now();
                let url = format!("{}{path}", market.app);
                if let Ok(answer) = market.http.get(url).send().await
                    && answer.status().is_success()
                    && answer.bytes().await.is_ok()
                {
                    market
                        .metrics
                        .record(Kind::Photo, started - market.started, started.elapsed());
                }
            });
        }
    }
}

/// The actions labelled `label` among the buttons and Quick Replies of `messages`.
pub fn offered_any(messages: &Value, label: &str) -> Vec<Value> {
    fn find(value: &Value, label: &str, found: &mut Vec<Value>) {
        match value {
            Value::Object(fields) => {
                let labelled = fields
                    .get("label")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(label));
                if labelled && fields.contains_key("type") {
                    found.push(value.clone());
                }
                fields.values().for_each(|value| find(value, label, found));
            }
            Value::Array(values) => values.iter().for_each(|value| find(value, label, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    find(messages, label, &mut found);
    found
}

fn offers(messages: &Value, label: &str) -> bool {
    !offered_any(messages, label).is_empty()
}

/// One villager in one journey.
pub struct Person {
    market: Arc<Market>,
    id: String,
    /// When their next event is due
    at: Instant,
}

impl Person {
    pub fn new(market: &Arc<Market>, id: String, at: Instant) -> Self {
        Self {
            market: Arc::clone(market),
            id,
            at,
        }
    }

    /// Sends the event, due at `self.at` however busy the app is, and waits for its reply.
    async fn send(&mut self, kind: &str, fields: Value, reply: Kind) -> Step {
        let market = Arc::clone(&self.market);
        tokio::time::sleep_until(self.at.into()).await;
        let due = self.at;
        let at = due - market.started;
        market.metrics.record(Kind::Lateness, at, due.elapsed());

        let token = format!("load-{}-{}", market.run, market.number());
        let (waiter, delivered) = oneshot::channel();
        market.line.expect_reply(&token, waiter);
        let mut event = json!({
            "type": kind,
            "mode": "active",
            "timestamp": 1_761_300_000_000_u64,
            "webhookEventId": token,
            "deliveryContext": { "isRedelivery": false },
            "replyToken": token,
            "source": { "type": "user", "userId": self.id },
        });
        for (key, value) in fields.as_object().into_iter().flatten() {
            event[key] = value.clone();
        }
        let body = json!({ "destination": "Uloadtest", "events": [event] }).to_string();
        let acknowledged = market
            .http
            .post(format!("{}/webhook/line", market.app))
            .header("x-line-signature", market.sign(&body))
            .body(body)
            .send()
            .await
            .map_err(|_| "webhook unreachable".to_owned())?;
        market.metrics.record(Kind::Ack, at, due.elapsed());
        if !acknowledged.status().is_success() {
            return Err(format!("webhook answered {}", acknowledged.status()));
        }

        let answer = tokio::time::timeout(PATIENCE, delivered)
            .await
            .map_err(|_| "no reply in time".to_owned())?
            .map_err(|_| "no reply".to_owned())?;
        market.metrics.record(reply, at, answer.at - due);
        self.at = answer.at + market.think();
        if answer.messages.to_string().contains(SLOW_DOWN) {
            return Err("told to slow down".to_owned());
        }
        market.fetch_photos(&answer.messages);
        Ok(answer.messages)
    }

    async fn says(&mut self, text: &str) -> Step {
        let message = json!({ "id": "1", "type": "text", "quoteToken": "q", "text": text });
        self.send("message", json!({ "message": message }), Kind::Reply)
            .await
    }

    async fn taps(&mut self, data: &str) -> Step {
        let postback = json!({ "postback": { "data": data } });
        self.send("postback", postback, Kind::Reply).await
    }

    async fn sends_photo(&mut self) -> Step {
        let id = self.market.number().to_string();
        let photo = json!({ "id": id, "type": "image", "contentProvider": { "type": "line" } });
        self.send("message", json!({ "message": photo }), Kind::ImageReply)
            .await
    }

    /// Taps the one action labelled `label`, or one of several at random.
    async fn presses(&mut self, messages: &Value, label: &str) -> Step {
        let actions = offered_any(messages, label);
        if actions.is_empty() {
            return Err(format!("\"{label}\" wasn't offered"));
        }
        let action = &actions[self.market.pick(actions.len())];
        match action["type"].as_str() {
            Some("postback") => self.taps(action["data"].as_str().unwrap_or_default()).await,
            Some("message") => self.says(action["text"].as_str().unwrap_or_default()).await,
            _ => Err(format!("\"{label}\" can't be tapped")),
        }
    }

    /// Picks a category from `messages` and returns its products.
    async fn opens_a_category(&mut self, messages: &Value) -> Step {
        let category = self.market.category_to_browse();
        self.presses(messages, &category).await
    }

    /// Puts a product in the cart and goes to its summary, choosing `payment` if the shop takes it.
    async fn checks_out(&mut self, payment: &str) -> Step {
        let categories = self.taps("buy").await?;
        let products = self.opens_a_category(&categories).await?;
        let asked = self.presses(&products, "ใส่ตะกร้า").await?;
        let added = self.presses(&asked, "2").await?;
        let mut step = self.presses(&added, "สั่งซื้อเลย").await?;
        for _ in 0..5 {
            if offers(&step, "ยืนยันสั่งซื้อ") {
                return Ok(step);
            }
            let next = [payment, "ไม่มี", "ใช่", "ให้ไปส่ง", "รับเอง", "เงินสดตอนรับของ"]
                .into_iter()
                .find(|label| offers(&step, label))
                .ok_or_else(|| "checkout stopped".to_owned())?;
            step = self.presses(&step, next).await?;
        }
        Err("checkout never reached its summary".to_owned())
    }

    /// Confirms the order `summary` shows.
    async fn confirms(&mut self, summary: &Value) -> Result<(), String> {
        let placed = self.presses(summary, "ยืนยันสั่งซื้อ").await?;
        if !placed.to_string().contains("สั่งซื้อเรียบร้อย") {
            return Err("no order placed".to_owned());
        }
        self.market.orders_placed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

pub async fn browses(mut person: Person) -> Result<(), String> {
    person.says("เมนู").await?;
    let categories = person.taps("buy").await?;
    let products = person.opens_a_category(&categories).await?;
    person.presses(&products, "ดูร้าน").await?;
    Ok(())
}

pub async fn searches(mut person: Person) -> Result<(), String> {
    const WORDS: [&str; 6] = ["ไข่", "ขนมครก", "ส้มตำ 12", "กาแฟ สูตร", "หมูปิ้ง", "ทุเรียน"];
    person.taps("search").await?;
    let word = WORDS[person.market.pick(WORDS.len())];
    let found = person.says(word).await?;
    if offers(&found, "ดูเพิ่มเติม") {
        person.presses(&found, "ดูเพิ่มเติม").await?;
    }
    Ok(())
}

pub async fn pays_cash(mut person: Person) -> Result<(), String> {
    let summary = person.checks_out("เงินสดตอนรับของ").await?;
    person.confirms(&summary).await
}

/// Orders and, if the shop takes PromptPay, sends a slip once the seller accepts.
pub async fn pays_by_promptpay(mut person: Person) -> Result<(), String> {
    let summary = person.checks_out("โอนพร้อมเพย์").await?;
    let by_promptpay = summary.to_string().contains("พร้อมเพย์");
    let (waiter, accepted) = oneshot::channel();
    person.market.line.expect_slip_push(&person.id, waiter);
    person.confirms(&summary).await?;
    if !by_promptpay {
        return Ok(());
    }
    let notice = tokio::time::timeout(Duration::from_secs(180), accepted)
        .await
        .map_err(|_| "the seller never accepted".to_owned())?
        .map_err(|_| "no accepted push".to_owned())?;
    person.at = Instant::now() + person.market.think();
    person.presses(&notice, "ส่งสลิป").await?;
    person.sends_photo().await?;
    Ok(())
}

/// Takes the order a push announced to its end, each step on what the last reply offered, or
/// records a payment the push asks about.
pub async fn sells(mut person: Person, push: Value) -> Result<(), String> {
    const NEXT: [&str; 4] = ["กำลังไปส่ง", "พร้อมให้รับ", "ส่งมอบแล้ว", "ได้รับเงินแล้ว"];
    let mut reply = if offers(&push, "รับออเดอร์") {
        person.presses(&push, "รับออเดอร์").await?
    } else {
        person.presses(&push, "ได้รับเงินแล้ว").await?
    };
    for _ in 0..NEXT.len() {
        let Some(next) = NEXT.into_iter().find(|label| offers(&reply, label)) else {
            return Ok(());
        };
        reply = person.presses(&reply, next).await?;
    }
    Ok(())
}

/// Lists a product with two photos.
pub async fn lists(mut person: Person) -> Result<(), String> {
    let name = format!("สินค้าทดสอบ {}", person.market.number());
    let category = person.market.category_to_list_in();
    person.taps("add_product").await?;
    person.sends_photo().await?;
    person.sends_photo().await?;
    for answer in ["ถัดไป", &name, "85", "ชิ้น", &category, "30"] {
        person.says(answer).await?;
    }
    let preview = person.says("ข้าม").await?;
    let listed = person.presses(&preview, "ยืนยันลงขาย").await?;
    if offers(&listed, "เพิ่มสินค้าอีก") {
        Ok(())
    } else {
        Err("not listed".to_owned())
    }
}

/// Adds the OA, accepts the notice and opens the menu.
pub async fn joins(mut person: Person) -> Result<(), String> {
    let notice = person
        .send(
            "follow",
            json!({ "follow": { "isUnblocked": false } }),
            Kind::Reply,
        )
        .await?;
    person.presses(&notice, "ยอมรับ").await?;
    person.says("เมนู").await?;
    Ok(())
}

/// A photo sent at `at`, by a seller who has just started listing: the burst of NFR-PERF-04.
pub async fn starts_listing_with_a_photo(mut person: Person, at: Instant) -> Result<(), String> {
    person.taps("add_product").await?;
    person.at = at;
    person.sends_photo().await?;
    person.says("ยกเลิก").await?;
    Ok(())
}
