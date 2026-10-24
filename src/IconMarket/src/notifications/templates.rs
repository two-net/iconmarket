//! Message templates: every user-facing Thai text, kept together and apart from logic
//! (NFR-MNT-03). Text is polite, short and plain, in the configured voice (FR-UI-08), and
//! every error says what to do next without technical words (NFR-USE-03).
//!
//! Each flow adds its messages, labels and card layouts here. Cards are built from the LINE
//! message types in [`line::flex`](crate::line::flex).

#![allow(
    dead_code,
    reason = "the flows that send these messages are built by later issues"
)]

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::catalog::images::Rejected;
use crate::config::{AppConfig, PoliteParticle};
use crate::dispatcher::menu::{Item, Tab};
use crate::line::flex::{
    Bubble, BubbleSize, Button, ButtonHeight, ButtonStyle, Carousel, FlexBox, FlexContainer,
    Justify, Size, Spacing, Text, Weight,
};
use crate::line::{Action, Message, QuickReply, QuickReplyItem, limits};
use crate::thai::{self, Label, NumberError};
use crate::users::access::Denied;

/// Builds messages in the configured voice and time zone.
pub struct Templates {
    particle: PoliteParticle,
    time_zone: Tz,
}

impl Templates {
    pub fn new(app: &AppConfig) -> Self {
        Self {
            particle: app.polite_particle,
            time_zone: app.time_zone,
        }
    }

    /// Ends a statement: ค่ะ or ครับ.
    fn kha(&self) -> &'static str {
        match self.particle {
            PoliteParticle::Kha => "ค่ะ",
            PoliteParticle::Khrap => "ครับ",
        }
    }

    /// Ends a question, or follows นะ: คะ or ครับ.
    fn kha_question(&self) -> &'static str {
        match self.particle {
            PoliteParticle::Kha => "คะ",
            PoliteParticle::Khrap => "ครับ",
        }
    }

    /// `22 ต.ค. 2569 14:30`, in the business time zone
    pub fn date_time(&self, at: DateTime<Utc>) -> String {
        thai::format_date_time(at, self.time_zone)
    }

    /// `22 ต.ค. 2569`, in the business time zone
    pub fn date(&self, at: DateTime<Utc>) -> String {
        thai::format_date(at, self.time_zone)
    }

    /// Something failed on our side; the user can retry or start over.
    pub fn something_went_wrong(&self) -> String {
        let (kha, menu) = (self.kha(), labels::MENU.as_str());
        format!("ขอโทษ{kha} ระบบขัดข้องชั่วคราว ลองใหม่อีกครั้ง หรือพิมพ์ \"{menu}\" เพื่อเริ่มใหม่{kha}")
    }

    /// A daily AI limit is reached (FR-AI-09): the assistant rests until tomorrow, and the
    /// menus still work.
    pub fn ai_resting(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ตอนนี้ผู้ช่วยขอพักก่อนนะ{kha_question} พรุ่งนี้กลับมาถามใหม่ได้ ระหว่างนี้เลือกจากเมนูด้านล่างได้เลย{kha}"
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// The AI failed, timed out or kept answering badly (FR-AI-05, FR-AI-10).
    pub fn ai_unavailable(&self) -> Message {
        let kha = self.kha();
        let text =
            format!("ขอโทษ{kha} ผู้ช่วยตอบไม่ได้ในตอนนี้ ลองใหม่อีกครั้ง หรือเลือกจากเมนูด้านล่างได้เลย{kha}");
        Message::text(text).with_quick_reply(menus())
    }

    /// Why an action was refused (NFR-SEC-02, NFR-SEC-10): what the user can still do, and
    /// where to ask. A button tapped too late gets this rather than a technical error.
    pub fn denied(&self, denied: Denied) -> Message {
        let (kha, help) = (self.kha(), labels::HELP.as_str());
        let text = match denied {
            Denied::Suspended => format!(
                "ขอโทษ{kha} บัญชีของคุณถูกระงับการใช้งานชั่วคราว ยังดูคำสั่งซื้อของคุณและเมนูช่วยเหลือได้ตามปกติ สอบถามได้ที่ \"{help}\" แล้วเลือก \"ติดต่อผู้ดูแล\"{kha}"
            ),
            Denied::ShopSuspended => format!(
                "ขอโทษ{kha} ร้านของคุณถูกระงับชั่วคราว จึงแก้ไขร้านและสินค้าไม่ได้ แต่ยังดูแลออเดอร์ที่มีอยู่ได้ตามปกติ สอบถามได้ที่ \"{help}\" แล้วเลือก \"ติดต่อผู้ดูแล\"{kha}"
            ),
            Denied::NotAllowed => {
                format!("ขอโทษ{kha} รายการนี้ทำไม่ได้ เลือกจากเมนูด้านล่างได้เลย{kha}")
            }
            Denied::NotFound => {
                format!("ขอโทษ{kha} ไม่พบรายการนี้แล้ว อาจถูกลบหรือเปลี่ยนไป เลือกจากเมนูด้านล่างได้เลย{kha}")
            }
        };
        Message::text(text).with_quick_reply(menus())
    }

    /// The user's guided flow went unanswered for `minutes` and was ended (FR-UI-15). Goes first
    /// in the reply to their next event.
    pub fn step_expired(&self, minutes: u32) -> Message {
        let kha = self.kha();
        let text = format!(
            "ขั้นตอนที่ทำค้างไว้หมดเวลาแล้ว{kha} เพราะไม่ได้ตอบนานเกิน {minutes} นาที ถ้ายังต้องการ เริ่มใหม่จากเมนูได้เลย{kha}"
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// A guided flow or a proposal was cancelled (FR-UI-04). The main menu follows.
    pub fn cancelled(&self) -> Message {
        let kha = self.kha();
        Message::text(format!("ยกเลิกแล้ว{kha} เลือกเมนูที่ต้องการได้เลย{kha}"))
    }

    /// The last resort of routing (FR-UI-12): what to do instead. The main menu follows.
    pub fn fallback(&self) -> Message {
        let (kha, help) = (self.kha(), labels::HELP.as_str());
        Message::text(format!(
            "ขอโทษ{kha} ยังไม่เข้าใจข้อความนี้ เลือกจากเมนูด้านล่าง หรือพิมพ์ \"{help}\" เพื่อดูวิธีใช้ได้เลย{kha}"
        ))
    }

    /// Stickers, videos, files, and voice messages while voice is off (FR-UI-09). The main
    /// menu follows.
    pub fn unsupported(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ข้อความแบบนี้ยังใช้ไม่ได้ พิมพ์เป็นข้อความ หรือเลือกจากเมนูด้านล่างได้เลย{kha}"
        ))
    }

    /// A photo nothing asked for (FR-UI-14).
    pub fn image_not_expected(&self) -> Message {
        let kha = self.kha();
        let text = format!(
            "ได้รับรูปแล้ว{kha} ตอนนี้ยังไม่ต้องใช้รูป ถ้าต้องใช้ ระบบจะขอเอง ระหว่างนี้เลือกจากเมนูด้านล่างได้เลย{kha}"
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// A location no step asked for (FR-UI-14).
    pub fn location_not_expected(&self) -> Message {
        let kha = self.kha();
        let text = format!(
            "ได้รับตำแหน่งแล้ว{kha} ตอนนี้ยังไม่ต้องใช้ตำแหน่ง ถ้าต้องใช้ ระบบจะขอเอง ระหว่างนี้เลือกจากเมนูด้านล่างได้เลย{kha}"
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// The user sends more than `limits.messagesPerUserPerMinute` (NFR-SEC-05). Sent once; what
    /// they send until they slow down is ignored.
    pub fn slow_down(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ส่งข้อความเร็วเกินไปนิดหนึ่ง{kha} รอสักครู่แล้วค่อยส่งใหม่นะ{kha_question}"
        ))
    }

    /// Why a typed number was not accepted, and how to type it.
    pub fn number_problem(&self, error: NumberError) -> String {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        match error {
            NumberError::NotANumber => {
                format!("ขอโทษ{kha} อ่านตัวเลขไม่ออก พิมพ์เป็นตัวเลขได้เลย{kha} เช่น 20 หรือ ๒๐")
            }
            NumberError::TooManyDecimals => {
                format!("ขอโทษ{kha} ใส่ทศนิยมได้ไม่เกิน 2 ตำแหน่ง พิมพ์ใหม่ได้เลย{kha} เช่น 12.50")
            }
            NumberError::NotWhole => {
                format!("ขอโทษ{kha} ตรงนี้ต้องเป็นจำนวนเต็ม พิมพ์ใหม่ได้เลย{kha} เช่น 2 หรือ 10")
            }
            NumberError::TooLarge => {
                format!("ขอโทษ{kha} ตัวเลขนี้มากเกินไป ลองตรวจดูแล้วพิมพ์ใหม่อีกครั้งนะ{kha_question}")
            }
        }
    }

    /// Why a photo was not kept, and what to send instead (FR-PROD-04).
    pub fn image_rejected(&self, rejected: Rejected) -> String {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        match rejected {
            Rejected::TooLarge { max_mb } => format!(
                "ขอโทษ{kha} รูปนี้ใหญ่เกิน {max_mb} เมกะไบต์ ลองส่งรูปที่เล็กลง หรือแคปหน้าจอรูปนั้นแล้วส่งแทนได้เลย{kha}"
            ),
            Rejected::NotJpegOrPng => {
                format!("ขอโทษ{kha} ไฟล์นี้ไม่ใช่รูปภาพ ส่งเป็นรูปถ่ายจากกล้องหรืออัลบั้มได้เลย{kha}")
            }
            Rejected::Unreadable => {
                format!("ขอโทษ{kha} เปิดรูปนี้ไม่ได้ ลองถ่ายใหม่หรือเลือกรูปอื่นแล้วส่งอีกครั้งนะ{kha_question}")
            }
            Rejected::Full { max } => format!(
                "ขอโทษ{kha} สินค้าหนึ่งชิ้นมีรูปได้ไม่เกิน {max} รูป ถ้าอยากใช้รูปนี้ เปลี่ยนรูปเดิมได้ที่ \"สินค้าของฉัน\"{kha}"
            ),
            Rejected::Unavailable => {
                format!("ขอโทษ{kha} รับรูปไม่สำเร็จ ส่งรูปเดิมอีกครั้งได้เลย{kha}")
            }
        }
    }
}

/// Where to carry on when the assistant can't help: the main menu (FR-UI-05) and help
/// (FR-HELP-01), as if typed.
fn menus() -> QuickReply {
    QuickReply::new(
        [&labels::MENU, &labels::HELP].map(|label| Action::message(label.clone(), label.as_str())),
    )
}

/// `message` with the cancel button among its Quick Replies, as every prompt of a guided flow
/// has (FR-UI-04). Sends `ยกเลิก` as if typed.
pub fn with_cancel(mut message: Message) -> Message {
    let cancel = Action::message(labels::CANCEL.clone(), labels::CANCEL.as_str());
    let quick_reply = message
        .quick_reply
        .get_or_insert_with(|| QuickReply { items: Vec::new() });
    if !quick_reply.items.iter().any(|item| item.action == cancel) {
        quick_reply.items.push(QuickReplyItem { action: cancel });
    }
    message
}

/// The main menu as a message (FR-UI-05): a card for each of `tabs`, with a button for each
/// function. `has_shop` turns "เปิดร้าน" into "ร้านของฉัน".
pub fn main_menu(tabs: impl IntoIterator<Item = Tab>, has_shop: bool) -> Message {
    let cards = tabs
        .into_iter()
        .map(|tab| menu_card(tab, has_shop))
        .collect();
    Message::flex("เมนูหลัก", Carousel::new(cards))
}

fn menu_card(tab: Tab, has_shop: bool) -> Bubble {
    let title = Text {
        size: Some(Size::Lg),
        weight: Some(Weight::Bold),
        ..Text::new(tab_title(tab).as_str())
    };
    let buttons = tab.items().map(|item| {
        let label = item_label(item, has_shop).clone();
        let button = Button {
            style: Some(ButtonStyle::Secondary),
            height: Some(ButtonHeight::Sm),
            ..Button::new(item.postback().button(label))
        };
        button.into()
    });
    Bubble {
        size: Some(BubbleSize::Kilo),
        header: Some(FlexBox::vertical([title.into()])),
        body: Some(FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(buttons)
        }),
        ..Bubble::default()
    }
}

fn tab_title(tab: Tab) -> &'static Label {
    match tab {
        Tab::Buyer => &labels::BUYER,
        Tab::Seller => &labels::SELLER,
        Tab::Admin => &labels::ADMIN,
    }
}

fn item_label(item: Item, has_shop: bool) -> &'static Label {
    match item {
        Item::Buy => &labels::BUY,
        Item::Search => &labels::SEARCH,
        Item::Cart => &labels::CART,
        Item::MyOrders => &labels::MY_ORDERS,
        Item::MyShop if has_shop => &labels::MY_SHOP,
        Item::MyShop => &labels::OPEN_SHOP,
        Item::Help => &labels::HELP,
        Item::ShopOrders => &labels::SHOP_ORDERS,
        Item::AddProduct => &labels::ADD_PRODUCT,
        Item::MyProducts => &labels::MY_PRODUCTS,
        Item::OpenClose => &labels::OPEN_CLOSE,
        Item::Sales => &labels::SALES,
        Item::ShopSettings => &labels::SHOP_SETTINGS,
        Item::ApproveShops => &labels::APPROVE_SHOPS,
        Item::ReviewQueue => &labels::REVIEW_QUEUE,
        Item::Users => &labels::USERS,
        Item::Categories => &labels::CATEGORIES,
        Item::Stats => &labels::STATS,
        Item::Announcements => &labels::ANNOUNCEMENTS,
    }
}

/// Page `number` (from 0) of `items` as a carousel (FR-LINE-11, FR-SRCH-03): a `card` for
/// each item on the page and, when items remain, a "ดูเพิ่มเติม" bubble whose button sends
/// the postback `more(next_page)`. `per_page` is capped so that bubble always fits.
pub fn carousel_page<T>(
    items: &[T],
    number: usize,
    per_page: usize,
    card: impl Fn(&T) -> Bubble,
    more: impl FnOnce(usize) -> String,
) -> FlexContainer {
    let per_page = per_page.clamp(1, limits::BUBBLES_PER_CAROUSEL - 1);
    let page = limits::page(items, number, per_page);
    let mut bubbles: Vec<Bubble> = page.items.iter().map(card).collect();
    if let Some(next) = page.next {
        // A carousel's bubbles must all be the same size
        let size = bubbles.first().and_then(|bubble| bubble.size);
        bubbles.push(more_bubble(
            size,
            Action::postback(labels::MORE.clone(), more(next)),
        ));
    }
    Carousel::new(bubbles).into()
}

/// The last bubble of a carousel page: one button for the next page.
fn more_bubble(size: Option<BubbleSize>, more: Action) -> Bubble {
    let button = Button {
        style: Some(ButtonStyle::Link),
        ..Button::new(more)
    };
    let body = FlexBox {
        justify_content: Some(Justify::Center),
        ..FlexBox::vertical([button.into()])
    };
    Bubble {
        size,
        body: Some(body),
        ..Bubble::default()
    }
}

/// Page `number` (from 0) of `items` as Quick Reply buttons (FR-LINE-11): a `button` for each
/// item on the page and, when items remain, "ดูเพิ่มเติม" sending the postback
/// `more(next_page)`. `per_page` is capped so that button fits; leave room for the flow's
/// own buttons, such as cancel (FR-UI-04).
pub fn quick_reply_page<T>(
    items: &[T],
    number: usize,
    per_page: usize,
    button: impl Fn(&T) -> Action,
    more: impl FnOnce(usize) -> String,
) -> Vec<Action> {
    let per_page = per_page.clamp(1, limits::QUICK_REPLY_ITEMS - 1);
    let page = limits::page(items, number, per_page);
    let mut actions: Vec<Action> = page.items.iter().map(button).collect();
    if let Some(next) = page.next {
        actions.push(Action::postback(labels::MORE.clone(), more(next)));
    }
    actions
}

/// Declares fixed labels and lists them all in `ALL`, so the length test covers every one.
macro_rules! labels {
    ($($(#[$doc:meta])* $name:ident = $text:literal,)*) => {
        $(
            $(#[$doc])*
            pub static $name: Label = Label::from_static($text);
        )*

        /// Every fixed label, with its name
        pub static ALL: &[(&str, &Label)] = &[$((stringify!($name), &$name)),*];
    };
}

/// Fixed button and Quick Reply labels (NFR-USE-04). Labels with run-time parts are built
/// with [`Label::fit`](crate::thai::Label::fit).
pub mod labels {
    use crate::thai::Label;

    labels! {
        /// Leaves a guided flow and discards its draft (FR-UI-04)
        CANCEL = "ยกเลิก",
        /// Shows the main menu (FR-UI-05)
        MENU = "เมนู",
        /// The next page of a long list (FR-LINE-11)
        MORE = "ดูเพิ่มเติม",
        CONFIRM = "ยืนยัน",
        EDIT = "แก้ไข",
        /// Help topics (FR-HELP-01)
        HELP = "ช่วยเหลือ",
        /// The user's orders, as typed (FR-UI-12)
        ORDERS = "ออเดอร์",
        /// Cancels the flow and clears the AI's memory of the conversation (FR-AI-11)
        RESTART = "เริ่มใหม่",

        // The menu's tabs and functions (§3.1.2)
        BUYER = "ผู้ซื้อ",
        SELLER = "ผู้ขาย",
        ADMIN = "ผู้ดูแล",
        BUY = "ซื้อของ",
        SEARCH = "ค้นหา",
        CART = "ตะกร้า",
        MY_ORDERS = "คำสั่งซื้อของฉัน",
        OPEN_SHOP = "เปิดร้าน",
        MY_SHOP = "ร้านของฉัน",
        SHOP_ORDERS = "ออเดอร์ร้าน",
        ADD_PRODUCT = "เพิ่มสินค้า",
        MY_PRODUCTS = "สินค้าของฉัน",
        OPEN_CLOSE = "เปิด/ปิดร้าน",
        SALES = "ยอดขาย",
        SHOP_SETTINGS = "ตั้งค่าร้าน",
        APPROVE_SHOPS = "อนุมัติร้าน",
        REVIEW_QUEUE = "รายการรอตรวจ",
        USERS = "ผู้ใช้",
        CATEGORIES = "หมวดหมู่",
        STATS = "สถิติ",
        ANNOUNCEMENTS = "ประกาศ",
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use chrono::TimeZone;

    use super::*;
    use crate::line::Body;
    use crate::line::flex::{Component, Text};
    use crate::thai::Label;

    const ERRORS: [NumberError; 4] = [
        NumberError::NotANumber,
        NumberError::TooManyDecimals,
        NumberError::NotWhole,
        NumberError::TooLarge,
    ];

    const DENIALS: [Denied; 4] = [
        Denied::Suspended,
        Denied::ShopSuspended,
        Denied::NotAllowed,
        Denied::NotFound,
    ];

    const REJECTIONS: [Rejected; 5] = [
        Rejected::TooLarge { max_mb: 10 },
        Rejected::NotJpegOrPng,
        Rejected::Unreadable,
        Rejected::Full { max: 3 },
        Rejected::Unavailable,
    ];

    fn templates(particle: PoliteParticle) -> Templates {
        Templates {
            particle,
            time_zone: chrono_tz::Asia::Bangkok,
        }
    }

    fn text(message: &Message) -> &str {
        match &message.body {
            Body::Text { text } => text,
            body => panic!("not text: {body:?}"),
        }
    }

    /// Every message text, for checks that apply to all of them.
    fn all_texts(t: &Templates) -> Vec<String> {
        let mut texts = vec![
            t.something_went_wrong(),
            text(&t.ai_resting()).to_owned(),
            text(&t.ai_unavailable()).to_owned(),
            text(&t.step_expired(15)).to_owned(),
            text(&t.cancelled()).to_owned(),
            text(&t.fallback()).to_owned(),
            text(&t.unsupported()).to_owned(),
            text(&t.image_not_expected()).to_owned(),
            text(&t.location_not_expected()).to_owned(),
            text(&t.slow_down()).to_owned(),
        ];
        texts.extend(ERRORS.map(|error| t.number_problem(error)));
        texts.extend(REJECTIONS.map(|rejected| t.image_rejected(rejected)));
        texts.extend(DENIALS.map(|denied| text(&t.denied(denied)).to_owned()));
        texts
    }

    #[test]
    fn labels_fit_on_a_button() {
        for (name, label) in labels::ALL {
            let chars = label.as_str().chars().count();
            assert!(
                chars <= Label::MAX_CHARS,
                "label {name} ({:?}) has {chars} characters, more than {}",
                label.as_str(),
                Label::MAX_CHARS
            );
        }
    }

    #[test]
    fn errors_have_no_technical_text() {
        for particle in [PoliteParticle::Kha, PoliteParticle::Khrap] {
            for text in all_texts(&templates(particle)) {
                assert!(!text.chars().any(|c| c.is_ascii_alphabetic()), "{text}");
            }
        }
    }

    #[test]
    fn male_voice_never_uses_the_female_particle() {
        for text in all_texts(&templates(PoliteParticle::Khrap)) {
            assert!(text.contains("ครับ"), "{text}");
            assert!(!text.contains("ค่ะ") && !text.contains("คะ"), "{text}");
        }
    }

    #[test]
    fn female_voice_uses_kha_for_statements_and_questions() {
        let t = templates(PoliteParticle::Kha);
        for text in all_texts(&t) {
            assert!(!text.contains("ครับ"), "{text}");
        }
        assert!(t.something_went_wrong().ends_with("ค่ะ"));
        assert!(t.number_problem(NumberError::TooLarge).ends_with("นะคะ"));
    }

    /// FR-AI-09, FR-AI-10: when the assistant can't help, the user is offered the menus.
    #[test]
    fn ai_fallbacks_offer_the_menus() {
        let t = templates(PoliteParticle::Kha);
        assert!(text(&t.ai_resting()).contains("พัก"));
        assert!(text(&t.ai_unavailable()).starts_with("ขอโทษค่ะ"));
        for message in [t.ai_resting(), t.ai_unavailable()] {
            let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
            let actions: Vec<_> = quick_reply.items.iter().map(|item| &item.action).collect();
            assert_eq!(
                actions,
                [
                    &Action::message(labels::MENU.clone(), "เมนู"),
                    &Action::message(labels::HELP.clone(), "ช่วยเหลือ"),
                ]
            );
            limits::check(&[message]).unwrap();
        }
    }

    /// NFR-SEC-10, FR-ADM-04: a refusal says what still works and offers the menus.
    #[test]
    fn refusals_say_what_still_works_and_offer_the_menus() {
        let t = templates(PoliteParticle::Kha);
        assert!(text(&t.denied(Denied::Suspended)).contains("ยังดูคำสั่งซื้อของคุณ"));
        assert!(text(&t.denied(Denied::ShopSuspended)).contains("ยังดูแลออเดอร์ที่มีอยู่"));
        for denied in DENIALS {
            let message = t.denied(denied);
            assert!(text(&message).starts_with("ขอโทษค่ะ"), "{denied:?}");
            let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
            assert_eq!(quick_reply.items.len(), 2, "{denied:?}");
            limits::check(&[message]).unwrap();
        }
    }

    /// FR-UI-15: an expired step says how long it waited, and where to start again.
    #[test]
    fn an_expired_step_says_why_and_offers_the_menus() {
        let message = templates(PoliteParticle::Kha).step_expired(15);
        assert!(text(&message).contains("เกิน 15 นาที"));
        assert_eq!(message.quick_reply.as_ref().unwrap().items.len(), 2);
        limits::check(&[message]).unwrap();
    }

    /// FR-UI-04: every prompt of a flow gets the cancel button, once, after its own.
    #[test]
    fn prompts_get_the_cancel_button_once() {
        let cancel = Action::message(labels::CANCEL.clone(), "ยกเลิก");
        let plain = with_cancel(Message::text("ชื่อสินค้าคืออะไรคะ"));
        let actions = |message: &Message| -> Vec<Action> {
            let items = &message.quick_reply.as_ref().expect("a Quick Reply").items;
            items.iter().map(|item| item.action.clone()).collect()
        };
        assert_eq!(actions(&plain), slice::from_ref(&cancel));

        let zone = Action::message(Label::fit("ซอย 1"), "ซอย 1");
        let choices = Message::text("อยู่ซอยไหนคะ").with_quick_reply(QuickReply::new([zone.clone()]));
        let choices = with_cancel(choices);
        assert_eq!(actions(&choices), [zone, cancel]);
        assert_eq!(actions(&with_cancel(choices.clone())), actions(&choices));
        limits::check(&[plain, choices]).unwrap();
    }

    /// FR-PROD-04: a refused photo comes with the limit it broke and what to send instead.
    #[test]
    fn refused_photos_are_explained_with_their_limit() {
        let t = templates(PoliteParticle::Kha);
        assert!(
            t.image_rejected(Rejected::TooLarge { max_mb: 10 })
                .contains("ใหญ่เกิน 10 เมกะไบต์")
        );
        assert!(
            t.image_rejected(Rejected::Full { max: 3 })
                .contains("ไม่เกิน 3 รูป")
        );
        for rejected in REJECTIONS {
            let text = t.image_rejected(rejected);
            assert!(text.starts_with("ขอโทษค่ะ"), "{text}");
            limits::check(&[Message::text(text)]).unwrap();
        }
    }

    /// A card for product `number`, sized like search's
    fn card(number: &u32) -> Bubble {
        Bubble {
            size: Some(BubbleSize::Kilo),
            body: Some(FlexBox::vertical([
                Text::new(format!("สินค้า {number}")).into()
            ])),
            ..Bubble::default()
        }
    }

    /// The bubbles of a carousel page, and the postback data of its "more" bubble if any
    fn carousel(page: FlexContainer) -> (Vec<Bubble>, Option<String>) {
        let FlexContainer::Carousel(carousel) = page else {
            panic!("not a carousel");
        };
        let mut bubbles = carousel.contents;
        let more = bubbles.last().and_then(|bubble| match &bubble.body {
            Some(body) => match body.contents.as_slice() {
                [Component::Button(button)] => match &button.action {
                    Action::Postback { label, data, .. } if *label == labels::MORE => {
                        Some(data.clone())
                    }
                    _ => None,
                },
                _ => None,
            },
            None => None,
        });
        if more.is_some() {
            bubbles.pop();
        }
        (bubbles, more)
    }

    /// AC (#6): a list longer than a carousel holds is split into pages with "ดูเพิ่มเติม".
    #[test]
    fn long_lists_are_paged_with_a_more_bubble() {
        let products: Vec<u32> = (1..=30).collect();
        let page = |number| {
            carousel_page(&products, number, 11, card, |next| {
                format!("search:page={next}")
            })
        };

        let pages: Vec<_> = (0..3).map(page).collect();
        for page in &pages {
            let message = Message::flex("สินค้า", page.clone());
            limits::check(&[message]).unwrap();
        }
        let pages: Vec<_> = pages.into_iter().map(carousel).collect();
        assert_eq!(pages[0].0.len(), 11);
        assert_eq!(pages[0].1.as_deref(), Some("search:page=1"));
        assert_eq!(pages[1].0.len(), 11);
        assert_eq!(pages[1].1.as_deref(), Some("search:page=2"));
        assert_eq!(pages[2].0.len(), 8);
        assert_eq!(pages[2].1, None);

        let shown: Vec<_> = pages.iter().flat_map(|(bubbles, _)| bubbles).collect();
        let expected: Vec<_> = products.iter().map(card).collect();
        assert_eq!(
            shown,
            expected.iter().collect::<Vec<_>>(),
            "every item once, in order"
        );
    }

    #[test]
    fn the_more_bubble_matches_the_cards_and_fits() {
        let products: Vec<u32> = (1..=30).collect();
        // More than a carousel holds is capped, leaving room for "ดูเพิ่มเติม"
        let FlexContainer::Carousel(page) = carousel_page(&products, 0, 50, card, |_| "m".into())
        else {
            panic!("not a carousel");
        };
        assert_eq!(page.contents.len(), limits::BUBBLES_PER_CAROUSEL);
        assert!(
            page.contents
                .iter()
                .all(|bubble| bubble.size == Some(BubbleSize::Kilo))
        );

        // A list that fits needs no "ดูเพิ่มเติม"
        let (bubbles, more) = carousel(carousel_page(&products[..10], 0, 10, card, |_| "m".into()));
        assert_eq!((bubbles.len(), more), (10, None));
    }

    #[test]
    fn long_quick_reply_lists_are_paged() {
        let zones: Vec<String> = (1..=20).map(|i| format!("ซอย {i}")).collect();
        let button = |zone: &String| Action::postback(Label::fit(zone.as_str()), zone.as_str());
        let first = quick_reply_page(&zones, 0, 99, button, |next| format!("zone:page={next}"));
        assert_eq!(first.len(), limits::QUICK_REPLY_ITEMS);
        assert_eq!(
            first.last(),
            Some(&Action::postback(labels::MORE.clone(), "zone:page=1"))
        );

        let second = quick_reply_page(&zones, 1, 99, button, |next| format!("zone:page={next}"));
        assert_eq!(second.len(), 8);
        assert!(second.iter().all(|action| action.label() != &labels::MORE));
    }

    #[test]
    fn dates_use_the_configured_time_zone() {
        let at = Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap();
        let t = templates(PoliteParticle::Kha);
        assert_eq!(t.date_time(at), "22 ต.ค. 2569 14:30");
        assert_eq!(t.date(at), "22 ต.ค. 2569");
    }
}
