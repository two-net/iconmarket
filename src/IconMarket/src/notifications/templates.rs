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

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use url::Url;

use crate::admin::statistics::Statistics;
use crate::catalog::images::Rejected;
use crate::catalog::products::Availability;
use crate::catalog::shops::sales::Sales;
use crate::catalog::shops::summary::Day;
use crate::config::{AiProvider, AppConfig, PoliteParticle};
use crate::dispatcher::menu::{Item, Tab};
use crate::line::flex::{
    Align, AspectMode, Bubble, BubbleSize, Button, ButtonHeight, ButtonStyle, Carousel, Component,
    FlexBox, FlexContainer, Hero, Image, Justify, Separator, Size, Spacing, Text, Weight,
};
use crate::line::{Action, Message, QuickReply, QuickReplyItem, limits};
use crate::orders::cart;
use crate::orders::periods::Orders;
use crate::payments;
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{order, product, shop};
use crate::thai::{self, Label, Money, NumberError};
use crate::users::access::Denied;
use crate::users::help::Topic;

/// Builds messages in the configured voice and time zone.
pub struct Templates {
    /// `app.name`
    app_name: String,
    particle: PoliteParticle,
    time_zone: Tz,
}

impl Templates {
    pub fn new(app: &AppConfig) -> Self {
        Self {
            app_name: app.name.clone(),
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

    /// The business time zone (`app.timeZone`), whose days number orders too (FR-ORD-01)
    pub fn time_zone(&self) -> Tz {
        self.time_zone
    }

    /// The particle that ends a statement, ค่ะ or ครับ, for text the AI writes in the same voice
    /// (FR-UI-08)
    pub fn particle(&self) -> &'static str {
        self.kha()
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

    /// What the AI assistant says above the cards of what it found or proposed when it said
    /// nothing itself (FR-AI-02).
    pub fn assistant_found(&self) -> Message {
        Message::text(format!("นี่คือที่หาให้{}", self.kha()))
    }

    /// The AI assistant's proposal to open shop `name`, or close it for now if not `open`
    /// (FR-SHOP-05), to confirm with `switch` (FR-AI-03): what that does.
    pub fn confirm_shop_open(&self, name: &str, open: bool, switch: Action) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let shop = shop_called(name);
        let (headline, then) = if open {
            (
                format!("เปิด{shop}ไหม{kha_question}"),
                "ผู้ซื้อจะสั่งสินค้าได้ตามปกติ".to_owned(),
            )
        } else {
            (
                format!("ปิด{shop}ชั่วคราวไหม{kha_question}"),
                format!("ผู้ซื้อยังเห็นสินค้า แต่จะขึ้นว่า \"{CLOSED}\" และสั่งไม่ได้ ออเดอร์ที่มีอยู่ยังทำต่อตามปกติ"),
            )
        };
        let tap = format!("กด \"{}\" เพื่อยืนยัน{kha}", switch.label().as_str());
        notice(&headline, &[then, tap], vec![switch])
    }

    /// Why an action was refused (NFR-SEC-02, NFR-SEC-10): what the user can still do, and
    /// where to ask. A button tapped too late gets this rather than a technical error.
    pub fn denied(&self, denied: Denied) -> Message {
        self.refused(denied, None)
    }

    /// [`denied`](Self::denied), with `reason`, why an admin suspended the user or their shop,
    /// when that is why (FR-ADM-04).
    pub fn refused(&self, denied: Denied, reason: Option<&str>) -> Message {
        let (kha, help) = (self.kha(), labels::HELP.as_str());
        let reason = reason
            .map(|reason| format!(" เหตุผล: {reason}"))
            .unwrap_or_default();
        let text = match denied {
            Denied::Suspended => format!(
                "ขอโทษ{kha} บัญชีของคุณถูกระงับการใช้งานชั่วคราว{reason} ยังดูคำสั่งซื้อของคุณและเมนูช่วยเหลือได้ตามปกติ สอบถามได้ที่ \"{help}\" แล้วเลือก \"ติดต่อผู้ดูแล\"{kha}"
            ),
            Denied::ShopSuspended => format!(
                "ขอโทษ{kha} ร้านของคุณถูกระงับชั่วคราว{reason} จึงแก้ไขร้านและสินค้าไม่ได้ แต่ยังดูแลออเดอร์ที่มีอยู่ได้ตามปกติ สอบถามได้ที่ \"{help}\" แล้วเลือก \"ติดต่อผู้ดูแล\"{kha}"
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

    /// A number typed in words, read by the AI as `read` (FR-AI-13): asks whether that is what
    /// was meant. The Quick Reply typing it in digits follows, then the step's choices.
    pub fn number_read(&self, read: &str) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "หมายถึง {read} ใช่ไหม{kha_question} ถ้าใช่ แตะปุ่ม \"{read}\" ด้านล่าง หรือพิมพ์เป็นตัวเลขใหม่ได้เลย{kha}"
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

    /// The answer to following the OA (FR-ONB-01), greeting the user by their LINE name if
    /// known. What they have still to do follows.
    pub fn welcome(&self, name: Option<&str>, village: &str) -> Message {
        let (kha, app) = (self.kha(), &self.app_name);
        let you = name.map(|name| format!(" คุณ{name}")).unwrap_or_default();
        Message::text(format!(
            "สวัสดี{kha}{you} ยินดีต้อนรับสู่ {app} ตลาดออนไลน์ของ{village} สั่งของจากร้านในหมู่บ้าน หรือเปิดร้านขายเองก็ได้{kha}"
        ))
    }

    /// The answer to following the OA again once onboarded (FR-ONB-01). The main menu follows.
    pub fn welcome_back(&self, name: Option<&str>, village: &str) -> Message {
        let (kha, app) = (self.kha(), &self.app_name);
        let you = name.map(|name| format!(" คุณ{name}")).unwrap_or_default();
        Message::text(format!(
            "ยินดีต้อนรับกลับมา{kha}{you} {app} ตลาดออนไลน์ของ{village} เลือกจากเมนูด้านล่างได้เลย{kha}"
        ))
    }

    /// The privacy notice, which the user must accept before anything but help (FR-ONB-02):
    /// the operator's text, then what is collected and why, how long it is kept, who receives
    /// it and how to exercise one's rights (NFR-PRIV-02).
    pub fn privacy_notice(&self, privacy: &PrivacyNotice<'_>) -> Message {
        let kha = self.kha();
        let accept = labels::ACCEPT.as_str();
        let (headline, intro) = if privacy.updated {
            (
                "ประกาศความเป็นส่วนตัวฉบับใหม่",
                format!(
                    "เราปรับปรุงประกาศความเป็นส่วนตัว กรุณาอ่านแล้วกด \"{accept}\" อีกครั้งเพื่อใช้งานต่อ{kha}"
                ),
            )
        } else {
            (
                "ประกาศความเป็นส่วนตัว",
                format!("ก่อนเริ่มใช้งาน กรุณาอ่านแล้วกด \"{accept}\"{kha}"),
            )
        };
        let facts = &privacy.facts;
        let mut details = vec![intro, facts.text.to_owned()];
        details.extend(privacy_lines(facts));
        details.push(format!("ฉบับที่ {}", facts.version));

        let mut buttons = vec![privacy.accept.clone()];
        if let Some(url) = facts.url {
            buttons.push(Action::Uri {
                label: labels::FULL_NOTICE.clone(),
                uri: url.to_owned(),
            });
        }
        notice(headline, &details, buttons).with_quick_reply(help_only())
    }

    /// Asks for `village.joinCode` (FR-ONB-03), and where to get it.
    pub fn ask_join_code(&self, village: &str, admin_contact: Option<&str>) -> Message {
        let kha = self.kha();
        let ask = admin_contact.map_or_else(
            || "สอบถามได้จากผู้ดูแลหมู่บ้าน".to_owned(),
            |contact| format!("สอบถามได้ที่ {contact}"),
        );
        Message::text(format!(
            "อีกขั้นเดียว{kha} พิมพ์รหัสเข้าร่วมของ{village}ได้เลย{kha} ถ้ายังไม่มีรหัส {ask}"
        ))
        .with_quick_reply(help_only())
    }

    /// A wrong join code, with how many tries are left before attempts are blocked (FR-ONB-03).
    pub fn join_code_wrong(&self, tries_left: usize) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} รหัสไม่ถูกต้อง ลองพิมพ์ใหม่อีกครั้งได้เลย{kha} (ลองได้อีก {tries_left} ครั้ง)"
        ))
        .with_quick_reply(help_only())
    }

    /// Too many wrong join codes: attempts are blocked `until` (FR-ONB-03).
    pub fn join_code_blocked(&self, until: DateTime<Utc>) -> Message {
        let (kha, until) = (self.kha(), self.date_time(until));
        Message::text(format!(
            "ขอโทษ{kha} ใส่รหัสผิดหลายครั้งเกินไป ลองใหม่ได้ตั้งแต่ {until} หรือสอบถามรหัสจากผู้ดูแลหมู่บ้านได้เลย{kha}"
        ))
        .with_quick_reply(help_only())
    }

    /// Onboarding is complete (FR-ONB-02, FR-ONB-03). The main menu follows.
    pub fn ready(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอบคุณ{kha} พร้อมใช้งานแล้ว เลือกจากเมนูด้านล่างได้เลย{kha}"
        ))
    }

    /// The answer to `#myid` (FR-ONB-08): what it is for, then the ID alone, to copy.
    pub fn my_id(&self, line_user_id: &str) -> Vec<Message> {
        let kha = self.kha();
        let what = format!(
            "นี่คือ LINE user ID ของคุณ{kha} กดค้างที่ข้อความถัดไปเพื่อคัดลอก ผู้ดูแลระบบใช้เพิ่มผู้ดูแลใน admin.lineUserIds ได้{kha}"
        );
        vec![Message::text(what), Message::text(line_user_id)]
    }

    /// Asks for the zone of the address (FR-ONB-04), first of its steps: one of
    /// `village.zones`, `offered` as Quick Replies, or typed when it lists none. `first` when
    /// the address is asked for the first time, so the user learns why; `current` when it is
    /// being corrected.
    pub fn ask_zone(&self, first: bool, offered: bool, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let why = if first {
            format!("ขอที่อยู่สำหรับส่งของก่อนนะ{kha_question} กรอกครั้งเดียว ครั้งต่อไปใช้ที่อยู่นี้ได้เลย ")
        } else {
            String::new()
        };
        let ask = if offered {
            format!("บ้านอยู่โซนไหน{kha_question} เลือกจากปุ่มด้านล่างได้เลย{kha}")
        } else {
            format!("บ้านอยู่ซอยหรือโซนไหน{kha_question} พิมพ์มาได้เลย{kha} เช่น ซอย 3")
        };
        Message::text(format!("{why}{ask}{}", now(current)))
    }

    /// Asks for the house number (FR-ONB-04).
    pub fn ask_house_number(&self, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "บ้านเลขที่เท่าไร{kha_question} พิมพ์มาได้เลย{kha} เช่น 45/2{}",
            now(current)
        ))
    }

    /// Asks for the optional landmark note of at most `max_chars` (FR-ONB-04).
    pub fn ask_landmark(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question, skip) = (self.kha(), self.kha_question(), labels::SKIP.as_str());
        Message::text(format!(
            "มีจุดสังเกตให้หาบ้านง่ายขึ้นไหม{kha_question} เช่น รั้วสีเขียว ตรงข้ามศาลา พิมพ์ได้ไม่เกิน {max_chars} ตัวอักษร ถ้าไม่มี กด \"{skip}\" ได้เลย{kha}{}",
            now(current)
        ))
    }

    /// Asks for the optional phone number (FR-ONB-04, NFR-PRIV-04). `current`, as shown, when
    /// one is being corrected, which can then be removed.
    pub fn ask_phone(&self, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอเบอร์โทรไว้ให้ร้านติดต่อตอนส่งของได้ไหม{kha_question} พิมพ์มาได้เลย{kha} เช่น 0812345678 {}{}",
            self.phone_optional(current.is_some()),
            now(current)
        ))
    }

    /// Asks for the optional shared location (FR-ONB-04, NFR-PRIV-04). `shared` when one is
    /// being corrected, which can then be removed.
    pub fn ask_location(&self, shared: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let share = labels::SHARE_LOCATION.as_str();
        let optional = if shared {
            format!(
                "ถ้าต้องการลบตำแหน่งเดิม กด \"{}\" ได้เลย{kha}",
                labels::REMOVE_LOCATION.as_str()
            )
        } else {
            format!(
                "ไม่บังคับ{kha} ถ้าไม่สะดวก กด \"{}\" ได้เลย{kha}",
                labels::SKIP.as_str()
            )
        };
        Message::text(format!(
            "แชร์ตำแหน่งบ้านไว้ให้ร้านหาบ้านเจอง่ายขึ้นไหม{kha_question} กด \"{share}\" แล้วเลือกตำแหน่งบ้าน {optional}"
        ))
    }

    /// A zone typed that `village.zones` doesn't list (FR-ONB-04).
    pub fn zone_unknown(&self) -> Message {
        let kha = self.kha();
        Message::text(format!("ขอโทษ{kha} ไม่พบโซนนี้ เลือกจากปุ่มด้านล่างได้เลย{kha}"))
    }

    /// Text longer than the step takes.
    pub fn text_too_long(&self, max_chars: usize) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ข้อความยาวเกินไป พิมพ์ใหม่ให้ไม่เกิน {max_chars} ตัวอักษรได้เลย{kha}"
        ))
    }

    /// Not a valid Thai phone number (FR-ONB-04): the formats taken, with examples.
    /// `removable` as for [`ask_phone`](Self::ask_phone).
    pub fn phone_invalid(&self, removable: bool) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} เบอร์โทรนี้ไม่ถูกต้อง พิมพ์เบอร์มือถือ 10 หลักที่ขึ้นต้นด้วย 06 08 หรือ 09 เช่น 0812345678 หรือเบอร์บ้าน 9 หลัก เช่น 021234567 {}",
            self.phone_optional(removable)
        ))
    }

    /// Text where a shared location was asked for. `shared` as for
    /// [`ask_location`](Self::ask_location).
    pub fn location_expected(&self, shared: bool) -> Message {
        let (kha, share) = (self.kha(), labels::SHARE_LOCATION.as_str());
        let none = if shared {
            &labels::REMOVE_LOCATION
        } else {
            &labels::SKIP
        };
        Message::text(format!(
            "ขอโทษ{kha} ตรงนี้ต้องแชร์ตำแหน่ง กด \"{share}\" แล้วเลือกตำแหน่งบ้าน หรือกด \"{}\" ได้เลย{kha}",
            none.as_str()
        ))
    }

    /// The address is saved: `address` as [`address_line`] shows it.
    pub fn address_saved(&self, address: &str) -> Message {
        Message::text(format!("บันทึกที่อยู่แล้ว{} {address}", self.kha()))
    }

    /// The phone number is saved, as shown, or removed.
    pub fn phone_saved(&self, phone: Option<&str>) -> Message {
        let kha = self.kha();
        Message::text(match phone {
            Some(phone) => format!("บันทึกเบอร์โทร {phone} แล้ว{kha}"),
            None => format!("ลบเบอร์โทรแล้ว{kha}"),
        })
    }

    /// The location is saved, or removed.
    pub fn location_saved(&self, shared: bool) -> Message {
        let kha = self.kha();
        Message::text(if shared {
            format!("บันทึกตำแหน่งแล้ว{kha}")
        } else {
            format!("ลบตำแหน่งแล้ว{kha}")
        })
    }

    /// The phone number is optional (NFR-PRIV-04): how to go without, or remove the one given.
    fn phone_optional(&self, removable: bool) -> String {
        let kha = self.kha();
        if removable {
            format!(
                "ถ้าต้องการลบเบอร์เดิม กด \"{}\" ได้เลย{kha}",
                labels::REMOVE_PHONE.as_str()
            )
        } else {
            format!(
                "ไม่บังคับ{kha} ถ้าไม่สะดวก กด \"{}\" ได้เลย{kha}",
                labels::SKIP.as_str()
            )
        }
    }

    /// "ข้อมูลของฉัน" (FR-ONB-05, NFR-PRIV-03): every piece of personal data held about the
    /// user, theirs to see, with buttons to correct what they can.
    pub fn my_data(&self, data: &MyData<'_>) -> Message {
        let given = |value: Option<&str>| value.unwrap_or(NOT_GIVEN).to_owned();
        let location = |location: Option<(f64, f64)>| {
            location.map_or_else(
                || NOT_GIVEN.to_owned(),
                |(latitude, longitude)| format!("{latitude:.5}, {longitude:.5}"),
            )
        };
        let consent = data.consent.map_or_else(
            || NONE.to_owned(),
            |(version, at)| format!("ฉบับที่ {version} เมื่อ {}", self.date_time(at)),
        );
        let mut sections = vec![
            section(
                "บัญชี LINE",
                [
                    row("ชื่อ", data.display_name.unwrap_or(NONE)),
                    row("รูปโปรไฟล์", if data.has_picture { "มี" } else { NONE }),
                    row("LINE user ID", data.line_user_id.unwrap_or(NONE)),
                    row("ความยินยอม", &consent),
                ],
            ),
            section(
                "ที่อยู่สำหรับส่งของ",
                [
                    row("โซน", &given(data.zone)),
                    row("บ้านเลขที่", &given(data.house_number)),
                    row("จุดสังเกต", &given(data.landmark_note)),
                    row(
                        "เบอร์โทร",
                        &given(data.phone.map(thai::format_phone).as_deref()),
                    ),
                    row("ตำแหน่ง", &location(data.location)),
                ],
            ),
        ];
        if let Some(shop) = &data.shop {
            let rows = [
                row(
                    "เบอร์ร้าน",
                    &given(shop.phone.map(thai::format_phone).as_deref()),
                ),
                row("พร้อมเพย์", &given(shop.promptpay_id)),
                row("ที่อยู่รับของ", &given(shop.pickup_address)),
                row("ตำแหน่งรับของ", &location(shop.pickup_location)),
                line(&format!("แก้ไขได้ที่ \"{}\"", labels::SHOP_SETTINGS.as_str())),
            ];
            sections.push(section("ร้านของฉัน", rows));
        }
        let mut held = Vec::new();
        if data.orders_with_address > 0 {
            held.push(format!(
                "ที่อยู่ส่งของใน {} คำสั่งซื้อ ดูได้ที่ \"{}\"",
                data.orders_with_address,
                labels::MY_ORDERS.as_str()
            ));
        }
        let days = &data.retention;
        if data.slips > 0 {
            held.push(format!(
                "รูปสลิปโอนเงิน {} รูป เก็บ {} วันหลังคำสั่งซื้อจบ",
                data.slips, days.slip_days
            ));
        }
        held.push(format!(
            "ข้อความที่คุณส่งถึงระบบ เก็บ {} วัน",
            days.webhook_event_days
        ));
        held.push(format!("การแจ้งเตือนที่ส่งถึงคุณ เก็บ {} วัน", days.push_log_days));
        if let Some(ai_days) = days.ai_message_days {
            held.push(format!("ข้อความที่คุยกับผู้ช่วย AI เก็บ {ai_days} วัน"));
        }
        sections.push(section("ข้อมูลอื่นที่เก็บไว้", held.iter().map(|text| line(text))));

        let title = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            ..Text::new(labels::MY_DATA.as_str())
        };
        let mut body = Vec::new();
        for (i, section) in sections.into_iter().enumerate() {
            if i > 0 {
                body.push(Separator::default().into());
            }
            body.push(section.into());
        }
        let buttons = data.edit.iter().map(|action| {
            let button = Button {
                style: Some(ButtonStyle::Secondary),
                height: Some(ButtonHeight::Sm),
                ..Button::new(action.clone())
            };
            button.into()
        });
        let card = Bubble {
            size: Some(BubbleSize::Mega),
            header: Some(FlexBox::vertical([title.into()])),
            body: Some(FlexBox {
                spacing: Some(Spacing::Lg),
                ..FlexBox::vertical(body)
            }),
            footer: Some(FlexBox {
                spacing: Some(Spacing::Sm),
                ..FlexBox::vertical(buttons)
            }),
            ..Bubble::default()
        };
        Message::flex(labels::MY_DATA.as_str(), card)
    }

    /// Asks to confirm deleting the user's account (FR-ONB-07, FR-UI-13): what goes, their shop
    /// `shop` closed for good with its products, what stays, and that it can't be undone.
    /// `confirm` deletes it; cancelling keeps it.
    pub fn confirm_delete_account(&self, shop: Option<&str>, confirm: Action) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let shop = shop
            .map(|name| format!(" ร้าน \"{name}\" จะปิดถาวรและสินค้าทั้งหมดถูกลบ"))
            .unwrap_or_default();
        let text = format!(
            "ลบบัญชีของคุณใช่ไหม{kha_question} ระบบจะลบชื่อ รูปโปรไฟล์ เบอร์โทร ที่อยู่ ตำแหน่ง และบัญชี LINE ที่ผูกไว้{shop} ประวัติคำสั่งซื้อยังอยู่ แต่ไม่ระบุว่าเป็นคุณ ลบแล้วกู้คืนไม่ได้ ถ้าแน่ใจ กด \"{}\" ได้เลย{kha}",
            confirm.label().as_str()
        );
        with_cancel(Message::text(text).with_quick_reply(QuickReply::new([confirm])))
    }

    /// The account can't be deleted while orders are open (FR-ONB-07, AC-13): `as_buyer` the user
    /// placed, `as_seller` at their shop. Then where to see them.
    pub fn account_deletion_refused(&self, as_buyer: u64, as_seller: u64) -> Message {
        let kha = self.kha();
        let mut open = Vec::new();
        if as_buyer > 0 {
            open.push(format!("ที่คุณสั่งซื้อ {as_buyer} รายการ"));
        }
        if as_seller > 0 {
            open.push(format!("ของร้านคุณ {as_seller} รายการ"));
        }
        let text = format!(
            "ยังลบบัญชีไม่ได้{kha} เพราะมีคำสั่งซื้อที่ยังไม่จบ: {} เมื่อคำสั่งซื้อทั้งหมดเสร็จหรือยกเลิกแล้ว กด \"{}\" ได้อีกครั้ง{kha}",
            open.join(" และ "),
            labels::DELETE_ACCOUNT.as_str()
        );
        let mut choices = Vec::new();
        if as_buyer > 0 {
            choices.push(Item::MyOrders.postback().button(labels::MY_ORDERS.clone()));
        }
        if as_seller > 0 {
            choices.push(
                Item::ShopOrders
                    .postback()
                    .button(labels::SHOP_ORDERS.clone()),
            );
        }
        choices.push(Action::message(labels::MENU.clone(), labels::MENU.as_str()));
        Message::text(text).with_quick_reply(QuickReply::new(choices))
    }

    /// The account is deleted (FR-ONB-07). Writing again starts over as someone new, so there
    /// are no buttons to carry on with.
    pub fn account_deleted(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ลบบัญชีของคุณแล้ว{kha} ขอบคุณที่ใช้ {} ถ้าอยากกลับมาใช้อีก ส่งข้อความมาได้เลย ระบบจะเริ่มให้ใหม่{kha}",
            self.app_name
        ))
    }

    /// The account is kept, its deletion not having been confirmed in time; `again` asks again.
    pub fn account_kept(&self, again: Action) -> Message {
        let kha = self.kha();
        let text = format!(
            "ยังไม่ได้ลบบัญชี{kha} ถ้าจะลบ กด \"{}\" อีกครั้งได้เลย{kha}",
            again.label().as_str()
        );
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([again, menu]))
    }

    /// Asks for a category's name (FR-ADM-05) of at most `max_chars`: a new one, or a new name
    /// for the one named `current`.
    pub fn ask_category_name(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let typed = format!("พิมพ์มาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha}");
        Message::text(match current {
            None => format!("หมวดหมู่ใหม่ชื่ออะไรดี{kha_question} {typed} เช่น ขนม/ของหวาน"),
            Some(_) => format!("ชื่อใหม่ของหมวดนี้คืออะไร{kha_question} {typed}{}", now(current)),
        })
    }

    /// Another category is already called `name` (FR-ADM-05).
    pub fn category_name_taken(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} มีหมวด \"{name}\" อยู่แล้ว พิมพ์ชื่ออื่นได้เลย{kha}"
        ))
    }

    /// Category `name` was added, last in the order (FR-ADM-05).
    pub fn category_added(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "เพิ่มหมวด \"{name}\" ไว้ท้ายรายการแล้ว{kha} เลื่อนขึ้นได้จากปุ่มของหมวดนั้นเลย{kha}"
        ))
    }

    /// Category `from` is now called `to` (FR-ADM-05).
    pub fn category_renamed(&self, from: &str, to: &str) -> Message {
        let kha = self.kha();
        Message::text(format!("เปลี่ยนชื่อหมวด \"{from}\" เป็น \"{to}\" แล้ว{kha}"))
    }

    /// Category `name` was deactivated (FR-ADM-05): what that hides, and what it keeps.
    pub fn category_deactivated(&self, name: &str) -> Message {
        let (kha, buy) = (self.kha(), labels::BUY.as_str());
        Message::text(format!(
            "ปิดใช้งานหมวด \"{name}\" แล้ว{kha} ผู้ซื้อจะไม่เห็นหมวดนี้ใน \"{buy}\" และลงสินค้าใหม่ในหมวดนี้ไม่ได้ สินค้าเดิมยังอยู่ครบ{kha}"
        ))
    }

    /// Category `name` was activated again (FR-ADM-05).
    pub fn category_activated(&self, name: &str) -> Message {
        let (kha, buy) = (self.kha(), labels::BUY.as_str());
        Message::text(format!(
            "เปิดใช้งานหมวด \"{name}\" แล้ว{kha} ผู้ซื้อเห็นหมวดนี้ใน \"{buy}\" อีกครั้ง{kha}"
        ))
    }

    /// Asks to confirm deleting category `name`, which can't be undone (FR-ADM-05, FR-UI-13).
    /// `confirm` deletes it; cancelling keeps it.
    pub fn confirm_delete_category(&self, name: &str, confirm: Action) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ลบหมวด \"{name}\" ใช่ไหม{kha_question} ลบแล้วกู้คืนไม่ได้ ถ้าแน่ใจ กด \"{}\" ได้เลย{kha}",
            confirm.label().as_str()
        );
        with_cancel(Message::text(text).with_quick_reply(QuickReply::new([confirm])))
    }

    /// Category `name` was deleted (FR-ADM-05).
    pub fn category_deleted(&self, name: &str) -> Message {
        Message::text(format!("ลบหมวด \"{name}\" แล้ว{}", self.kha()))
    }

    /// Category `name` has or had products, so can't be deleted (FR-ADM-05). `deactivate`
    /// hides it instead, while it is active.
    pub fn category_in_use(&self, name: &str, deactivate: Option<Action>) -> Message {
        let kha = self.kha();
        let why = format!("ขอโทษ{kha} หมวด \"{name}\" มีสินค้าอยู่หรือเคยมี จึงลบไม่ได้");
        match deactivate {
            Some(deactivate) => {
                let text = format!(
                    "{why} ถ้าไม่ต้องการใช้หมวดนี้แล้ว กด \"{}\" แทนได้เลย{kha}",
                    deactivate.label().as_str()
                );
                Message::text(text).with_quick_reply(QuickReply::new([deactivate]))
            }
            None => Message::text(format!("{why} หมวดนี้ปิดใช้งานอยู่แล้ว ผู้ซื้อจึงไม่เห็น{kha}")),
        }
    }

    /// "หมวดหมู่" (FR-ADM-05): page `page` of every category in the order buyers see them, a
    /// card each with its buttons, and "ดูเพิ่มเติม" sending `more(next_page)`. `add` adds one.
    pub fn categories(
        &self,
        cards: &[CategoryCard<'_>],
        page: usize,
        more: impl FnOnce(usize) -> String,
        add: Action,
    ) -> Message {
        let add = QuickReply::new([add]);
        if cards.is_empty() {
            let kha = self.kha();
            let text = format!(
                "ยังไม่มีหมวดหมู่เลย{kha} กด \"{}\" เพื่อเริ่มได้เลย{kha}",
                labels::ADD_CATEGORY.as_str()
            );
            return Message::text(text).with_quick_reply(add);
        }
        let page = carousel_page(cards, page, CATEGORIES_PER_PAGE, category_card, more);
        Message::flex(labels::CATEGORIES.as_str(), page).with_quick_reply(add)
    }

    /// "สถิติ" (FR-ADM-06, FR-NOT-05): `statistics` as they were `at`. Orders are a table of
    /// their statuses by period, closed by the value of those completed. `again` shows them
    /// again.
    pub fn statistics(&self, statistics: &Statistics, at: DateTime<Utc>, again: Action) -> Message {
        let count = |n: u64, unit: &str| format!("{} {unit}", thai::group_thousands(n));
        let periods: Vec<&Orders> = statistics.orders.iter().collect();
        let ai = statistics.ai_calls.map_or_else(
            || "ปิดอยู่".to_owned(),
            |calls| {
                let limit = thai::group_thousands(calls.limit.into());
                format!("{} / {limit} ครั้ง", thai::group_thousands(calls.today))
            },
        );
        let sections = [
            section(
                "ผู้ใช้",
                [
                    row("ทั้งหมด", &count(statistics.users, "คน")),
                    row("ใหม่ใน 7 วัน", &count(statistics.new_users, "คน")),
                ],
            ),
            section(
                "ร้านค้า",
                [
                    row("อนุมัติแล้ว", &count(statistics.approved_shops, "ร้าน")),
                    row("เปิดอยู่ตอนนี้", &count(statistics.open_shops, "ร้าน")),
                ],
            ),
            section("คำสั่งซื้อ", orders_table(&periods)),
            section("ผู้ช่วย AI", [row("ใช้วันนี้", &ai)]),
            section(
                "การแจ้งเตือนเดือนนี้",
                [
                    row("ส่งแล้ว", &count(statistics.pushes.sent, "ครั้ง")),
                    row("ส่งไม่สำเร็จ", &count(statistics.pushes.failed, "ครั้ง")),
                ],
            ),
        ];
        let card = self.figures_card(&labels::STATS, None, at, sections, &[again]);
        Message::flex(labels::STATS.as_str(), card)
    }

    /// "ยอดขาย" (FR-SHOP-11): the `sales` of shop `shop_name` as they were `at`. Orders are a
    /// table of their statuses by period, closed by the value of those completed and those of
    /// them unpaid; then each period's best-selling products, with how many sold and for how
    /// much. `buttons` go at its foot.
    pub fn sales(
        &self,
        shop_name: &str,
        sales: &Sales,
        at: DateTime<Utc>,
        buttons: Vec<Action>,
    ) -> Message {
        let periods = &sales.periods;
        let orders: Vec<&Orders> = periods.iter().map(|period| &period.orders).collect();
        let mut table = orders_table(&orders);
        let unpaid = periods
            .iter()
            .map(|period| thai::group_thousands(period.unpaid));
        table.push(table_row(labels::FILTER_UNPAID.as_str(), unpaid, false));
        let unpaid_values = periods.iter().map(|period| period.unpaid_value.to_string());
        table.push(table_row("ยอดค้างชำระ", unpaid_values, false));
        let mut top = Vec::new();
        for period in periods {
            top.push(table_row(&period_name(period.orders.days), [], true));
            if period.top.is_empty() {
                top.push(table_row("ยังไม่มี", [], false));
            }
            for (rank, product) in (1..).zip(&period.top) {
                let sold = [
                    format!(
                        "{} {}",
                        thai::group_thousands(product.quantity),
                        product.unit
                    ),
                    product.value.to_string(),
                ];
                top.push(table_row(&format!("{rank}. {}", product.name), sold, false));
            }
        }
        let sections = [section("คำสั่งซื้อ", table), section("สินค้าขายดี", top)];
        let card = self.figures_card(&labels::SALES, Some(shop_name), at, sections, &buttons);
        Message::flex(labels::SALES.as_str(), card)
    }

    /// The daily sales summary (FR-NOT-06): how the seller's shop did on the day of `at`, the
    /// orders placed and the sales completed, with the best seller of those, then the AI's
    /// `comment` on it, if any, as an aside. `buttons` go at its foot.
    pub fn daily_summary_notice(
        &self,
        at: DateTime<Utc>,
        day: &Day,
        comment: Option<&str>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("สรุปยอดขายวันนี้{kha} {}", self.date(at));
        let placed = thai::group_thousands(day.placed);
        let completed = thai::group_thousands(day.completed);
        let mut details = vec![
            (format!("ออเดอร์ใหม่ {placed} รายการ"), Tone::Plain),
            (
                format!("ขายสำเร็จ {completed} รายการ รวม {}", day.completed_value),
                Tone::Plain,
            ),
        ];
        if let Some(best) = day.top.first() {
            let quantity = thai::group_thousands(best.quantity);
            let line = format!("ขายดีสุด: {} {quantity} {}", best.name, best.unit);
            details.push((line, Tone::Plain));
        }
        if let Some(comment) = comment {
            details.push((format!("ผู้ช่วย AI: {comment}"), Tone::Muted));
        }
        Message::flex(&headline, toned_notice_card(&headline, &details, buttons))
    }

    /// A card of figures: `title`, what they are `about` if it says, as they were `at`, then
    /// `sections` one under another, and `buttons` at its foot.
    fn figures_card(
        &self,
        title: &Label,
        about: Option<&str>,
        at: DateTime<Utc>,
        sections: impl IntoIterator<Item = FlexBox>,
        buttons: &[Action],
    ) -> Bubble {
        let title = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            ..Text::new(title.as_str())
        };
        let about = about.map(|about| {
            let about = Text {
                size: Some(Size::Sm),
                wrap: Some(true),
                ..Text::new(about)
            };
            about.into()
        });
        let as_of = Text {
            size: Some(Size::Xs),
            color: Some(MUTED.to_owned()),
            ..Text::new(format!("ณ {}", self.date_time(at)))
        };
        let header = std::iter::once(title.into())
            .chain(about)
            .chain([as_of.into()]);
        let mut body = Vec::new();
        for (i, section) in sections.into_iter().enumerate() {
            if i > 0 {
                body.push(Separator::default().into());
            }
            body.push(section.into());
        }
        Bubble {
            size: Some(BubbleSize::Mega),
            header: Some(FlexBox::vertical(header)),
            body: Some(FlexBox {
                spacing: Some(Spacing::Lg),
                ..FlexBox::vertical(body)
            }),
            footer: footer_of(buttons, false),
            ..Bubble::default()
        }
    }

    /// "ซื้อของ" (FR-SRCH-01): the active categories, `choices` a page of them.
    pub fn choose_category(&self, choices: Vec<Action>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!("อยากซื้ออะไรดี{kha_question} เลือกหมวดได้เลย{kha}"))
            .with_quick_reply(QuickReply::new(choices))
    }

    /// "ซื้อของ" with no active category. The main menu follows.
    pub fn nothing_to_browse(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ตอนนี้ยังไม่มีหมวดสินค้าให้เลือก{kha} ลองใหม่ภายหลัง หรือเลือกจากเมนูด้านล่างได้เลย{kha}"
        ))
    }

    /// "ค้นหา" (FR-SRCH-04): what to search for. The cancel button follows.
    pub fn ask_keyword(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "อยากหาอะไรดี{kha_question} พิมพ์ชื่อสินค้า ร้าน หรือหมวดที่อยากหามาได้เลย เช่น ขนมครก{kha}"
        ))
    }

    /// "ดูเพิ่มเติม" under a search's results, once the search is no longer kept: what to search
    /// for, again. The cancel button follows.
    pub fn search_expired(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ผลการค้นหานี้หมดเวลาแล้ว{kha} พิมพ์สิ่งที่อยากหาอีกครั้งได้เลย{kha}"
        ))
    }

    /// A buyer's photo searched by (FR-SRCH-10): what the AI took it for, `keyword`. What it
    /// finds follows.
    pub fn photo_searched(&self, keyword: &str) -> Message {
        Message::text(format!("จากรูป ลองหา \"{keyword}\" ให้แล้ว{}", self.kha()))
    }

    /// A buyer's photo in which the AI saw nothing to search for (FR-SRCH-10): what to search
    /// for, typed. The cancel button follows.
    pub fn photo_not_recognized(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ดูจากรูปแล้วยังไม่แน่ใจว่าเป็นอะไร{kha} พิมพ์ชื่อสินค้าที่อยากหามาได้เลย เช่น ขนมครก{kha}"
        ))
    }

    /// A page of a product list (FR-SRCH-02, FR-SRCH-03): of what was `browsed`, a card for each
    /// of `cards` (FR-SRCH-07) and, when more follow, "ดูเพิ่มเติม" sending the postback `more`;
    /// `next` are its Quick Replies.
    pub fn product_list(
        &self,
        browsed: Browsed<'_>,
        cards: &[ProductCard<'_>],
        more: Option<String>,
        next: Vec<Action>,
    ) -> Message {
        let bubbles = cards.iter().map(product_card).collect();
        Message::flex(headline(browsed), carousel(bubbles, more))
            .with_quick_reply(QuickReply::new(next))
    }

    /// A product's details, from a tap on its card (FR-SRCH-07): its card with its description
    /// and shop, then the rest of its photos; `next` are its Quick Replies.
    pub fn product_details(&self, details: &ProductDetails<'_>, next: Vec<Action>) -> Message {
        let (first, rest) = details
            .photos
            .split_first()
            .expect("a photo, or the placeholder");
        let card = product_details_card(details, first);
        let contents: FlexContainer = if rest.is_empty() {
            card.into()
        } else {
            // Beside the card, as many as a carousel holds
            let photos = rest
                .iter()
                .take(limits::BUBBLES_PER_CAROUSEL - 1)
                .map(photo_card);
            Carousel::new(std::iter::once(card).chain(photos).collect()).into()
        };
        Message::flex(details.name, contents).with_quick_reply(QuickReply::new(next))
    }

    /// A page of "ร้านค้าทั้งหมด" (FR-SRCH-06), or the shops the AI assistant found, as
    /// `browsed` says: a card for each of `fronts`, as buyers see shops (FR-SHOP-09), and, when
    /// more follow, "ดูเพิ่มเติม" sending the postback `more`; `next` are its Quick Replies.
    pub fn shop_list(
        &self,
        browsed: Browsed<'_>,
        fronts: &[ShopFront<'_>],
        more: Option<String>,
        next: Vec<Action>,
    ) -> Message {
        let bubbles = fronts
            .iter()
            .map(|front| self.shop_front_bubble(front))
            .collect();
        Message::flex(headline(browsed), carousel(bubbles, more))
            .with_quick_reply(QuickReply::new(next))
    }

    /// Nothing to show of what was `browsed` (FR-SRCH-08): says so, and suggests where else to
    /// look, `suggestions`.
    pub fn nothing_found(&self, browsed: Browsed<'_>, suggestions: Vec<Action>) -> Message {
        let kha = self.kha();
        let text = match browsed {
            Browsed::Category(name) => {
                format!("ตอนนี้หมวด {name} ยังไม่มีสินค้า{kha} ลองดูหมวดอื่น หรือค้นหาจากชื่อสินค้าได้เลย{kha}")
            }
            Browsed::Shop(name) => format!(
                "ตอนนี้{}ยังไม่มีสินค้าลงขาย{kha} ลองดูร้านอื่น หรือเลือกดูจากหมวดได้เลย{kha}",
                shop_called(name)
            ),
            Browsed::NewToday => {
                format!("วันนี้ยังไม่มีของใหม่{kha} ลองเลือกดูจากหมวดได้เลย{kha}")
            }
            Browsed::Search(keyword) => format!(
                "ไม่พบสินค้าที่ตรงกับ \"{keyword}\"{kha} ลองพิมพ์ให้สั้นลงหรือใช้คำอื่น หรือเลือกดูจากหมวดได้เลย{kha}"
            ),
            Browsed::Shops => {
                format!("ตอนนี้ยังไม่มีร้านค้าในตลาด{kha} ลองใหม่ภายหลังได้เลย{kha}")
            }
            Browsed::Found => {
                format!("ยังไม่เจอที่ตรงกับที่ถาม{kha} ลองถามด้วยคำอื่น หรือเลือกดูจากหมวดได้เลย{kha}")
            }
        };
        Message::text(text).with_quick_reply(QuickReply::new(suggestions))
    }

    /// "ใส่ตะกร้า" (FR-CART-01), or "แก้จำนวน" on its line if `change`: how many of product
    /// `name`, sold by `unit`, as in Appendix A.2, with its ready date if it is a pre-order
    /// (FR-PROD-11); `in_cart` is what its line holds now. The quantities that fit follow as
    /// Quick Replies, then "ระบุเอง" and the cancel button.
    pub fn ask_cart_quantity(
        &self,
        name: &str,
        unit: &str,
        ready_on: Option<NaiveDate>,
        in_cart: i32,
        change: bool,
    ) -> Message {
        let kha_question = self.kha_question();
        let name = format!("\"{name}\"{}", ready_note(ready_on));
        Message::text(match (change, in_cart) {
            (true, _) => {
                format!("{name} ในตะกร้ามี {in_cart} {unit} เปลี่ยนเป็นกี่{unit}{kha_question}")
            }
            (false, 0) => format!("{name} เอากี่{unit}{kha_question}"),
            (false, _) => {
                format!("{name} อยู่ในตะกร้าแล้ว {in_cart} {unit} เอาเพิ่มอีกกี่{unit}{kha_question}")
            }
        })
    }

    /// "ระบุเอง": asks to type the quantity, from 1 to `most` of `unit` (FR-CART-01). The cancel
    /// button follows.
    pub fn ask_typed_quantity(&self, most: i32, unit: &str) -> Message {
        let kha = self.kha();
        let range = if most > 1 {
            format!("ตั้งแต่ 1 ถึง {most} {unit}")
        } else {
            format!("แค่ 1 {unit}")
        };
        Message::text(format!(
            "พิมพ์จำนวนเป็นตัวเลขได้เลย{kha} ใส่ได้{range} เช่น 1 หรือ ๑"
        ))
    }

    /// A quantity more than the line of product `name`, sold by `unit`, can take (FR-CART-01):
    /// says what is `left` of it, or `max`, the most a line holds, if fewer are left or it is
    /// unlimited; then what the line holds already, `in_cart`, when adding to it, and how many
    /// more fit. The quantities that fit follow as Quick Replies, if any do.
    pub fn quantity_too_many(
        &self,
        name: &str,
        unit: &str,
        left: Option<i32>,
        max: i32,
        in_cart: i32,
    ) -> Message {
        let kha = self.kha();
        let room = (left.map_or(max, |left| left.min(max)) - in_cart).max(0);
        let most = match left {
            Some(left) if left < max => format!("\"{name}\" เหลือ {left} {unit}"),
            _ => format!("ใส่ตะกร้าได้รายการละไม่เกิน {max} {unit}"),
        };
        let held = if in_cart > 0 {
            format!(" ในตะกร้ามีแล้ว {in_cart} {unit}")
        } else {
            String::new()
        };
        let then = match (room, in_cart) {
            (0, _) => format!("จึงใส่เพิ่มไม่ได้แล้ว{kha}"),
            (_, 0) => format!("พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลย{kha}"),
            (room, _) => {
                format!("จึงใส่เพิ่มได้อีกไม่เกิน {room} {unit} พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลย{kha}")
            }
        };
        Message::text(format!("ขอโทษ{kha} {most}{held} {then}"))
    }

    /// A quantity of 0 of a product sold by `unit` (FR-CART-01).
    pub fn quantity_too_small(&self, unit: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ใส่ได้ตั้งแต่ 1 {unit} ขึ้นไป พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลย{kha}"
        ))
    }

    /// A product of the user's own shop, which they can't order (FR-CART-07).
    pub fn own_product(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} สินค้านี้เป็นของร้านคุณเอง จึงใส่ตะกร้าไม่ได้ ลองดูสินค้าร้านอื่นได้เลย{kha}"
        ))
    }

    /// Product `name` of shop `shop` can't be ordered now, as its `availability` says
    /// (FR-PROD-06, FR-SHOP-05), so it can't go in the cart.
    pub fn not_orderable(&self, name: &str, shop: &str, availability: Availability) -> Message {
        let kha = self.kha();
        Message::text(match availability {
            Availability::ShopClosed => format!(
                "ขอโทษ{kha} ตอนนี้{}ปิดอยู่ จึงใส่ \"{name}\" ลงตะกร้าไม่ได้ ลองใหม่ตอนร้านเปิด หรือดูสินค้าร้านอื่นได้เลย{kha}",
                shop_called(shop)
            ),
            _ => format!("ขอโทษ{kha} \"{name}\" หมดแล้ว จึงใส่ตะกร้าไม่ได้ ลองดูสินค้าอื่นได้เลย{kha}"),
        })
    }

    /// The cart holds `max` products already, as many as it may (FR-CART-07).
    pub fn cart_full(&self, max: usize) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอโทษ{kha} ตะกร้ามีสินค้าครบ {max} รายการแล้ว สั่งซื้อหรือลบบางรายการออกก่อน แล้วค่อยใส่เพิ่มนะ{kha_question}"
        ))
    }

    /// Product `name` is in the cart (FR-CART-01): its line holds `quantity` of `unit`, `total`
    /// in all, as in Appendix A.2, ready on `ready_on` if a pre-order (FR-PROD-11); `more` if it
    /// was there already and now holds more. Where to carry on follows as Quick Replies.
    pub fn added_to_cart(
        &self,
        name: &str,
        quantity: i32,
        unit: &str,
        total: Money,
        ready_on: Option<NaiveDate>,
        more: bool,
    ) -> Message {
        let (kha, line) = (self.kha(), cart_line(name, quantity, unit, total, ready_on));
        Message::text(if more {
            format!("ใส่เพิ่มแล้ว{kha} ตอนนี้ในตะกร้ามี {line}")
        } else {
            format!("ใส่ตะกร้าแล้ว{kha} {line}")
        })
    }

    /// The label of the button confirming the AI assistant's proposal to put `quantity` of
    /// `unit` in the cart, as in Appendix A.1: *ใส่ 2 แผง*.
    pub fn add_quantity(&self, quantity: i32, unit: &str) -> Label {
        Label::fit(format!("ใส่ {quantity} {unit}"))
    }

    /// A proposal to put product `name` in the cart, confirmed already (FR-UI-13). Where to
    /// carry on follows as Quick Replies.
    pub fn cart_offer_done(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ใส่ \"{name}\" ลงตะกร้าไปแล้ว{kha} ดูตะกร้าหรือเลือกซื้อต่อได้เลย{kha}"
        ))
    }

    /// A proposal to put product `name`, if still known, in the cart, that expired or was
    /// cancelled (FR-UI-13). Where to carry on follows as Quick Replies.
    pub fn cart_offer_gone(&self, name: Option<&str>) -> Message {
        let kha = self.kha();
        let what = name.map_or_else(|| "สินค้านี้".to_owned(), |name| format!("\"{name}\""));
        Message::text(format!(
            "ปุ่มใส่ {what} ลงตะกร้าหมดเวลาแล้ว{kha} กด \"{}\" ที่สินค้า หรือถามผู้ช่วยใหม่ได้เลย{kha}",
            labels::ADD_TO_CART.as_str()
        ))
    }

    /// Product `name` changed since putting `quantity` of it in the cart was proposed, so
    /// nothing was put there (FR-UI-13): it is `price` a `unit` now, `total` for as many. The
    /// proposal, again, follows as a Quick Reply.
    pub fn cart_offer_changed(
        &self,
        name: &str,
        quantity: i32,
        unit: &str,
        price: Money,
        total: Money,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "\"{name}\" มีการเปลี่ยนแปลง{kha} ตอนนี้ราคา {} ใส่ {quantity} {unit} รวม {total} ไหม{kha_question}",
            price_per_unit(price, unit)
        ))
    }

    /// The line of product `name` now holds `quantity` of `unit`, `total` in all (FR-CART-03),
    /// ready on `ready_on` if a pre-order (FR-PROD-11). The cart follows.
    pub fn quantity_changed(
        &self,
        name: &str,
        quantity: i32,
        unit: &str,
        total: Money,
        ready_on: Option<NaiveDate>,
    ) -> Message {
        let line = cart_line(name, quantity, unit, total, ready_on);
        Message::text(format!("แก้จำนวนแล้ว{} {line}", self.kha()))
    }

    /// Product `name` is out of the cart (FR-CART-03). The cart follows.
    pub fn removed_from_cart(&self, name: &str) -> Message {
        Message::text(format!("เอา \"{name}\" ออกจากตะกร้าแล้ว{}", self.kha()))
    }

    /// "ตะกร้า" with nothing in it; `next` are its Quick Replies.
    pub fn empty_cart(&self, next: Vec<Action>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ตะกร้ายังว่างอยู่{kha} เลือกซื้อของ หรือค้นหาสินค้าที่อยากได้ได้เลย{kha}"
        ))
        .with_quick_reply(QuickReply::new(next))
    }

    /// "ตะกร้า" (FR-CART-03): page `page` of the cart, a card for each of `shops`, and
    /// "ดูเพิ่มเติม" sending `more(next_page)` when more follow; `next` are its Quick Replies.
    pub fn cart(
        &self,
        shops: &[CartShop<'_>],
        page: usize,
        more: impl FnOnce(usize) -> String,
        next: Vec<Action>,
    ) -> Message {
        let page = carousel_page(shops, page, CART_SHOPS_PER_PAGE, cart_shop_card, more);
        Message::flex(labels::CART.as_str(), page).with_quick_reply(QuickReply::new(next))
    }

    /// Checkout's first step (FR-CART-04), as in Appendix A.2: picked up at the shop, or
    /// delivered. The choices follow as Quick Replies, the delivery fee on its button.
    pub fn ask_pickup_or_delivery(&self) -> Message {
        Message::text(format!("รับเองที่ร้าน หรือให้ร้านไปส่ง{}", self.kha_question()))
    }

    /// Where to deliver (FR-CART-04), as in Appendix A.2: `address` as [`address_line`] shows
    /// it, at the location the buyer shared if `located`. "ใช่", "เปลี่ยนที่อยู่" and
    /// "แชร์ตำแหน่ง" follow as Quick Replies.
    pub fn confirm_delivery_address(&self, address: &str, located: bool) -> Message {
        let located = if located {
            " ตามตำแหน่งที่แชร์ไว้"
        } else {
            ""
        };
        Message::text(format!(
            "ส่งที่ {address}{located} ใช่ไหม{}",
            self.kha_question()
        ))
    }

    /// How the buyer pays (FR-CART-04), as in Appendix A.2. The methods the shop takes follow
    /// as Quick Replies.
    pub fn ask_payment_method(&self) -> Message {
        Message::text(format!("ชำระเงินแบบไหน{}", self.kha_question()))
    }

    /// An optional note for the shop (FR-CART-04), as in Appendix A.2: typed, or "ไม่มี".
    pub fn ask_order_note(&self) -> Message {
        Message::text(format!("มีอะไรฝากถึงร้านไหม{} พิมพ์ได้เลย", self.kha_question()))
    }

    /// The shop checked out is the buyer's own (FR-CART-07).
    pub fn own_shop_order(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ร้านนี้เป็นร้านของคุณเอง จึงสั่งซื้อไม่ได้ ลองดูสินค้าร้านอื่นได้เลย{kha}"
        ))
    }

    /// No line of shop `shop` in the cart can be ordered now (FR-CART-03).
    pub fn nothing_to_order(&self, shop: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ตอนนี้ในตะกร้าไม่มีสินค้าของ{}ที่สั่งได้{kha} ดูตะกร้าหรือเลือกซื้อต่อได้เลย{kha}",
            shop_called(shop)
        ))
    }

    /// The lines of shop `shop` come to `subtotal`, less than its minimum order, `minimum`
    /// (FR-CART-05).
    pub fn below_minimum_order(&self, shop: &str, minimum: Money, subtotal: Money) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอโทษ{kha} {}รับออเดอร์ขั้นต่ำ {minimum} ตอนนี้ในตะกร้ามี {subtotal} เลือกซื้อเพิ่มอีกนิดนะ{kha_question}",
            shop_called(shop)
        ))
    }

    /// More lines than an order holds, `max` (FR-CART-07).
    pub fn too_many_to_order(&self, max: usize) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอโทษ{kha} สั่งซื้อได้ครั้งละไม่เกิน {max} รายการ ลบบางรายการออกจากตะกร้าก่อนนะ{kha_question}"
        ))
    }

    /// Shop `shop` takes none of the payment methods allowed: PromptPay is the only one, and it
    /// has no PromptPay ID (FR-CART-04).
    pub fn no_payment_method(&self, shop: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} {}ยังไม่ได้ตั้งพร้อมเพย์ไว้ จึงยังสั่งซื้อไม่ได้ ลองสั่งร้านอื่นก่อนได้เลย{kha}",
            shop_called(shop)
        ))
    }

    /// The shop no longer offers how the buyer chose to get the order or pay for it
    /// (FR-CART-04), so they choose again.
    pub fn checkout_options_changed(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอโทษ{kha} ร้านเปลี่ยนวิธีรับของหรือการชำระเงินแล้ว เลือกใหม่อีกครั้งนะ{kha_question}"
        ))
    }

    /// Checkout's summary (FR-CART-04), as in Appendix A.2: `order` as it would be placed, with
    /// `buttons`, "ยืนยันสั่งซื้อ", "แก้ไข" and "ยกเลิก".
    pub fn checkout_summary(&self, order: &OrderDetails<'_>, buttons: &[Action]) -> Message {
        let card = order_card("สรุปคำสั่งซื้อ", None, order, buttons);
        Message::flex(format!("สรุปคำสั่งซื้อ รวม {}", order.total), card)
    }

    /// The order `number`, `status` and `order`, just placed (FR-CART-08), as in Appendix A.2,
    /// or placed by an earlier tap of the same confirmation if `again` (FR-UI-13); `buttons` are
    /// what the buyer can do about it, "ยกเลิกออเดอร์" while it waits for the shop.
    pub fn order_placed(
        &self,
        number: &str,
        status: order::Status,
        order: &OrderDetails<'_>,
        again: bool,
        buttons: &[Action],
    ) -> Message {
        let kha = self.kha();
        let headline = if again {
            format!("สั่งซื้อนี้ไว้แล้ว{kha}")
        } else {
            format!("สั่งซื้อเรียบร้อย{kha}")
        };
        let status = order_status_name(status);
        let card = order_card(&headline, Some((number, status)), order, buttons);
        Message::flex(format!("{headline} เลขที่ {number} · {status}"), card)
    }

    /// A confirmed checkout placed no order, as `changes` happened since its summary
    /// (FR-CART-05, FR-CART-06): "สินค้าหมดแล้ว" if a product sold out (AC-03), then each
    /// change. `again` when an updated summary follows (AC-12).
    pub fn checkout_changed(&self, changes: &[CheckoutChange<'_>], again: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let sold_out = changes
            .iter()
            .any(|change| matches!(change, CheckoutChange::SoldOut { .. }));
        let what = if sold_out {
            "สินค้าหมดแล้ว"
        } else {
            "ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ"
        };
        let mut lines = vec![format!("ขอโทษ{kha} {what} จึงยังไม่ได้สั่งซื้อ")];
        lines.extend(changes.iter().map(|change| match *change {
            CheckoutChange::SoldOut { name } => format!("• \"{name}\" หมดแล้ว"),
            CheckoutChange::NotEnough { name, left, unit } => {
                format!("• \"{name}\" เหลือแค่ {left} {unit}")
            }
            CheckoutChange::Unavailable { name } => format!("• \"{name}\" ตอนนี้ไม่มีขาย"),
            CheckoutChange::ShopClosed { shop } => {
                format!("• {}ไม่รับออเดอร์ตอนนี้", shop_called(shop))
            }
            CheckoutChange::Price {
                name,
                unit,
                from,
                to,
            } => format!(
                "• \"{name}\" ราคาเปลี่ยนจาก {} เป็น {}",
                price_per_unit(from, unit),
                price_per_unit(to, unit)
            ),
            CheckoutChange::DeliveryFee { from, to } => {
                format!("• ค่าส่งเปลี่ยนจาก {from} เป็น {to}")
            }
            CheckoutChange::ReadyOn {
                name,
                ready_on: Some(day),
            } => format!("• \"{name}\" เปลี่ยนวันพร้อมเป็น {}", thai::format_day(day)),
            CheckoutChange::ReadyOn {
                name,
                ready_on: None,
            } => format!("• \"{name}\" ไม่ต้อง{}แล้ว", labels::PRE_ORDER.as_str()),
        }));
        lines.push(if again {
            format!(
                "ตรวจสรุปใหม่ด้านล่าง แล้วกด \"{}\" อีกครั้งนะ{kha_question}",
                labels::CONFIRM_ORDER.as_str()
            )
        } else {
            format!("ดูตะกร้าหรือเลือกซื้อต่อได้เลย{kha}")
        });
        Message::text(lines.join("\n"))
    }

    /// A checkout's confirmation came too late, or after it was cancelled (FR-UI-13), so
    /// nothing was ordered. Where to carry on follows.
    pub fn checkout_gone(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "สรุปคำสั่งซื้อนี้หมดเวลาหรือถูกยกเลิกแล้ว จึงยังไม่ได้สั่งซื้อ{kha} ถ้ายังจะสั่ง กด \"{}\" อีกครั้งได้เลย{kha}",
            labels::ORDER_FROM_SHOP.as_str()
        ))
    }

    /// Tells the seller of new order `number` (FR-ORD-03), as in Appendix A.4: its lines, delivery
    /// fee and total, how it is received and paid, and who ordered it, as much as FR-ORD-11 shows
    /// of an order yet to be accepted. `buttons` are "รับออเดอร์" and "ปฏิเสธ".
    pub fn new_order_notice(
        &self,
        number: &str,
        order: &NewOrder<'_>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("ออเดอร์ใหม่{kha} {number}");
        let mut details: Vec<String> = order
            .lines
            .iter()
            .map(|line| {
                format!(
                    "{} × {} {} = {}{}",
                    line.name,
                    line.quantity,
                    line.unit,
                    line.total,
                    ready_note(line.ready_on)
                )
            })
            .collect();
        let delivered = order.fulfillment == order::Fulfillment::Delivery;
        details.push(if delivered {
            format!(
                "{} {} · รวม {}",
                labels::DELIVERY_FEE.as_str(),
                order.delivery_fee,
                order.total
            )
        } else {
            format!("รวม {}", order.total)
        });
        let received = match order.zone {
            Some(zone) if delivered => format!("ส่งที่ {zone}"),
            _ => fulfillment_name(order.fulfillment).to_owned(),
        };
        details.push(format!("{received} · {}", payment_name(order.payment)));
        let buyer = match (order.buyer, order.zone) {
            (Some(buyer), Some(zone)) if !delivered => Some(format!("{buyer} · {zone}")),
            (Some(buyer), _) => Some(buyer.to_owned()),
            (None, _) => None,
        };
        if let Some(buyer) = buyer {
            details.push(format!("ลูกค้า: {buyer}"));
        }
        notice(&headline, &details, buttons)
    }

    /// The seller accepted order `number` (Table 4.6-B), as in Appendix A.4: how to reach its
    /// buyer, now that it is theirs to hand over (FR-ORD-11). `buttons` are the next step and
    /// "ยกเลิกออเดอร์".
    pub fn order_accepted(
        &self,
        number: &str,
        contact: &Contact<'_>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("รับออเดอร์ {number} แล้ว{kha}");
        let mut details = vec![match &contact.address {
            Some(address) => format!("ส่งที่ {address}"),
            None => fulfillment_name(order::Fulfillment::Pickup).to_owned(),
        }];
        if let Some(buyer) = contact.buyer {
            details.push(format!("ลูกค้า: {buyer}"));
        }
        if let Some(phone) = contact.phone {
            details.push(format!("โทร {}", thai::format_phone(phone)));
        }
        if let Some(note) = contact.note {
            details.push(format!("ฝากถึงร้าน: {note}"));
        }
        notice(&headline, &details, buttons)
    }

    /// Tells the buyer shop `shop` accepted their order `number` (FR-ORD-08), as in Appendix
    /// A.4, and, if it is paid by PromptPay, what to transfer where (FR-PAY-03). `buttons` are
    /// what they can do now, "ขอยกเลิก".
    pub fn order_accepted_notice(
        &self,
        shop: &str,
        number: &str,
        promptpay: Option<&PromptPayDue<'_>>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!(
            "{}รับออเดอร์ {number} แล้ว{kha} กำลังเตรียมของ",
            shop_called(shop)
        );
        let details = match promptpay {
            Some(PromptPayDue {
                id: Some(id),
                total,
            }) => vec![
                format!("โอนพร้อมเพย์ {total}"),
                format!("เลขพร้อมเพย์: {}", payments::format_promptpay_id(id)),
                format!("ชื่อร้าน: {shop}"),
                format!(
                    "โอนแล้วกด \"{}\" แล้วส่งรูปสลิปในแชตนี้{kha}",
                    labels::SEND_SLIP.as_str()
                ),
            ],
            // The shop took its PromptPay ID away since the order was placed
            Some(PromptPayDue { id: None, total }) => vec![
                format!("โอนพร้อมเพย์ {total}"),
                format!("ร้านยังไม่มีเลขพร้อมเพย์ สอบถามเลขที่จะโอนกับร้านได้โดยตรง{kha}"),
            ],
            None => Vec::new(),
        };
        notice(&headline, &details, buttons)
    }

    /// Asks the seller whether they got order `number`'s `total` (FR-PAY-07), as they handed it
    /// over, or, `by_buyer`, as its buyer says they received it. `buttons` are "ได้รับเงินแล้ว"
    /// and "ยังไม่ได้รับ".
    pub fn ask_paid(
        &self,
        number: &str,
        total: Money,
        by_buyer: bool,
        buttons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let headline = if by_buyer {
            format!("ลูกค้าแจ้งว่าได้รับออเดอร์ {number} แล้ว{kha}")
        } else {
            format!("ออเดอร์ {number} สำเร็จแล้ว{kha}")
        };
        let details = [format!("ได้รับเงิน {total} แล้วหรือยัง{kha_question}")];
        notice(&headline, &details, buttons)
    }

    /// The seller, or an admin, recorded order `number` as `status`, paid or refunded
    /// (FR-PAY-08). `buttons` are what they can do now.
    pub fn payment_recorded(
        &self,
        number: &str,
        status: order::PaymentStatus,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = if status == order::PaymentStatus::Refunded {
            format!("บันทึกว่าคืนเงินออเดอร์ {number} แล้ว{kha}")
        } else {
            format!("บันทึกว่าได้รับเงินออเดอร์ {number} แล้ว{kha}")
        };
        notice(&headline, &[], buttons)
    }

    /// Tells the buyer shop `shop`, or an admin if `by_admin`, recorded their order `number` as
    /// `status`, paid or refunded (FR-PAY-08). `buttons` are what they can do now.
    pub fn payment_recorded_notice(
        &self,
        shop: &str,
        number: &str,
        status: order::PaymentStatus,
        by_admin: bool,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let who = if by_admin {
            "ผู้ดูแล".to_owned()
        } else {
            shop_called(shop)
        };
        let headline = if status == order::PaymentStatus::Refunded {
            format!("{who}แจ้งว่าคืนเงินค่าออเดอร์ {number} แล้ว{kha}")
        } else {
            format!("{who}ยืนยันว่าได้รับเงินค่าออเดอร์ {number} แล้ว{kha}")
        };
        notice(&headline, &[], buttons)
    }

    /// Asks the buyer for the photo of the slip of order `number`'s transfer of `total`
    /// (FR-PAY-05).
    pub fn ask_slip(&self, number: &str, total: Money) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ส่งรูปสลิปโอนเงินค่าออเดอร์ {number} ยอด {total} มาในแชตนี้ได้เลย{kha}"
        ))
    }

    /// Asks the buyer whether the photo they sent unasked is the slip of order `number`'s
    /// transfer of `total` (FR-UI-14), with "ใช่" and "ไม่ใช่".
    pub fn is_it_the_slip(&self, number: &str, total: Money) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ได้รับรูปแล้ว{kha} รูปนี้เป็นสลิปโอนเงินค่าออเดอร์ {number} ยอด {total} ใช่ไหม{kha_question} ถ้าใช่ ระบบจะส่งให้ร้านตรวจสอบ{kha}"
        );
        let answers = [&labels::YES, &labels::NOT_SLIP]
            .map(|label| Action::message(label.clone(), label.as_str()));
        Message::text(text).with_quick_reply(QuickReply::new(answers.to_vec()))
    }

    /// The buyer's slip of order `number` went to its seller (FR-PAY-05). `buttons` are what they
    /// can do now.
    pub fn slip_sent(&self, number: &str, buttons: Vec<Action>) -> Message {
        let kha = self.kha();
        let headline = format!("ส่งสลิปออเดอร์ {number} ให้ร้านแล้ว{kha}");
        let details = [format!(
            "รอร้านตรวจสอบยอดเงิน{kha} สถานะการชำระเงิน: {}",
            payment_status_name(order::PaymentStatus::AwaitingConfirmation)
        )];
        notice(&headline, &details, buttons)
    }

    /// The photo the buyer sent unasked isn't the slip (FR-UI-14), so it isn't kept. The main
    /// menu follows.
    pub fn not_a_slip(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "รับทราบ{kha} ไม่ได้ส่งรูปนี้ให้ร้าน เลือกเมนูที่ต้องการได้เลย{kha}"
        ))
    }

    /// Tells the seller `buyer` sent the slip of order `number`, whose total is `total`
    /// (FR-PAY-05), and, beside the total, what the AI read from it if it did (FR-PAY-06): what
    /// doesn't match stands out, and it is said to be advice. The slip follows as an image whose
    /// link lasts `hours` (AC-14). `buttons` are "ได้รับเงินแล้ว" and "ยังไม่ได้รับ".
    pub fn slip_received_notice(
        &self,
        number: &str,
        total: Money,
        buyer: Option<&str>,
        hours: u32,
        reading: Option<&SlipReading<'_>>,
        buttons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let headline = format!("ลูกค้าส่งสลิปโอนเงินออเดอร์ {number} แล้ว{kha}");
        let mut details = vec![(format!("ยอดออเดอร์ {total}"), Tone::Plain)];
        if let Some(reading) = reading {
            details.push(match reading.amount {
                Some((amount, true)) => {
                    (format!("ยอดในสลิป {amount} ✓ ตรงกับยอดออเดอร์"), Tone::Plain)
                }
                Some((amount, false)) => (
                    format!("ยอดในสลิป {amount} ⚠️ ไม่ตรงกับยอดออเดอร์"),
                    Tone::Warning,
                ),
                None => ("⚠️ อ่านยอดในสลิปไม่ได้".to_owned(), Tone::Warning),
            });
            match reading.paid_at {
                Some((at, true)) => {
                    details.push((format!("โอนเมื่อ {}", self.date_time(at)), Tone::Plain));
                }
                Some((at, false)) => details.push((
                    format!("โอนเมื่อ {} ⚠️ ไม่ตรงกับช่วงเวลาของออเดอร์", self.date_time(at)),
                    Tone::Warning,
                )),
                None => {}
            }
            if let Some(reference) = reading.reference {
                details.push((format!("เลขอ้างอิง {reference}"), Tone::Muted));
            }
            details.push((
                format!("ระบบอ่านจากสลิปอัตโนมัติ อาจผิดพลาดได้ ตรวจยอดในบัญชีก่อนกดตอบนะ{kha_question}"),
                Tone::Muted,
            ));
        }
        if let Some(buyer) = buyer {
            details.push((format!("ลูกค้า: {buyer}"), Tone::Plain));
        }
        details.push((
            format!("ตรวจดูว่าเงินเข้าบัญชีแล้ว แล้วกดตอบด้านล่างได้เลย{kha} รูปสลิปเปิดดูได้ภายใน {hours} ชั่วโมง"),
            Tone::Plain,
        ));
        Message::flex(&headline, toned_notice_card(&headline, &details, buttons))
    }

    /// The seller didn't get the transfer of order `number`, whose slip is refused (FR-PAY-05):
    /// its buyer is told. `buttons` are what they can do now.
    pub fn slip_not_received(&self, number: &str, buttons: Vec<Action>) -> Message {
        let kha = self.kha();
        let headline = format!("บันทึกว่ายังไม่ได้รับเงินออเดอร์ {number}{kha}");
        let details = [format!("แจ้งลูกค้าให้ตรวจสอบการโอนแล้ว{kha}")];
        notice(&headline, &details, buttons)
    }

    /// Tells the buyer shop `shop` didn't get the transfer of order `number`'s `total` they sent
    /// the slip of (FR-PAY-05). `buttons` are what they can do now: "ส่งสลิป", while they can send
    /// another.
    pub fn slip_not_received_notice(
        &self,
        shop: &str,
        number: &str,
        total: Money,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!(
            "{}แจ้งว่ายังไม่ได้รับเงินค่าออเดอร์ {number}{kha}",
            shop_called(shop)
        );
        let details = [
            format!("ยอดที่ต้องโอน {total}"),
            format!("ตรวจสอบการโอนอีกครั้ง หรือสอบถามร้านได้โดยตรง{kha}"),
        ];
        notice(&headline, &details, buttons)
    }

    /// Asks for the message to `to`, the other party of order `number`, of at most `max_chars`
    /// characters (FR-ORD-13).
    pub fn ask_message(&self, number: &str, to: OrderParty<'_>, max_chars: usize) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "พิมพ์ข้อความถึง{} เรื่องออเดอร์ {number} ได้เลย{kha} ไม่เกิน {max_chars} ตัวอักษร ระบบจะส่งต่อให้ทางแชตนี้{kha}",
            to.called()
        ))
    }

    /// Tells the other party of order `number` that `from` sent them `text` (FR-ORD-13): led by
    /// the order's number, as is its preview in LINE's notifications and chat list, which also
    /// shows the text. `buttons` are the answer back, then the order in full.
    pub fn relayed_message_notice(
        &self,
        number: &str,
        from: OrderParty<'_>,
        text: &str,
        buttons: Vec<Action>,
    ) -> Message {
        let from = from.called();
        let headline = format!("{number} · ข้อความจาก{from}");
        let card = notice_card(&headline, &[text.to_owned()], buttons);
        Message::flex(format!("{number} {from}: {text}"), card)
    }

    /// The message went to `to`, the other party of order `number` (FR-ORD-13). `buttons` are
    /// what the sender can do now, another message among them.
    pub fn message_relayed(
        &self,
        number: &str,
        to: OrderParty<'_>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("ส่งข้อความถึง{} แล้ว{kha}", to.called());
        notice(&headline, &[format!("ออเดอร์ {number}")], buttons)
    }

    /// No message can go to `to`, the other party of order `number`, who doesn't follow the OA
    /// (FR-NOT-02). `buttons` are what the sender can do instead.
    pub fn message_unreachable(
        &self,
        number: &str,
        to: OrderParty<'_>,
        buttons: Vec<Action>,
    ) -> Message {
        let (kha, app) = (self.kha(), &self.app_name);
        let to = to.called();
        let headline = format!("ส่งข้อความถึง{to}ไม่ได้{kha}");
        let details = [
            format!("ออเดอร์ {number}"),
            format!("{to}ไม่ได้เป็นเพื่อนกับ {app} ในไลน์อยู่ จึงส่งข้อความต่อให้ไม่ได้{kha}"),
        ];
        notice(&headline, &details, buttons)
    }

    /// Asks the buyer how many stars order `number` from shop `shop` earns (FR-ORD-14): `stars`
    /// give them, five first.
    pub fn ask_stars(&self, number: &str, shop: &str, stars: Vec<Action>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ออเดอร์ {number} จาก{} ถูกใจแค่ไหน{kha_question} ให้คะแนน 1–5 ดาวได้เลย{kha}",
            shop_called(shop)
        ))
        .with_quick_reply(QuickReply::new(stars))
    }

    /// Order `number` got `stars` (FR-ORD-14): asks for what the buyer would tell its shop, in
    /// at most `max_chars`, which the shop sees on the order. `skip` goes without.
    pub fn ask_rating_comment(
        &self,
        number: &str,
        stars: i16,
        max_chars: usize,
        skip: Action,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ให้ออเดอร์ {number} {} แล้ว{kha} อยากบอกอะไรถึงร้านไหม{kha_question} พิมพ์ได้ไม่เกิน {max_chars} ตัวอักษร ร้านจะเห็นในออเดอร์นี้ ถ้าไม่มีกด \"{}\" ได้เลย{kha}",
            stars_line(stars),
            skip.label().as_str()
        ))
        .with_quick_reply(QuickReply::new([skip]))
    }

    /// Order `number`'s rating is done (FR-ORD-14). `carry_on` are where to go next.
    pub fn rating_saved(&self, number: &str, carry_on: Vec<Action>) -> Message {
        let kha = self.kha();
        Message::text(format!("ขอบคุณที่ให้คะแนนออเดอร์ {number}{kha}"))
            .with_quick_reply(QuickReply::new(carry_on))
    }

    /// Order `number` was rated `stars` already, and is rated once (FR-ORD-14). `carry_on`
    /// are where to go next.
    pub fn already_rated(&self, number: &str, stars: i16, carry_on: Vec<Action>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ออเดอร์ {number} ให้คะแนนไปแล้ว ({}) ให้ได้ออเดอร์ละครั้งเดียว{kha}",
            stars_line(stars)
        ))
        .with_quick_reply(QuickReply::new(carry_on))
    }

    /// The seller has yet to get the money of order `number`, handed over (FR-PAY-07): it stays
    /// under "ค้างชำระ". `buttons` are "ได้รับเงินแล้ว", for when they do.
    pub fn left_unpaid(&self, number: &str, buttons: Vec<Action>) -> Message {
        let kha = self.kha();
        let headline = format!("ออเดอร์ {number} ยัง{}{kha}", labels::FILTER_UNPAID.as_str());
        let details = [format!(
            "ดูได้ที่ \"{}\" > \"{}\" ได้รับเงินแล้วกด \"{}\"{kha}",
            labels::SHOP_ORDERS.as_str(),
            labels::FILTER_UNPAID.as_str(),
            labels::MARK_PAID.as_str()
        )];
        notice(&headline, &details, buttons)
    }

    /// Tells the buyer their order `number` from shop `shop` is now `status` (FR-ORD-08): ready
    /// to pick up, on its way, or handed over. `buttons` are what they can do now, such as
    /// "ได้รับของแล้ว".
    pub fn order_progress_notice(
        &self,
        shop: &str,
        number: &str,
        status: order::Status,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let shop = shop_called(shop);
        let headline = match status {
            order::Status::Ready => format!("ออเดอร์ {number} พร้อมให้รับที่{shop}แล้ว{kha}"),
            order::Status::Delivering => format!("{shop}กำลังไปส่งออเดอร์ {number}{kha}"),
            _ => format!("ออเดอร์ {number} สำเร็จแล้ว ขอบคุณที่อุดหนุน{shop}{kha}"),
        };
        notice(&headline, &[], buttons)
    }

    /// Tells the buyer shop `shop` turned down their order `number`, for `reason` (FR-ORD-04,
    /// FR-ORD-08). `buttons` carry on shopping.
    pub fn order_rejected_notice(
        &self,
        shop: &str,
        number: &str,
        reason: &str,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("{}ไม่รับออเดอร์ {number}{kha}", shop_called(shop));
        let details = [
            format!("เหตุผล: {reason}"),
            format!("เลือกซื้อสินค้าอื่นได้เลย{kha}"),
        ];
        notice(&headline, &details, buttons)
    }

    /// Tells the buyer their order `number` from shop `shop` was cancelled by the shop, or by an
    /// admin if `by_admin`, for `reason` (FR-ORD-04, FR-ORD-08, FR-ORD-12); `refund` if they had
    /// paid, so the shop owes it back (FR-PAY-09). `buttons` carry on shopping.
    pub fn order_cancelled_notice(
        &self,
        shop: &str,
        number: &str,
        by_admin: bool,
        reason: &str,
        refund: bool,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = if by_admin {
            format!("ผู้ดูแลยกเลิกออเดอร์ {number}{kha}")
        } else {
            format!("{}ยกเลิกออเดอร์ {number}{kha}", shop_called(shop))
        };
        let mut details = vec![format!("เหตุผล: {reason}")];
        if refund {
            details.push(format!(
                "คุณชำระเงินไปแล้ว {}จะคืนเงินให้ ตกลงกับร้านได้โดยตรง{kha}",
                shop_called(shop)
            ));
        }
        details.push(format!("เลือกซื้อสินค้าอื่นได้เลย{kha}"));
        notice(&headline, &details, buttons)
    }

    /// Tells the buyer shop `shop` did not answer their order `number` within `minutes`, so it
    /// expired (FR-ORD-05, AC-11). `buttons` carry on shopping.
    pub fn order_expired_notice(
        &self,
        shop: &str,
        number: &str,
        minutes: u32,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("ออเดอร์ {number} หมดเวลา{kha}");
        let details = [
            format!(
                "{}ไม่ได้ตอบรับภายใน {minutes} นาที ระบบจึงยกเลิกให้อัตโนมัติ{kha}",
                shop_called(shop)
            ),
            format!("เลือกซื้อสินค้าอื่นได้เลย{kha}"),
        ];
        notice(&headline, &details, buttons)
    }

    /// Tells the seller the buyer cancelled order `number` before it was accepted (FR-ORD-06,
    /// Table 4.8-A).
    pub fn order_cancelled_by_buyer_notice(&self, number: &str) -> Message {
        let kha = self.kha();
        let headline = format!("ลูกค้ายกเลิกออเดอร์ {number}{kha}");
        let details = [format!("ไม่ต้องเตรียมของ คืนจำนวนสินค้าเข้าสต็อกแล้ว{kha}")];
        notice(&headline, &details, Vec::new())
    }

    /// Tells the seller an admin cancelled order `number`, for `reason` (FR-ORD-12); `refund` if
    /// the buyer had paid, so the shop owes it back (FR-PAY-09). `buttons` are what they can do
    /// now, "คืนเงินแล้ว" once they have.
    pub fn order_cancelled_by_admin_notice(
        &self,
        number: &str,
        reason: &str,
        refund: bool,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!("ผู้ดูแลยกเลิกออเดอร์ {number}{kha}");
        let mut details = vec![
            format!("เหตุผล: {reason}"),
            format!("คืนจำนวนสินค้าเข้าสต็อกแล้ว{kha}"),
        ];
        if refund {
            details.push(refund_line(kha));
        }
        notice(&headline, &details, buttons)
    }

    /// Tells the seller order `number` expired, unanswered within `minutes` (FR-ORD-05, AC-11).
    pub fn order_expired_seller_notice(&self, number: &str, minutes: u32) -> Message {
        let kha = self.kha();
        let headline = format!("ออเดอร์ {number} หมดเวลา{kha}");
        let details = [format!(
            "ไม่ได้ตอบรับภายใน {minutes} นาที ระบบจึงยกเลิกให้และคืนจำนวนสินค้าเข้าสต็อกแล้ว{kha}"
        )];
        notice(&headline, &details, Vec::new())
    }

    /// Tells the seller the buyer asks to cancel order `number`, which they accepted (FR-ORD-07):
    /// `buttons`, "ยกเลิกออเดอร์" and "ไม่ยกเลิก", decide.
    pub fn cancellation_requested_notice(&self, number: &str, buttons: Vec<Action>) -> Message {
        let kha = self.kha();
        let headline = format!("ลูกค้าขอยกเลิกออเดอร์ {number}{kha}");
        let details = [format!(
            "กด \"{}\" เพื่อยกเลิกและคืนจำนวนสินค้าเข้าสต็อก หรือ \"{}\" เพื่อทำออเดอร์ต่อ{kha}",
            labels::CANCEL_ORDER.as_str(),
            labels::KEEP_ORDER.as_str()
        )];
        notice(&headline, &details, buttons)
    }

    /// The buyer asked to cancel order `number` (FR-ORD-07), or had already if `again`: the
    /// seller decides.
    pub fn cancellation_requested(&self, number: &str, again: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let asked = if again {
            format!("ขอยกเลิกออเดอร์ {number} ไปแล้ว{kha}")
        } else {
            format!("ส่งคำขอยกเลิกออเดอร์ {number} ถึงร้านแล้ว{kha}")
        };
        Message::text(format!("{asked} รอร้านตอบนะ{kha_question}"))
    }

    /// Tells the buyer shop `shop` goes on with their order `number` rather than cancel it, as
    /// they asked (FR-ORD-07). `buttons` are what they can do now.
    pub fn cancellation_kept_notice(
        &self,
        shop: &str,
        number: &str,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!(
            "{}ขอทำออเดอร์ {number} ต่อ ยังไม่ยกเลิกให้{kha}",
            shop_called(shop)
        );
        notice(&headline, &[], buttons)
    }

    /// The seller goes on with order `number` rather than cancel it as the buyer asked
    /// (FR-ORD-07). `buttons` are its next step.
    pub fn cancellation_kept(&self, number: &str, buttons: Vec<Action>) -> Message {
        let kha = self.kha();
        notice(&format!("ทำออเดอร์ {number} ต่อ{kha}"), &[], buttons)
    }

    /// `by` moved order `number` to `status` (Table 4.6-B), other than accepting it, which says
    /// more ([`order_accepted`](Self::order_accepted)); `refund` if it was paid, so the shop owes
    /// it back (FR-PAY-09). `buttons` are what they can do now.
    pub fn order_moved(
        &self,
        number: &str,
        status: order::Status,
        by: ActorType,
        refund: bool,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let restocked = format!("คืนจำนวนสินค้าเข้าสต็อกแล้ว{kha}");
        let (headline, mut details) = match status {
            order::Status::Ready => (format!("ออเดอร์ {number} พร้อมให้รับแล้ว{kha}"), vec![]),
            order::Status::Delivering => (format!("ออเดอร์ {number} กำลังไปส่ง{kha}"), vec![]),
            order::Status::Completed if by == ActorType::Buyer => {
                (format!("ออเดอร์ {number} สำเร็จแล้ว ขอบคุณที่แจ้ง{kha}"), vec![])
            }
            order::Status::Rejected => (format!("ปฏิเสธออเดอร์ {number} แล้ว{kha}"), vec![restocked]),
            order::Status::Cancelled if by == ActorType::Buyer => {
                (format!("ยกเลิกออเดอร์ {number} แล้ว{kha}"), vec![])
            }
            order::Status::Cancelled => (format!("ยกเลิกออเดอร์ {number} แล้ว{kha}"), vec![restocked]),
            _ => (format!("ออเดอร์ {number} สำเร็จแล้ว{kha}"), vec![]),
        };
        if refund && by == ActorType::Seller {
            details.push(refund_line(kha));
        } else if refund {
            details.push(format!("ลูกค้าชำระเงินแล้ว ร้านต้องคืนเงินให้ลูกค้า{kha}"));
        }
        notice(&headline, &details, buttons)
    }

    /// Order `number` is `status` now, and its payment `payment` if that is what was asked
    /// about, so what was asked of it can't be done (FR-UI-13, FR-ORD-02): it moved meanwhile,
    /// or the same button was tapped again. `buttons` are what can be done now.
    pub fn order_now(
        &self,
        number: &str,
        status: order::Status,
        payment: Option<order::PaymentStatus>,
        buttons: Vec<Action>,
    ) -> Message {
        let kha = self.kha();
        let headline = format!(
            "ออเดอร์ {number} ตอนนี้สถานะ \"{}\"{kha}",
            order_status_name(status)
        );
        let details: Vec<String> = payment
            .map(|payment| format!("การชำระเงิน: {}", payment_status_name(payment)))
            .into_iter()
            .collect();
        notice(&headline, &details, buttons)
    }

    /// Asks why order `number` is moved to `status`, rejected or cancelled, by an admin if
    /// `by_admin` (FR-ORD-04): `reasons`, sent as if typed, are those of FR-ORD-04.
    pub fn ask_order_reason(
        &self,
        number: &str,
        status: order::Status,
        by_admin: bool,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let act = if status == order::Status::Rejected {
            "ปฏิเสธ"
        } else {
            "ยกเลิก"
        };
        let seen = if by_admin {
            "ทั้งลูกค้าและร้าน"
        } else {
            "ลูกค้า"
        };
        Message::text(format!(
            "{act}ออเดอร์ {number} เพราะอะไร{kha_question} เลือกด้านล่างได้เลย {seen}จะเห็นเหตุผลนี้{kha}"
        ))
        .with_quick_reply(QuickReply::new(reasons))
    }

    /// Asks for the reason, "อื่นๆ" having been picked (FR-ORD-04), in at most `max_chars`.
    pub fn ask_other_order_reason(&self, max_chars: usize) -> Message {
        let kha = self.kha();
        Message::text(format!("พิมพ์เหตุผลมาได้เลย{kha} ไม่เกิน {max_chars} ตัวอักษร"))
    }

    /// The order an admin found by its number in "ผู้ใช้" (FR-ADM-04), with `buttons`,
    /// "ยกเลิกออเดอร์" while it is open (FR-ORD-12).
    pub fn order_found(&self, order: &FoundOrder<'_>, buttons: Vec<Action>) -> Message {
        let headline = format!("ออเดอร์ {}", order.number);
        let mut details = vec![
            format!("สถานะ: {}", order_status_name(order.status)),
            shop_called(order.shop),
        ];
        if let Some(buyer) = order.buyer {
            details.push(format!("ลูกค้า: {buyer}"));
        }
        details.push(format!(
            "รวม {} · {} · {}",
            order.total,
            payment_name(order.payment),
            payment_status_name(order.payment_status)
        ));
        details.push(format!("สั่งเมื่อ {}", self.date_time(order.placed_at)));
        if let Some(rating) = order.rating {
            details.push(format!("{RATING}: {}", rating_line(rating)));
        }
        notice(&headline, &details, buttons)
    }

    /// "คำสั่งซื้อของฉัน" (FR-ORD-10): `cards`, a page of the buyer's orders, open ones first,
    /// then the rest, and "ดูเพิ่มเติม" sending `more` when more follow. `carry_on` are where to
    /// go next.
    pub fn my_orders(
        &self,
        cards: &[OrderCard<'_>],
        more: Option<String>,
        carry_on: Vec<Action>,
    ) -> Message {
        let quick_reply = QuickReply::new(carry_on);
        if cards.is_empty() {
            let kha = self.kha();
            let text = format!("ยังไม่มีคำสั่งซื้อ{kha} เลือกซื้อของได้เลย{kha}");
            return Message::text(text).with_quick_reply(quick_reply);
        }
        let cards = cards
            .iter()
            .map(|card| self.order_view_card(card, false))
            .collect();
        Message::flex(labels::MY_ORDERS.as_str(), carousel(cards, more))
            .with_quick_reply(quick_reply)
    }

    /// "ออเดอร์ร้าน" (FR-ORD-10): `cards`, a page of the shop's orders that `filter` picks, and
    /// "ดูเพิ่มเติม" sending `more` when more follow. `filters` offer the others, and where else
    /// to go.
    pub fn shop_orders(
        &self,
        filter: &Label,
        cards: &[OrderCard<'_>],
        more: Option<String>,
        filters: Vec<Action>,
    ) -> Vec<Message> {
        let kha = self.kha();
        let filter = filter.as_str();
        let quick_reply = QuickReply::new(filters);
        if cards.is_empty() {
            let text = format!("ตอนนี้ไม่มีออเดอร์ \"{filter}\"{kha}");
            return vec![Message::text(text).with_quick_reply(quick_reply)];
        }
        let heading = Message::text(format!("ออเดอร์ \"{filter}\" ของร้าน{kha}"));
        let cards = cards
            .iter()
            .map(|card| self.order_view_card(card, false))
            .collect();
        let alt_text = format!("{} · {filter}", labels::SHOP_ORDERS.as_str());
        let list = Message::flex(alt_text, carousel(cards, more)).with_quick_reply(quick_reply);
        vec![heading, list]
    }

    /// Order `card` in full (FR-ORD-10), with "ดูแผนที่" where FR-ORD-11 shows a location. `back`
    /// returns to the list it is in.
    pub fn order_details(&self, card: &OrderCard<'_>, back: Vec<Action>) -> Message {
        let alt_text = format!("ออเดอร์ {}", card.number);
        Message::flex(alt_text, self.order_view_card(card, true))
            .with_quick_reply(QuickReply::new(back))
    }

    /// Order `card` as a list shows it, or in full if `full`: its number, status and when it was
    /// placed, its shop on its buyer's card, its lines, at most [`ORDER_LINES_ON_CARD`] of them
    /// unless `full`, the rest counted, its amounts, how it is received and paid, and as much of
    /// how to reach the other party as FR-ORD-11 allows. In full, "ดูแผนที่" follows its buttons
    /// where that shows a location.
    fn order_view_card(&self, card: &OrderCard<'_>, full: bool) -> Bubble {
        let headline = Text {
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(format!("ออเดอร์ {}", card.number))
        };
        let mut contents: Vec<Component> = vec![
            headline.into(),
            row("สถานะ", order_status_name(card.status)),
            row("สั่งเมื่อ", &self.date_time(card.placed_at)),
            Separator::default().into(),
        ];
        if let Some(shop) = card.shop {
            let shop = Text {
                size: Some(Size::Sm),
                weight: Some(Weight::Bold),
                wrap: Some(true),
                ..Text::new(shop_called(shop))
            };
            contents.push(shop.into());
        }
        let shown = if full {
            card.lines.len()
        } else {
            card.lines.len().min(ORDER_LINES_ON_CARD)
        };
        for line in &card.lines[..shown] {
            let amounts = line_amounts(
                line.name,
                line.quantity,
                line.unit,
                line.price,
                line.total,
                line.ready_on,
            );
            let line = FlexBox {
                spacing: Some(Spacing::Sm),
                ..FlexBox::vertical(amounts)
            };
            contents.push(line.into());
        }
        let rest = card.lines.len() - shown;
        if rest > 0 {
            let rest = Text {
                size: Some(Size::Sm),
                color: Some(MUTED.to_owned()),
                ..Text::new(format!("และอีก {rest} รายการ"))
            };
            contents.push(rest.into());
        }
        contents.push(Separator::default().into());
        let delivered = card.fulfillment == order::Fulfillment::Delivery;
        if full {
            contents.push(amount("ค่าสินค้า", card.subtotal, false));
        }
        if delivered {
            let fee = amount(labels::DELIVERY_FEE.as_str(), card.delivery_fee, false);
            contents.push(fee);
        }
        contents.push(amount("รวม", card.total, true));
        contents.push(Separator::default().into());

        // How it is received, then who it is for or how to reach the shop, as FR-ORD-11 allows
        let received = fulfillment_name(card.fulfillment).to_owned();
        let (received, pickup, people, location) = match &card.contact {
            OrderContact::None => (received, None, Vec::new(), None),
            OrderContact::Buyer { name, zone } => {
                let received = match zone {
                    Some(zone) if delivered => format!("ส่งที่ {zone}"),
                    _ => received,
                };
                let buyer = match (name, zone) {
                    (Some(name), Some(zone)) if !delivered => Some(format!("{name} · {zone}")),
                    (Some(name), _) => Some((*name).to_owned()),
                    (None, _) => None,
                };
                let people = buyer.map(|buyer| row("ลูกค้า", &buyer));
                (received, None, people.into_iter().collect(), None)
            }
            OrderContact::BuyerFull { contact, location } => {
                let received = match &contact.address {
                    Some(address) => format!("ส่งที่ {address}"),
                    None => received,
                };
                let mut people = Vec::new();
                if let Some(buyer) = contact.buyer {
                    people.push(row("ลูกค้า", buyer));
                }
                if let Some(phone) = contact.phone {
                    people.push(row("โทร", &thai::format_phone(phone)));
                }
                if let Some(note) = contact.note {
                    people.push(row("ฝากถึงร้าน", note));
                }
                (received, None, people, *location)
            }
            OrderContact::Shop {
                pickup,
                location,
                phone,
                promptpay,
            } => {
                let phone =
                    phone.map(|phone| row(labels::SHOP_PHONE.as_str(), &thai::format_phone(phone)));
                let promptpay = promptpay.map(|id| {
                    row(
                        labels::PROMPTPAY.as_str(),
                        &payments::format_promptpay_id(id),
                    )
                });
                let people = phone.into_iter().chain(promptpay).collect();
                (received, *pickup, people, *location)
            }
        };
        contents.push(row("รับของ", &received));
        if let Some(pickup) = pickup {
            contents.push(row(labels::PICKUP_ADDRESS.as_str(), pickup));
        }
        // Whether it is paid once it is to be: an order never accepted, or called off unpaid,
        // owes nothing
        let owed = card.status.is_active() || card.status == order::Status::Completed;
        let paid = payment_name(card.payment);
        let paid = match card.payment_status {
            _ if card.unpaid => format!("{paid} · {}", labels::FILTER_UNPAID.as_str()),
            order::PaymentStatus::Unpaid if !owed => paid.to_owned(),
            status => format!("{paid} · {}", payment_status_name(status)),
        };
        contents.push(row("ชำระเงิน", &paid));
        contents.extend(people);
        if let Some(rating) = card.rating {
            contents.push(row(RATING, &rating_line(rating)));
        }

        let mut buttons = card.buttons.clone();
        if full && let Some((latitude, longitude)) = location {
            buttons.push(Action::Uri {
                label: labels::SEE_MAP.clone(),
                uri: map_url(latitude, longitude),
            });
        }
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical(contents)
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            body: Some(body),
            footer: footer(&buttons),
            ..Bubble::default()
        }
    }

    /// "ช่วยเหลือ" (FR-HELP-01): a button for each of `topics`, those of the user's roles.
    pub fn help(&self, topics: impl IntoIterator<Item = Topic>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let title = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            ..Text::new(labels::HELP.as_str())
        };
        let intro = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
            ..Text::new(format!("อยากรู้เรื่องไหน{kha_question} เลือกได้เลย{kha}"))
        };
        let buttons = topics.into_iter().map(|topic| {
            let label = topic_label(topic).clone();
            let button = Button {
                style: Some(ButtonStyle::Secondary),
                height: Some(ButtonHeight::Sm),
                ..Button::new(topic.postback().button(label))
            };
            button.into()
        });
        let card = Bubble {
            size: Some(BubbleSize::Mega),
            header: Some(FlexBox::vertical([title.into()])),
            body: Some(FlexBox {
                spacing: Some(Spacing::Sm),
                ..FlexBox::vertical(std::iter::once(intro.into()).chain(buttons))
            }),
            ..Bubble::default()
        };
        Message::flex(labels::HELP.as_str(), card)
    }

    /// A help topic (FR-HELP-01): its `article`, with buttons for `next`, what to do about it,
    /// and back to the topics and the menu.
    pub fn help_topic(&self, article: String, next: Vec<Action>) -> Message {
        let back = [&labels::HELP, &labels::MENU]
            .map(|label| Action::message(label.clone(), label.as_str()));
        Message::text(article).with_quick_reply(QuickReply::new(next.into_iter().chain(back)))
    }

    /// The text of help topic `topic` (FR-HELP-01) for a user and configuration as `facts`
    /// says. It is all the AI assistant may answer a how-to question from (FR-AI-14), so it
    /// is plain text, complete on its own.
    pub fn help_article(&self, topic: Topic, facts: &HelpFacts<'_>) -> String {
        let kha = self.kha();
        let quoted = |label: &Label| format!("\"{}\"", label.as_str());
        let (buy, search, cart, my_orders) = (
            quoted(&labels::BUY),
            quoted(&labels::SEARCH),
            quoted(&labels::CART),
            quoted(&labels::MY_ORDERS),
        );
        let mut lines = Vec::new();
        match topic {
            Topic::Ordering => {
                lines.push(format!("วิธีสั่งซื้อ{kha}"));
                lines.push(format!(
                    "1. กด {buy} แล้วเลือกหมวด หรือกด {search} แล้วพิมพ์ชื่อสินค้า"
                ));
                lines.push("2. กด \"ใส่ตะกร้า\" ที่สินค้า แล้วเลือกจำนวน".to_owned());
                lines.push(format!(
                    "3. เปิด {cart} แล้วสั่งซื้อทีละร้าน เลือกรับเองหรือให้ร้านไปส่ง และเลือกวิธีชำระเงิน"
                ));
                lines.push("4. ตรวจสรุปแล้วกด \"ยืนยันสั่งซื้อ\"".to_owned());
                let told = if facts.status_pushes {
                    "ระบบจะแจ้งในแชตนี้ทุกครั้งที่สถานะเปลี่ยน "
                } else {
                    ""
                };
                lines.push(format!(
                    "จากนั้นรอร้านรับออเดอร์ {told}ดูออเดอร์ทั้งหมดได้ที่ {my_orders} และยกเลิกเองได้ก่อนร้านรับออเดอร์{kha}"
                ));
                lines.push(format!(
                    "สินค้าที่เขียนว่า \"{}\" จะบอกวันที่ของพร้อมไว้ที่สินค้า ในตะกร้า และในออเดอร์{kha}",
                    labels::PRE_ORDER.as_str()
                ));
                lines.push(format!(
                    "ได้รับของแล้ว กด \"{}\" ที่ออเดอร์ เพื่อให้ร้าน 1–5 ดาวได้ ออเดอร์ละครั้ง{kha}",
                    labels::RATE_ORDER.as_str()
                ));
                if facts.assistant {
                    lines.push(format!(
                        "จะพิมพ์บอกสิ่งที่อยากได้ก็ได้{kha} เช่น \"อยากได้ไข่ไก่ 2 แผง\" ผู้ช่วยจะหาให้ แต่จะใส่ตะกร้าหรือสั่งซื้อก็ต่อเมื่อคุณกดยืนยันเอง{kha}"
                    ));
                }
            }
            Topic::Selling if facts.seller => {
                lines.push(format!("วิธีลงสินค้าและดูแลร้าน{kha}"));
                lines.push(format!(
                    "- {}: ใส่รูป ชื่อ ราคา หน่วย หมวด และจำนวน แล้วกด \"ยืนยันลงขาย\"",
                    quoted(&labels::ADD_PRODUCT)
                ));
                if facts.listing {
                    lines.push(format!(
                        "- หรือส่งรูปสินค้าพร้อมข้อความสั้นๆ เช่น \"{LISTING_EXAMPLE}\" ผู้ช่วยจะร่างให้ตรวจก่อนลงขาย"
                    ));
                    lines.push(
                        "- แก้ราคาหรือจำนวนได้ด้วยข้อความสั้นๆ เช่น \"ขนมครก เหลือ 10\" หรือ \"ไข่ไก่ ราคา 115\" แล้วกดยืนยัน"
                            .to_owned(),
                    );
                }
                lines.push(format!(
                    "- {}: แก้ราคา เติมจำนวน ซ่อน หรือลบสินค้า",
                    quoted(&labels::MY_PRODUCTS)
                ));
                lines.push(format!(
                    "- รับสั่งล่วงหน้า: กด {} แล้วเลือก {} เพื่อบอกวันที่ของพร้อม ผู้ซื้อจะเห็นวันนี้ที่สินค้าและในออเดอร์",
                    quoted(&labels::EDIT),
                    quoted(&labels::PRE_ORDER)
                ));
                lines.push(format!(
                    "- {}: ปิดร้านชั่วคราวได้ ออเดอร์ที่มีอยู่ยังดำเนินต่อ",
                    quoted(&labels::OPEN_CLOSE)
                ));
                lines.push(format!(
                    "- {}: แก้ข้อมูลร้าน ใส่เวลาเปิดร้าน และเลือกว่าจะให้ผู้ซื้อเห็นเบอร์ร้านไหม",
                    quoted(&labels::SHOP_SETTINGS)
                ));
                lines.push(format!(
                    "- {}: กดรับหรือปฏิเสธออเดอร์ใหม่ภายใน {} นาที ไม่อย่างนั้นออเดอร์จะหมดเวลา",
                    quoted(&labels::SHOP_ORDERS),
                    facts.pending_minutes
                ));
                lines.push(format!(
                    "สินค้าหรือร้านที่แก้ไขอาจถูกพักไว้ให้ผู้ดูแลตรวจก่อน ระบบจะแจ้งให้ทราบ{kha}"
                ));
            }
            Topic::Selling => {
                lines.push(format!("วิธีเปิดร้านและลงสินค้า{kha}"));
                lines.push(format!(
                    "1. กด {} แล้วกรอกชื่อร้าน คำอธิบาย ที่รับของ เบอร์ติดต่อ และเลือกว่ารับเองหรือส่งได้",
                    quoted(&labels::OPEN_SHOP)
                ));
                lines.push(if facts.require_approval {
                    "2. รอผู้ดูแลอนุมัติ ระบบจะแจ้งผลในแชตนี้".to_owned()
                } else {
                    "2. ร้านเปิดได้ทันที".to_owned()
                });
                lines.push(format!(
                    "3. จากนั้นจะมีเมนู {} ให้กด {} ใส่รูป ชื่อ ราคา หน่วย หมวด และจำนวน แล้วกด \"ยืนยันลงขาย\"",
                    quoted(&labels::SELLER),
                    quoted(&labels::ADD_PRODUCT)
                ));
                lines.push(format!("หนึ่งคนเปิดร้านได้ 1 ร้าน{kha}"));
            }
            Topic::Payment => {
                lines.push(format!(
                    "การชำระเงิน{kha} ผู้ซื้อจ่ายเงินให้ร้านโดยตรง ระบบไม่ได้รับหรือถือเงินไว้"
                ));
                if facts.cash {
                    lines.push("- เงินสด: จ่ายตอนรับของที่ร้าน หรือตอนร้านมาส่ง".to_owned());
                }
                if facts.promptpay {
                    lines.push(
                        "- โอนพร้อมเพย์: เมื่อร้านรับออเดอร์ ระบบจะส่งเลขพร้อมเพย์ของร้านและยอดที่ต้องโอน โอนแล้วกด \"ส่งสลิป\" และส่งรูปสลิปในแชตนี้"
                            .to_owned(),
                    );
                }
                lines.push(format!(
                    "ดูสถานะการชำระเงินได้ที่ {my_orders}{kha}"
                ));
                if facts.seller {
                    let mut shop = "สำหรับร้าน: ได้รับเงินแล้วกด \"ได้รับเงินแล้ว\" ที่ออเดอร์".to_owned();
                    if facts.promptpay {
                        shop.push_str(&format!(
                            " ใส่เลขพร้อมเพย์ของร้านได้ที่ {}",
                            quoted(&labels::SHOP_SETTINGS)
                        ));
                    }
                    shop.push_str(&format!(
                        " ถ้าออเดอร์ที่จ่ายแล้วถูกยกเลิก ให้คืนเงินกับลูกค้าโดยตรง แล้วกด \"คืนเงินแล้ว\"{kha}"
                    ));
                    lines.push(shop);
                }
            }
            Topic::MyData => {
                lines.push(format!(
                    "{} แสดงข้อมูลส่วนตัวทั้งหมดที่ระบบเก็บไว้เกี่ยวกับคุณ{kha} ทั้งชื่อ รูปโปรไฟล์ ที่อยู่ เบอร์โทร ตำแหน่ง และข้อมูลร้าน พร้อมบอกว่าเก็บไว้นานเท่าไร",
                    quoted(&labels::MY_DATA)
                ));
                lines.push(format!(
                    "แก้ที่อยู่ เบอร์โทร หรือตำแหน่งได้จากปุ่มในหน้านั้น เบอร์โทรและตำแหน่งไม่บังคับ จะลบออกก็ได้{kha}"
                ));
            }
            Topic::Privacy => {
                let privacy = &facts.privacy;
                lines.push(format!("ความเป็นส่วนตัว{kha}"));
                lines.push(privacy.text.to_owned());
                lines.extend(privacy_lines(privacy));
                let full = if privacy.url.is_some() {
                    format!(" อ่านฉบับเต็มได้จากปุ่ม {}", quoted(&labels::FULL_NOTICE))
                } else {
                    String::new()
                };
                lines.push(format!(
                    "ประกาศความเป็นส่วนตัวฉบับที่ {}{full}{kha}",
                    privacy.version
                ));
            }
            Topic::Report => {
                lines.push(format!(
                    "เจอสินค้าหรือร้านที่ไม่เหมาะสม แจ้งผู้ดูแลได้{kha} เช่น ข้อมูลไม่ตรงความจริง ขายของต้องห้าม หรือหลอกลวง"
                ));
                lines.push(format!(
                    "กด {} ที่สินค้าหรือร้านนั้น แล้วเลือกหรือพิมพ์ว่าเกิดอะไรขึ้น ผู้ดูแลจะได้รับเรื่องและตรวจสอบ",
                    quoted(&labels::REPORT)
                ));
                lines.push(format!(
                    "ถ้าเป็นเรื่องอื่น กด {} ได้เลย{kha}",
                    quoted(&labels::CONTACT_ADMINS)
                ));
            }
            Topic::Contact => match facts.admin_contact {
                Some(contact) => {
                    lines.push(format!("ติดต่อผู้ดูแลได้ที่นี่{kha}"));
                    lines.push(contact.to_owned());
                }
                None => lines.push(format!(
                    "ติดต่อผู้ดูแลหมู่บ้านได้โดยตรง{kha} ถ้าเป็นปัญหาของสินค้าหรือร้าน กด {} ที่สินค้าหรือร้านนั้นได้เลย{kha}",
                    quoted(&labels::REPORT)
                )),
            },
            Topic::DeleteAccount => {
                lines.push(format!(
                    "{} คือการถอนความยินยอมและลบข้อมูลที่ระบุตัวคุณ{kha}",
                    quoted(&labels::DELETE_ACCOUNT)
                ));
                lines.push(
                    "- ลบได้เมื่อไม่มีออเดอร์ที่ยังไม่จบ ทั้งที่คุณสั่งซื้อและของร้านคุณ".to_owned(),
                );
                lines.push(
                    "- ลบชื่อ รูปโปรไฟล์ เบอร์โทร ที่อยู่ ตำแหน่ง และบัญชี LINE ที่ผูกไว้".to_owned(),
                );
                lines.push("- ถ้ามีร้าน ร้านจะปิดถาวรและสินค้าทั้งหมดถูกลบ".to_owned());
                lines.push("- ประวัติคำสั่งซื้อยังอยู่ แต่ไม่ระบุว่าเป็นคุณ".to_owned());
                lines.push(format!(
                    "ลบแล้วกู้คืนไม่ได้ ระบบจะถามยืนยันอีกครั้งก่อนลบ{kha}"
                ));
            }
            Topic::Admin => {
                lines.push(format!(
                    "งานของผู้ดูแล{kha} ทำได้จากเมนู {}",
                    quoted(&labels::ADMIN)
                ));
                lines.push(format!(
                    "- {}: ดูคำขอเปิดร้าน แล้วอนุมัติ หรือไม่อนุมัติพร้อมเหตุผล",
                    quoted(&labels::APPROVE_SHOPS)
                ));
                lines.push(format!(
                    "- {}: สินค้าและร้านที่ถูกพัก และเรื่องที่ผู้ใช้แจ้งปัญหา อนุมัติ นำออก ระงับร้าน หรือปิดเรื่อง",
                    quoted(&labels::REVIEW_QUEUE)
                ));
                lines.push(format!(
                    "- {}: ค้นหาจากชื่อ เบอร์โทร หรือเลขออเดอร์ แล้วระงับหรือยกเลิกการระงับ",
                    quoted(&labels::USERS)
                ));
                lines.push(format!(
                    "- {}: เพิ่ม เปลี่ยนชื่อ เรียงลำดับ และปิดใช้งานหมวด",
                    quoted(&labels::CATEGORIES)
                ));
                lines.push(format!(
                    "- {} และ {}: ดูภาพรวมของตลาด และส่งประกาศถึงทุกคน",
                    quoted(&labels::STATS),
                    quoted(&labels::ANNOUNCEMENTS)
                ));
                if facts.admin_alerts {
                    lines.push(
                        "ระบบจะแจ้งเตือนเมื่อมีคำขอเปิดร้าน สินค้าถูกพัก หรือมีผู้แจ้งปัญหา".to_owned(),
                    );
                }
                lines.push(format!(
                    "ทุกอย่างที่ผู้ดูแลทำถูกบันทึกไว้ และทำได้จากเมนูเท่านั้น ผู้ช่วย AI ทำแทนไม่ได้{kha}"
                ));
            }
        }
        lines.join("\n")
    }

    /// Asks what is wrong with `what`, reported (FR-HELP-02), in at most `max_chars`.
    /// `reasons` are common ones, sent as if typed.
    pub fn ask_report_reason(
        &self,
        what: &Reported<'_>,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "แจ้งปัญหา{} เกิดอะไรขึ้น{kha_question} เลือกด้านล่าง หรือพิมพ์เล่ามาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha}",
            what.named()
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// The user has already reported `what`, and admins have yet to look into it.
    pub fn already_reported(&self, what: &Reported<'_>) -> Message {
        let kha = self.kha();
        let text = format!(
            "คุณแจ้งปัญหา{} ไว้แล้ว{kha} ผู้ดูแลกำลังตรวจสอบ ไม่ต้องแจ้งซ้ำ{kha}",
            what.named()
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// A report on `what` was received (FR-HELP-02). The main menu follows.
    pub fn report_received(&self, what: &Reported<'_>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ได้รับเรื่องแล้ว ขอบคุณที่แจ้ง{kha} ผู้ดูแลจะตรวจสอบ{} โดยเร็ว{kha}",
            what.named()
        ))
    }

    /// Tells the admins a user reported `what` at `at`, for `reason` (Table 4.8-A). `review`
    /// opens the review queue. It doesn't say who reported it; the queue does.
    pub fn report_alert(
        &self,
        what: &Reported<'_>,
        reason: &str,
        at: DateTime<Utc>,
        review: Action,
    ) -> Message {
        let headline = format!("มีผู้แจ้งปัญหาใหม่{}", self.kha());
        let what = match what {
            Reported::Product { name, shop } => format!("สินค้า: {name} ({shop})"),
            Reported::Shop { name } => format!("ร้าน: {name}"),
        };
        let details = [
            what,
            format!("เหตุผล: {reason}"),
            format!("แจ้งเมื่อ {}", self.date_time(at)),
        ];
        notice(&headline, &details, vec![review])
    }

    /// Tells the admins moderation held `what` for review at `at`, for `reason` (FR-PROD-09,
    /// Table 4.8-A), or left a shop's application for them to approve: `buttons` open the
    /// review queue or the applications.
    pub fn held_alert(
        &self,
        what: &Reported<'_>,
        reason: &str,
        at: DateTime<Utc>,
        buttons: Vec<Action>,
    ) -> Message {
        let headline = format!("มีรายการรอตรวจใหม่{}", self.kha());
        let what = match what {
            Reported::Product { name, shop } => format!("สินค้า: {name} ({shop})"),
            Reported::Shop { name } => format!("ร้าน: {name}"),
        };
        let details = [
            what,
            format!("เหตุผล: {reason}"),
            format!("พักไว้เมื่อ {}", self.date_time(at)),
        ];
        notice(&headline, &details, buttons)
    }

    /// Why moderation held something, for the admins: `keyword` of `catalog.prohibitedKeywords`
    /// is in its text (FR-PROD-09).
    pub fn held_for_keyword(&self, keyword: &str) -> String {
        format!("มีคำต้องห้าม \"{keyword}\"")
    }

    /// Why moderation held something, for the admins: the AI's `reason` (FR-AI-12), which it
    /// may have left empty.
    pub fn held_by_ai(&self, reason: &str) -> String {
        match reason.trim() {
            "" => "AI ขอให้ตรวจ".to_owned(),
            reason => format!("AI: {reason}"),
        }
    }

    /// Asks for the shop's name, of at most `max_chars` (FR-SHOP-01): the first question of an
    /// application, which says how it goes, or a new name for `current`.
    pub fn ask_shop_name(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let intro = match current {
            None => format!("มาเปิดร้านกัน{kha} ระบบจะถามทีละข้อ ไม่กี่นาทีก็เสร็จ "),
            Some(_) => String::new(),
        };
        Message::text(format!(
            "{intro}ร้านชื่ออะไรดี{kha_question} พิมพ์มาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha} เช่น ร้านป้าแดง{}",
            now(current)
        ))
    }

    /// Asks for the shop's short description, of at most `max_chars` (FR-SHOP-01).
    pub fn ask_shop_description(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "บอกลูกค้าสั้นๆ ว่าร้านขายอะไร{kha_question} พิมพ์ได้ไม่เกิน {max_chars} ตัวอักษร{kha} เช่น ไข่ไก่ ผักสวนครัว ส่งทุกเช้า{}",
            now(current)
        ))
    }

    /// Asks for the shop's optional photo (FR-SHOP-01). `has_photo` when one was given, which
    /// can then be removed.
    pub fn ask_shop_photo(&self, has_photo: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let optional = if has_photo {
            format!(
                "ส่งรูปใหม่มาแทนได้เลย ถ้าไม่ใช้รูป กด \"{}\" ได้เลย{kha}",
                labels::REMOVE.as_str()
            )
        } else {
            format!(
                "ไม่บังคับ{kha} ถ้ายังไม่มี กด \"{}\" ได้เลย{kha}",
                labels::SKIP.as_str()
            )
        };
        Message::text(format!(
            "มีรูปหน้าร้านหรือสินค้าเด่นของร้านไหม{kha_question} ส่งมา 1 รูป ลูกค้าจะเห็นรูปนี้ที่ร้าน {optional}"
        ))
    }

    /// Asks where buyers pick their orders up, of at most `max_chars` (FR-SHOP-01). `home` when
    /// the user's address can be used as it is; `none`, if given, goes without one, a location
    /// being shared instead.
    pub fn ask_pickup_address(
        &self,
        max_chars: usize,
        home: bool,
        none: Option<&Label>,
        current: Option<&str>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let home = if home {
            format!(
                "ถ้าเป็นที่เดียวกับบ้าน กด \"{}\" หรือ",
                labels::USE_HOME_ADDRESS.as_str()
            )
        } else {
            String::new()
        };
        let none = none
            .map(|none| format!(" ถ้าจะแชร์ตำแหน่งร้านแทน กด \"{}\" ได้เลย{kha}", none.as_str()))
            .unwrap_or_default();
        Message::text(format!(
            "ลูกค้ามารับของที่ไหน{kha_question} {home}พิมพ์ที่อยู่ร้านมาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha}{none}{}",
            now(current)
        ))
    }

    /// Asks for the shop's location (FR-SHOP-01). `home` when the user's own can be used;
    /// `none`, if given, goes without, the shop having an address; `shared` when one is.
    pub fn ask_pickup_location(&self, home: bool, none: Option<&Label>, shared: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let home = if home {
            format!(
                " หรือกด \"{}\" ถ้าร้านอยู่ที่บ้าน",
                labels::USE_HOME_LOCATION.as_str()
            )
        } else {
            String::new()
        };
        let none = match none {
            Some(none) => format!(" ถ้าไม่สะดวก กด \"{}\" ได้เลย{kha}", none.as_str()),
            None => format!(" ต้องแชร์ไว้{kha} เพราะร้านยังไม่มีที่อยู่"),
        };
        let current = if shared {
            now(Some("แชร์ไว้แล้ว"))
        } else {
            String::new()
        };
        Message::text(format!(
            "แชร์ตำแหน่งร้านไว้ให้ลูกค้าหาเจอง่ายขึ้นไหม{kha_question} กด \"{}\" แล้วเลือกตำแหน่งร้าน{home}{none}{current}",
            labels::SHARE_LOCATION.as_str()
        ))
    }

    /// Asks for the shop's contact phone, which is required (FR-SHOP-01). `own` when the
    /// user's phone is offered.
    pub fn ask_shop_phone(&self, own: bool, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let own = if own {
            "ถ้าใช้เบอร์เดียวกับของคุณ กดเบอร์ด้านล่าง หรือ"
        } else {
            ""
        };
        Message::text(format!(
            "ลูกค้าติดต่อร้านได้ที่เบอร์อะไร{kha_question} {own}พิมพ์มาได้เลย{kha} เช่น 0812345678{}",
            now(current)
        ))
    }

    /// Not a valid Thai phone number, where the shop's is required (FR-SHOP-01).
    pub fn shop_phone_invalid(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} เบอร์โทรนี้ไม่ถูกต้อง พิมพ์เบอร์มือถือ 10 หลักที่ขึ้นต้นด้วย 06 08 หรือ 09 เช่น 0812345678 หรือเบอร์บ้าน 9 หลัก เช่น 021234567 ได้เลย{kha}"
        ))
    }

    /// Asks how buyers get their orders: picked up, delivered or either (FR-SHOP-01).
    pub fn ask_fulfillment(&self, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ลูกค้ารับของได้แบบไหนบ้าง{kha_question} เลือกจากปุ่มด้านล่างได้เลย{kha}{}",
            now(current)
        ))
    }

    /// Asks for the delivery fee, of at most `max` (FR-SHOP-01).
    pub fn ask_delivery_fee(&self, max: Money, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ค่าส่งครั้งละเท่าไร{kha_question} พิมพ์เป็นตัวเลขได้เลย เช่น 10 ไม่เกิน {max} ถ้าส่งฟรี กด \"{}\" ได้เลย{kha}{}",
            labels::NO_DELIVERY_FEE.as_str(),
            now(current)
        ))
    }

    /// Asks for the optional minimum order, of at most `max` (FR-SHOP-01).
    pub fn ask_minimum_order(&self, max: Money, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ต้องสั่งขั้นต่ำเท่าไรต่อออเดอร์{kha_question} พิมพ์เป็นตัวเลขได้เลย เช่น 50 ไม่เกิน {max} ถ้าไม่มีขั้นต่ำ กด \"{}\" ได้เลย{kha}{}",
            labels::NO_MINIMUM.as_str(),
            now(current)
        ))
    }

    /// An amount over `max`, the most the step takes.
    pub fn amount_too_large(&self, max: Money) -> Message {
        let kha = self.kha();
        Message::text(format!("ขอโทษ{kha} ตรงนี้ใส่ได้ไม่เกิน {max} พิมพ์ใหม่ได้เลย{kha}"))
    }

    /// Asks for the optional PromptPay ID buyers transfer to (FR-SHOP-01). `phone` when the
    /// shop's phone is offered; `none` goes without, or removes the one given.
    pub fn ask_promptpay(&self, phone: bool, none: &Label, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let phone = if phone {
            " หรือกดเบอร์ร้านด้านล่าง"
        } else {
            ""
        };
        Message::text(format!(
            "มีพร้อมเพย์ให้ลูกค้าโอนเงินไหม{kha_question} พิมพ์เบอร์มือถือหรือเลขบัตรประชาชน 13 หลักที่ผูกพร้อมเพย์ไว้{phone} ไม่บังคับ{kha} ถ้าไม่มี กด \"{}\" ได้เลย{kha}{}",
            none.as_str(),
            now(current)
        ))
    }

    /// Not a PromptPay ID: the forms taken. `none` goes without one.
    pub fn promptpay_invalid(&self, none: &Label) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} เลขพร้อมเพย์นี้ไม่ถูกต้อง พิมพ์เบอร์มือถือ 10 หลัก เช่น 0812345678 หรือเลขบัตรประชาชน 13 หลักได้เลย ถ้าไม่มี กด \"{}\" ได้เลย{kha}",
            none.as_str()
        ))
    }

    /// The shop would have neither a pickup address nor a location (FR-SHOP-01).
    pub fn pickup_needed(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ร้านต้องมีที่อยู่หรือตำแหน่งอย่างน้อยหนึ่งอย่าง ลูกค้าจะได้รู้ว่ามารับของที่ไหน{kha}"
        ))
    }

    /// Text where the shop's photo was asked for. `none` goes without one.
    pub fn photo_expected(&self, none: &Label) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ตรงนี้ต้องส่งเป็นรูป{kha} กด \"{}\" หรือ \"{}\" ได้เลย ถ้าไม่ใช้รูป กด \"{}\" ได้เลย{kha}",
            labels::TAKE_PHOTO.as_str(),
            labels::CHOOSE_PHOTO.as_str(),
            none.as_str()
        ))
    }

    /// Text where the shop's location was asked for. `none`, if given, goes without one.
    pub fn shop_location_expected(&self, none: Option<&Label>) -> Message {
        let kha = self.kha();
        let none = none
            .map(|none| format!(" หรือกด \"{}\"", none.as_str()))
            .unwrap_or_default();
        Message::text(format!(
            "ตรงนี้ต้องแชร์ตำแหน่ง{kha} กด \"{}\" แล้วเลือกตำแหน่งร้าน{none} ได้เลย{kha}",
            labels::SHARE_LOCATION.as_str()
        ))
    }

    /// The photo sent earlier in the application is gone, so it is asked for again.
    pub fn photo_lost(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} รูปร้านที่ส่งไว้หายไปแล้ว ส่งรูปมาใหม่อีกครั้งได้เลย{kha}"
        ))
    }

    /// The application before it is sent (FR-SHOP-01): `card`, whose buttons send it or
    /// correct a part. `approval` when an admin approves it first (FR-SHOP-03); `submit` says
    /// what sends it.
    pub fn shop_summary(
        &self,
        card: &ShopCard<'_>,
        approval: bool,
        submit: &Label,
    ) -> Vec<Message> {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let then = if approval {
            " แล้วผู้ดูแลจะตรวจก่อนเปิดร้าน"
        } else {
            " แล้วร้านจะเปิดทันที"
        };
        let text = format!(
            "ตรวจข้อมูลร้านอีกครั้งนะ{kha_question} ถ้าถูกต้อง กด \"{}\"{then} ถ้าต้องการแก้ กด \"{}\" ได้เลย{kha}",
            submit.as_str(),
            labels::EDIT.as_str()
        );
        let bubble = self.shop_bubble(card, None);
        vec![
            Message::text(text),
            Message::flex(format!("สรุปข้อมูลร้าน: {}", card.name), bubble),
        ]
    }

    /// Asks which part of a summary to correct, of a shop application or a product being added;
    /// `parts` are its buttons.
    pub fn choose_part(&self, parts: Vec<Action>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!("แก้ข้อไหนดี{kha_question} เลือกได้เลย{kha}"))
            .with_quick_reply(QuickReply::new(parts))
    }

    /// The application for shop `name` was sent, for an admin to approve (FR-SHOP-03). The
    /// main menu follows.
    pub fn application_sent(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ส่งคำขอเปิด{}แล้ว{kha} ผู้ดูแลจะตรวจแล้วแจ้งผลในแชตนี้ ระหว่างนี้เลือกเมนูอื่นได้เลย{kha}",
            shop_called(name)
        ))
    }

    /// Shop `name` is open at once, no approval being needed (FR-SHOP-03). The main menu, now
    /// with the seller's, follows.
    pub fn shop_opened(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "เปิด{}แล้ว{kha} เริ่มลงสินค้าได้เลย กด \"{}\" ในเมนูผู้ขายได้เลย{kha}",
            shop_called(name),
            labels::ADD_PRODUCT.as_str()
        ))
    }

    /// "ร้านของฉัน" while the application for shop `name`, sent at `at`, awaits an admin
    /// (FR-SHOP-03). A user owns one shop (FR-SHOP-02), so they can't apply again meanwhile.
    pub fn application_pending(&self, name: &str, at: DateTime<Utc>) -> Message {
        let kha = self.kha();
        let text = format!(
            "คำขอเปิด{} ส่งเมื่อ {} กำลังรอผู้ดูแลอนุมัติ{kha} ระบบจะแจ้งผลในแชตนี้ หนึ่งคนเปิดได้ 1 ร้าน จึงยังส่งคำขอใหม่ไม่ได้{kha}",
            shop_called(name),
            self.date_time(at)
        );
        Message::text(text).with_quick_reply(menus())
    }

    /// "ร้านของฉัน" once the application for shop `name` was turned down, for `reason`
    /// (FR-SHOP-04): `again` applies again.
    pub fn application_rejected(&self, name: &str, reason: Option<&str>, again: Action) -> Message {
        let kha = self.kha();
        let reason = reason
            .map(|reason| format!(" เหตุผล: {reason}"))
            .unwrap_or_default();
        let text = format!(
            "คำขอเปิด{}ยังไม่ผ่านการอนุมัติ{kha}{reason} แก้ไขแล้วส่งใหม่ได้ กด \"{}\" ได้เลย{kha}",
            shop_called(name),
            again.label().as_str()
        );
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([again, menu]))
    }

    /// "ร้านของฉัน" for the owner of approved shop `name`, `open` or closed for now
    /// (FR-SHOP-05): `actions` manage it. Its card comes first.
    pub fn my_shop(&self, name: &str, open: bool, actions: Vec<Action>) -> Message {
        let kha = self.kha();
        let now = if open {
            "ตอนนี้ร้านเปิดอยู่"
        } else {
            "ตอนนี้ร้านปิดอยู่ ผู้ซื้อจึงยังสั่งไม่ได้"
        };
        Message::text(format!(
            "{}ของคุณได้รับอนุมัติแล้ว{kha} {now} ดูแลร้านได้จากเมนูผู้ขาย เลือกด้านล่างได้เลย{kha}",
            shop_called(name)
        ))
        .with_quick_reply(QuickReply::new(actions))
    }

    /// "ร้านของฉัน" for the owner of shop `name`, held for review (FR-PROD-09): `actions` manage
    /// it, as they do an approved one. Its card comes first.
    pub fn my_shop_held(&self, name: &str, actions: Vec<Action>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "{}กำลังรอผู้ดูแลตรวจ{kha} ระหว่างนี้ผู้ซื้อจะยังไม่เห็นร้านและสินค้าของร้าน แต่ยังดูแลร้านได้จากเมนูผู้ขาย เลือกด้านล่างได้เลย{kha}",
            shop_called(name)
        ))
        .with_quick_reply(QuickReply::new(actions))
    }

    /// Shop `name` is now `open` or closed (FR-SHOP-05); `changed` unless it already was.
    /// `switch` turns it back. Closed, its products stay visible but can't be ordered, and its
    /// orders go on.
    pub fn shop_open_state(
        &self,
        name: &str,
        open: bool,
        changed: bool,
        switch: Action,
    ) -> Message {
        let kha = self.kha();
        let shop = shop_called(name);
        let done = match (open, changed) {
            (true, true) => format!("เปิด{shop}แล้ว{kha}"),
            (true, false) => format!("{shop}เปิดอยู่แล้ว{kha}"),
            (false, true) => format!("ปิด{shop}แล้ว{kha}"),
            (false, false) => format!("{shop}ปิดอยู่แล้ว{kha}"),
        };
        let then = if open {
            " ผู้ซื้อสั่งสินค้าได้ตามปกติ".to_owned()
        } else {
            format!(" ผู้ซื้อยังเห็นสินค้า แต่จะขึ้นว่า \"{CLOSED}\" และสั่งไม่ได้ ออเดอร์ที่มีอยู่ยังทำต่อตามปกติ")
        };
        let text = format!(
            "{done}{then} ถ้าจะเปลี่ยน กด \"{}\" ได้เลย{kha}",
            switch.label().as_str()
        );
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([switch, menu]))
    }

    /// "ตั้งค่าร้าน" (FR-SHOP-06): asks what to change; `parts` are its buttons. The shop's card
    /// comes first.
    pub fn shop_settings(&self, parts: Vec<Action>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ต้องการแก้ส่วนไหนของร้าน{kha_question} เลือกด้านล่างได้เลย แก้แล้วบันทึกทันที{kha}"
        ))
        .with_quick_reply(QuickReply::new(parts))
    }

    /// A change in "ตั้งค่าร้าน" is saved. The shop's card follows.
    pub fn shop_settings_saved(&self) -> Message {
        Message::text(format!("บันทึกแล้ว{}", self.kha()))
    }

    /// A change in "ตั้งค่าร้าน" is saved, and the shop held for review (FR-SHOP-06,
    /// FR-PROD-09). Its owner isn't told why; the admins are. The shop's card follows.
    pub fn shop_held_saved(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "บันทึกแล้ว{kha} ขอให้ผู้ดูแลตรวจร้านก่อน ระหว่างนี้ผู้ซื้อจะยังไม่เห็นร้านและสินค้าของร้าน แต่ยังดูแลร้านได้ตามปกติ{kha}"
        ))
    }

    /// A part of "ตั้งค่าร้าน" is left as it was. The shop's card follows.
    pub fn shop_settings_unchanged(&self) -> Message {
        Message::text(format!("ยังใช้ข้อมูลเดิมอยู่{}", self.kha()))
    }

    /// Asks for the opening hours shown on the shop's card, of at most `max_chars`
    /// (FR-SHOP-07). `none` goes without, or removes those given.
    pub fn ask_opening_hours(
        &self,
        max_chars: usize,
        none: &Label,
        current: Option<&str>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ร้านเปิดช่วงไหน{kha_question} พิมพ์มาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha} เช่น ทุกวัน 06:00–10:00 ลูกค้าจะเห็นที่หน้าร้าน ถ้าไม่ระบุ กด \"{}\" ได้เลย{kha}{}",
            none.as_str(),
            now(current)
        ))
    }

    /// The shop's phone is now shown to buyers with an active order, or hidden from them
    /// (FR-SHOP-08). The shop's card follows.
    pub fn phone_visibility(&self, show: bool) -> Message {
        let kha = self.kha();
        Message::text(if show {
            format!("แสดงเบอร์ร้านให้ผู้ซื้อแล้ว{kha} ผู้ซื้อที่มีออเดอร์กับร้านจะเห็นเบอร์นี้{kha}")
        } else {
            format!("ซ่อนเบอร์ร้านจากผู้ซื้อแล้ว{kha} ผู้ซื้อจะไม่เห็นเบอร์ร้านในออเดอร์{kha}")
        })
    }

    /// The owner's shop as buyers see it (FR-SHOP-09): `front`, with `settings` and the menu
    /// to carry on.
    pub fn shop_front_preview(&self, front: &ShopFront<'_>, settings: Action) -> Vec<Message> {
        let kha = self.kha();
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        vec![
            Message::text(format!("ผู้ซื้อจะเห็นร้านของคุณแบบนี้{kha}")),
            self.shop_front(front)
                .with_quick_reply(QuickReply::new([settings, menu])),
        ]
    }

    /// A shop's card as buyers see it (FR-SHOP-09).
    pub fn shop_front(&self, front: &ShopFront<'_>) -> Message {
        Message::flex(front.name, self.shop_front_bubble(front))
    }

    /// A button of an application that is no longer in progress: cancelled, expired or sent.
    /// `open_shop` starts again.
    pub fn application_gone(&self, open_shop: Action) -> Message {
        let kha = self.kha();
        let text = format!(
            "คำขอนี้ไม่ได้ทำต่อแล้ว{kha} อาจยกเลิกหรือหมดเวลาไป ถ้ายังอยากเปิดร้าน กด \"{}\" เพื่อเริ่มใหม่ได้เลย{kha}",
            open_shop.label().as_str()
        );
        Message::text(text).with_quick_reply(QuickReply::new([open_shop]))
    }

    /// The owner's card of their shop: `card`, without buttons.
    pub fn shop(&self, card: &ShopCard<'_>) -> Message {
        Message::flex(card.name, self.shop_bubble(card, None))
    }

    /// "อนุมัติร้าน" (FR-ADM-02): page `page` of the applications awaiting an admin, oldest
    /// first, a card each with its buttons, and "ดูเพิ่มเติม" sending `more(next_page)`.
    pub fn applications(
        &self,
        cards: &[ShopCard<'_>],
        page: usize,
        more: impl FnOnce(usize) -> String,
    ) -> Message {
        if cards.is_empty() {
            let kha = self.kha();
            return Message::text(format!(
                "ตอนนี้ไม่มีคำขอเปิดร้านที่รออนุมัติ{kha} ถ้ามีคำขอใหม่ จะแสดงที่นี่{kha}"
            ))
            .with_quick_reply(admin_menus());
        }
        let card = |card: &ShopCard<'_>| self.shop_bubble(card, None);
        let page = carousel_page(cards, page, APPLICATIONS_PER_PAGE, card, more);
        Message::flex(labels::APPROVE_SHOPS.as_str(), page).with_quick_reply(admin_menus())
    }

    /// Tells the admins of a new application (Table 4.8-A), as in Appendix A.5: `card`, with
    /// the buttons that approve or reject it.
    pub fn shop_application_alert(&self, card: &ShopCard<'_>) -> Message {
        let headline = format!("มีคำขอเปิดร้านใหม่{}", self.kha());
        let bubble = self.shop_bubble(card, Some(&headline));
        Message::flex(headline, bubble)
    }

    /// Shop `name` is approved (FR-ADM-02), as in Appendix A.5.
    pub fn shop_approved(&self, name: &str) -> Message {
        Message::text(format!("อนุมัติ{}แล้ว{}", shop_called(name), self.kha()))
            .with_quick_reply(admin_menus())
    }

    /// The application for shop `name` was decided already, by an earlier tap or another
    /// admin: `approved` or rejected (FR-UI-13).
    pub fn application_decided(&self, name: &str, approved: bool) -> Message {
        let kha = self.kha();
        let decided = if approved {
            "ได้รับอนุมัติไปแล้ว"
        } else {
            "ไม่ได้รับอนุมัติไปแล้ว"
        };
        Message::text(format!(
            "คำขอเปิด{}{decided}{kha} ไม่ต้องทำอะไรเพิ่ม{kha}",
            shop_called(name)
        ))
        .with_quick_reply(admin_menus())
    }

    /// The application changed since its card was sent: the owner applied again. Its current
    /// card follows (FR-UI-13).
    pub fn application_changed(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "คำขอนี้มีการแก้ไขใหม่{kha} ตรวจข้อมูลล่าสุดด้านล่างอีกครั้งนะ{kha_question}"
        ))
    }

    /// Asks why shop `name` is turned down, in at most `max_chars` (FR-ADM-02). `reasons` are
    /// common ones, sent as if typed.
    pub fn ask_rejection_reason(
        &self,
        name: &str,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ไม่อนุมัติ{}เพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร เจ้าของร้านจะเห็นเหตุผลนี้{kha}",
            shop_called(name)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// Shop `name` is turned down, and its owner told why (FR-ADM-02, FR-SHOP-04).
    pub fn shop_rejected(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ไม่อนุมัติ{}แล้ว{kha} แจ้งเหตุผลถึงเจ้าของร้านแล้ว{kha}",
            shop_called(name)
        ))
        .with_quick_reply(admin_menus())
    }

    /// Tells the owner their shop `name` is approved (FR-SHOP-04), as in Appendix A.5:
    /// `add_product` starts selling.
    pub fn shop_approved_notice(&self, name: &str, add_product: Action) -> Message {
        let kha = self.kha();
        let headline = format!("ร้านของคุณได้รับอนุมัติแล้ว{kha}");
        let details = [format!("{} เริ่มลงสินค้าได้เลย{kha}", shop_called(name))];
        notice(&headline, &details, vec![add_product])
    }

    /// Tells the owner the application for their shop `name` was turned down, for `reason`
    /// (FR-SHOP-04): `again` applies again.
    pub fn shop_rejected_notice(&self, name: &str, reason: &str, again: Action) -> Message {
        let kha = self.kha();
        let headline = format!("คำขอเปิดร้านยังไม่ผ่านการอนุมัติ{kha}");
        let details = [
            shop_called(name),
            format!("เหตุผล: {reason}"),
            format!("แก้ไขแล้วส่งคำขอใหม่ได้{kha}"),
        ];
        notice(&headline, &details, vec![again])
    }

    /// "รายการรอตรวจ" (FR-ADM-03): page `page` of the products and shops held for review or
    /// reported, the one waiting longest first, a card each with its buttons, and "ดูเพิ่มเติม"
    /// sending `more(next_page)`.
    pub fn review_queue(
        &self,
        cards: &[ReviewCard<'_>],
        page: usize,
        more: impl FnOnce(usize) -> String,
    ) -> Message {
        if cards.is_empty() {
            let kha = self.kha();
            return Message::text(format!(
                "ตอนนี้ไม่มีรายการรอตรวจ{kha} ถ้ามีสินค้าหรือร้านถูกพัก หรือมีผู้แจ้งปัญหา จะแสดงที่นี่{kha}"
            ))
            .with_quick_reply(review_menus());
        }
        let card = |card: &ReviewCard<'_>| self.review_bubble(card);
        let page = carousel_page(cards, page, REVIEWS_PER_PAGE, card, more);
        Message::flex(labels::REVIEW_QUEUE.as_str(), page).with_quick_reply(review_menus())
    }

    /// One card of "รายการรอตรวจ": `card`, as it is now.
    pub fn review_card(&self, card: &ReviewCard<'_>) -> Message {
        Message::flex(labels::REVIEW_QUEUE.as_str(), self.review_bubble(card))
    }

    /// What a card of "รายการรอตรวจ" shows changed since it was sent: its product or shop was
    /// edited, or its state changed. Its current card follows (FR-UI-13).
    pub fn review_changed(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "รายการนี้มีการเปลี่ยนแปลง{kha} ตรวจข้อมูลล่าสุดด้านล่างอีกครั้งนะ{kha_question}"
        ))
    }

    /// What a card of "รายการรอตรวจ" shows was dealt with already, by an earlier tap or another
    /// admin (FR-UI-13). The queue as it is now follows.
    pub fn review_handled(&self) -> Message {
        let kha = self.kha();
        Message::text(format!("รายการนี้มีผู้ดูแลจัดการไปแล้ว{kha} ไม่ต้องทำอะไรเพิ่ม{kha}"))
    }

    /// Asks why product `name` of shop `shop` is removed, in at most `max_chars` (FR-ADM-03).
    /// `reasons` are common ones, sent as if typed.
    pub fn ask_removal_reason(
        &self,
        name: &str,
        shop: &str,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "นำสินค้า \"{name}\" ของ{}ออกเพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร ผู้ขายจะเห็นเหตุผลนี้ และผู้ซื้อจะไม่เห็นสินค้านี้อีก{kha}",
            shop_called(shop)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// Asks why shop `name` is suspended, in at most `max_chars` (FR-ADM-03). `reasons` are
    /// common ones, sent as if typed.
    pub fn ask_shop_suspension_reason(
        &self,
        name: &str,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ระงับ{}เพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร เจ้าของร้านจะเห็นเหตุผลนี้ ระหว่างระงับ ผู้ซื้อจะไม่เห็นร้านและสินค้าของร้าน{kha}",
            shop_called(name)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// `what`, held for review, is approved (FR-ADM-03): a product listed, or a shop shown to
    /// buyers again.
    pub fn review_approved(&self, what: &Reported<'_>) -> Message {
        let kha = self.kha();
        let text = match what {
            Reported::Product { name, .. } => format!("อนุมัติสินค้า \"{name}\" แล้ว{kha}"),
            Reported::Shop { name } => {
                format!("อนุมัติ{}แล้ว{kha} ผู้ซื้อเห็นร้านอีกครั้งแล้ว{kha}", shop_called(name))
            }
        };
        Message::text(text).with_quick_reply(review_menus())
    }

    /// Product `name` is removed, and its seller told why (FR-ADM-03).
    pub fn product_removed(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "นำสินค้า \"{name}\" ออกแล้ว{kha} แจ้งเหตุผลถึงผู้ขายแล้ว{kha}"
        ))
        .with_quick_reply(review_menus())
    }

    /// Shop `name` is suspended, and its owner told why (FR-ADM-03).
    pub fn shop_suspended(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ระงับ{}แล้ว{kha} ผู้ซื้อจะไม่เห็นร้านและสินค้าของร้าน และแจ้งเหตุผลถึงเจ้าของร้านแล้ว{kha}",
            shop_called(name)
        ))
        .with_quick_reply(review_menus())
    }

    /// The reports on `what` are closed, and it is left as it is (FR-ADM-03).
    pub fn reports_dismissed(&self, what: &Reported<'_>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ปิดเรื่อง{} แล้ว{kha} ไม่ได้เปลี่ยนแปลงอะไร{kha}",
            what.named()
        ))
        .with_quick_reply(review_menus())
    }

    /// Tells the seller their product `name` was removed, for `reason` (FR-ADM-03, Table 4.8-A):
    /// `my_products` shows their products.
    pub fn product_removed_notice(&self, name: &str, reason: &str, my_products: Action) -> Message {
        let kha = self.kha();
        let headline = format!("สินค้าของคุณถูกนำออก{kha}");
        let details = [
            format!("สินค้า \"{name}\""),
            format!("เหตุผล: {reason}"),
            format!(
                "ผู้ซื้อจะไม่เห็นสินค้านี้อีก สอบถามได้ที่ \"{}\" แล้วเลือก \"{}\"{kha}",
                labels::HELP.as_str(),
                labels::CONTACT_ADMINS.as_str()
            ),
        ];
        notice(&headline, &details, vec![my_products])
    }

    /// Tells the owner their shop `name` was suspended, for `reason` (FR-ADM-03, Table 4.8-A).
    pub fn shop_suspended_notice(&self, name: &str, reason: &str) -> Message {
        let kha = self.kha();
        let headline = format!("ร้านของคุณถูกระงับชั่วคราว{kha}");
        let details = [
            shop_called(name),
            format!("เหตุผล: {reason}"),
            format!(
                "ระหว่างนี้ผู้ซื้อจะไม่เห็นร้านและสินค้าของร้าน แต่ยังดูแลออเดอร์ที่มีอยู่ได้ สอบถามได้ที่ \"{}\" แล้วเลือก \"{}\"{kha}",
                labels::HELP.as_str(),
                labels::CONTACT_ADMINS.as_str()
            ),
        ];
        let help = Action::message(labels::HELP.clone(), labels::HELP.as_str());
        notice(&headline, &details, vec![help])
    }

    /// Tells the owner the suspension of their shop `name` was lifted (FR-ADM-04): `my_shop`
    /// shows it.
    pub fn shop_unsuspended_notice(&self, name: &str, my_shop: Action) -> Message {
        let kha = self.kha();
        let headline = format!("ร้านของคุณกลับมาขายได้แล้ว{kha}");
        let details = [format!(
            "ผู้ดูแลยกเลิกการระงับ{}แล้ว ผู้ซื้อเห็นร้านอีกครั้ง และดูแลร้านได้ตามปกติ{kha}",
            shop_called(name)
        )];
        notice(&headline, &details, vec![my_shop])
    }

    /// Tells the user their account was suspended, for `reason` (FR-ADM-04, Table 4.8-A).
    pub fn account_suspended_notice(&self, reason: &str) -> Message {
        let kha = self.kha();
        let headline = format!("บัญชีของคุณถูกระงับการใช้งานชั่วคราว{kha}");
        let details = [
            format!("เหตุผล: {reason}"),
            format!(
                "ระหว่างนี้สั่งซื้อ ขายของ และใช้ผู้ช่วยตอบแชตไม่ได้ แต่ยังดูคำสั่งซื้อของคุณและเมนูช่วยเหลือได้ตามปกติ สอบถามได้ที่ \"{}\" แล้วเลือก \"{}\"{kha}",
                labels::HELP.as_str(),
                labels::CONTACT_ADMINS.as_str()
            ),
        ];
        let help = Action::message(labels::HELP.clone(), labels::HELP.as_str());
        notice(&headline, &details, vec![help])
    }

    /// Tells the user the suspension of their account was lifted (FR-ADM-04).
    pub fn account_unsuspended_notice(&self) -> Message {
        let kha = self.kha();
        let headline = format!("บัญชีของคุณใช้งานได้ตามปกติแล้ว{kha}");
        let details = [format!("ผู้ดูแลยกเลิกการระงับแล้ว สั่งซื้อและขายของได้ตามปกติ{kha}")];
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        notice(&headline, &details, vec![menu])
    }

    /// Asks who to find in "ผู้ใช้" (FR-ADM-04): by display name, phone or order number.
    pub fn ask_user_query(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ค้นหาผู้ใช้จากอะไรดี{kha_question} พิมพ์ชื่อ เบอร์โทร หรือเลขที่ออเดอร์มาได้เลย{kha}"
        ))
    }

    /// No user matches `query`; the step asks again.
    pub fn no_user_found(&self, query: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ไม่พบผู้ใช้จาก \"{query}\"{kha} ลองพิมพ์ชื่อ เบอร์โทร หรือเลขที่ออเดอร์อีกครั้ง{kha}"
        ))
    }

    /// The users found in "ผู้ใช้" (FR-ADM-04), a card each with its buttons: at most
    /// [`USERS_FOUND`], after a note when `more` matched than that.
    pub fn users_found(&self, cards: &[UserCard<'_>], more: bool) -> Vec<Message> {
        let mut messages = Vec::new();
        if more {
            let kha = self.kha();
            messages.push(Message::text(format!(
                "พบมากกว่า {USERS_FOUND} คน{kha} แสดง {USERS_FOUND} คนแรก ถ้าไม่พบคนที่ต้องการ พิมพ์ให้เจาะจงขึ้นแล้วค้นหาอีกครั้ง{kha}"
            )));
        }
        let bubbles = cards
            .iter()
            .take(USERS_FOUND)
            .map(|card| self.user_bubble(card))
            .collect();
        messages.push(
            Message::flex(labels::USERS.as_str(), carousel(bubbles, None))
                .with_quick_reply(users_menus()),
        );
        messages
    }

    /// One user's card in "ผู้ใช้": `card`, as it is now.
    pub fn user_card(&self, card: &UserCard<'_>) -> Message {
        Message::flex(labels::USERS.as_str(), self.user_bubble(card))
            .with_quick_reply(users_menus())
    }

    /// Asks why the user called `name` is suspended, in at most `max_chars` (FR-ADM-04).
    /// `reasons` are common ones, sent as if typed.
    pub fn ask_suspension_reason(
        &self,
        name: Option<&str>,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ระงับบัญชีของ{}เพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร ผู้ใช้จะเห็นเหตุผลนี้ ระหว่างระงับ จะสั่งซื้อ ขายของ และใช้ผู้ช่วยตอบแชตไม่ได้ และผู้ซื้อจะไม่เห็นร้านของเขา{kha}",
            user_called(name)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// Asks why the suspension of the user called `name` is lifted, in at most `max_chars`
    /// (FR-ADM-04). `reasons` are common ones, sent as if typed.
    pub fn ask_unsuspension_reason(
        &self,
        name: Option<&str>,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ยกเลิกการระงับบัญชีของ{}เพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha}",
            user_called(name)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// Asks why the suspension of shop `name` is lifted, in at most `max_chars` (FR-ADM-04).
    /// `reasons` are common ones, sent as if typed.
    pub fn ask_shop_unsuspension_reason(
        &self,
        name: &str,
        max_chars: usize,
        reasons: Vec<Action>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ยกเลิกการระงับ{}เพราะอะไร{kha_question} เลือกด้านล่าง หรือพิมพ์เหตุผลมาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha}",
            shop_called(name)
        );
        Message::text(text).with_quick_reply(QuickReply::new(reasons))
    }

    /// The user called `name` is suspended, and told why (FR-ADM-04).
    pub fn user_suspended(&self, name: Option<&str>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ระงับบัญชีของ{}แล้ว{kha} แจ้งเหตุผลถึงผู้ใช้แล้ว{kha}",
            user_called(name)
        ))
        .with_quick_reply(users_menus())
    }

    /// The suspension of the user called `name` is lifted, and they are told (FR-ADM-04).
    pub fn user_unsuspended(&self, name: Option<&str>) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ยกเลิกการระงับบัญชีของ{}แล้ว{kha} แจ้งผู้ใช้แล้ว{kha}",
            user_called(name)
        ))
        .with_quick_reply(users_menus())
    }

    /// The suspension of shop `name` is lifted, and its owner told (FR-ADM-04).
    pub fn shop_unsuspended(&self, name: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ยกเลิกการระงับ{}แล้ว{kha} ผู้ซื้อเห็นร้านอีกครั้ง และแจ้งเจ้าของร้านแล้ว{kha}",
            shop_called(name)
        ))
        .with_quick_reply(users_menus())
    }

    /// The account of the user called `name` is `suspended` already, or not, by an earlier tap
    /// or another admin (FR-UI-13). Their current card follows.
    pub fn account_already(&self, name: Option<&str>, suspended: bool) -> Message {
        let kha = self.kha();
        let state = if suspended {
            "ถูกระงับอยู่แล้ว"
        } else {
            "ใช้งานได้ตามปกติอยู่แล้ว"
        };
        Message::text(format!(
            "บัญชีของ{}{state}{kha} ไม่ต้องทำอะไรเพิ่ม{kha}",
            user_called(name)
        ))
    }

    /// The user's shop isn't suspended, or no longer (FR-UI-13). Their current card follows.
    pub fn shop_not_suspended(&self) -> Message {
        let kha = self.kha();
        Message::text(format!("ร้านนี้ไม่ได้ถูกระงับแล้ว{kha} ไม่ต้องทำอะไรเพิ่ม{kha}"))
    }

    /// An admin's account can't be suspended: the admin role comes from the configuration
    /// (FR-ADM-01). Their card follows.
    pub fn admin_not_suspended(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ระงับบัญชีผู้ดูแลไม่ได้{kha} ถ้าจำเป็น ให้ผู้ดูแลระบบนำออกจากรายชื่อผู้ดูแลก่อน{kha}"
        ))
    }

    /// Asks for a new product's photos, `count` of at most `max` given (FR-PROD-01): the first
    /// question of "เพิ่มสินค้า", which says how it goes, or, `editing`, a change to them.
    pub fn ask_product_photos(&self, count: usize, max: usize, editing: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let next = labels::NEXT.as_str();
        let text = if count == 0 {
            let intro = if editing {
                String::new()
            } else {
                format!("มาลงสินค้ากัน{kha} ระบบจะถามทีละข้อ ไม่กี่นาทีก็เสร็จ ")
            };
            format!(
                "{intro}มีรูปสินค้าไหม{kha_question} ส่งมาได้ไม่เกิน {max} รูป ลูกค้าจะเห็นรูปเหล่านี้ที่สินค้า ไม่บังคับ{kha} ถ้ายังไม่มี กด \"{}\" ได้เลย{kha}",
                labels::SKIP.as_str()
            )
        } else {
            let more = if count < max {
                format!(
                    "ส่งเพิ่มได้อีก {} รูป ถ้าครบแล้ว กด \"{next}\" ได้เลย{kha}",
                    max - count
                )
            } else {
                format!("ครบแล้ว กด \"{next}\" ได้เลย{kha}")
            };
            let remove = if editing {
                format!(
                    " ถ้าจะเปลี่ยนรูปใหม่ทั้งหมด กด \"{}\" ได้เลย{kha}",
                    labels::REMOVE.as_str()
                )
            } else {
                String::new()
            };
            format!("ตอนนี้มีรูปสินค้า {count} รูป{kha} {more}{remove}")
        };
        Message::text(text)
    }

    /// A seller's photo that nothing asked for (FR-UI-14): an offer to list it as a new product,
    /// which `left` more photos can join.
    pub fn listing_offered(&self, left: usize) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let more = if left > 0 {
            format!(
                " ส่งรูปเพิ่มได้อีก {left} รูป หรือกด \"{}\" เพื่อไปต่อได้เลย{kha}",
                labels::NEXT.as_str()
            )
        } else {
            String::new()
        };
        Message::text(format!(
            "ได้รับรูปแล้ว{kha} จะลงขายสินค้านี้ใช่ไหม{kha_question} ระบบจะถามทีละข้อ{kha}{more} ถ้าไม่ได้จะขาย กด \"{}\" ได้เลย{kha}",
            labels::CANCEL.as_str()
        ))
    }

    /// Asks for a new product in one message while the AI drafts listings (FR-PROD-02), at the
    /// photos step: `count` of at most `max` photos are given. "ข้าม" or "ถัดไป" goes step by
    /// step instead.
    pub fn ask_listing_message(&self, count: usize, max: usize) -> Message {
        let kha = self.kha();
        let ask = format!(
            "พิมพ์ชื่อ ราคา และจำนวนมาในข้อความเดียวได้เลย{kha} เช่น \"{LISTING_EXAMPLE}\" ผู้ช่วยจะร่างให้ตรวจก่อนลงขาย"
        );
        let text = if count == 0 {
            format!(
                "มาลงสินค้ากัน{kha} {ask} ส่งรูปสินค้ามาด้วยก็ได้ ไม่เกิน {max} รูป ถ้าจะตอบทีละข้อ กด \"{}\" ได้เลย{kha}",
                labels::SKIP.as_str()
            )
        } else {
            let more = if count < max {
                format!(" ส่งรูปเพิ่มได้อีก {} รูป", max - count)
            } else {
                String::new()
            };
            format!(
                "ตอนนี้มีรูปสินค้า {count} รูป{kha} {ask}{more} ถ้าจะตอบทีละข้อ กด \"{}\" ได้เลย{kha}",
                labels::NEXT.as_str()
            )
        };
        Message::text(text)
    }

    /// A seller's photo that nothing asked for (FR-UI-14), while the AI drafts listings: an
    /// offer to list it from one message (Appendix A.3), which `left` more photos can join.
    pub fn listing_draft_offered(&self, left: usize) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let more = if left > 0 {
            format!(" ส่งรูปเพิ่มได้อีก {left} รูป")
        } else {
            String::new()
        };
        Message::text(format!(
            "ได้รับรูปแล้ว{kha} จะลงขายใช่ไหม{kha_question} พิมพ์ชื่อ ราคา และจำนวนมาได้เลย เช่น \"{LISTING_EXAMPLE}\"{more} ถ้าจะตอบทีละข้อ กด \"{}\" ถ้าไม่ได้จะขาย กด \"{}\" ได้เลย{kha}",
            labels::NEXT.as_str(),
            labels::CANCEL.as_str()
        ))
    }

    /// The AI drafted the product but some parts it must have (FR-PROD-02); the question for the
    /// first follows.
    pub fn listing_drafted_partly(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!("ร่างสินค้าให้แล้ว{kha} ขอข้อมูลเพิ่มอีกนิดนะ{kha_question}"))
    }

    /// The AI found nothing of a product in the seller's message; the question for its name
    /// follows, step by step.
    pub fn listing_not_understood(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขอโทษ{kha} ยังอ่านข้อมูลสินค้าจากข้อความนี้ไม่ได้ มาตอบทีละข้อกันนะ{kha_question}"
        ))
    }

    /// The AI couldn't draft the product, `resting` at a daily limit (FR-AI-09) or failing
    /// (FR-AI-10); the question for its name follows, step by step.
    pub fn listing_not_drafted(&self, resting: bool) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let why = if resting {
            format!("ตอนนี้ผู้ช่วยขอพักก่อนนะ{kha_question}")
        } else {
            format!("ขอโทษ{kha} ตอนนี้ผู้ช่วยร่างสินค้าให้ไม่ได้")
        };
        Message::text(format!("{why} มาตอบทีละข้อกันแทน{kha}"))
    }

    /// A photo was added to the product being added, which now has `count` of at most `max`.
    /// The step's question follows.
    pub fn product_photo_added(&self, count: usize, max: usize) -> Message {
        let kha = self.kha();
        Message::text(format!("ได้รูปแล้ว{kha} ตอนนี้มีรูปสินค้า {count} จาก {max} รูป"))
    }

    /// A photo past the `max` a product may have (FR-PROD-04), so not kept. The step's question
    /// follows.
    pub fn product_photos_full(&self, max: usize) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} สินค้าหนึ่งชิ้นมีรูปได้ไม่เกิน {max} รูป รูปนี้จึงไม่ได้ใช้{kha}"
        ))
    }

    /// The photos sent earlier for the product are gone, so they are asked for again before it
    /// can be saved; `done` goes on without them.
    pub fn product_photos_lost(&self, done: &Label) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} รูปสินค้าที่ส่งไว้หายไปแล้ว ส่งรูปมาใหม่อีกครั้ง หรือกด \"{}\" ได้เลย{kha}",
            done.as_str()
        ))
    }

    /// Asks for the product's name, of at most `max_chars` (FR-PROD-03), or a new name for
    /// `current`.
    pub fn ask_product_name(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "สินค้าชื่ออะไร{kha_question} พิมพ์มาได้เลย ไม่เกิน {max_chars} ตัวอักษร{kha} เช่น ไข่ไก่เบอร์ 2{}",
            now(current)
        ))
    }

    /// Asks for the product's price per unit, of at most `max` (FR-PROD-03).
    pub fn ask_product_price(&self, max: Money, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขายราคาเท่าไร{kha_question} พิมพ์ราคาต่อหน่วยเป็นตัวเลขได้เลย{kha} เช่น 20 หรือ 12.50 ได้ไม่เกิน {max}{}",
            now(current)
        ))
    }

    /// A price under `min`, the least a product may cost (FR-PROD-03).
    pub fn price_too_low(&self, min: Money) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ราคาต้องไม่ต่ำกว่า {min} พิมพ์ราคาเป็นตัวเลขได้เลย{kha} เช่น 20 หรือ 12.50"
        ))
    }

    /// Asks what the product is sold by, of at most `max_chars` (FR-PROD-03): one of the
    /// suggested units, or one typed.
    pub fn ask_product_unit(&self, max_chars: usize, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ขายเป็นหน่วยอะไร{kha_question} เช่น ชิ้น กล่อง แผง เลือกจากปุ่มด้านล่าง หรือพิมพ์เองได้ไม่เกิน {max_chars} ตัวอักษร{kha}{}",
            now(current)
        ))
    }

    /// Asks for the product's category, one of the active ones offered as buttons (FR-PROD-03).
    pub fn ask_product_category(&self, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "สินค้านี้อยู่หมวดไหน{kha_question} เลือกจากปุ่มด้านล่างได้เลย{kha}{}",
            now(current)
        ))
    }

    /// A category that is no longer active, so not offered for new products (FR-PROD-03).
    pub fn category_inactive(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} หมวดนี้ปิดใช้งานแล้ว เลือกหมวดอื่นจากปุ่มด้านล่างได้เลย{kha}"
        ))
    }

    /// No category is active, so no product can be listed (FR-PROD-03).
    pub fn no_categories(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ตอนนี้ยังไม่มีหมวดสินค้าให้เลือก จึงยังลงสินค้าไม่ได้ ผู้ดูแลต้องเพิ่มหมวดก่อน ลองใหม่ภายหลังได้เลย{kha}"
        ))
    }

    /// Asks how many of the product, sold by `unit`, there are for new orders, at most `max`
    /// (FR-PROD-03), or that there is no set number.
    pub fn ask_product_stock(&self, max: u32, unit: &str, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ตอนนี้มีขายกี่{unit}{kha_question} พิมพ์เป็นตัวเลขได้เลย เช่น 30 ได้ไม่เกิน {} ถ้าไม่จำกัดหรือทำตามสั่ง กด \"{}\" ได้เลย{kha}{}",
            thai::group_thousands(max.into()),
            labels::UNLIMITED_STOCK.as_str(),
            now(current)
        ))
    }

    /// A stock over `max`, the most a product may have (FR-PROD-03).
    pub fn stock_too_large(&self, max: u32) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} จำนวนใส่ได้ไม่เกิน {} พิมพ์ใหม่ได้เลย ถ้าไม่จำกัดหรือทำตามสั่ง กด \"{}\" ได้เลย{kha}",
            thai::group_thousands(max.into()),
            labels::UNLIMITED_STOCK.as_str()
        ))
    }

    /// Asks for the product's optional description, of at most `max_chars` (FR-PROD-03). `none`
    /// goes without, or removes the one given.
    pub fn ask_product_description(
        &self,
        max_chars: usize,
        none: &Label,
        current: Option<&str>,
    ) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "อยากบอกรายละเอียดสินค้าเพิ่มไหม{kha_question} เช่น ทำสดทุกเช้า พิมพ์ได้ไม่เกิน {max_chars} ตัวอักษร ไม่บังคับ{kha} ถ้าไม่มี กด \"{}\" ได้เลย{kha}{}",
            none.as_str(),
            now(current)
        ))
    }

    /// Asks for the day a pre-order is ready (FR-PROD-11), from tomorrow to `latest`, or that it
    /// is no pre-order.
    pub fn ask_product_ready_on(&self, latest: NaiveDate, current: Option<&str>) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ถ้ารับสั่งล่วงหน้า ของจะพร้อมวันไหน{kha_question} กดเลือกวัน หรือพิมพ์วันที่ เช่น 15 พ.ย. ได้ตั้งแต่พรุ่งนี้ถึง {} ผู้ซื้อจะเห็นวันนี้ที่สินค้า ถ้าไม่ได้สั่งล่วงหน้า กด \"{}\" ได้เลย{kha}{}",
            thai::format_day(latest),
            labels::NOT_PRE_ORDER.as_str(),
            now(current)
        ))
    }

    /// A ready date that couldn't be read as a day (FR-PROD-11).
    pub fn ready_day_unknown(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ไม่เข้าใจวันที่ พิมพ์แบบ 15 พ.ย. หรือ 15/11 หรือกดเลือกวันได้เลย{kha}"
        ))
    }

    /// A ready date that isn't after today: a pre-order is ready from tomorrow on (FR-PROD-11).
    pub fn ready_day_too_soon(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} วันพร้อมต้องเป็นพรุ่งนี้หรือหลังจากนั้น กดเลือกวันหรือพิมพ์ใหม่ได้เลย{kha} ถ้าพร้อมขายแล้ว กด \"{}\" ได้เลย{kha}",
            labels::NOT_PRE_ORDER.as_str()
        ))
    }

    /// A ready date after `latest`, the last one a pre-order may have (FR-PROD-11).
    pub fn ready_day_too_late(&self, latest: NaiveDate) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "ขอโทษ{kha} ตั้งวันพร้อมได้ไม่เกิน {} กดเลือกวันหรือพิมพ์ใหม่ได้เลย{kha}",
            thai::format_day(latest)
        ))
    }

    /// The product before it is listed (FR-PROD-01): `preview`, whose buttons list it or correct
    /// a part.
    pub fn product_preview(&self, preview: &ProductPreview<'_>) -> Vec<Message> {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        // Appendix A.3
        let check = if preview.drafted {
            format!("ร่างสินค้าให้แล้ว{kha} ตรวจดูก่อนนะ{kha_question}")
        } else {
            format!("ตรวจสินค้าอีกครั้งนะ{kha_question}")
        };
        let text = format!(
            "{check} ถ้าถูกต้อง กด \"{}\" แล้วลูกค้าจะเห็นสินค้าทันที ถ้าต้องการแก้ กด \"{}\" ได้เลย{kha}",
            labels::CONFIRM_LISTING.as_str(),
            labels::EDIT.as_str()
        );
        vec![
            Message::text(text),
            Message::flex(
                format!("ตัวอย่างสินค้า: {}", preview.name),
                self.product_bubble(preview),
            ),
        ]
    }

    /// Product `name` is listed, and buyers see it (FR-PROD-01, Appendix A.3): `next` are what
    /// to do next, before the menu.
    pub fn product_listed(&self, name: &str, next: Vec<Action>) -> Message {
        let kha = self.kha();
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(format!("ลงขาย {name} แล้ว{kha} ลูกค้าเห็นสินค้านี้แล้ว{kha}"))
            .with_quick_reply(QuickReply::new(next.into_iter().chain([menu])))
    }

    /// Product `name` is saved but held for review, so buyers don't see it yet (FR-PROD-09).
    /// The seller isn't told why; the admins are. `next` are what to do next, before the menu.
    pub fn product_held(&self, name: &str, next: Vec<Action>) -> Message {
        let kha = self.kha();
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(format!(
            "บันทึก {name} แล้ว{kha} ขอให้ผู้ดูแลตรวจก่อนลงขาย ระหว่างนี้ผู้ซื้อยังไม่เห็นสินค้านี้ ดูสถานะได้ที่ \"{}\"{kha}",
            labels::MY_PRODUCTS.as_str()
        ))
        .with_quick_reply(QuickReply::new(next.into_iter().chain([menu])))
    }

    /// A button of "เพิ่มสินค้า" that is no longer in progress: listed, cancelled or expired.
    /// `add_product` starts again.
    pub fn product_flow_gone(&self, add_product: Action) -> Message {
        let kha = self.kha();
        let text = format!(
            "รายการนี้ไม่ได้ทำต่อแล้ว{kha} อาจลงขายไปแล้ว ยกเลิก หรือหมดเวลาไป ถ้าจะลงสินค้าใหม่ กด \"{}\" ได้เลย{kha}",
            add_product.label().as_str()
        );
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([add_product, menu]))
    }

    /// "สินค้าของฉัน" (FR-PROD-05): page `page` of the seller's products, a card each with its
    /// buttons, and "ดูเพิ่มเติม" sending `more(next_page)`. `add` is "เพิ่มสินค้า", which is all
    /// there is without any product.
    pub fn my_products(
        &self,
        cards: &[MyProductCard<'_>],
        page: usize,
        more: impl FnOnce(usize) -> String,
        add: Action,
    ) -> Message {
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        if cards.is_empty() {
            let kha = self.kha();
            let text = format!(
                "ร้านยังไม่มีสินค้าเลย{kha} กด \"{}\" เพื่อลงขายได้เลย{kha}",
                add.label().as_str()
            );
            return Message::text(text).with_quick_reply(QuickReply::new([add, menu]));
        }
        let page = carousel_page(cards, page, PRODUCTS_PER_PAGE, my_product_card, more);
        Message::flex(labels::MY_PRODUCTS.as_str(), page)
            .with_quick_reply(QuickReply::new([add, menu]))
    }

    /// The seller's product `card` as it is now (FR-PROD-05), with its buttons, then where to
    /// carry on: "สินค้าของฉัน" and the menu.
    pub fn my_product(&self, card: &MyProductCard<'_>) -> Message {
        let mine = Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone());
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::flex(card.name, my_product_card(card))
            .with_quick_reply(QuickReply::new([mine, menu]))
    }

    /// Names product `name`, a part of which the seller is about to change in "สินค้าของฉัน".
    /// The question follows.
    pub fn editing_product(&self, name: &str) -> Message {
        Message::text(format!("แก้ไขสินค้า \"{name}\"{}", self.kha()))
    }

    /// The product's price is now `price` per `unit`, for new orders only: those placed keep the
    /// price they were placed at (FR-PROD-08). Its card follows.
    pub fn product_price_saved(&self, price: Money, unit: &str) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "เปลี่ยนราคาเป็น {} แล้ว{kha} ราคาใหม่ใช้กับออเดอร์ใหม่ ส่วนออเดอร์ที่สั่งไว้แล้วยังใช้ราคาเดิม{kha}",
            price_per_unit(price, unit)
        ))
    }

    /// The product sold by `unit` now has `stock` for new orders (FR-PROD-05): sold out at 0,
    /// orderable again if it was sold out (FR-PROD-06). Its card follows.
    pub fn product_stock_saved(
        &self,
        stock: Option<i32>,
        unit: &str,
        was_sold_out: bool,
    ) -> Message {
        let kha = self.kha();
        Message::text(match stock {
            Some(0) => format!(
                "ตั้งจำนวนเป็น 0 แล้ว{kha} ลูกค้าจะเห็นว่า \"{}\" และสั่งไม่ได้จนกว่าจะเติมของ{kha}",
                availability(Availability::SoldOut)
            ),
            _ if was_sold_out => format!("เติมของแล้ว{kha} ลูกค้าสั่งสินค้านี้ได้อีกครั้ง{kha}"),
            stock => format!("แก้จำนวนเป็น {} แล้ว{kha}", stock_line(stock, unit)),
        })
    }

    /// A part of the product other than its price and stock is saved. Its card follows.
    pub fn product_saved(&self) -> Message {
        Message::text(format!("บันทึกแล้ว{}", self.kha()))
    }

    /// A part of the product is saved, and the product held for review (FR-PROD-09). The seller
    /// isn't told why; the admins are. Its card, saying "รอตรวจ", follows.
    pub fn product_held_saved(&self) -> Message {
        let kha = self.kha();
        Message::text(format!(
            "บันทึกแล้ว{kha} ขอให้ผู้ดูแลตรวจสินค้านี้ก่อน ระหว่างนี้ผู้ซื้อยังไม่เห็นสินค้านี้{kha}"
        ))
    }

    /// A part of the product is left as it was. Its card follows.
    pub fn product_unchanged(&self) -> Message {
        Message::text(format!("ยังใช้ข้อมูลเดิมอยู่{}", self.kha()))
    }

    /// Product `name` is now `visible` to buyers or hidden from them (FR-PROD-05); `changed`
    /// unless it already was. Its card follows.
    pub fn product_visibility(&self, name: &str, visible: bool, changed: bool) -> Message {
        let kha = self.kha();
        Message::text(match (visible, changed) {
            (true, true) => format!("แสดง \"{name}\" แล้ว{kha} ผู้ซื้อเห็นสินค้านี้อีกครั้ง{kha}"),
            (true, false) => format!("\"{name}\" แสดงอยู่แล้ว{kha}"),
            (false, true) => format!(
                "ซ่อน \"{name}\" แล้ว{kha} ผู้ซื้อจะไม่เห็นสินค้านี้จนกว่าจะกด \"{}\"{kha}",
                labels::SHOW_PRODUCT.as_str()
            ),
            (false, false) => format!("\"{name}\" ซ่อนอยู่แล้ว{kha}"),
        })
    }

    /// Asks to confirm deleting product `name` (FR-PROD-05, FR-UI-13): buyers no longer see it,
    /// its orders go on, and it can't be undone. `confirm` deletes it; cancelling keeps it.
    pub fn confirm_delete_product(&self, name: &str, confirm: Action) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ลบสินค้า \"{name}\" ใช่ไหม{kha_question} ผู้ซื้อจะไม่เห็นสินค้านี้อีก ออเดอร์ที่สั่งไว้แล้วยังทำต่อตามปกติ ลบแล้วกู้คืนไม่ได้ ถ้าแน่ใจ กด \"{}\" ได้เลย{kha}",
            confirm.label().as_str()
        );
        with_cancel(Message::text(text).with_quick_reply(QuickReply::new([confirm])))
    }

    /// Asks to confirm a quick update (FR-PROD-10, FR-UI-13): `update`'s price and stock, from what
    /// they are to what the seller asked. `confirm` saves them; cancelling leaves them.
    pub fn confirm_product_update(&self, update: &ProductUpdate<'_>, confirm: Action) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let unit = update.unit;
        let mut lines = vec![format!("เปลี่ยน \"{}\" ตามนี้ใช่ไหม{kha_question}", update.name)];
        if let Some((from, to)) = update.price {
            lines.push(format!(
                "ราคา: {} → {}",
                price_per_unit(from, unit),
                price_per_unit(to, unit)
            ));
        }
        if let Some((from, to)) = update.stock {
            lines.push(format!(
                "จำนวน: {} → {}",
                stock_line(from, unit),
                stock_line(to, unit)
            ));
        }
        lines.push(format!(
            "ถ้าถูกต้อง กด \"{}\" ได้เลย{kha}",
            confirm.label().as_str()
        ));
        let text = Message::text(lines.join("\n")).with_quick_reply(QuickReply::new([confirm]));
        with_cancel(text)
    }

    /// The product a quick update was confirmed for changed since it was asked; it is asked
    /// again as it is now, which follows.
    pub fn product_update_changed(&self) -> Message {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        Message::text(format!(
            "ข้อมูลสินค้านี้เปลี่ยนไปแล้ว{kha} ตรวจอีกครั้งนะ{kha_question}"
        ))
    }

    /// A quick update to product `name`, if still known, wasn't confirmed in time, or was
    /// cancelled, so it is as it was. Where to carry on follows.
    pub fn product_not_updated(&self, name: Option<&str>) -> Message {
        let kha = self.kha();
        let what = name.map_or_else(|| "สินค้านี้".to_owned(), |name| format!(" \"{name}\""));
        Message::text(format!(
            "ยังไม่ได้เปลี่ยน{what}{kha} ถ้าจะเปลี่ยน พิมพ์บอกมาใหม่ หรือแก้ที่ \"{}\" ได้เลย{kha}",
            labels::MY_PRODUCTS.as_str()
        ))
    }

    /// The product a quick update names isn't one of the seller's (FR-PROD-10): `my_products`
    /// lists those, then the menu.
    pub fn quick_update_not_found(&self, my_products: Action) -> Message {
        let kha = self.kha();
        let text = format!(
            "ไม่พบสินค้านี้ในร้าน{kha} ดูและแก้สินค้าได้ที่ \"{}\"{kha}",
            my_products.label().as_str()
        );
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([my_products, menu]))
    }

    /// Product `name` was deleted (DR-04), or kept, the deletion not having been confirmed in
    /// time; then where to carry on: "สินค้าของฉัน" and the menu.
    pub fn product_deleted(&self, name: &str, deleted: bool) -> Message {
        let kha = self.kha();
        let text = if deleted {
            format!("ลบสินค้า \"{name}\" แล้ว{kha}")
        } else {
            format!(
                "ยังไม่ได้ลบสินค้า \"{name}\"{kha} ถ้าจะลบ กด \"{}\" ที่สินค้านั้นใน \"{}\" อีกครั้งได้เลย{kha}",
                labels::DELETE.as_str(),
                labels::MY_PRODUCTS.as_str()
            )
        };
        let mine = Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone());
        let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
        Message::text(text).with_quick_reply(QuickReply::new([mine, menu]))
    }

    /// A product's preview (FR-PROD-01): its first photo, name, price per unit and description,
    /// then its category, stock and photos, with `preview.buttons` at the foot, the first one
    /// primary.
    fn product_bubble(&self, preview: &ProductPreview<'_>) -> Bubble {
        let hero = preview.photo.map(|url| {
            Hero::Image(Image {
                size: Some(Size::Full),
                aspect_ratio: Some("20:13".to_owned()),
                aspect_mode: Some(AspectMode::Cover),
                ..Image::new(url)
            })
        });
        let name = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(preview.name)
        };
        let price = Text {
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(price_per_unit(preview.price, preview.unit))
        };
        let mut top: Vec<Component> = vec![name.into(), price.into()];
        if let Some(description) = preview.description {
            let description = Text {
                size: Some(Size::Sm),
                wrap: Some(true),
                ..Text::new(description)
            };
            top.push(description.into());
        }
        let photos = match preview.photos {
            0 => NONE.to_owned(),
            count => format!("{count} รูป"),
        };
        let mut rows = vec![
            row("หมวดหมู่", preview.category),
            row("จำนวน", &stock_line(preview.stock, preview.unit)),
        ];
        rows.extend(preview.ready_on.map(ready_row));
        rows.push(row("รูปสินค้า", &photos));
        let details = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(rows)
        };
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical(
                top.into_iter()
                    .chain([Separator::default().into(), details.into()]),
            )
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            hero,
            body: Some(body),
            footer: footer(&preview.buttons),
            ..Bubble::default()
        }
    }

    /// A shop's card (FR-SHOP-01): its photo, name and description, and how it sells, with
    /// who applied and when for the admins, `heading` above, and `card.buttons` at the foot,
    /// the first one primary.
    fn shop_bubble(&self, card: &ShopCard<'_>, heading: Option<&str>) -> Bubble {
        let header = heading.map(|heading| {
            let heading = Text {
                weight: Some(Weight::Bold),
                wrap: Some(true),
                ..Text::new(heading)
            };
            FlexBox::vertical([heading.into()])
        });
        let hero = card.photo.map(|url| {
            Hero::Image(Image {
                size: Some(Size::Full),
                aspect_ratio: Some("20:13".to_owned()),
                aspect_mode: Some(AspectMode::Cover),
                ..Image::new(url)
            })
        });
        let name = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(card.name)
        };
        let description = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
            ..Text::new(card.description)
        };
        let location = if card.pickup_location {
            "แชร์ไว้แล้ว"
        } else {
            "ไม่ได้แชร์"
        };
        let phone = card.phone.map(thai::format_phone);
        let minimum = card.minimum_order.map(|minimum| minimum.to_string());
        let promptpay = match card.promptpay {
            PromptPay::Number(number) => number.map(payments::format_promptpay_id),
            PromptPay::Given(true) => Some("มี".to_owned()),
            PromptPay::Given(false) => None,
        };
        let fulfillment =
            fulfillment_line(card.offers_pickup, card.offers_delivery, card.delivery_fee);
        let mut rows = Vec::new();
        if let Some(running) = &card.running {
            let state = if running.held {
                "รอตรวจ (ผู้ซื้อยังไม่เห็นร้าน)"
            } else if running.is_open {
                "เปิดอยู่"
            } else {
                "ปิดอยู่ (ผู้ซื้อสั่งไม่ได้)"
            };
            rows.push(row("สถานะร้าน", state));
            rows.push(row("เวลาเปิด", running.opening_hours.unwrap_or(NOT_SET)));
        }
        rows.extend([
            row("ที่รับของ", card.pickup_address.unwrap_or(NONE)),
            row("ตำแหน่ง", location),
            row("เบอร์ร้าน", phone.as_deref().unwrap_or(NONE)),
        ]);
        if let Some(running) = &card.running {
            let shown = if running.show_phone {
                "ผู้ซื้อที่มีออเดอร์เห็น"
            } else {
                "ซ่อนจากผู้ซื้อ"
            };
            rows.push(row("แสดงเบอร์", shown));
        }
        rows.extend([
            row("การรับส่ง", &fulfillment),
            row("ยอดขั้นต่ำ", minimum.as_deref().unwrap_or(NONE)),
            row("พร้อมเพย์", promptpay.as_deref().unwrap_or(NONE)),
        ]);
        if let Some(applicant) = &card.applicant {
            rows.push(row("ผู้สมัคร", applicant.display_name.unwrap_or(NONE)));
            rows.push(row("ส่งเมื่อ", &self.date_time(applicant.at)));
        }
        let details = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(rows)
        };
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical([
                name.into(),
                description.into(),
                Separator::default().into(),
                details.into(),
            ])
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            header,
            hero,
            body: Some(body),
            footer: footer(&card.buttons),
            ..Bubble::default()
        }
    }

    /// A shop's card as buyers see it (FR-SHOP-09): its photo, name, whether it is open, its
    /// description and how it sells, with "ดูสินค้า" at the foot. Nothing that reaches its
    /// owner: no address, location, phone or PromptPay ID (NFR-PRIV-04).
    fn shop_front_bubble(&self, front: &ShopFront<'_>) -> Bubble {
        let hero = front.photo.map(|url| {
            Hero::Image(Image {
                size: Some(Size::Full),
                aspect_ratio: Some("20:13".to_owned()),
                aspect_mode: Some(AspectMode::Cover),
                ..Image::new(url)
            })
        });
        let name = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(front.name)
        };
        let (state, color) = if front.is_open {
            ("เปิดอยู่", OPEN_COLOR)
        } else {
            (CLOSED, CLOSED_COLOR)
        };
        let state = Text {
            size: Some(Size::Sm),
            weight: Some(Weight::Bold),
            color: Some(color.to_owned()),
            ..Text::new(state)
        };
        let description = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
            ..Text::new(front.description)
        };
        let minimum = front.minimum_order.map(|minimum| minimum.to_string());
        let fulfillment = fulfillment_line(
            front.offers_pickup,
            front.offers_delivery,
            front.delivery_fee,
        );
        let mut rows = vec![
            row("การรับส่ง", &fulfillment),
            row("ยอดขั้นต่ำ", minimum.as_deref().unwrap_or(NONE)),
        ];
        if let Some(hours) = front.opening_hours {
            rows.push(row("เวลาเปิด", hours));
        }
        if let Some(rating) = &front.rating {
            let rated = format!(
                "★ {} ({} รีวิว)",
                rating.average(),
                thai::group_thousands(rating.count)
            );
            rows.push(row(RATING, &rated));
        }
        let details = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(rows)
        };
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical([
                name.into(),
                state.into(),
                description.into(),
                Separator::default().into(),
                details.into(),
            ])
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            hero,
            body: Some(body),
            footer: footer(std::slice::from_ref(&front.products)),
            ..Bubble::default()
        }
    }

    /// A card of "รายการรอตรวจ" (FR-ADM-03): the photo, name and state of a product or shop,
    /// its latest reports, who made them and when, and `card.buttons` at the foot, the first
    /// one primary.
    fn review_bubble(&self, card: &ReviewCard<'_>) -> Bubble {
        let hero = card.photo.map(|url| {
            Hero::Image(Image {
                size: Some(Size::Full),
                aspect_ratio: Some("20:13".to_owned()),
                aspect_mode: Some(AspectMode::Cover),
                ..Image::new(url)
            })
        });
        let (name, kind) = match card.what {
            Reported::Product { name, shop } => (name, format!("สินค้าของ{}", shop_called(shop))),
            Reported::Shop { name } => (name, "ร้านค้า".to_owned()),
        };
        let name = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(name)
        };
        let kind = Text {
            size: Some(Size::Sm),
            color: Some(MUTED.to_owned()),
            wrap: Some(true),
            ..Text::new(kind)
        };
        let mut top: Vec<Component> = vec![name.into(), kind.into()];
        let (state, color, shop_state) = match card.state {
            Reviewed::Product { status, shop } => {
                let (state, color) = match status {
                    product::Status::Held => ("รอตรวจ (ผู้ซื้อยังไม่เห็น)", MUTED),
                    product::Status::Active => ("ลงขายอยู่", OPEN_COLOR),
                    product::Status::Hidden => ("ผู้ขายซ่อนไว้ (ผู้ซื้อไม่เห็น)", MUTED),
                    product::Status::Removed => ("ถูกนำออกแล้ว", CLOSED_COLOR),
                    product::Status::Draft | product::Status::Deleted => ("ผู้ขายลบแล้ว", MUTED),
                };
                let shop = (shop != shop::Status::Approved).then(|| reviewed_shop(shop).0);
                (state, color, shop)
            }
            Reviewed::Shop(status) => {
                let (state, color) = reviewed_shop(status);
                (state, color, None)
            }
        };
        let state = Text {
            size: Some(Size::Sm),
            weight: Some(Weight::Bold),
            color: Some(color.to_owned()),
            wrap: Some(true),
            ..Text::new(state)
        };
        top.push(state.into());
        if let Some(shop_state) = shop_state {
            top.push(line(&format!("ร้าน: {shop_state}")));
        }
        let mut reports: Vec<Component> = card
            .reports
            .iter()
            .take(REVIEW_REPORTS)
            .map(|report| {
                let by = match report.by {
                    Reporter::Moderation => "ระบบ",
                    Reporter::User(name) => name.unwrap_or("ผู้ใช้"),
                };
                line(&format!(
                    "{by} · {}: {}",
                    self.date_time(report.at),
                    report.reason
                ))
            })
            .collect();
        if let Some(rest) = card.reports.len().checked_sub(REVIEW_REPORTS)
            && rest > 0
        {
            reports.push(line(&format!("และอีก {rest} เรื่อง")));
        }
        let reports = section(&format!("เรื่องที่แจ้ง {} เรื่อง", card.reports.len()), reports);
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical(
                top.into_iter()
                    .chain([Separator::default().into(), reports.into()]),
            )
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            hero,
            body: Some(body),
            footer: footer(&card.buttons),
            ..Bubble::default()
        }
    }

    /// A user's card in "ผู้ใช้" (FR-ADM-04): their name, whether they are suspended and why,
    /// their roles, zone and when they joined, their shop and how it stands, and `card.buttons`
    /// at the foot, the first one primary. No phone or house number (NFR-PRIV-04).
    fn user_bubble(&self, card: &UserCard<'_>) -> Bubble {
        let name = Text {
            size: Some(Size::Lg),
            weight: Some(Weight::Bold),
            wrap: Some(true),
            ..Text::new(card.display_name.unwrap_or("ไม่มีชื่อ"))
        };
        let (state, color) = if card.suspended {
            ("ถูกระงับ", CLOSED_COLOR)
        } else {
            ("ใช้งานอยู่", OPEN_COLOR)
        };
        let state = Text {
            size: Some(Size::Sm),
            weight: Some(Weight::Bold),
            color: Some(color.to_owned()),
            ..Text::new(state)
        };
        let mut roles = vec![labels::BUYER.as_str()];
        if card.seller {
            roles.push(labels::SELLER.as_str());
        }
        if card.admin {
            roles.push(labels::ADMIN.as_str());
        }
        let mut rows = Vec::new();
        if card.suspended {
            rows.push(row("เหตุผลที่ระงับ", card.suspension_reason.unwrap_or(NONE)));
        }
        rows.extend([
            row("บทบาท", &roles.join(", ")),
            row("โซน", card.zone.unwrap_or(NOT_GIVEN)),
            row("เข้าร่วมเมื่อ", &self.date(card.joined)),
        ]);
        if let Some(shop) = &card.shop {
            let state = match shop.status {
                shop::Status::Pending => "รออนุมัติ",
                shop::Status::Approved => "อนุมัติแล้ว",
                shop::Status::Rejected => "ไม่ผ่านการอนุมัติ",
                shop::Status::Held => "รอตรวจ",
                shop::Status::Suspended => "ถูกระงับ",
                shop::Status::Closed => "ปิดถาวร",
            };
            rows.push(row("ร้าน", &shop_called(shop.name)));
            rows.push(row("สถานะร้าน", state));
            if shop.status == shop::Status::Suspended {
                rows.push(row("เหตุผลที่ระงับร้าน", shop.suspension_reason.unwrap_or(NONE)));
            }
        }
        let details = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(rows)
        };
        let body = FlexBox {
            spacing: Some(Spacing::Md),
            ..FlexBox::vertical([
                name.into(),
                state.into(),
                Separator::default().into(),
                details.into(),
            ])
        };
        Bubble {
            size: Some(BubbleSize::Mega),
            body: Some(body),
            footer: footer(&card.buttons),
            ..Bubble::default()
        }
    }
}

/// A card's `buttons` at its foot, the first one primary; `None` without any.
fn footer(buttons: &[Action]) -> Option<FlexBox> {
    footer_of(buttons, true)
}

/// A card's `buttons` at its foot, the first one primary if `primary`; `None` without any.
fn footer_of(buttons: &[Action], primary: bool) -> Option<FlexBox> {
    (!buttons.is_empty()).then(|| {
        let buttons = buttons.iter().enumerate().map(|(i, action)| {
            let style = if i == 0 && primary {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Secondary
            };
            let button = Button {
                style: Some(style),
                height: Some(ButtonHeight::Sm),
                ..Button::new(action.clone())
            };
            button.into()
        });
        FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(buttons)
        }
    })
}

/// Applications on a page of "อนุมัติร้าน". Their cards are the largest the market sends, so
/// fewer than a carousel holds fit its 50 KB (FR-LINE-11).
pub const APPLICATIONS_PER_PAGE: usize = 8;

/// Tells the seller the buyer of an order cancelled had paid, so they are to refund them and
/// record it (FR-PAY-09).
fn refund_line(kha: &str) -> String {
    format!(
        "ลูกค้าชำระเงินแล้ว กรุณาคืนเงินให้ลูกค้า แล้วกด \"{}\"{kha}",
        labels::MARK_REFUNDED.as_str()
    )
}

/// The shop called `name` as a sentence names it: `ร้านป้าแดง` as it is, and a name that
/// doesn't say it is a shop with `ร้าน` before it, as in Appendix A.5.
pub fn shop_called(name: &str) -> String {
    if name.starts_with("ร้าน") {
        name.to_owned()
    } else if name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        format!("ร้าน {name}")
    } else {
        format!("ร้าน{name}")
    }
}

/// How a shop's buyers get their orders, picked up or delivered, and what delivery costs.
fn fulfillment_line(pickup: bool, delivery: bool, fee: Money) -> String {
    let fee = if fee == Money::ZERO {
        "ส่งฟรี".to_owned()
    } else {
        format!("ค่าส่ง {fee}")
    };
    match (pickup, delivery) {
        (true, true) => format!("รับเองหรือให้ร้านไปส่ง {fee}"),
        (false, true) => format!("ร้านไปส่ง {fee}"),
        _ => labels::PICKUP_ONLY.as_str().to_owned(),
    }
}

/// What a product's card says of whether it can be ordered (FR-SRCH-07): `เหลือ 5`, `สั่งได้`,
/// `หมด` or `ร้านปิด`.
pub fn availability(availability: Availability) -> String {
    match availability {
        Availability::Left(left) => format!("เหลือ {left}"),
        Availability::Orderable => "สั่งได้".to_owned(),
        Availability::SoldOut => "หมด".to_owned(),
        Availability::ShopClosed => CLOSED.to_owned(),
    }
}

/// What a pre-order's card, and each line of a cart or order holding one, says of it
/// (FR-PROD-11): `สั่งล่วงหน้า · พร้อม 5 พ.ย. 2569`.
pub fn ready_line(day: NaiveDate) -> String {
    format!(
        "{} · พร้อม {}",
        labels::PRE_ORDER.as_str(),
        thai::format_day(day)
    )
}

/// The same, within a sentence: ` (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569)` after what is ordered, or
/// nothing for what is no pre-order.
fn ready_note(ready_on: Option<NaiveDate>) -> String {
    ready_on
        .map(|day| {
            format!(
                " ({} พร้อม {})",
                labels::PRE_ORDER.as_str(),
                thai::format_day(day)
            )
        })
        .unwrap_or_default()
}

/// The row of a seller's card saying a product is a pre-order, ready on `day`.
fn ready_row(day: NaiveDate) -> Component {
    let ready = format!("พร้อม {}", thai::format_day(day));
    row(labels::PRE_ORDER.as_str(), &ready)
}

/// `stars` of 5, as a rating shows them (FR-ORD-14): `★★★★☆`.
pub fn stars_line(stars: i16) -> String {
    let stars = usize::try_from(stars.clamp(0, 5)).expect("0 to 5");
    format!("{}{}", "★".repeat(stars), "☆".repeat(5 - stars))
}

/// The Quick Reply that gives `stars` (FR-ORD-14): `★★★★`.
pub fn stars_label(stars: i16) -> Label {
    let stars = usize::try_from(stars.clamp(1, 5)).expect("1 to 5");
    Label::fit("★".repeat(stars))
}

/// What an order's `rating` shows: its stars, then what its buyer wrote, quoted, below them.
fn rating_line(rating: OrderRating<'_>) -> String {
    let stars = stars_line(rating.stars);
    match rating.comment {
        Some(comment) => format!("{stars}\n\"{comment}\""),
        None => stars,
    }
}

/// A product's price per unit, as its card says it: `฿20 / กล่อง` (FR-UI-07).
pub fn price_per_unit(price: Money, unit: &str) -> String {
    format!("{price} / {unit}")
}

/// How many of a product sold by `unit` there are, as its seller sees it: `30 กล่อง`, sold out at
/// 0, or `None`, unlimited or made to order (FR-PROD-03).
pub fn stock_line(stock: Option<i32>, unit: &str) -> String {
    match stock {
        None => labels::UNLIMITED_STOCK.as_str().to_owned(),
        Some(0) => format!("0 {unit} (ลูกค้าจะเห็นว่าหมด)"),
        Some(count) => format!(
            "{} {unit}",
            thai::group_thousands(count.unsigned_abs().into())
        ),
    }
}

/// What a closed shop and its products show (FR-SHOP-05)
const CLOSED: &str = "ร้านปิด";
/// The row of a rated order, and of a shop's average (FR-ORD-14)
const RATING: &str = "คะแนน";
/// A product in one message, for the AI to draft (FR-PROD-02, Appendix A.3)
const LISTING_EXAMPLE: &str = "ขนมครก กล่องละ 20 มี 30 กล่อง";
/// The colours of whether a shop is open, on its card
const OPEN_COLOR: &str = "#06C755";
const CLOSED_COLOR: &str = "#D93025";
/// A pre-order's ready date: neither open nor closed (FR-PROD-11)
const PRE_ORDER_COLOR: &str = "#E37400";

/// A shop's card as buyers see it (FR-SHOP-09).
pub struct ShopFront<'a> {
    pub name: &'a str,
    pub description: &'a str,
    /// Its photo's `/img/p` link
    pub photo: Option<&'a Url>,
    /// Whether it takes orders now; closed, its products can't be ordered (FR-SHOP-05)
    pub is_open: bool,
    /// Free text such as *ทุกวัน 06:00–10:00* (FR-SHOP-07)
    pub opening_hours: Option<&'a str>,
    pub offers_pickup: bool,
    pub offers_delivery: bool,
    pub delivery_fee: Money,
    pub minimum_order: Option<Money>,
    /// Its buyers' average rating, once there are enough to show (FR-ORD-14)
    pub rating: Option<ShopRating>,
    /// "ดูสินค้า"
    pub products: Action,
}

/// The average of a shop's ratings, as its card shows it (FR-ORD-14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShopRating {
    /// The average of their stars in tenths: 47 is 4.7
    pub tenths: u64,
    /// How many there are
    pub count: u64,
}

impl ShopRating {
    /// The average to one decimal: `4.7`.
    pub fn average(&self) -> String {
        format!("{}.{}", self.tenths / 10, self.tenths % 10)
    }
}

/// A buyer's rating of an order (FR-ORD-14): its stars, 1 to 5, and what they wrote, if they
/// did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderRating<'a> {
    pub stars: i16,
    pub comment: Option<&'a str>,
}

/// A product being added, as its preview shows it (FR-PROD-01).
pub struct ProductPreview<'a> {
    pub name: &'a str,
    pub price: Money,
    pub unit: &'a str,
    pub category: &'a str,
    /// `None`: unlimited or made to order
    pub stock: Option<i32>,
    pub description: Option<&'a str>,
    /// The day it is ready, if it is a pre-order (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
    /// Its first photo's `/img/p` link
    pub photo: Option<&'a Url>,
    /// How many photos it has
    pub photos: usize,
    /// Whether the AI drafted it (FR-PROD-02), for the seller to check
    pub drafted: bool,
    pub buttons: Vec<Action>,
}

/// A quick update to a product's price or stock, to confirm (FR-PROD-10): each from what it is
/// to what the seller asked, `None` where it doesn't change.
pub struct ProductUpdate<'a> {
    pub name: &'a str,
    pub unit: &'a str,
    pub price: Option<(Money, Money)>,
    /// As `products.stock` keeps it: `None` is unlimited or made to order
    pub stock: Option<(Option<i32>, Option<i32>)>,
}

/// A product as its seller sees it in "สินค้าของฉัน" (FR-PROD-05).
pub struct MyProductCard<'a> {
    pub name: &'a str,
    pub price: Money,
    pub unit: &'a str,
    pub category: &'a str,
    /// What is left for new orders: `None` is unlimited or made to order, and 0 sold out
    /// (FR-PROD-06)
    pub stock: Option<i32>,
    /// The day it is ready, while it is a pre-order (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
    /// Whether buyers see it (FR-PROD-07)
    pub status: product::Status,
    /// Its first photo's `/img/p` link
    pub photo: Option<&'a Url>,
    /// How many photos it has
    pub photos: usize,
    /// Its buttons, a row each
    pub buttons: Vec<Vec<Action>>,
    /// Whether the first button is primary: "เติมของ", for a sold-out product (FR-PROD-06)
    pub primary: bool,
}

/// What a buyer browsed (FR-SRCH): what a list's headline, and the reply when it has nothing,
/// name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browsed<'a> {
    /// The products of the category with this name (FR-SRCH-01)
    Category(&'a str),
    /// The products of the shop with this name (FR-SHOP-09)
    Shop(&'a str),
    /// "ของใหม่วันนี้" (FR-SRCH-09)
    NewToday,
    /// What a search for this keyword found (FR-SRCH-04)
    Search(&'a str),
    /// "ร้านค้าทั้งหมด" (FR-SRCH-06)
    Shops,
    /// What the AI assistant found or proposed (FR-AI-02)
    Found,
}

/// A product's card as buyers see it in a list (FR-SRCH-07).
pub struct ProductCard<'a> {
    pub name: &'a str,
    pub price: Money,
    pub unit: &'a str,
    /// Its shop's name
    pub shop: &'a str,
    pub availability: Availability,
    /// The day it is ready, while it is a pre-order (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
    /// Its first photo's `/img/p` link, or the placeholder's
    pub photo: &'a Url,
    /// "ใส่ตะกร้า", while it can be ordered (FR-PROD-06)
    pub add_to_cart: Option<Action>,
    /// "ดูร้าน": its shop's card
    pub view_shop: Action,
    /// A tap anywhere else on the card: its details
    pub details: Action,
}

/// A product's details as buyers see them, from a tap on its card (FR-SRCH-07).
pub struct ProductDetails<'a> {
    pub name: &'a str,
    pub price: Money,
    pub unit: &'a str,
    pub description: Option<&'a str>,
    /// Its shop's name
    pub shop: &'a str,
    pub availability: Availability,
    /// The day it is ready, while it is a pre-order (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
    /// Its photos' `/img/p` links, in order, or the placeholder's alone
    pub photos: &'a [Url],
    /// "ใส่ตะกร้า" first while it can be ordered, then "ดูร้าน" and "แจ้งปัญหา"
    pub buttons: Vec<Action>,
}

/// A shop's lines in a buyer's cart, as its card there shows them (FR-CART-03).
pub struct CartShop<'a> {
    pub name: &'a str,
    pub lines: Vec<CartLine<'a>>,
    /// That of the lines that can be ordered
    pub subtotal: Money,
    /// Whether some lines are left out of it, as they can't be ordered
    pub excluded: bool,
    /// "สั่งซื้อร้านนี้", while a line can be ordered
    pub checkout: Option<Action>,
}

/// A line of a buyer's cart, as its shop's card there shows it (FR-CART-03).
pub struct CartLine<'a> {
    pub name: &'a str,
    pub unit: &'a str,
    /// The product's price now
    pub price: Money,
    pub quantity: i32,
    pub total: Money,
    /// The day it is ready, while its product is a pre-order (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
    /// Whether it can be ordered, and why not
    pub state: cart::State,
    /// "แก้จำนวน" while its product can be ordered, then "ลบออก"
    pub buttons: Vec<Action>,
}

/// An order as checkout's summary and the order's card show it (FR-CART-04, FR-CART-08).
pub struct OrderDetails<'a> {
    /// Its shop's name
    pub shop: &'a str,
    pub lines: Vec<OrderLine<'a>>,
    pub subtotal: Money,
    /// ฿0 for pickup
    pub delivery_fee: Money,
    pub total: Money,
    pub fulfillment: order::Fulfillment,
    /// Where it is delivered, as [`address_line`] shows it; `None` for pickup
    pub address: Option<String>,
    pub payment: order::PaymentMethod,
    /// For the shop (FR-CART-04)
    pub note: Option<&'a str>,
}

/// A line of an order: its product as it was when ordered (DR-03).
pub struct OrderLine<'a> {
    pub name: &'a str,
    pub unit: &'a str,
    pub price: Money,
    pub quantity: i32,
    pub total: Money,
    /// The day it is ready, if it was ordered in advance (FR-PROD-11)
    pub ready_on: Option<NaiveDate>,
}

/// What changed since checkout's summary, so its confirmation placed no order (FR-CART-05).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckoutChange<'a> {
    /// None left (FR-CART-06)
    SoldOut {
        name: &'a str,
    },
    /// Fewer left than the line holds
    NotEnough {
        name: &'a str,
        left: i32,
        unit: &'a str,
    },
    /// Hidden, held, removed or deleted
    Unavailable {
        name: &'a str,
    },
    /// The shop takes no orders now
    ShopClosed {
        shop: &'a str,
    },
    Price {
        name: &'a str,
        unit: &'a str,
        from: Money,
        to: Money,
    },
    DeliveryFee {
        from: Money,
        to: Money,
    },
    /// A pre-order's ready date changed, or it is no pre-order any more (FR-PROD-11)
    ReadyOn {
        name: &'a str,
        ready_on: Option<NaiveDate>,
    },
}

/// A new order as its seller is told of it (FR-ORD-03): what FR-ORD-11 shows of an order yet
/// to be accepted, without the house number, phone or note.
pub struct NewOrder<'a> {
    pub lines: Vec<OrderLine<'a>>,
    /// ฿0 for pickup
    pub delivery_fee: Money,
    pub total: Money,
    pub fulfillment: order::Fulfillment,
    pub payment: order::PaymentMethod,
    /// The buyer's display name
    pub buyer: Option<&'a str>,
    /// The buyer's zone
    pub zone: Option<&'a str>,
}

/// How the seller reaches the buyer of an order they accepted (FR-ORD-11), as in Appendix A.4.
pub struct Contact<'a> {
    /// The buyer's display name
    pub buyer: Option<&'a str>,
    /// Where it is delivered, as [`address_line`] shows it; `None` for pickup
    pub address: Option<String>,
    /// The buyer's phone, its digits, if they gave one
    pub phone: Option<&'a str>,
    /// For the shop (FR-CART-04)
    pub note: Option<&'a str>,
}

/// What one party of an order sees of how to reach the other (FR-ORD-11, NFR-PRIV-04), as
/// [the order's status allows](crate::orders): cards show this and nothing else of it.
pub enum OrderContact<'a> {
    /// Nothing: the buyer's view of an order the shop is yet to accept or that is over
    None,
    /// The seller's view of an order yet to be accepted, its buyer's display name and zone, or
    /// of one over, the name only
    Buyer {
        name: Option<&'a str>,
        zone: Option<&'a str>,
    },
    /// The seller's view of an active order: how to reach its buyer, and where they shared they
    /// are, as latitude and longitude, if they did
    BuyerFull {
        contact: Contact<'a>,
        location: Option<(f64, f64)>,
    },
    /// The buyer's view of an active order: where to pick it up, as the shop gave it, the
    /// shop's phone, unless its seller hides it (FR-SHOP-08), and, while a PromptPay order is
    /// yet to be paid, the shop's PromptPay ID, its digits (FR-PAY-03)
    Shop {
        pickup: Option<&'a str>,
        location: Option<(f64, f64)>,
        phone: Option<&'a str>,
        promptpay: Option<&'a str>,
    },
}

/// An order's card in "คำสั่งซื้อของฉัน" or "ออเดอร์ร้าน" (FR-ORD-10), or in full.
pub struct OrderCard<'a> {
    pub number: &'a str,
    pub status: order::Status,
    pub placed_at: DateTime<Utc>,
    /// Its shop's name on its buyer's card; `None` on its seller's
    pub shop: Option<&'a str>,
    /// As they were ordered (DR-03)
    pub lines: Vec<OrderLine<'a>>,
    pub subtotal: Money,
    /// ฿0 for pickup
    pub delivery_fee: Money,
    pub total: Money,
    pub fulfillment: order::Fulfillment,
    pub payment: order::PaymentMethod,
    pub payment_status: order::PaymentStatus,
    /// Whether it is handed over but not paid, on its seller's card: "ค้างชำระ" (FR-PAY-07)
    pub unpaid: bool,
    pub contact: OrderContact<'a>,
    /// Its buyer's rating, once completed and rated (FR-ORD-14), on both parties' cards
    pub rating: Option<OrderRating<'a>>,
    /// What the viewer can do about it now
    pub buttons: Vec<Action>,
}

/// An order as an admin finds it by its number (FR-ADM-04, FR-ORD-12).
pub struct FoundOrder<'a> {
    pub number: &'a str,
    pub status: order::Status,
    /// Its shop's name
    pub shop: &'a str,
    /// The buyer's display name
    pub buyer: Option<&'a str>,
    pub total: Money,
    pub payment: order::PaymentMethod,
    pub payment_status: order::PaymentStatus,
    pub placed_at: DateTime<Utc>,
    /// Its buyer's rating, if they rated it (FR-ORD-14)
    pub rating: Option<OrderRating<'a>>,
}

/// What the buyer of an order paid by PromptPay transfers, and where, once it is accepted
/// (FR-PAY-03).
pub struct PromptPayDue<'a> {
    /// The shop's PromptPay ID, its digits; `None` if it took it away since the order was
    /// placed
    pub id: Option<&'a str>,
    /// The order's total, exactly
    pub total: Money,
}

/// What the AI read from a payment slip, as its seller is shown it beside what the order expects
/// (FR-PAY-06): advice, never a decision. What it couldn't read is `None`.
pub struct SlipReading<'a> {
    /// The amount transferred, and whether it is the order's total
    pub amount: Option<(Money, bool)>,
    /// When it was transferred, and whether that can be the order's transfer: not before the
    /// order was placed, nor after the slip was sent
    pub paid_at: Option<(DateTime<Utc>, bool)>,
    /// Letters and digits only
    pub reference: Option<&'a str>,
}

/// Whom a message on an order is from, or for (FR-ORD-13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderParty<'a> {
    /// Its shop, by name
    Shop(&'a str),
    /// Its buyer, by display name if LINE gave one
    Buyer(Option<&'a str>),
}

impl OrderParty<'_> {
    /// What the other party calls it: "ร้านป้าแดง", or "ลูกค้า สมชาย".
    fn called(self) -> String {
        match self {
            Self::Shop(name) => shop_called(name),
            Self::Buyer(Some(name)) => format!("ลูกค้า {name}"),
            Self::Buyer(None) => "ลูกค้า".to_owned(),
        }
    }
}

/// The reason an order is cancelled for when its seller grants the buyer's request (FR-ORD-07).
pub const BUYER_ASKED_TO_CANCEL: &str = "ลูกค้าขอยกเลิก";

/// Who a deleted account's past orders show as their buyer (FR-ONB-07)
pub const DELETED_ACCOUNT: &str = "ผู้ใช้ที่ลบบัญชีแล้ว";

/// What a shop's card shows (FR-SHOP-01): its owner's, an applicant's or an admin's view.
pub struct ShopCard<'a> {
    pub name: &'a str,
    pub description: &'a str,
    /// Its photo's `/img/p` link
    pub photo: Option<&'a Url>,
    pub pickup_address: Option<&'a str>,
    /// Whether a location is shared; the card doesn't show where (NFR-PRIV-04)
    pub pickup_location: bool,
    /// Its digits, as kept
    pub phone: Option<&'a str>,
    pub offers_pickup: bool,
    pub offers_delivery: bool,
    pub delivery_fee: Money,
    pub minimum_order: Option<Money>,
    pub promptpay: PromptPay<'a>,
    /// How an approved shop is run, for its owner; `None` for an application
    pub running: Option<Running<'a>>,
    /// Who applied and when, for the admins
    pub applicant: Option<Applicant<'a>>,
    pub buttons: Vec<Action>,
}

/// How an approved shop is run, as its owner's card shows it.
pub struct Running<'a> {
    /// Open or closed for now (FR-SHOP-05)
    pub is_open: bool,
    /// Held for review, so buyers don't see it whether open or not (FR-PROD-09)
    pub held: bool,
    /// FR-SHOP-07
    pub opening_hours: Option<&'a str>,
    /// Whether buyers with an active order see the phone (FR-SHOP-08)
    pub show_phone: bool,
}

/// The PromptPay ID as a shop's card shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptPay<'a> {
    /// To its owner: the ID, if any
    Number(Option<&'a str>),
    /// To admins: only whether there is one (NFR-PRIV-04)
    Given(bool),
}

/// Who applied to open a shop, and when, as the admins see it (FR-ADM-02).
pub struct Applicant<'a> {
    pub display_name: Option<&'a str>,
    pub at: DateTime<Utc>,
}

/// Where an admin carries on after deciding: the other applications, and the main menu.
fn admin_menus() -> QuickReply {
    QuickReply::new([
        Item::ApproveShops
            .postback()
            .button(labels::APPROVE_SHOPS.clone()),
        Action::message(labels::MENU.clone(), labels::MENU.as_str()),
    ])
}

/// A product or shop in "รายการรอตรวจ" (FR-ADM-03), as the admins see it.
pub struct ReviewCard<'a> {
    pub what: Reported<'a>,
    /// Its first photo's `/img/p` link
    pub photo: Option<&'a Url>,
    pub state: Reviewed,
    /// Its open reports, the latest first; the card shows [`REVIEW_REPORTS`] of them
    pub reports: Vec<ReviewReport<'a>>,
    pub buttons: Vec<Action>,
}

/// How a product or shop in "รายการรอตรวจ" stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reviewed {
    Product {
        status: product::Status,
        /// Its shop's
        shop: shop::Status,
    },
    Shop(shop::Status),
}

/// An open report, as a card of "รายการรอตรวจ" shows it.
pub struct ReviewReport<'a> {
    pub by: Reporter<'a>,
    pub reason: &'a str,
    pub at: DateTime<Utc>,
}

/// Who filed a report (FR-HELP-02, FR-PROD-09).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reporter<'a> {
    /// Moderation, holding what it found suspicious
    Moderation,
    /// A user, by their display name if LINE gave one
    User(Option<&'a str>),
}

/// A user, as the admins see them in "ผู้ใช้" (FR-ADM-04).
pub struct UserCard<'a> {
    pub display_name: Option<&'a str>,
    pub seller: bool,
    pub admin: bool,
    pub suspended: bool,
    /// Why, while suspended
    pub suspension_reason: Option<&'a str>,
    pub zone: Option<&'a str>,
    pub joined: DateTime<Utc>,
    pub shop: Option<UserShop<'a>>,
    pub buttons: Vec<Action>,
}

/// The shop of a user in "ผู้ใช้".
pub struct UserShop<'a> {
    pub name: &'a str,
    pub status: shop::Status,
    /// Why, while suspended
    pub suspension_reason: Option<&'a str>,
}

/// Where an admin carries on in "รายการรอตรวจ": the queue, and the main menu.
fn review_menus() -> QuickReply {
    QuickReply::new([
        Item::ReviewQueue
            .postback()
            .button(labels::REVIEW_QUEUE.clone()),
        Action::message(labels::MENU.clone(), labels::MENU.as_str()),
    ])
}

/// Where an admin carries on in "ผู้ใช้": another search, and the main menu.
fn users_menus() -> QuickReply {
    QuickReply::new([
        Item::Users.postback().button(labels::FIND_AGAIN.clone()),
        Action::message(labels::MENU.clone(), labels::MENU.as_str()),
    ])
}

/// The user called `name` as a sentence names them: ` "ป้าแดง" `, or `ผู้ใช้คนนี้` when LINE
/// gave no name.
fn user_called(name: Option<&str>) -> String {
    match name {
        Some(name) => format!(" \"{name}\" "),
        None => "ผู้ใช้คนนี้".to_owned(),
    }
}

/// How a shop in "รายการรอตรวจ" stands, and the colour that says it.
fn reviewed_shop(status: shop::Status) -> (&'static str, &'static str) {
    match status {
        shop::Status::Held => ("รอตรวจ (ผู้ซื้อยังไม่เห็นร้าน)", MUTED),
        shop::Status::Approved => ("เปิดให้ผู้ซื้อเห็นอยู่", OPEN_COLOR),
        shop::Status::Suspended => ("ถูกระงับแล้ว", CLOSED_COLOR),
        shop::Status::Closed => ("ปิดร้านถาวรแล้ว", MUTED),
        shop::Status::Pending | shop::Status::Rejected => ("ยังไม่ได้รับอนุมัติ", MUTED),
    }
}

/// Products and shops on a page of "รายการรอตรวจ". Their cards carry reports as long as a
/// report's reason may be, so fewer than a carousel holds fit its 50 KB (FR-LINE-11).
pub const REVIEWS_PER_PAGE: usize = 6;
/// The reports a card of "รายการรอตรวจ" shows, the latest; it counts the rest
pub const REVIEW_REPORTS: usize = 3;
/// Orders on a page of "คำสั่งซื้อของฉัน" or "ออเดอร์ร้าน" (FR-ORD-10). Their cards may say as
/// much as an order's address and note, so fewer than a carousel holds fit its 50 KB
/// (FR-LINE-11).
pub const ORDERS_PER_PAGE: usize = 6;
/// The lines an order's card in a list shows; it counts the rest, which its details show
pub const ORDER_LINES_ON_CARD: usize = 3;

/// A link that opens `latitude`, `longitude` on a map, in the maps app if there is one
/// (FR-ORD-11).
fn map_url(latitude: f64, longitude: f64) -> String {
    format!("https://www.google.com/maps/search/?api=1&query={latitude:.6},{longitude:.6}")
}
/// The users one search of "ผู้ใช้" shows, as many as a carousel holds (FR-LINE-11)
pub const USERS_FOUND: usize = limits::BUBBLES_PER_CAROUSEL - 2;

/// What "ข้อมูลของฉัน" shows for a value the user hasn't given
const NOT_GIVEN: &str = "ยังไม่ได้ให้ไว้";
/// What it shows for one that LINE didn't give
const NONE: &str = "ไม่มี";
/// What a shop's card shows for what its owner may set but hasn't
const NOT_SET: &str = "ไม่ได้ระบุ";

/// ` (ตอนนี้: …)`, after a prompt for a value being corrected.
fn now(current: Option<&str>) -> String {
    current
        .map(|current| format!(" (ตอนนี้: {current})"))
        .unwrap_or_default()
}

/// The address as one line, as the seller and the buyer see it: `ซอย 3 บ้านเลขที่ 45/2
/// (รั้วสีเขียว)`.
pub fn address_line(zone: &str, house_number: &str, landmark_note: Option<&str>) -> String {
    let landmark = landmark_note
        .map(|note| format!(" ({note})"))
        .unwrap_or_default();
    format!("{zone} บ้านเลขที่ {house_number}{landmark}")
}

/// A section of a card: a `title`, then its `rows`.
fn section(title: &str, rows: impl IntoIterator<Item = Component>) -> FlexBox {
    let title = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        ..Text::new(title)
    };
    FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(std::iter::once(title.into()).chain(rows))
    }
}

/// A `label` and its `value` side by side.
fn row(label: &str, value: &str) -> Component {
    let label = Text {
        size: Some(Size::Sm),
        color: Some(MUTED.to_owned()),
        flex: Some(2),
        ..Text::new(label)
    };
    let value = Text {
        size: Some(Size::Sm),
        wrap: Some(true),
        flex: Some(3),
        ..Text::new(value)
    };
    FlexBox::baseline([label.into(), value.into()]).into()
}

/// A row of a table: its `label`, then its `values` in narrower columns, aligned right; in bold
/// for a heading or a total.
fn table_row(label: &str, values: impl IntoIterator<Item = String>, bold: bool) -> Component {
    let weight = bold.then_some(Weight::Bold);
    let label = Text {
        size: Some(Size::Xs),
        weight,
        color: (!bold).then(|| MUTED.to_owned()),
        wrap: Some(true),
        flex: Some(3),
        ..Text::new(label)
    };
    let values = values.into_iter().map(|value| {
        let value = Text {
            size: Some(Size::Xs),
            weight,
            wrap: Some(true),
            align: Some(Align::End),
            flex: Some(2),
            ..Text::new(value)
        };
        value.into()
    });
    FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::horizontal(std::iter::once(label.into()).chain(values))
    }
    .into()
}

/// The rows of a table of orders by status, a column for each of `periods`: a heading, a row
/// for each status, their total, and the value of those completed.
fn orders_table(periods: &[&Orders]) -> Vec<Component> {
    let mut rows = vec![table_row(
        "สถานะ",
        periods.iter().map(|period| period_name(period.days)),
        true,
    )];
    for (i, &(status, _)) in periods[0].by_status.iter().enumerate() {
        let counts = periods.iter().map(|period| period.by_status[i].1);
        let counts = counts.map(thai::group_thousands);
        rows.push(table_row(order_status_name(status), counts, false));
    }
    let totals = periods
        .iter()
        .map(|period| thai::group_thousands(period.total()));
    rows.push(table_row("รวม", totals, true));
    let values = periods
        .iter()
        .map(|period| period.completed_value.to_string());
    rows.push(table_row("ยอดที่สำเร็จ", values, true));
    rows
}

/// A period of `days` days ending today, as figures name it.
fn period_name(days: u64) -> String {
    match days {
        1 => "วันนี้".to_owned(),
        days => format!("{days} วัน"),
    }
}

/// A line of a section on its own.
fn line(text: &str) -> Component {
    let text = Text {
        size: Some(Size::Sm),
        wrap: Some(true),
        ..Text::new(text)
    };
    text.into()
}

/// The colour of the labels of a card's rows
const MUTED: &str = "#888888";
/// The colour of what doesn't match, such as a slip's amount (FR-PAY-06)
const WARNING_COLOR: &str = CLOSED_COLOR;

/// Categories on a page of "หมวดหมู่", leaving room for "ดูเพิ่มเติม" (FR-LINE-11)
pub const CATEGORIES_PER_PAGE: usize = limits::BUBBLES_PER_CAROUSEL - 1;

/// A category's card in "หมวดหมู่": its place and name, whether buyers see it, its products,
/// and its buttons, the moves side by side above the rest.
fn category_card(card: &CategoryCard<'_>) -> Bubble {
    let title = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(format!("{}. {}", card.rank, card.name))
    };
    let status = if card.is_active {
        "เปิดใช้"
    } else {
        "ปิดใช้ (ผู้ซื้อไม่เห็น)"
    };
    let body = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical([
            row("สถานะ", status),
            row("สินค้า", &format!("{} รายการ", card.products)),
        ])
    };
    let buttons = |actions: Vec<Action>| -> Component {
        let buttons = actions.into_iter().map(|action| {
            let button = Button {
                style: Some(ButtonStyle::Secondary),
                height: Some(ButtonHeight::Sm),
                flex: Some(1),
                ..Button::new(action)
            };
            button.into()
        });
        FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::horizontal(buttons)
        }
        .into()
    };
    let moves: Vec<_> = card.up.iter().chain(&card.down).cloned().collect();
    let rows = [
        moves,
        vec![card.rename.clone()],
        vec![card.toggle.clone(), card.delete.clone()],
    ];
    let footer = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(
            rows.into_iter()
                .filter(|actions| !actions.is_empty())
                .map(buttons),
        )
    };
    Bubble {
        size: Some(BubbleSize::Kilo),
        header: Some(FlexBox::vertical([title.into()])),
        body: Some(body),
        footer: Some(footer),
        ..Bubble::default()
    }
}

/// Products on a page of a list, leaving room for "ดูเพิ่มเติม" (FR-SRCH-03)
pub const PRODUCTS_PER_PAGE: usize = 10;
/// Shops on a page of "ร้านค้าทั้งหมด", leaving room for "ดูเพิ่มเติม" (FR-SRCH-06)
pub const SHOPS_PER_PAGE: usize = 10;

/// The headline of a list of what was `browsed`: its alt text.
fn headline(browsed: Browsed<'_>) -> String {
    match browsed {
        Browsed::Category(name) => format!("สินค้าหมวด {name}"),
        Browsed::Shop(name) => format!("สินค้าของ{}", shop_called(name)),
        Browsed::NewToday => labels::NEW_TODAY.as_str().to_owned(),
        Browsed::Search(keyword) => format!("ผลการค้นหา \"{keyword}\""),
        Browsed::Shops => labels::ALL_SHOPS.as_str().to_owned(),
        Browsed::Found => "ที่ผู้ช่วยหาให้".to_owned(),
    }
}

/// A product's card in a list (FR-SRCH-07): its photo, name, price per unit, shop and whether it
/// can be ordered, with "ใส่ตะกร้า" while it can, and "ดูร้าน". A tap anywhere else shows its
/// details.
fn product_card(card: &ProductCard<'_>) -> Bubble {
    let name = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        max_lines: Some(2),
        ..Text::new(card.name)
    };
    let price = Text {
        size: Some(Size::Sm),
        wrap: Some(true),
        ..Text::new(price_per_unit(card.price, card.unit))
    };
    let shop = Text {
        size: Some(Size::Xs),
        color: Some(MUTED.to_owned()),
        wrap: Some(true),
        max_lines: Some(1),
        ..Text::new(card.shop)
    };
    let mut contents = vec![
        name.into(),
        price.into(),
        shop.into(),
        availability_line(card.availability),
    ];
    contents.extend(card.ready_on.map(|day| ready_text(day, true)));
    let body = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(contents)
    };
    let buttons: Vec<Action> = card
        .add_to_cart
        .iter()
        .chain([&card.view_shop])
        .cloned()
        .collect();
    Bubble {
        size: Some(BubbleSize::Kilo),
        hero: Some(photo(card.photo)),
        body: Some(body),
        footer: footer_of(&buttons, card.add_to_cart.is_some()),
        action: Some(card.details.clone()),
        ..Bubble::default()
    }
}

/// A product's details (FR-SRCH-07): its photo `first`, name, price per unit, whether it can be
/// ordered, description and shop, with its buttons, "ใส่ตะกร้า" primary while it can be.
fn product_details_card(details: &ProductDetails<'_>, first: &Url) -> Bubble {
    let name = Text {
        size: Some(Size::Lg),
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(details.name)
    };
    let price = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(price_per_unit(details.price, details.unit))
    };
    let mut top: Vec<Component> = vec![
        name.into(),
        price.into(),
        availability_line(details.availability),
    ];
    top.extend(details.ready_on.map(|day| ready_text(day, true)));
    if let Some(description) = details.description {
        let description = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
            ..Text::new(description)
        };
        top.push(description.into());
    }
    let body = FlexBox {
        spacing: Some(Spacing::Md),
        ..FlexBox::vertical(
            top.into_iter()
                .chain([Separator::default().into(), row("ร้าน", details.shop)]),
        )
    };
    Bubble {
        size: Some(BubbleSize::Mega),
        hero: Some(photo(first)),
        body: Some(body),
        footer: footer_of(&details.buttons, details.availability.orderable()),
        ..Bubble::default()
    }
}

/// One more of a product's photos, beside its details.
fn photo_card(url: &Url) -> Bubble {
    Bubble {
        size: Some(BubbleSize::Mega),
        hero: Some(photo(url)),
        ..Bubble::default()
    }
}

/// A product's photo at the top of its card, as large as cards show photos (20:13).
fn photo(url: &Url) -> Hero {
    Hero::Image(Image {
        size: Some(Size::Full),
        aspect_ratio: Some("20:13".to_owned()),
        aspect_mode: Some(AspectMode::Cover),
        ..Image::new(url)
    })
}

/// Whether a product can be ordered, as its card says it (FR-SRCH-07): in the colour of an
/// open shop if it can, of a closed one if not.
fn availability_line(of: Availability) -> Component {
    let color = if of.orderable() {
        OPEN_COLOR
    } else {
        CLOSED_COLOR
    };
    let text = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        color: Some(color.to_owned()),
        ..Text::new(availability(of))
    };
    text.into()
}

/// That a pre-order is ready on `day` (FR-PROD-11), under its availability on its card, wrapped
/// to fit a narrow one if `wrap`, or under its amounts on a line of a cart or order, whose wider
/// card has room for it, and when full, none to spare (FR-LINE-11).
fn ready_text(day: NaiveDate, wrap: bool) -> Component {
    let text = Text {
        size: Some(Size::Sm),
        color: Some(PRE_ORDER_COLOR.to_owned()),
        wrap: wrap.then_some(true),
        ..Text::new(ready_line(day))
    };
    text.into()
}

/// Shops on a page of "ตะกร้า", leaving room for "ดูเพิ่มเติม". A cart holds few enough lines
/// that a page of them fits LINE's 50 KB (FR-LINE-11).
pub const CART_SHOPS_PER_PAGE: usize = 10;

/// A line of the cart in a sentence: `ไข่ไก่เบอร์ 2 × 2 แผง = ฿220`, as in Appendix A.2, then
/// when a pre-order is ready (FR-PROD-11).
fn cart_line(
    name: &str,
    quantity: i32,
    unit: &str,
    total: Money,
    ready_on: Option<NaiveDate>,
) -> String {
    let ready = ready_note(ready_on);
    format!("{name} × {quantity} {unit} = {total}{ready}")
}

/// Why a line of the cart, of a product sold by `unit`, can't be ordered, as its card flags it
/// (FR-CART-03); `None` if it can.
pub fn cart_problem(state: cart::State, unit: &str) -> Option<String> {
    match state {
        cart::State::Orderable => None,
        cart::State::SoldOut => Some(availability(Availability::SoldOut)),
        cart::State::ShopClosed => Some(CLOSED.to_owned()),
        cart::State::Unavailable => Some("ตอนนี้ไม่มีขาย".to_owned()),
        cart::State::NotEnough(left) => Some(format!("เหลือแค่ {left} {unit}")),
    }
}

/// A shop's card in the cart (FR-CART-03): its name, then each line with its buttons, then the
/// subtotal of the lines that can be ordered, saying when others are left out, and
/// "สั่งซื้อร้านนี้" while any can.
fn cart_shop_card(card: &CartShop<'_>) -> Bubble {
    let name = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(shop_called(card.name))
    };
    let mut contents: Vec<Component> = vec![name.into()];
    for line in &card.lines {
        contents.push(Separator::default().into());
        contents.push(cart_line_box(line));
    }
    contents.push(Separator::default().into());
    let label = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        flex: Some(2),
        ..Text::new("รวม")
    };
    let subtotal = Text {
        weight: Some(Weight::Bold),
        align: Some(Align::End),
        flex: Some(3),
        ..Text::new(card.subtotal.to_string())
    };
    contents.push(FlexBox::baseline([label.into(), subtotal.into()]).into());
    if card.excluded {
        let note = Text {
            size: Some(Size::Xs),
            color: Some(MUTED.to_owned()),
            wrap: Some(true),
            ..Text::new("ไม่รวมรายการที่สั่งไม่ได้")
        };
        contents.push(note.into());
    }
    let body = FlexBox {
        spacing: Some(Spacing::Md),
        ..FlexBox::vertical(contents)
    };
    Bubble {
        size: Some(BubbleSize::Mega),
        body: Some(body),
        footer: footer(card.checkout.as_slice()),
        ..Bubble::default()
    }
}

/// A line on its shop's card in the cart: its name, quantity and unit price, and total, why it
/// can't be ordered if it can't, and its buttons side by side.
fn cart_line_box(line: &CartLine<'_>) -> Component {
    let mut contents = line_amounts(
        line.name,
        line.quantity,
        line.unit,
        line.price,
        line.total,
        line.ready_on,
    );
    if let Some(problem) = cart_problem(line.state, line.unit) {
        let problem = Text {
            size: Some(Size::Sm),
            weight: Some(Weight::Bold),
            color: Some(CLOSED_COLOR.to_owned()),
            wrap: Some(true),
            ..Text::new(problem)
        };
        contents.push(problem.into());
    }
    let buttons = line.buttons.iter().map(|action| {
        let button = Button {
            style: Some(ButtonStyle::Secondary),
            height: Some(ButtonHeight::Sm),
            ..Button::new(action.clone())
        };
        button.into()
    });
    let buttons = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::horizontal(buttons.collect::<Vec<_>>())
    };
    contents.push(buttons.into());
    FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(contents)
    }
    .into()
}

/// A line of the cart or of an order: the product's name, then its quantity and unit price,
/// and its total, then when it is ready if it is a pre-order (FR-PROD-11).
fn line_amounts(
    name: &str,
    quantity: i32,
    unit: &str,
    price: Money,
    total: Money,
    ready_on: Option<NaiveDate>,
) -> Vec<Component> {
    let name = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        wrap: Some(true),
        max_lines: Some(2),
        ..Text::new(name)
    };
    let each = Text {
        size: Some(Size::Sm),
        color: Some(MUTED.to_owned()),
        wrap: Some(true),
        flex: Some(3),
        ..Text::new(format!("{quantity} {unit} × {price}"))
    };
    let total = Text {
        size: Some(Size::Sm),
        align: Some(Align::End),
        flex: Some(2),
        ..Text::new(total.to_string())
    };
    let mut contents = vec![
        name.into(),
        FlexBox::baseline([each.into(), total.into()]).into(),
    ];
    contents.extend(ready_on.map(|day| ready_text(day, false)));
    contents
}

/// Checkout's summary or an order's card (FR-CART-04, FR-CART-08): the `headline`, the order's
/// number and status once placed, then its shop and lines, its subtotal, delivery fee and
/// total, how it is received and paid, the note, and `buttons`, the first one primary.
fn order_card(
    headline: &str,
    placed: Option<(&str, &str)>,
    order: &OrderDetails<'_>,
    buttons: &[Action],
) -> Bubble {
    let headline = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(headline)
    };
    let mut contents: Vec<Component> = vec![headline.into()];
    if let Some((number, status)) = placed {
        contents.push(row("เลขที่", number));
        contents.push(row("สถานะ", status));
    }
    contents.push(Separator::default().into());
    let shop = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(shop_called(order.shop))
    };
    contents.push(shop.into());
    for line in &order.lines {
        let amounts = line_amounts(
            line.name,
            line.quantity,
            line.unit,
            line.price,
            line.total,
            line.ready_on,
        );
        let line = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(amounts)
        };
        contents.push(line.into());
    }
    contents.push(Separator::default().into());
    contents.push(amount("ค่าสินค้า", order.subtotal, false));
    if order.fulfillment == order::Fulfillment::Delivery {
        contents.push(amount(
            labels::DELIVERY_FEE.as_str(),
            order.delivery_fee,
            false,
        ));
    }
    contents.push(amount("รวม", order.total, true));
    contents.push(Separator::default().into());
    let received = match &order.address {
        Some(address) => format!("ส่งที่ {address}"),
        None => fulfillment_name(order.fulfillment).to_owned(),
    };
    contents.push(row("รับของ", &received));
    contents.push(row("ชำระเงิน", payment_name(order.payment)));
    if let Some(note) = order.note {
        contents.push(row("ฝากถึงร้าน", note));
    }
    let body = FlexBox {
        spacing: Some(Spacing::Md),
        ..FlexBox::vertical(contents)
    };
    Bubble {
        size: Some(BubbleSize::Mega),
        body: Some(body),
        footer: footer(buttons),
        ..Bubble::default()
    }
}

/// An amount of an order, its `label` beside it, in bold if it is the `total`.
fn amount(label: &str, value: Money, total: bool) -> Component {
    let weight = total.then_some(Weight::Bold);
    let label = Text {
        size: Some(Size::Sm),
        weight,
        color: (!total).then(|| MUTED.to_owned()),
        flex: Some(2),
        ..Text::new(label)
    };
    let value = Text {
        size: Some(Size::Sm),
        weight,
        align: Some(Align::End),
        flex: Some(3),
        ..Text::new(value.to_string())
    };
    FlexBox::baseline([label.into(), value.into()]).into()
}

/// How an order is received, as its card says when it isn't delivered to an address shown
/// (Appendix A.2).
pub fn fulfillment_name(fulfillment: order::Fulfillment) -> &'static str {
    match fulfillment {
        order::Fulfillment::Pickup => labels::PICKUP_ONLY.as_str(),
        order::Fulfillment::Delivery => "ส่งถึงบ้าน",
    }
}

/// How an order is paid, as the buyer chose it (FR-PAY-02).
pub fn payment_name(payment: order::PaymentMethod) -> &'static str {
    match payment {
        order::PaymentMethod::Cash => labels::CASH_ON_RECEIPT.as_str(),
        order::PaymentMethod::PromptPay => labels::PROMPTPAY_TRANSFER.as_str(),
    }
}

/// How far an order's payment is (FR-PAY-08).
pub fn payment_status_name(status: order::PaymentStatus) -> &'static str {
    match status {
        order::PaymentStatus::Unpaid => "ยังไม่ชำระ",
        order::PaymentStatus::AwaitingConfirmation => "รอร้านตรวจสอบ",
        order::PaymentStatus::Paid => "ชำระแล้ว",
        order::PaymentStatus::RefundDue => "รอคืนเงิน",
        order::PaymentStatus::Refunded => labels::MARK_REFUNDED.as_str(),
    }
}

/// An order's status as the buyer sees it (Table 4.6-A).
pub fn order_status_name(status: order::Status) -> &'static str {
    match status {
        order::Status::Pending => "รอร้านยืนยัน",
        order::Status::Accepted => "ร้านรับออเดอร์แล้ว",
        order::Status::Ready => "พร้อมให้รับ",
        order::Status::Delivering => "กำลังไปส่ง",
        order::Status::Completed => "สำเร็จ",
        order::Status::Rejected => "ร้านไม่รับออเดอร์",
        order::Status::Cancelled => "ยกเลิกแล้ว",
        order::Status::Expired => "หมดเวลา",
    }
}

/// A product's card in "สินค้าของฉัน" (FR-PROD-05): its first photo, name and price per unit,
/// whether buyers see it and whether it is sold out (FR-PROD-06), its stock, category and
/// photos, and its buttons in rows, side by side.
fn my_product_card(card: &MyProductCard<'_>) -> Bubble {
    let hero = card.photo.map(|url| {
        Hero::Image(Image {
            size: Some(Size::Full),
            aspect_ratio: Some("20:13".to_owned()),
            aspect_mode: Some(AspectMode::Cover),
            ..Image::new(url)
        })
    });
    let name = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(card.name)
    };
    let price = Text {
        size: Some(Size::Sm),
        wrap: Some(true),
        ..Text::new(price_per_unit(card.price, card.unit))
    };
    let (state, color) = match card.status {
        product::Status::Active if card.stock == Some(0) => {
            (availability(Availability::SoldOut), CLOSED_COLOR)
        }
        product::Status::Active => ("ลงขายอยู่".to_owned(), OPEN_COLOR),
        product::Status::Hidden => ("ซ่อนอยู่ (ผู้ซื้อไม่เห็น)".to_owned(), MUTED),
        product::Status::Held => ("รอตรวจ (ผู้ซื้อยังไม่เห็น)".to_owned(), MUTED),
        product::Status::Removed => ("ถูกนำออก (ผู้ซื้อไม่เห็น)".to_owned(), CLOSED_COLOR),
        product::Status::Draft | product::Status::Deleted => ("ไม่ได้ลงขาย".to_owned(), MUTED),
    };
    let state = Text {
        size: Some(Size::Sm),
        weight: Some(Weight::Bold),
        color: Some(color.to_owned()),
        wrap: Some(true),
        ..Text::new(state)
    };
    let photos = match card.photos {
        0 => NONE.to_owned(),
        count => format!("{count} รูป"),
    };
    let mut rows = vec![row("จำนวน", &stock_line(card.stock, card.unit))];
    rows.extend(card.ready_on.map(ready_row));
    rows.extend([row("หมวดหมู่", card.category), row("รูปสินค้า", &photos)]);
    let details = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(rows)
    };
    let body = FlexBox {
        spacing: Some(Spacing::Md),
        ..FlexBox::vertical([
            name.into(),
            price.into(),
            state.into(),
            Separator::default().into(),
            details.into(),
        ])
    };
    let mut first = card.primary;
    let rows = card
        .buttons
        .iter()
        .filter(|row| !row.is_empty())
        .map(|row| {
            let buttons = row.iter().map(|action| {
                let style = if std::mem::take(&mut first) {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Secondary
                };
                let button = Button {
                    style: Some(style),
                    height: Some(ButtonHeight::Sm),
                    flex: Some(1),
                    ..Button::new(action.clone())
                };
                button.into()
            });
            let row: Component = FlexBox {
                spacing: Some(Spacing::Sm),
                ..FlexBox::horizontal(buttons.collect::<Vec<_>>())
            }
            .into();
            row
        });
    let footer = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(rows.collect::<Vec<_>>())
    };
    Bubble {
        size: Some(BubbleSize::Mega),
        hero,
        body: Some(body),
        footer: Some(footer),
        ..Bubble::default()
    }
}

/// "ข้อมูลของฉัน" as shown (FR-ONB-05): the personal data of the user's row, of their shop's,
/// and what other records hold about them (DR-10).
pub struct MyData<'a> {
    pub display_name: Option<&'a str>,
    pub has_picture: bool,
    pub line_user_id: Option<&'a str>,
    /// The privacy notice version consented to, and when (FR-ONB-02)
    pub consent: Option<(&'a str, DateTime<Utc>)>,
    pub zone: Option<&'a str>,
    pub house_number: Option<&'a str>,
    pub landmark_note: Option<&'a str>,
    /// Its digits, as kept
    pub phone: Option<&'a str>,
    /// Latitude and longitude
    pub location: Option<(f64, f64)>,
    /// What their shop holds, if they have one
    pub shop: Option<MyShop<'a>>,
    /// Their orders that keep a delivery address (DR-03)
    pub orders_with_address: u64,
    /// The payment slips of their orders still held (§5.4)
    pub slips: u64,
    pub retention: Retention,
    /// The buttons that correct the address, phone and location
    pub edit: Vec<Action>,
}

/// The personal data of a user's shop (FR-SHOP-01).
pub struct MyShop<'a> {
    /// Its digits, as kept
    pub phone: Option<&'a str>,
    pub promptpay_id: Option<&'a str>,
    pub pickup_address: Option<&'a str>,
    pub pickup_location: Option<(f64, f64)>,
}

/// How long records about the user are kept (§5.4), as "ข้อมูลของฉัน" says.
pub struct Retention {
    pub slip_days: u32,
    pub webhook_event_days: u32,
    pub push_log_days: u32,
    /// `None` while the AI is off, as nothing is sent to it then
    pub ai_message_days: Option<u32>,
}

/// A category as "หมวดหมู่" shows it (FR-ADM-05).
pub struct CategoryCard<'a> {
    /// Its place in the order buyers see, from 1
    pub rank: usize,
    pub name: &'a str,
    pub is_active: bool,
    /// Its products that aren't deleted
    pub products: u64,
    /// Moves it up; `None` for the first
    pub up: Option<Action>,
    /// Moves it down; `None` for the last
    pub down: Option<Action>,
    pub rename: Action,
    /// Deactivates it, or activates it again
    pub toggle: Action,
    pub delete: Action,
}

/// What the help topics say depends on (FR-HELP-01): the user's roles, and what is configured.
pub struct HelpFacts<'a> {
    /// The user owns an `APPROVED` shop
    pub seller: bool,
    pub admin: bool,
    /// The AI assistant understands typed requests (FR-AI-01)
    pub assistant: bool,
    /// The AI drafts listings from a photo and a message (FR-PROD-02)
    pub listing: bool,
    /// `shop.requireApproval`
    pub require_approval: bool,
    /// `order.pendingTimeoutMinutes`
    pub pending_minutes: u32,
    /// Whether `payment.methods` has cash, and PromptPay
    pub cash: bool,
    pub promptpay: bool,
    /// `notification.orderStatusToBuyer`
    pub status_pushes: bool,
    /// `notification.adminAlerts`
    pub admin_alerts: bool,
    /// `village.adminContact`
    pub admin_contact: Option<&'a str>,
    pub privacy: PrivacyFacts<'a>,
}

/// What a report is about (FR-HELP-02), as the reporter and the admins see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reported<'a> {
    Product { name: &'a str, shop: &'a str },
    Shop { name: &'a str },
}

impl Reported<'_> {
    /// ` สินค้า "ไข่ไก่" ของร้านป้าแดง`, or ` ร้าน "ร้านป้าแดง"`, after a verb.
    fn named(&self) -> String {
        match self {
            Self::Product { name, shop } => format!(" สินค้า \"{name}\" ของ{shop}"),
            Self::Shop { name } => format!(" ร้าน \"{name}\""),
        }
    }
}

/// The privacy notice as shown before consent (FR-ONB-02, NFR-PRIV-02).
pub struct PrivacyNotice<'a> {
    pub facts: PrivacyFacts<'a>,
    /// Whether the user consented to an earlier version, so is asked again (NFR-PRIV-01)
    pub updated: bool,
    /// The button that consents to this version
    pub accept: Action,
}

/// What the privacy notice says (NFR-PRIV-02), which help's "ความเป็นส่วนตัว" repeats
/// (FR-HELP-01).
pub struct PrivacyFacts<'a> {
    /// `privacy.noticeText`
    pub text: &'a str,
    /// `privacy.noticeVersion`
    pub version: &'a str,
    /// `privacy.noticeUrl`: the full notice
    pub url: Option<&'a str>,
    /// The AI providers what users send the AI goes to; none while the AI is off
    pub ai_providers: Vec<AiProvider>,
    /// How long AI conversations are kept (`retention.aiMessageDays`)
    pub ai_message_days: u32,
    /// How long payment slips are kept once their order ends (`retention.slipDays`); `None`
    /// while PromptPay is off, as there are no slips then
    pub slip_days: Option<u32>,
}

/// What the privacy notice says about the user's data (NFR-PRIV-02): what is collected and why,
/// how long it is kept, who receives it, and the user's rights.
pub fn privacy_lines(facts: &PrivacyFacts<'_>) -> [String; 4] {
    let ai: Vec<_> = facts
        .ai_providers
        .iter()
        .filter_map(|&provider| ai_provider_name(provider))
        .collect();
    let (my_data, delete) = (labels::MY_DATA.as_str(), labels::DELETE_ACCOUNT.as_str());

    let mut collected =
        "ข้อมูลที่เก็บ: ชื่อและรูปโปรไฟล์ LINE, ที่อยู่สำหรับส่งของ, เบอร์โทรและตำแหน่ง (ไม่บังคับ), คำสั่งซื้อ".to_owned();
    let mut kept = "ระยะเวลาเก็บ: ข้อมูลบัญชีและที่อยู่เก็บจนกว่าคุณจะลบบัญชี".to_owned();
    let mut receives = "ผู้ที่ได้รับข้อมูล: LINE ผู้ให้บริการแชตนี้".to_owned();
    if let Some(days) = facts.slip_days {
        collected.push_str(", รูปสลิปโอนเงิน");
        kept.push_str(&format!(", รูปสลิปเก็บ {days} วันหลังคำสั่งซื้อจบ"));
    }
    if !ai.is_empty() {
        let (names, days) = (ai.join(" และ "), facts.ai_message_days);
        collected.push_str(", ข้อความและรูปที่ส่งถึงผู้ช่วย AI");
        kept.push_str(&format!(", ข้อความที่คุยกับผู้ช่วย AI เก็บ {days} วัน"));
        receives.push_str(&format!(
            ", {names} ผู้ให้บริการ AI ซึ่งได้รับเฉพาะข้อความและรูปที่จำเป็นต่อการตอบ"
        ));
    }
    collected.push_str(" เพื่อใช้ซื้อขายและส่งของกันในหมู่บ้าน");
    kept.push_str(", ประวัติคำสั่งซื้อเก็บไว้ แต่ลบข้อมูลที่ระบุตัวคุณออกเมื่อลบบัญชี");
    receives.push_str(", และผู้ซื้อหรือร้านค้าในคำสั่งซื้อเดียวกัน ซึ่งเห็นเฉพาะข้อมูลสำหรับส่งของและติดต่อ");
    let rights =
        format!("สิทธิของคุณ: ดูและแก้ไขข้อมูลได้ที่ \"{my_data}\" ถอนความยินยอมและลบข้อมูลได้ที่ \"{delete}\"");
    [collected, kept, receives, rights]
}

/// The company and product an AI provider is known by, as the privacy notice names it
/// (NFR-PRIV-02). `None` for the mock provider, which sends nothing anywhere.
pub fn ai_provider_name(provider: AiProvider) -> Option<&'static str> {
    match provider {
        AiProvider::Claude => Some("Anthropic (Claude)"),
        AiProvider::Gemini => Some("Google (Gemini)"),
        AiProvider::Gpt => Some("OpenAI (GPT)"),
        AiProvider::Grok => Some("xAI (Grok)"),
        AiProvider::Mock => None,
    }
}

/// Help as a Quick Reply, for users who can use nothing else yet (FR-UI-12).
fn help_only() -> QuickReply {
    QuickReply::new([Action::message(labels::HELP.clone(), labels::HELP.as_str())])
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

/// `messages` with `choices` as the Quick Replies of the last.
pub fn with_choices(mut messages: Vec<Message>, choices: Vec<Action>) -> Vec<Message> {
    if choices.is_empty() {
        return messages;
    }
    if let Some(last) = messages.pop() {
        messages.push(last.with_quick_reply(QuickReply::new(choices)));
    }
    messages
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

pub(super) fn tab_title(tab: Tab) -> &'static Label {
    match tab {
        Tab::Buyer => &labels::BUYER,
        Tab::Seller => &labels::SELLER,
        Tab::Admin => &labels::ADMIN,
    }
}

pub(super) fn item_label(item: Item, has_shop: bool) -> &'static Label {
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

/// The label of help topic `topic` (FR-HELP-01).
pub fn topic_label(topic: Topic) -> &'static Label {
    match topic {
        Topic::Ordering => &labels::HOW_TO_ORDER,
        Topic::Selling => &labels::SELLING,
        Topic::Payment => &labels::PAYMENT,
        Topic::MyData => &labels::MY_DATA,
        Topic::Privacy => &labels::PRIVACY,
        Topic::Report => &labels::REPORT,
        Topic::Contact => &labels::CONTACT_ADMINS,
        Topic::DeleteAccount => &labels::DELETE_ACCOUNT,
        Topic::Admin => &labels::ADMIN_WORK,
    }
}

/// A pushed notification (FR-NOT-03), as in Appendix A.4: the `headline`, which is also its
/// preview in LINE's notifications and chat list, the `details` on lines of their own, and a
/// button for each thing the recipient can do about it straight away, the first one primary:
/// "ออเดอร์ใหม่ค่ะ IM-20261022-0007" with "รับออเดอร์" and "ปฏิเสธ".
pub fn notice(headline: &str, details: &[String], buttons: Vec<Action>) -> Message {
    Message::flex(headline, notice_card(headline, details, buttons))
}

/// The card of a [`notice`], for one whose preview isn't its headline.
fn notice_card(headline: &str, details: &[String], buttons: Vec<Action>) -> Bubble {
    let details: Vec<_> = details
        .iter()
        .map(|line| (line.clone(), Tone::Plain))
        .collect();
    toned_notice_card(headline, &details, buttons)
}

/// How a line of a notice's details shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Plain,
    /// Red and bold, as what doesn't match is (FR-PAY-06)
    Warning,
    /// Grey, as an aside is
    Muted,
}

/// The card of a [`notice`] whose details each show in their [`Tone`].
fn toned_notice_card(headline: &str, details: &[(String, Tone)], buttons: Vec<Action>) -> Bubble {
    let headline_text = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(headline)
    };
    let lines = details.iter().map(|(line, tone)| {
        let (color, weight) = match tone {
            Tone::Plain => (None, None),
            Tone::Warning => (Some(WARNING_COLOR), Some(Weight::Bold)),
            Tone::Muted => (Some(MUTED), None),
        };
        let text = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
            color: color.map(str::to_owned),
            weight,
            ..Text::new(line.as_str())
        };
        text.into()
    });
    let body = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical(std::iter::once(headline_text.into()).chain(lines))
    };
    let footer = (!buttons.is_empty()).then(|| {
        let buttons = buttons.into_iter().enumerate().map(|(i, action)| {
            let style = if i == 0 {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Secondary
            };
            let button = Button {
                style: Some(style),
                height: Some(ButtonHeight::Sm),
                ..Button::new(action)
            };
            button.into()
        });
        FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical(buttons)
        }
    });
    Bubble {
        body: Some(body),
        footer,
        ..Bubble::default()
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
    carousel(page.items.iter().map(card).collect(), page.next.map(more))
}

/// `cards`, a page of a list already paged, as a carousel (FR-LINE-11, FR-SRCH-03): with a
/// "ดูเพิ่มเติม" bubble last whose button sends the postback `more`, when more follow. There
/// must be room for that bubble.
pub fn carousel(mut cards: Vec<Bubble>, more: Option<String>) -> FlexContainer {
    debug_assert!(
        cards.len() < limits::BUBBLES_PER_CAROUSEL,
        "no room for ดูเพิ่มเติม"
    );
    if let Some(more) = more {
        // A carousel's bubbles must all be the same size
        let size = cards.first().and_then(|bubble| bubble.size);
        cards.push(more_bubble(
            size,
            Action::postback(labels::MORE.clone(), more),
        ));
    }
    Carousel::new(cards).into()
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
        /// Consents to the privacy notice (FR-ONB-02)
        ACCEPT = "ยอมรับ",
        /// Opens `privacy.noticeUrl` (FR-ONB-02)
        FULL_NOTICE = "อ่านฉบับเต็ม",
        /// Seeing and correcting one's personal data (FR-ONB-05)
        MY_DATA = "ข้อมูลของฉัน",
        /// Deleting one's account, which withdraws consent (FR-ONB-07, NFR-PRIV-03)
        DELETE_ACCOUNT = "ลบบัญชี",

        // The address and "ข้อมูลของฉัน" (FR-ONB-04, FR-ONB-05)
        /// Goes without an optional value
        SKIP = "ข้าม",
        /// Opens LINE's location picker
        SHARE_LOCATION = "แชร์ตำแหน่ง",
        EDIT_ADDRESS = "แก้ที่อยู่",
        EDIT_PHONE = "แก้เบอร์โทร",
        EDIT_LOCATION = "แก้ตำแหน่ง",
        /// Removes the phone number given (NFR-PRIV-04)
        REMOVE_PHONE = "ลบเบอร์โทร",
        /// Removes the location shared (NFR-PRIV-04)
        REMOVE_LOCATION = "ลบตำแหน่ง",

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

        // Categories (FR-ADM-05)
        ADD_CATEGORY = "เพิ่มหมวดหมู่",
        MOVE_UP = "เลื่อนขึ้น",
        MOVE_DOWN = "เลื่อนลง",
        RENAME = "เปลี่ยนชื่อ",
        /// Hides a category from browsing and new listings
        DEACTIVATE = "ปิดใช้งาน",
        ACTIVATE = "เปิดใช้งาน",
        DELETE = "ลบ",
        CONFIRM_DELETE = "ยืนยันลบ",

        // Statistics (FR-ADM-06)
        /// Shows the statistics again, as they are now
        REFRESH = "ดูตัวเลขล่าสุด",

        // Help topics (FR-HELP-01), with MY_DATA and DELETE_ACCOUNT
        HOW_TO_ORDER = "วิธีสั่งซื้อ",
        SELLING = "เปิดร้านและลงสินค้า",
        PAYMENT = "การชำระเงิน",
        PRIVACY = "ความเป็นส่วนตัว",
        /// Reports a product or shop (FR-HELP-02)
        REPORT = "แจ้งปัญหา",
        CONTACT_ADMINS = "ติดต่อผู้ดูแล",
        /// The admins' own topic
        ADMIN_WORK = "งานผู้ดูแล",

        // Common reasons for a report, sent as typed (FR-HELP-02)
        REASON_WRONG_INFO = "ข้อมูลไม่ตรง",
        REASON_PROHIBITED = "สินค้าต้องห้าม",
        REASON_SCAM = "หลอกลวง/โกง",
        REASON_RUDE = "ไม่สุภาพ",

        // Shop applications (FR-SHOP-01)
        /// Sends the application for an admin to approve
        SUBMIT_APPLICATION = "ส่งคำขอ",
        /// Sends it when no approval is needed (FR-SHOP-03)
        OPEN_NOW = "เปิดร้านเลย",
        /// Applies again once turned down (FR-SHOP-04)
        APPLY_AGAIN = "สมัครใหม่",
        TAKE_PHOTO = "ถ่ายรูป",
        CHOOSE_PHOTO = "เลือกรูป",
        /// Leaves a part as it is while correcting the application
        KEEP = "ใช้ของเดิม",
        /// Removes an optional part while correcting it, or a line from the cart (FR-CART-03)
        REMOVE = "ลบออก",
        USE_HOME_ADDRESS = "ใช้ที่อยู่บ้าน",
        USE_HOME_LOCATION = "ใช้ตำแหน่งบ้าน",
        PICKUP_ONLY = "รับเองที่ร้าน",
        DELIVERY_ONLY = "ร้านไปส่ง",
        PICKUP_AND_DELIVERY = "ได้ทั้งสองแบบ",
        NO_DELIVERY_FEE = "ไม่มีค่าส่ง",
        NO_MINIMUM = "ไม่มีขั้นต่ำ",
        // The parts of an application, to correct one
        SHOP_NAME = "ชื่อร้าน",
        SHOP_DESCRIPTION = "คำอธิบาย",
        SHOP_PHOTO = "รูปร้าน",
        PICKUP_ADDRESS = "ที่รับของ",
        PICKUP_LOCATION = "ตำแหน่งร้าน",
        SHOP_PHONE = "เบอร์ร้าน",
        FULFILLMENT = "การรับส่ง",
        DELIVERY_FEE = "ค่าส่ง",
        MINIMUM_ORDER = "ยอดขั้นต่ำ",
        PROMPTPAY = "พร้อมเพย์",

        // Running a shop (FR-SHOP-05 – FR-SHOP-09)
        /// Closes the shop for now; its products stay visible (FR-SHOP-05)
        CLOSE_SHOP = "ปิดร้าน",
        REOPEN_SHOP = "เปิดร้าน",
        /// A part of "ตั้งค่าร้าน" (FR-SHOP-07)
        OPENING_HOURS = "เวลาเปิดร้าน",
        /// Whether buyers with an active order see the shop's phone (FR-SHOP-08)
        HIDE_PHONE = "ซ่อนเบอร์ร้าน",
        SHOW_PHONE = "แสดงเบอร์ร้าน",
        /// The owner's shop as buyers see it (FR-SHOP-09)
        SHOP_FRONT = "ดูหน้าร้าน",
        /// A shop's products, from its card (FR-SHOP-09)
        VIEW_PRODUCTS = "ดูสินค้า",

        // Browsing and searching (FR-SRCH), with BUY and SEARCH
        /// Products listed or restocked in the last day (FR-SRCH-09)
        NEW_TODAY = "ของใหม่วันนี้",
        /// The shops buyers see (FR-SRCH-06)
        ALL_SHOPS = "ร้านค้าทั้งหมด",
        /// Asks for another keyword (FR-SRCH-08)
        SEARCH_AGAIN = "ค้นหาใหม่",
        /// Puts a product in the cart (FR-SRCH-07, FR-CART-01)
        ADD_TO_CART = "ใส่ตะกร้า",
        /// A product's shop, from its card (FR-SRCH-07)
        VIEW_SHOP = "ดูร้าน",

        // The cart (FR-CART), with CART, ADD_TO_CART and REMOVE
        /// Checks out the shop of the product just put in the cart (FR-CART-01)
        ORDER_NOW = "สั่งซื้อเลย",
        VIEW_CART = "ดูตะกร้า",
        /// Back to "ซื้อของ", once a product is in the cart
        CONTINUE_SHOPPING = "เลือกซื้อต่อ",
        /// Checks out a shop's lines in the cart (FR-CART-03)
        ORDER_FROM_SHOP = "สั่งซื้อร้านนี้",
        /// Asks to type a quantity other than those offered (FR-CART-01)
        TYPE_QUANTITY = "ระบุเอง",
        CHANGE_QUANTITY = "แก้จำนวน",

        // Checkout (FR-CART-04 – FR-CART-08), with EDIT and CANCEL, as in Appendix A.2
        /// Picks the order up at the shop
        PICK_UP = "รับเอง",
        /// Has the shop deliver it; its button also shows the fee
        DELIVER = "ให้ไปส่ง",
        /// Delivers to the address shown; also, the photo the buyer sent is the slip (FR-UI-14)
        YES = "ใช่",
        CHANGE_ADDRESS = "เปลี่ยนที่อยู่",
        CASH_ON_RECEIPT = "เงินสดตอนรับของ",
        PROMPTPAY_TRANSFER = "โอนพร้อมเพย์",
        /// Leaves the shop no note
        NO_NOTE = "ไม่มี",
        /// Places the order summarized (FR-CART-05)
        CONFIRM_ORDER = "ยืนยันสั่งซื้อ",
        /// Cancels an order the shop is yet to accept (FR-CART-08, FR-ORD-06), or, by its seller
        /// or an admin, one accepted (Table 4.6-B)
        CANCEL_ORDER = "ยกเลิกออเดอร์",

        // Acting on an order (Table 4.6-B), with CANCEL_ORDER, as in Appendix A.4
        ACCEPT_ORDER = "รับออเดอร์",
        REJECT_ORDER = "ปฏิเสธ",
        /// A pickup order is ready at the shop
        MARK_READY = "พร้อมให้รับ",
        /// A delivery order is on its way
        MARK_DELIVERING = "กำลังไปส่ง",
        /// The seller handed the order over
        MARK_COMPLETED = "ส่งมอบแล้ว",
        /// The buyer asks to cancel an accepted order; the seller decides (FR-ORD-07)
        REQUEST_CANCEL = "ขอยกเลิก",
        /// The buyer confirms receipt (FR-ORD-09)
        CONFIRM_RECEIPT = "ได้รับของแล้ว",
        /// The seller goes on with an order the buyer asked to cancel (FR-ORD-07)
        KEEP_ORDER = "ไม่ยกเลิก",
        /// The seller, or an admin, records an order paid (FR-PAY-07, FR-PAY-08)
        MARK_PAID = "ได้รับเงินแล้ว",
        /// The seller, asked whether an order handed over was paid, says not yet (FR-PAY-07)
        NOT_PAID = "ยังไม่ได้รับ",
        /// The seller, or an admin, records a paid order cancelled as refunded (FR-PAY-09)
        MARK_REFUNDED = "คืนเงินแล้ว",
        /// The buyer sends the slip of their PromptPay transfer (FR-PAY-05)
        SEND_SLIP = "ส่งสลิป",
        /// The photo the buyer sent unasked isn't the slip (FR-UI-14); with YES
        NOT_SLIP = "ไม่ใช่",
        /// The buyer of an active order sends its shop a message (FR-ORD-13)
        MESSAGE_SHOP = "ส่งข้อความถึงร้าน",
        /// The seller of an active order sends its buyer a message (FR-ORD-13)
        MESSAGE_BUYER = "ส่งข้อความถึงลูกค้า",
        /// An order's card in full, from "คำสั่งซื้อของฉัน" or "ออเดอร์ร้าน" (FR-ORD-10)
        ORDER_DETAILS = "ดูรายละเอียด",
        /// The buyer of a completed order rates it, once (FR-ORD-14)
        RATE_ORDER = "ให้คะแนน",
        /// Opens where to deliver or pick up an order on a map (FR-ORD-11)
        SEE_MAP = "ดูแผนที่",
        // The filters of "ออเดอร์ร้าน" (FR-ORD-10)
        /// Orders yet to be accepted
        FILTER_NEW = "ใหม่",
        /// Orders accepted, being prepared, ready or on their way
        FILTER_ACTIVE = "กำลังดำเนินการ",
        /// Orders handed over but not paid (FR-PAY-07), or cancelled and owed back (FR-PAY-09)
        FILTER_UNPAID = "ค้างชำระ",
        /// Orders over: completed, rejected, cancelled or expired
        FILTER_DONE = "เสร็จแล้ว/ยกเลิก",
        // The reasons for rejecting or cancelling an order, sent as typed (FR-ORD-04)
        REASON_SOLD_OUT = "สินค้าหมด",
        REASON_CANT_DELIVER = "ไม่สะดวกส่ง",
        REASON_SHOP_CLOSED = "ร้านปิดแล้ว",
        /// Asks for the reason to be typed
        REASON_OTHER = "อื่นๆ",

        // Adding a product (FR-PROD-01, FR-PROD-03)
        /// Lists the product previewed; buyers see it from then on
        CONFIRM_LISTING = "ยืนยันลงขาย",
        /// Adds another product once one is listed (Appendix A.3)
        ADD_ANOTHER = "เพิ่มสินค้าอีก",
        /// Moves on from the photos sent
        NEXT = "ถัดไป",
        /// A stock of no set number
        UNLIMITED_STOCK = "ไม่จำกัด / ทำตามสั่ง",
        /// Either half of it, typed alone, means it too
        UNLIMITED = "ไม่จำกัด",
        MADE_TO_ORDER = "ทำตามสั่ง",
        // The parts of a product, to correct one; CATEGORIES is its category
        PRODUCT_PHOTOS = "รูปสินค้า",
        PRODUCT_NAME = "ชื่อสินค้า",
        PRICE = "ราคา",
        UNIT = "หน่วย",
        STOCK = "จำนวน",
        PRODUCT_DESCRIPTION = "รายละเอียด",
        /// Its ready date, which makes it a pre-order (FR-PROD-11)
        PRE_ORDER = "สั่งล่วงหน้า",
        /// Not a pre-order: ready to sell now
        NOT_PRE_ORDER = "ไม่สั่งล่วงหน้า",
        /// The first ready date offered
        TOMORROW = "พรุ่งนี้",

        // "สินค้าของฉัน" (FR-PROD-05, FR-PROD-06), with EDIT and DELETE
        /// Asks for the stock of a sold-out product, to sell it again
        RESTOCK = "เติมของ",
        EDIT_STOCK = "แก้จำนวน",
        EDIT_PRICE = "แก้ราคา",
        CHANGE_PHOTOS = "เปลี่ยนรูป",
        /// Hides a product from buyers, or shows it again
        HIDE_PRODUCT = "ซ่อนสินค้า",
        SHOW_PRODUCT = "แสดงสินค้า",

        // Approving shops (FR-ADM-02)
        APPROVE = "อนุมัติ",
        REJECT = "ไม่อนุมัติ",
        // Common reasons for turning an application down, sent as typed
        REASON_INCOMPLETE = "ข้อมูลไม่ครบ",
        REASON_NOT_RESIDENT = "ไม่ได้อยู่ในหมู่บ้าน",
        REASON_UNREACHABLE = "ติดต่อไม่ได้",

        // The review queue (FR-ADM-03), with APPROVE
        /// Takes a product off sale for good, telling its seller why
        REMOVE_PRODUCT = "นำออก",
        SUSPEND_SHOP = "ระงับร้าน",
        /// Closes the reports on a product or shop, leaving it as it is
        DISMISS = "ปิดเรื่อง",

        // Users (FR-ADM-04)
        SUSPEND_ACCOUNT = "ระงับบัญชี",
        UNSUSPEND_ACCOUNT = "ยกเลิกการระงับ",
        UNSUSPEND_SHOP = "ยกเลิกระงับร้าน",
        /// Finds another user
        FIND_AGAIN = "ค้นหาอีกครั้ง",
        // Common reasons for lifting a suspension, sent as typed
        REASON_RESOLVED = "แก้ไขแล้ว",
        REASON_CHECKED = "ตรวจสอบแล้ว",
    }

    /// The units a product is suggested to be sold by, as Quick Replies (FR-PROD-03)
    pub static UNITS: [Label; 12] = [
        Label::from_static("ชิ้น"),
        Label::from_static("กล่อง"),
        Label::from_static("ถุง"),
        Label::from_static("แพ็ค"),
        Label::from_static("กิโลกรัม"),
        Label::from_static("ขีด"),
        Label::from_static("แผง"),
        Label::from_static("หวี"),
        Label::from_static("กำ"),
        Label::from_static("จาน"),
        Label::from_static("ชุด"),
        Label::from_static("ครั้ง"),
    ];
}

#[cfg(test)]
mod tests {
    use std::slice;

    use chrono::TimeZone;
    use sea_orm::Iterable;

    use super::*;
    use crate::catalog::browse::search::KEYWORD_CHARS;
    use crate::catalog::shops::sales::TopProduct;
    use crate::line::Body;
    use crate::line::flex::{Component, Text};
    use crate::orders::relay::MESSAGE_CHARS;
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

    /// Every change a checkout's confirmation can find (FR-CART-05)
    const CHANGES: [CheckoutChange<'static>; 6] = [
        CheckoutChange::SoldOut {
            name: "ไข่ไก่เบอร์ 2"
        },
        CheckoutChange::NotEnough {
            name: "นมสด",
            left: 1,
            unit: "ขวด",
        },
        CheckoutChange::Unavailable {
            name: "ผักบุ้ง"
        },
        CheckoutChange::ShopClosed {
            shop: "ร้านป้าแดง"
        },
        CheckoutChange::Price {
            name: "ไข่ไก่เบอร์ 0",
            unit: "แผง",
            from: Money::from_baht(110),
            to: Money::from_baht(120),
        },
        CheckoutChange::DeliveryFee {
            from: Money::from_baht(10),
            to: Money::from_baht(20),
        },
    ];

    /// A pre-order's ready date changed, and another is no pre-order any more (FR-PROD-11)
    const READY_CHANGES: [CheckoutChange<'static>; 2] = [
        CheckoutChange::ReadyOn {
            name: "ขนมเปี๊ยะ",
            ready_on: NaiveDate::from_ymd_opt(2026, 11, 7),
        },
        CheckoutChange::ReadyOn {
            name: "ขนมไหว้พระจันทร์",
            ready_on: None,
        },
    ];

    /// The ready date of a pre-order in the tests, as long as a day is written: `31 ธ.ค. 2569`
    fn ready_day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
    }

    const REJECTIONS: [Rejected; 5] = [
        Rejected::TooLarge { max_mb: 10 },
        Rejected::NotJpegOrPng,
        Rejected::Unreadable,
        Rejected::Full { max: 3 },
        Rejected::Unavailable,
    ];

    fn templates(particle: PoliteParticle) -> Templates {
        Templates {
            app_name: "IconMarket".to_owned(),
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
        let egg = Reported::Product {
            name: "ไข่ไก่",
            shop: "ร้านป้าแดง",
        };
        let shop = Reported::Shop {
            name: "ร้านป้าแดง"
        };
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
            text(&t.ask_join_code("หมู่บ้านไอคอน", None)).to_owned(),
            text(&t.ask_join_code("หมู่บ้านไอคอน", Some("ลุงชัย 081-234-5678"))).to_owned(),
            text(&t.join_code_wrong(3)).to_owned(),
            text(&t.join_code_blocked(Utc.with_ymd_and_hms(2026, 10, 24, 9, 10, 0).unwrap()))
                .to_owned(),
            text(&t.ready()).to_owned(),
            text(&t.ask_zone(true, true, None)).to_owned(),
            text(&t.ask_zone(false, false, Some("ซอย 3"))).to_owned(),
            text(&t.ask_house_number(Some("45/2"))).to_owned(),
            text(&t.ask_landmark(100, None)).to_owned(),
            text(&t.ask_phone(None)).to_owned(),
            text(&t.ask_phone(Some("081-234-5678"))).to_owned(),
            text(&t.ask_location(false)).to_owned(),
            text(&t.ask_location(true)).to_owned(),
            text(&t.zone_unknown()).to_owned(),
            text(&t.text_too_long(100)).to_owned(),
            text(&t.phone_invalid(false)).to_owned(),
            text(&t.phone_invalid(true)).to_owned(),
            text(&t.location_expected(false)).to_owned(),
            text(&t.location_expected(true)).to_owned(),
            text(&t.address_saved("ซอย 3 บ้านเลขที่ 45/2")).to_owned(),
            text(&t.phone_saved(Some("081-234-5678"))).to_owned(),
            text(&t.phone_saved(None)).to_owned(),
            text(&t.location_saved(true)).to_owned(),
            text(&t.location_saved(false)).to_owned(),
            text(&t.ask_category_name(20, None)).to_owned(),
            text(&t.ask_category_name(20, Some("ขนม"))).to_owned(),
            text(&t.category_name_taken("ขนม")).to_owned(),
            text(&t.category_added("ขนม")).to_owned(),
            text(&t.category_renamed("ขนม", "ขนม/ของหวาน")).to_owned(),
            text(&t.category_deactivated("ขนม")).to_owned(),
            text(&t.category_activated("ขนม")).to_owned(),
            text(&t.confirm_delete_category("ขนม", confirm_delete())).to_owned(),
            text(&t.category_deleted("ขนม")).to_owned(),
            text(&t.category_in_use("ขนม", None)).to_owned(),
            text(&t.category_in_use("ขนม", Some(deactivate()))).to_owned(),
            text(&t.categories(&[], 0, |_| unreachable!(), add_category())).to_owned(),
            text(&t.choose_category(vec![add_category()])).to_owned(),
            text(&t.nothing_to_browse()).to_owned(),
            text(&t.ask_keyword()).to_owned(),
            text(&t.search_expired()).to_owned(),
            text(&t.photo_searched(&"ข".repeat(KEYWORD_CHARS))).to_owned(),
            text(&t.photo_not_recognized()).to_owned(),
            text(&t.ask_report_reason(&REPORTED, 300, Vec::new())).to_owned(),
            text(&t.already_reported(&REPORTED)).to_owned(),
            text(&t.report_received(&REPORTED)).to_owned(),
            text(&t.ask_shop_name(50, None)).to_owned(),
            text(&t.ask_shop_name(50, Some("ร้านป้าแดง"))).to_owned(),
            text(&t.ask_shop_description(300, Some("ไข่ไก่"))).to_owned(),
            text(&t.ask_shop_photo(false)).to_owned(),
            text(&t.ask_shop_photo(true)).to_owned(),
            text(&t.ask_pickup_address(200, true, Some(&labels::SKIP), None)).to_owned(),
            text(&t.ask_pickup_address(200, false, None, Some("ซอย 3"))).to_owned(),
            text(&t.ask_pickup_location(true, Some(&labels::SKIP), false)).to_owned(),
            text(&t.ask_pickup_location(false, None, true)).to_owned(),
            text(&t.ask_shop_phone(true, None)).to_owned(),
            text(&t.ask_shop_phone(false, Some("081-234-5678"))).to_owned(),
            text(&t.shop_phone_invalid()).to_owned(),
            text(&t.ask_fulfillment(Some("รับเองที่ร้าน"))).to_owned(),
            text(&t.ask_delivery_fee(Money::from_baht(1_000), Some("฿10"))).to_owned(),
            text(&t.ask_minimum_order(Money::from_baht(100_000), None)).to_owned(),
            text(&t.amount_too_large(Money::from_baht(1_000))).to_owned(),
            text(&t.ask_promptpay(true, &labels::SKIP, None)).to_owned(),
            text(&t.promptpay_invalid(&labels::REMOVE)).to_owned(),
            text(&t.pickup_needed()).to_owned(),
            text(&t.photo_expected(&labels::SKIP)).to_owned(),
            text(&t.shop_location_expected(None)).to_owned(),
            text(&t.shop_location_expected(Some(&labels::SKIP))).to_owned(),
            text(&t.photo_lost()).to_owned(),
            text(&t.shop_summary(&shop_card(), true, &labels::SUBMIT_APPLICATION)[0]).to_owned(),
            text(&t.shop_summary(&shop_card(), false, &labels::OPEN_NOW)[0]).to_owned(),
            text(&t.choose_part(Vec::new())).to_owned(),
            text(&t.application_sent("ร้านป้าแดง")).to_owned(),
            text(&t.shop_opened("ร้านป้าแดง")).to_owned(),
            text(&t.application_pending(
                "ร้านป้าแดง",
                Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap(),
            ))
            .to_owned(),
            text(&t.application_rejected("ร้านป้าแดง", Some("ข้อมูลไม่ครบ"), apply_again())).to_owned(),
            text(&t.application_rejected("ร้านป้าแดง", None, apply_again())).to_owned(),
            text(&t.my_shop("ร้านป้าแดง", true, Vec::new())).to_owned(),
            text(&t.my_shop("ร้านป้าแดง", false, Vec::new())).to_owned(),
            text(&t.shop_open_state("ร้านป้าแดง", false, true, reopen())).to_owned(),
            text(&t.shop_open_state("ร้านป้าแดง", false, false, reopen())).to_owned(),
            text(&t.shop_open_state("ร้านป้าแดง", true, true, close())).to_owned(),
            text(&t.shop_open_state("ร้านป้าแดง", true, false, close())).to_owned(),
            text(&t.shop_settings(Vec::new())).to_owned(),
            text(&t.shop_settings_saved()).to_owned(),
            text(&t.shop_settings_unchanged()).to_owned(),
            text(&t.ask_opening_hours(50, &labels::SKIP, None)).to_owned(),
            text(&t.ask_opening_hours(50, &labels::REMOVE, Some("ทุกวัน 06:00–10:00"))).to_owned(),
            text(&t.phone_visibility(true)).to_owned(),
            text(&t.phone_visibility(false)).to_owned(),
            text(&t.shop_front_preview(&shop_front(), close())[0]).to_owned(),
            text(&t.application_gone(apply_again())).to_owned(),
            text(&t.applications(&[], 0, |_| unreachable!())).to_owned(),
            text(&t.shop_approved("ร้านป้าแดง")).to_owned(),
            text(&t.application_decided("ร้านป้าแดง", true)).to_owned(),
            text(&t.application_decided("ร้านป้าแดง", false)).to_owned(),
            text(&t.application_changed()).to_owned(),
            text(&t.ask_rejection_reason("ร้านป้าแดง", 300, Vec::new())).to_owned(),
            text(&t.shop_rejected("ร้านป้าแดง")).to_owned(),
            text(&t.review_queue(&[], 0, |_| unreachable!())).to_owned(),
            text(&t.review_changed()).to_owned(),
            text(&t.review_handled()).to_owned(),
            text(&t.ask_removal_reason("ไข่ไก่", "ร้านป้าแดง", 300, Vec::new())).to_owned(),
            text(&t.ask_shop_suspension_reason("ร้านป้าแดง", 300, Vec::new())).to_owned(),
            text(&t.review_approved(&egg)).to_owned(),
            text(&t.review_approved(&shop)).to_owned(),
            text(&t.product_removed("ไข่ไก่")).to_owned(),
            text(&t.shop_suspended("ร้านป้าแดง")).to_owned(),
            text(&t.reports_dismissed(&egg)).to_owned(),
            text(&t.ask_user_query()).to_owned(),
            text(&t.no_user_found("ป้าแดง")).to_owned(),
            text(&t.users_found(&[], true)[0]).to_owned(),
            text(&t.ask_suspension_reason(Some("ป้าแดง"), 300, Vec::new())).to_owned(),
            text(&t.ask_suspension_reason(None, 300, Vec::new())).to_owned(),
            text(&t.ask_unsuspension_reason(Some("ป้าแดง"), 300, Vec::new())).to_owned(),
            text(&t.ask_shop_unsuspension_reason("ร้านป้าแดง", 300, Vec::new())).to_owned(),
            text(&t.user_suspended(Some("ป้าแดง"))).to_owned(),
            text(&t.user_unsuspended(None)).to_owned(),
            text(&t.shop_unsuspended("ร้านป้าแดง")).to_owned(),
            text(&t.account_already(Some("ป้าแดง"), true)).to_owned(),
            text(&t.account_already(None, false)).to_owned(),
            text(&t.shop_not_suspended()).to_owned(),
            text(&t.admin_not_suspended()).to_owned(),
            text(&t.refused(Denied::Suspended, Some("ขายของปลอม"))).to_owned(),
            text(&t.refused(Denied::ShopSuspended, Some("ขายของปลอม"))).to_owned(),
            text(&t.ask_product_photos(0, 3, false)).to_owned(),
            text(&t.ask_product_photos(0, 3, true)).to_owned(),
            text(&t.ask_product_photos(1, 3, false)).to_owned(),
            text(&t.ask_product_photos(2, 3, true)).to_owned(),
            text(&t.ask_product_photos(3, 3, true)).to_owned(),
            text(&t.listing_offered(2)).to_owned(),
            text(&t.listing_offered(0)).to_owned(),
            text(&t.ask_listing_message(0, 3)).to_owned(),
            text(&t.ask_listing_message(1, 3)).to_owned(),
            text(&t.ask_listing_message(3, 3)).to_owned(),
            text(&t.listing_draft_offered(2)).to_owned(),
            text(&t.listing_draft_offered(0)).to_owned(),
            text(&t.listing_drafted_partly()).to_owned(),
            text(&t.listing_not_understood()).to_owned(),
            text(&t.listing_not_drafted(true)).to_owned(),
            text(&t.listing_not_drafted(false)).to_owned(),
            text(&t.product_photo_added(2, 3)).to_owned(),
            text(&t.product_photos_full(3)).to_owned(),
            text(&t.product_photos_lost(&labels::SKIP)).to_owned(),
            text(&t.ask_product_name(80, None)).to_owned(),
            text(&t.ask_product_name(80, Some("ไข่ไก่เบอร์ 2"))).to_owned(),
            text(&t.ask_product_price(Money::from_baht(100_000), Some("฿110"))).to_owned(),
            text(&t.price_too_low(Money::from_satang(1))).to_owned(),
            text(&t.ask_product_unit(20, Some("แผง"))).to_owned(),
            text(&t.ask_product_category(Some("ไข่/นม"))).to_owned(),
            text(&t.category_inactive()).to_owned(),
            text(&t.no_categories()).to_owned(),
            text(&t.ask_product_stock(99_999, "แผง", Some("30 แผง"))).to_owned(),
            text(&t.stock_too_large(99_999)).to_owned(),
            text(&t.ask_product_description(500, &labels::SKIP, None)).to_owned(),
            text(&t.ask_product_description(500, &labels::REMOVE, Some("ไข่สด"))).to_owned(),
            text(&t.ask_product_ready_on(ready_day(), None)).to_owned(),
            text(&t.ask_product_ready_on(ready_day(), Some("5 พ.ย. 2569"))).to_owned(),
            text(&t.ready_day_unknown()).to_owned(),
            text(&t.ready_day_too_soon()).to_owned(),
            text(&t.ready_day_too_late(ready_day())).to_owned(),
            text(&t.product_preview(&product_preview())[0]).to_owned(),
            text(
                &t.product_preview(&ProductPreview {
                    drafted: true,
                    ..product_preview()
                })[0],
            )
            .to_owned(),
            text(&t.product_listed("ไข่ไก่เบอร์ 2", Vec::new())).to_owned(),
            text(&t.product_flow_gone(add_product())).to_owned(),
            text(&t.my_products(&[], 0, |_| unreachable!(), add_product())).to_owned(),
            text(&t.editing_product("ไข่ไก่เบอร์ 2")).to_owned(),
            text(&t.product_price_saved(Money::from_baht(120), "แผง")).to_owned(),
            text(&t.product_stock_saved(Some(30), "แผง", true)).to_owned(),
            text(&t.product_stock_saved(Some(0), "แผง", false)).to_owned(),
            text(&t.product_stock_saved(None, "แผง", false)).to_owned(),
            text(&t.product_saved()).to_owned(),
            text(&t.product_unchanged()).to_owned(),
            text(&t.confirm_delete_product("ไข่ไก่เบอร์ 2", confirm_delete())).to_owned(),
            text(&t.confirm_product_update(&product_update(), confirm_update())).to_owned(),
            text(&t.product_update_changed()).to_owned(),
            text(&t.product_not_updated(Some("ขนมครก"))).to_owned(),
            text(&t.product_not_updated(None)).to_owned(),
            text(&t.quick_update_not_found(my_products())).to_owned(),
            text(&t.product_deleted("ไข่ไก่เบอร์ 2", true)).to_owned(),
            text(&t.product_deleted("ไข่ไก่เบอร์ 2", false)).to_owned(),
            text(&t.ask_cart_quantity("ไข่ไก่เบอร์ 2", "แผง", None, 0, false)).to_owned(),
            text(&t.ask_cart_quantity("ไข่ไก่เบอร์ 2", "แผง", None, 2, false)).to_owned(),
            text(&t.ask_cart_quantity("ไข่ไก่เบอร์ 2", "แผง", None, 2, true)).to_owned(),
            text(&t.ask_cart_quantity("ขนมเปี๊ยะ", "กล่อง", Some(ready_day()), 0, false)).to_owned(),
            text(&t.ask_typed_quantity(8, "แผง")).to_owned(),
            text(&t.ask_typed_quantity(1, "แผง")).to_owned(),
            text(&t.quantity_too_many("ไข่ไก่เบอร์ 2", "แผง", Some(8), 99, 0)).to_owned(),
            text(&t.quantity_too_many("ไข่ไก่เบอร์ 2", "แผง", Some(8), 99, 2)).to_owned(),
            text(&t.quantity_too_many("ไข่ไก่เบอร์ 2", "แผง", Some(8), 99, 8)).to_owned(),
            text(&t.quantity_too_many("ไข่ไก่เบอร์ 2", "แผง", None, 99, 0)).to_owned(),
            text(&t.quantity_too_small("แผง")).to_owned(),
            text(&t.own_product()).to_owned(),
            text(&t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::SoldOut)).to_owned(),
            text(&t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::ShopClosed)).to_owned(),
            text(&t.cart_full(20)).to_owned(),
            text(&t.added_to_cart("ไข่ไก่เบอร์ 2", 2, "แผง", Money::from_baht(220), None, false))
                .to_owned(),
            text(&t.added_to_cart("ไข่ไก่เบอร์ 2", 5, "แผง", Money::from_baht(550), None, true))
                .to_owned(),
            text(&t.added_to_cart(
                "ขนมเปี๊ยะ",
                2,
                "กล่อง",
                Money::from_baht(100),
                Some(ready_day()),
                false,
            ))
            .to_owned(),
            text(&t.quantity_changed("ไข่ไก่เบอร์ 2", 3, "แผง", Money::from_baht(330), None))
                .to_owned(),
            text(&t.removed_from_cart("ไข่ไก่เบอร์ 2")).to_owned(),
            text(&t.empty_cart(Vec::new())).to_owned(),
            text(&t.ask_pickup_or_delivery()).to_owned(),
            text(&t.confirm_delivery_address("ซอย 3 บ้านเลขที่ 45/2", false)).to_owned(),
            text(&t.confirm_delivery_address("ซอย 3 บ้านเลขที่ 45/2", true)).to_owned(),
            text(&t.ask_payment_method()).to_owned(),
            text(&t.ask_order_note()).to_owned(),
            text(&t.own_shop_order()).to_owned(),
            text(&t.nothing_to_order("ร้านป้าแดง")).to_owned(),
            text(&t.below_minimum_order("ร้านป้าแดง", Money::from_baht(100), Money::from_baht(55)))
                .to_owned(),
            text(&t.too_many_to_order(20)).to_owned(),
            text(&t.no_payment_method("ร้านป้าแดง")).to_owned(),
            text(&t.checkout_options_changed()).to_owned(),
            text(&t.checkout_changed(&CHANGES, true)).to_owned(),
            text(&t.checkout_changed(&CHANGES, false)).to_owned(),
            text(&t.checkout_changed(&[], true)).to_owned(),
            text(&t.checkout_changed(&READY_CHANGES, true)).to_owned(),
            text(&t.checkout_gone()).to_owned(),
        ];
        for (visible, changed) in [(true, true), (true, false), (false, true), (false, false)] {
            let visibility = t.product_visibility("ไข่ไก่เบอร์ 2", visible, changed);
            texts.push(text(&visibility).to_owned());
        }
        for browsed in BROWSED {
            texts.push(text(&t.nothing_found(browsed, Vec::new())).to_owned());
        }
        texts.extend(ERRORS.map(|error| t.number_problem(error)));
        texts.extend(REJECTIONS.map(|rejected| t.image_rejected(rejected)));
        texts.extend(DENIALS.map(|denied| text(&t.denied(denied)).to_owned()));
        texts.extend(order_texts(t));
        // Without the AI's comment, which is the AI's to write
        let summary = t.daily_summary_notice(summary_time(), &summary_day(), None, Vec::new());
        texts.push(notice_lines(&summary).join("\n"));
        texts
    }

    /// Every text of acting on an order (Table 4.6-B), a notice's lines joined. The order
    /// number has no Latin letters, for [`errors_have_no_technical_text`].
    fn order_texts(t: &Templates) -> Vec<String> {
        use order::Status::{
            Accepted, Cancelled, Completed, Delivering, Expired, Pending, Ready, Rejected,
        };
        let (number, shop) = ("20261022-0007", "ร้านป้าแดง");
        let mut notices = vec![
            t.new_order_notice(number, &new_order(order::Fulfillment::Delivery), Vec::new()),
            t.new_order_notice(number, &new_order(order::Fulfillment::Pickup), Vec::new()),
            t.order_accepted(number, &contact(true), Vec::new()),
            t.order_accepted(number, &contact(false), Vec::new()),
            t.order_accepted_notice(shop, number, None, Vec::new()),
            t.order_accepted_notice(shop, number, Some(&due(Some("0812345678"))), Vec::new()),
            t.order_accepted_notice(shop, number, Some(&due(None)), Vec::new()),
            t.ask_paid(number, Money::from_baht(230), false, Vec::new()),
            t.ask_paid(number, Money::from_baht(230), true, Vec::new()),
            t.left_unpaid(number, Vec::new()),
            t.slip_sent(number, Vec::new()),
            t.slip_received_notice(
                number,
                Money::from_baht(230),
                Some("สมชาย"),
                24,
                None,
                vec![],
            ),
            t.slip_received_notice(number, Money::from_baht(230), None, 24, None, vec![]),
            t.slip_not_received(number, Vec::new()),
            t.slip_not_received_notice(shop, number, Money::from_baht(230), Vec::new()),
            t.order_rejected_notice(shop, number, "สินค้าหมด", Vec::new()),
            t.order_expired_notice(shop, number, 60, Vec::new()),
            t.order_cancelled_by_buyer_notice(number),
            t.order_cancelled_by_admin_notice(number, "สินค้าหมด", false, Vec::new()),
            t.order_cancelled_by_admin_notice(number, "สินค้าหมด", true, Vec::new()),
            t.order_expired_seller_notice(number, 60),
            t.cancellation_requested_notice(number, Vec::new()),
            t.cancellation_kept_notice(shop, number, Vec::new()),
            t.cancellation_kept(number, Vec::new()),
        ];
        for status in [Ready, Delivering, Completed] {
            notices.push(t.order_progress_notice(shop, number, status, Vec::new()));
        }
        for reading in slip_readings() {
            let total = Money::from_baht(230);
            let notice = t.slip_received_notice(number, total, None, 24, Some(&reading), vec![]);
            notices.push(notice);
        }
        for (by_admin, refund) in [(false, false), (false, true), (true, false), (true, true)] {
            let reason = "ไม่สะดวกส่ง";
            let cancelled =
                t.order_cancelled_notice(shop, number, by_admin, reason, refund, vec![]);
            notices.push(cancelled);
        }
        for (status, by, refund) in [
            (Ready, ActorType::Seller, false),
            (Delivering, ActorType::Seller, false),
            (Completed, ActorType::Seller, false),
            (Completed, ActorType::Buyer, false),
            (Rejected, ActorType::Seller, false),
            (Cancelled, ActorType::Buyer, false),
            (Cancelled, ActorType::Seller, false),
            (Cancelled, ActorType::Seller, true),
            (Cancelled, ActorType::Admin, false),
            (Cancelled, ActorType::Admin, true),
        ] {
            notices.push(t.order_moved(number, status, by, refund, Vec::new()));
        }
        for status in [
            Pending, Accepted, Ready, Delivering, Completed, Rejected, Cancelled, Expired,
        ] {
            notices.push(t.order_now(number, status, None, Vec::new()));
        }
        for payment in order::PaymentStatus::iter() {
            notices.push(t.order_now(number, Completed, Some(payment), Vec::new()));
        }
        for (status, by_admin) in [
            (order::PaymentStatus::Paid, false),
            (order::PaymentStatus::Paid, true),
            (order::PaymentStatus::Refunded, false),
            (order::PaymentStatus::Refunded, true),
        ] {
            notices.push(t.payment_recorded(number, status, Vec::new()));
            let told = t.payment_recorded_notice(shop, number, status, by_admin, Vec::new());
            notices.push(told);
        }
        let mut texts: Vec<String> = notices
            .iter()
            .map(|notice| notice_lines(notice).join("\n"))
            .collect();
        for message in [
            t.cancellation_requested(number, false),
            t.cancellation_requested(number, true),
            t.ask_order_reason(number, Rejected, false, Vec::new()),
            t.ask_order_reason(number, Cancelled, false, Vec::new()),
            t.ask_order_reason(number, Cancelled, true, Vec::new()),
            t.ask_other_order_reason(300),
            t.my_orders(&[], None, Vec::new()),
            t.ask_slip(number, Money::from_baht(230)),
            t.is_it_the_slip(number, Money::from_baht(230)),
            t.not_a_slip(),
        ] {
            texts.push(text(&message).to_owned());
        }
        for filter in [
            &labels::FILTER_NEW,
            &labels::FILTER_ACTIVE,
            &labels::FILTER_UNPAID,
            &labels::FILTER_DONE,
        ] {
            texts.extend(
                t.shop_orders(filter, &[], None, Vec::new())
                    .iter()
                    .map(|m| text(m).to_owned()),
            );
        }
        texts
    }

    /// A new order of two lines, received by `fulfillment`.
    fn new_order(fulfillment: order::Fulfillment) -> NewOrder<'static> {
        NewOrder {
            lines: vec![
                OrderLine {
                    name: "ไข่ไก่เบอร์ 2",
                    unit: "แผง",
                    price: Money::from_baht(110),
                    quantity: 2,
                    total: Money::from_baht(220),
                    ready_on: None,
                },
                OrderLine {
                    name: "ขนมครก",
                    unit: "กล่อง",
                    price: Money::from_baht(20),
                    quantity: 1,
                    total: Money::from_baht(20),
                    ready_on: None,
                },
            ],
            delivery_fee: Money::from_baht(10),
            total: Money::from_baht(250),
            fulfillment,
            payment: order::PaymentMethod::Cash,
            buyer: Some("สมชาย"),
            zone: Some("ซอย 3"),
        }
    }

    /// How to reach สมชาย, at their address if `delivered`.
    fn contact(delivered: bool) -> Contact<'static> {
        Contact {
            buyer: Some("สมชาย"),
            address: delivered.then(|| address_line("ซอย 3", "45/2", Some("รั้วสีเขียว"))),
            phone: Some("0812345678"),
            note: Some("ไม่ใส่ถุง"),
        }
    }

    /// ฿230 to transfer to PromptPay ID `id`, or to ask the shop for if `None`.
    fn due(id: Option<&str>) -> PromptPayDue<'_> {
        PromptPayDue {
            id,
            total: Money::from_baht(230),
        }
    }

    /// 25 ต.ค. 2569 09:12 in Bangkok
    fn transferred_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 2, 12, 0).unwrap()
    }

    /// What the AI may read from a slip of ฿230 (FR-PAY-06): all of it, matching; the wrong
    /// amount at the wrong time; no amount
    fn slip_readings() -> [SlipReading<'static>; 3] {
        [
            SlipReading {
                amount: Some((Money::from_baht(230), true)),
                paid_at: Some((transferred_at(), true)),
                reference: Some("016298091203"),
            },
            SlipReading {
                amount: Some((Money::from_satang(20_050), false)),
                paid_at: Some((transferred_at(), false)),
                reference: None,
            },
            SlipReading {
                amount: None,
                paid_at: None,
                reference: None,
            },
        ]
    }

    /// The lines of a notice, each with its colour and whether it is bold.
    fn toned_lines(message: &Message) -> Vec<(&str, Option<&str>, bool)> {
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &message.body
        else {
            panic!("not a bubble");
        };
        let lines = card.body.as_ref().unwrap().contents.iter();
        lines
            .map(|component| match component {
                Component::Text(text) => (
                    text.text.as_str(),
                    text.color.as_deref(),
                    text.weight == Some(Weight::Bold),
                ),
                other => panic!("not text: {other:?}"),
            })
            .collect()
    }

    /// The lines of a notice, headline first, whether it has buttons or not.
    fn notice_lines(message: &Message) -> Vec<&str> {
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &message.body
        else {
            panic!("not a bubble");
        };
        let lines = card.body.as_ref().unwrap().contents.iter();
        lines
            .map(|component| match component {
                Component::Text(text) => text.text.as_str(),
                other => panic!("not text: {other:?}"),
            })
            .collect()
    }

    const BROWSED: [Browsed<'static>; 6] = [
        Browsed::Category("ขนม/ของหวาน"),
        Browsed::Shop("ร้านป้าแดง"),
        Browsed::NewToday,
        Browsed::Search("ขนมครก"),
        Browsed::Shops,
        Browsed::Found,
    ];

    const REPORTED: Reported<'static> = Reported::Product {
        name: "ไข่ไก่เบอร์ 2",
        shop: "ร้านป้าแดง",
    };

    fn confirm_delete() -> Action {
        Action::postback(
            labels::CONFIRM_DELETE.clone(),
            "confirm?k=category.delete&pa=1",
        )
    }

    fn confirm_update() -> Action {
        Action::postback(labels::CONFIRM.clone(), "confirm?k=product.update&pa=1")
    }

    fn my_products() -> Action {
        Action::postback(labels::MY_PRODUCTS.clone(), "my_products")
    }

    /// FR-PROD-10's *ขนมครก เหลือ 10*, and a new price with it
    fn product_update() -> ProductUpdate<'static> {
        ProductUpdate {
            name: "ขนมครก",
            unit: "กล่อง",
            price: Some((Money::from_baht(20), Money::from_baht(25))),
            stock: Some((Some(30), Some(10))),
        }
    }

    fn deactivate() -> Action {
        Action::postback(labels::DEACTIVATE.clone(), "categories.deactivate?c=1")
    }

    fn add_category() -> Action {
        Action::postback(labels::ADD_CATEGORY.clone(), "categories.add")
    }

    fn apply_again() -> Action {
        Action::postback(labels::APPLY_AGAIN.clone(), "open_shop.again")
    }

    /// Appendix A.5's shop, as its owner sees it.
    fn shop_card() -> ShopCard<'static> {
        ShopCard {
            name: "ร้านป้าแดง",
            description: "ไข่ไก่ ผักสวนครัว",
            photo: None,
            pickup_address: Some("ซอย 3 บ้านเลขที่ 45/2"),
            pickup_location: false,
            phone: Some("0812345678"),
            offers_pickup: true,
            offers_delivery: true,
            delivery_fee: Money::from_baht(10),
            minimum_order: None,
            promptpay: PromptPay::Number(None),
            running: None,
            applicant: None,
            buttons: Vec::new(),
        }
    }

    /// Appendix A.5's shop, as buyers see it.
    fn add_product() -> Action {
        Action::postback(labels::ADD_PRODUCT.clone(), "add_product")
    }

    fn product_preview() -> ProductPreview<'static> {
        ProductPreview {
            name: "ไข่ไก่เบอร์ 2",
            price: Money::from_baht(110),
            unit: "แผง",
            category: "ไข่/นม",
            stock: Some(30),
            description: None,
            ready_on: None,
            photo: None,
            photos: 0,
            drafted: false,
            buttons: Vec::new(),
        }
    }

    fn shop_front() -> ShopFront<'static> {
        ShopFront {
            name: "ร้านป้าแดง",
            description: "ไข่ไก่ ผักสวนครัว",
            photo: None,
            is_open: true,
            opening_hours: None,
            offers_pickup: true,
            offers_delivery: true,
            delivery_fee: Money::from_baht(10),
            minimum_order: None,
            rating: None,
            products: Action::postback(labels::VIEW_PRODUCTS.clone(), "browse.shop?shop=1"),
        }
    }

    fn close() -> Action {
        Action::postback(labels::CLOSE_SHOP.clone(), "shop.open?to=0")
    }

    fn reopen() -> Action {
        Action::postback(labels::REOPEN_SHOP.clone(), "shop.open?to=1")
    }

    /// The texts of `card`'s body, its rows' labels and values among them.
    fn card_texts(card: &Bubble) -> Vec<String> {
        fn texts(component: &Component, out: &mut Vec<String>) {
            match component {
                Component::Text(text) => out.push(text.text.clone()),
                Component::Box(flex_box) => {
                    for component in &flex_box.contents {
                        texts(component, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for component in &card.body.as_ref().unwrap().contents {
            texts(component, &mut out);
        }
        out
    }

    /// FR-ORD-14: a shop's card shows its average, to one decimal, and how many rated it, only
    /// once it has one to show.
    #[test]
    fn a_shops_card_shows_its_average_rating() {
        let t = templates(PoliteParticle::Kha);
        let texts = card_texts(&t.shop_front_bubble(&shop_front()));
        assert!(!texts.contains(&"คะแนน".to_owned()));
        let rated = ShopFront {
            rating: Some(ShopRating {
                tenths: 47,
                count: 1_234,
            }),
            ..shop_front()
        };
        let texts = card_texts(&t.shop_front_bubble(&rated));
        let at = texts.iter().position(|text| text == "คะแนน").unwrap();
        assert_eq!(texts[at + 1], "★ 4.7 (1,234 รีวิว)");
        let whole = ShopRating {
            tenths: 50,
            count: 3,
        };
        assert_eq!(whole.average(), "5.0");
    }

    /// FR-ORD-14: a rating shows its stars of 5, then its comment quoted, if there is one.
    #[test]
    fn a_rating_shows_its_stars_and_comment() {
        assert_eq!(stars_line(4), "★★★★☆");
        assert_eq!(stars_line(1), "★☆☆☆☆");
        assert_eq!(stars_label(3).as_str(), "★★★");
        let rated = OrderRating {
            stars: 5,
            comment: Some("ไข่สดมากค่ะ"),
        };
        assert_eq!(rating_line(rated), "★★★★★\n\"ไข่สดมากค่ะ\"");
        let silent = OrderRating {
            stars: 2,
            comment: None,
        };
        assert_eq!(rating_line(silent), "★★☆☆☆");
    }

    /// Appendix A.5: a shop is named as a sentence names it.
    #[test]
    fn a_shop_is_named_as_a_sentence_names_it() {
        assert_eq!(shop_called("ร้านป้าแดง"), "ร้านป้าแดง");
        assert_eq!(shop_called("ขนมบ้านสวน"), "ร้านขนมบ้านสวน");
        assert_eq!(shop_called("Baan Suan"), "ร้าน Baan Suan");
        let t = templates(PoliteParticle::Kha);
        assert_eq!(text(&t.shop_approved("ร้านป้าแดง")), "อนุมัติร้านป้าแดงแล้วค่ะ");
    }

    /// FR-SHOP-01, NFR-PRIV-04: a shop's card says how it sells; its owner sees the PromptPay
    /// ID, the admins only whether there is one, with who applied and when.
    #[test]
    fn a_shop_card_shows_what_its_viewer_may_see() {
        let t = templates(PoliteParticle::Kha);
        let owners = ShopCard {
            promptpay: PromptPay::Number(Some("1101700203450")),
            minimum_order: Some(Money::from_baht(50)),
            ..shop_card()
        };
        let texts = card_texts(&t.shop_bubble(&owners, None));
        let row = |label: &str| {
            let at = texts.iter().position(|text| text == label).expect(label);
            texts[at + 1].clone()
        };
        assert_eq!(texts[..2], ["ร้านป้าแดง", "ไข่ไก่ ผักสวนครัว"]);
        assert_eq!(row("ที่รับของ"), "ซอย 3 บ้านเลขที่ 45/2");
        assert_eq!(row("ตำแหน่ง"), "ไม่ได้แชร์");
        assert_eq!(row("เบอร์ร้าน"), "081-234-5678");
        assert_eq!(row("การรับส่ง"), "รับเองหรือให้ร้านไปส่ง ค่าส่ง ฿10");
        assert_eq!(row("ยอดขั้นต่ำ"), "฿50");
        assert_eq!(row("พร้อมเพย์"), "1-1017-00203-45-0");
        assert!(!texts.contains(&"ผู้สมัคร".to_owned()));

        let at = Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap();
        let admins = ShopCard {
            promptpay: PromptPay::Given(true),
            offers_pickup: false,
            delivery_fee: Money::ZERO,
            applicant: Some(Applicant {
                display_name: Some("ป้าแดง"),
                at,
            }),
            ..owners
        };
        let texts = card_texts(&t.shop_bubble(&admins, None));
        let row = |label: &str| {
            let at = texts.iter().position(|text| text == label).expect(label);
            texts[at + 1].clone()
        };
        assert_eq!(row("พร้อมเพย์"), "มี");
        assert_eq!(row("การรับส่ง"), "ร้านไปส่ง ส่งฟรี");
        assert_eq!(row("ผู้สมัคร"), "ป้าแดง");
        assert_eq!(row("ส่งเมื่อ"), "25 ต.ค. 2569 09:00");
        let pickup = ShopCard {
            offers_delivery: false,
            promptpay: PromptPay::Given(false),
            ..shop_card()
        };
        let texts = card_texts(&t.shop_bubble(&pickup, None));
        assert!(texts.contains(&"รับเองที่ร้าน".to_owned()));
        assert!(!texts.contains(&"มี".to_owned()));
    }

    /// FR-SHOP-05, FR-SHOP-07, FR-SHOP-08: the owner of an approved shop sees how it is run; an
    /// application shows none of it.
    #[test]
    fn an_owners_card_shows_how_the_shop_is_run() {
        let t = templates(PoliteParticle::Kha);
        let running = ShopCard {
            running: Some(Running {
                is_open: false,
                held: false,
                opening_hours: Some("ทุกวัน 06:00–10:00"),
                show_phone: false,
            }),
            ..shop_card()
        };
        let texts = card_texts(&t.shop_bubble(&running, None));
        let row = |label: &str| {
            let at = texts.iter().position(|text| text == label).expect(label);
            texts[at + 1].clone()
        };
        assert_eq!(row("สถานะร้าน"), "ปิดอยู่ (ผู้ซื้อสั่งไม่ได้)");
        assert_eq!(row("เวลาเปิด"), "ทุกวัน 06:00–10:00");
        assert_eq!(row("แสดงเบอร์"), "ซ่อนจากผู้ซื้อ");
        let open = ShopCard {
            running: Some(Running {
                is_open: true,
                held: false,
                opening_hours: None,
                show_phone: true,
            }),
            ..shop_card()
        };
        let texts = card_texts(&t.shop_bubble(&open, None));
        assert_eq!(texts[2..6], ["สถานะร้าน", "เปิดอยู่", "เวลาเปิด", "ไม่ได้ระบุ"]);
        assert!(texts.contains(&"ผู้ซื้อที่มีออเดอร์เห็น".to_owned()));
        let texts = card_texts(&t.shop_bubble(&shop_card(), None));
        for label in ["สถานะร้าน", "เวลาเปิด", "แสดงเบอร์"]
        {
            assert!(!texts.contains(&label.to_owned()), "{label}");
        }
    }

    /// FR-SRCH-07: a product's card says whether it can be ordered.
    #[test]
    fn availability_is_said_as_a_product_card_says_it() {
        assert_eq!(availability(Availability::Left(5)), "เหลือ 5");
        assert_eq!(availability(Availability::Orderable), "สั่งได้");
        assert_eq!(availability(Availability::SoldOut), "หมด");
        assert_eq!(availability(Availability::ShopClosed), "ร้านปิด");
    }

    /// FR-LINE-11: "ตั้งค่าร้าน" and the card buyers see fit, with the longest values a shop
    /// takes.
    #[test]
    fn running_a_shop_fits() {
        let t = templates(PoliteParticle::Khrap);
        let (name, description, hours) = ("ข".repeat(50), "ข".repeat(300), "ข".repeat(50));
        let photo = Url::parse("https://iconmarket.example.com/img/p/0123").unwrap();
        let card = ShopCard {
            name: &name,
            description: &description,
            photo: Some(&photo),
            minimum_order: Some(Money::from_satang(9_999_999)),
            running: Some(Running {
                is_open: false,
                held: false,
                opening_hours: Some(&hours),
                show_phone: false,
            }),
            ..shop_card()
        };
        let parts = (0..limits::QUICK_REPLY_ITEMS)
            .map(|_| Action::postback(labels::OPENING_HOURS.clone(), "shop.part?p=opening_hours"))
            .collect();
        limits::check(&[t.shop(&card), t.shop_settings(parts)]).unwrap();
        let front = ShopFront {
            name: &name,
            description: &description,
            photo: Some(&photo),
            is_open: false,
            opening_hours: Some(&hours),
            minimum_order: Some(Money::from_satang(9_999_999)),
            rating: Some(ShopRating {
                tenths: 50,
                count: 1_000_000,
            }),
            products: Action::postback(
                labels::VIEW_PRODUCTS.clone(),
                format!("browse.shop?shop={}", i64::MAX),
            ),
            ..shop_front()
        };
        let settings = Action::postback(labels::SHOP_SETTINGS.clone(), "shop_settings");
        limits::check(&t.shop_front_preview(&front, settings)).unwrap();
        limits::check(&[t.shop_open_state(&name, false, true, reopen())]).unwrap();
    }

    /// FR-LINE-11: the summary, the admins' alert and the owner's notices fit, with the longest
    /// values the application takes.
    #[test]
    fn shop_messages_with_everything_fit() {
        let t = templates(PoliteParticle::Khrap);
        let (name, description, address) = ("ข".repeat(50), "ข".repeat(300), "ข".repeat(200));
        let photo = Url::parse("https://iconmarket.example.com/img/p/0123").unwrap();
        let button = |label: &Label| Action::postback(label.clone(), "open_shop.submit?r=1");
        let card = ShopCard {
            name: &name,
            description: &description,
            photo: Some(&photo),
            pickup_address: Some(&address),
            pickup_location: true,
            minimum_order: Some(Money::from_satang(9_999_999)),
            promptpay: PromptPay::Number(Some("004999012345678")),
            buttons: vec![button(&labels::SUBMIT_APPLICATION), button(&labels::EDIT)],
            ..shop_card()
        };
        limits::check(&t.shop_summary(&card, true, &labels::SUBMIT_APPLICATION)).unwrap();
        limits::check(&[t.shop_application_alert(&card)]).unwrap();
        limits::check(&[t.shop(&card)]).unwrap();
        let add = Action::postback(labels::ADD_PRODUCT.clone(), "add_product");
        let reason = "ข".repeat(300);
        limits::check(&[
            t.shop_approved_notice(&name, add),
            t.shop_rejected_notice(&name, &reason, apply_again()),
        ])
        .unwrap();
    }

    /// FR-PROD-01, FR-LINE-11: the preview and what follows fit, with the longest values a
    /// product takes.
    #[test]
    fn product_messages_with_everything_fit() {
        let t = templates(PoliteParticle::Khrap);
        let (name, unit, description) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(500));
        let photo = Url::parse("https://iconmarket.example.com/img/p/0123").unwrap();
        let button = |label: &Label| {
            Action::postback(label.clone(), format!("add_product.submit?r={}", i64::MAX))
        };
        let preview = ProductPreview {
            name: &name,
            price: Money::from_baht(100_000),
            unit: &unit,
            category: "ขนม/ของหวาน",
            stock: Some(99_999),
            description: Some(&description),
            ready_on: Some(ready_day()),
            photo: Some(&photo),
            photos: 3,
            drafted: true,
            buttons: vec![button(&labels::CONFIRM_LISTING), button(&labels::EDIT)],
        };
        let messages = t.product_preview(&preview);
        limits::check(&messages).unwrap();
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &messages[1].body
        else {
            panic!("a card");
        };
        let texts = card_texts(card);
        assert_eq!(texts[1], format!("฿100,000 / {unit}"));
        assert!(texts.contains(&format!("99,999 {unit}")), "{texts:?}");
        assert!(texts.contains(&"3 รูป".to_owned()), "{texts:?}");
        let next = vec![add_product(), add_product()];
        limits::check(&[
            t.product_listed(&name, next),
            t.product_flow_gone(add_product()),
        ])
        .unwrap();
    }

    /// FR-PROD-03: how many there are, as the seller sees it.
    #[test]
    fn a_stock_is_said_with_its_unit() {
        assert_eq!(stock_line(Some(30), "กล่อง"), "30 กล่อง");
        assert_eq!(stock_line(Some(12_500), "ชิ้น"), "12,500 ชิ้น");
        assert_eq!(stock_line(Some(0), "แผง"), "0 แผง (ลูกค้าจะเห็นว่าหมด)");
        assert_eq!(stock_line(None, "จาน"), "ไม่จำกัด / ทำตามสั่ง");
        assert_eq!(
            price_per_unit(Money::from_satang(1_250), "ถุง"),
            "฿12.50 / ถุง"
        );
    }

    /// FR-PROD-10, FR-AI-04: a quick update says what it changes, from what it is, as the
    /// database has it, to what the seller asked; only what changes.
    #[test]
    fn a_quick_update_says_from_what_to_what() {
        let t = templates(PoliteParticle::Kha);
        let both = t.confirm_product_update(&product_update(), confirm_update());
        assert_eq!(
            text(&both),
            "เปลี่ยน \"ขนมครก\" ตามนี้ใช่ไหมคะ\nราคา: ฿20 / กล่อง → ฿25 / กล่อง\nจำนวน: 30 กล่อง → 10 กล่อง\nถ้าถูกต้อง กด \"ยืนยัน\" ได้เลยค่ะ"
        );
        let sold_out = ProductUpdate {
            price: None,
            stock: Some((None, Some(0))),
            ..product_update()
        };
        let sold_out = t.confirm_product_update(&sold_out, confirm_update());
        assert!(!text(&sold_out).contains("ราคา"), "{}", text(&sold_out));
        assert!(
            text(&sold_out).contains("จำนวน: ไม่จำกัด / ทำตามสั่ง → 0 กล่อง (ลูกค้าจะเห็นว่าหมด)"),
            "{}",
            text(&sold_out)
        );
        let (name, unit) = ("ข".repeat(80), "ข".repeat(20));
        let longest = ProductUpdate {
            name: &name,
            unit: &unit,
            price: Some((Money::from_baht(100_000), Money::from_satang(1))),
            stock: Some((Some(99_999), None)),
        };
        limits::check(&[t.confirm_product_update(&longest, confirm_update())]).unwrap();
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

    /// NFR-SEC-10, FR-ADM-04: a refusal says what still works and offers the menus; a
    /// suspension's, why.
    #[test]
    fn refusals_say_what_still_works_and_offer_the_menus() {
        let t = templates(PoliteParticle::Kha);
        assert!(text(&t.denied(Denied::Suspended)).contains("ยังดูคำสั่งซื้อของคุณ"));
        assert!(text(&t.denied(Denied::ShopSuspended)).contains("ยังดูแลออเดอร์ที่มีอยู่"));
        assert_eq!(
            text(&t.refused(Denied::Suspended, Some("โกงเงินมัดจำ"))),
            "ขอโทษค่ะ บัญชีของคุณถูกระงับการใช้งานชั่วคราว เหตุผล: โกงเงินมัดจำ ยังดูคำสั่งซื้อของคุณและเมนูช่วยเหลือได้ตามปกติ สอบถามได้ที่ \"ช่วยเหลือ\" แล้วเลือก \"ติดต่อผู้ดูแล\"ค่ะ"
        );
        assert!(
            text(&t.refused(Denied::ShopSuspended, Some("ขายของปลอม")))
                .starts_with("ขอโทษค่ะ ร้านของคุณถูกระงับชั่วคราว เหตุผล: ขายของปลอม จึงแก้ไข")
        );
        // Only a suspension has a reason
        assert_eq!(
            t.refused(Denied::NotFound, Some("ขายของปลอม")),
            t.denied(Denied::NotFound)
        );
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

    /// FR-NOT-03: a notification offers what the recipient can do, the first choice foremost.
    #[test]
    fn a_notice_has_its_headline_details_and_buttons() {
        let accept = Action::postback(Label::fit("รับออเดอร์"), "order.accept?o=7");
        let reject = Action::postback(Label::fit("ปฏิเสธ"), "order.reject?o=7");
        let details = [
            "ไข่ไก่เบอร์ 2 × 2 = ฿220".to_owned(),
            "ค่าส่ง ฿10 · รวม ฿230".to_owned(),
            "ส่งที่ ซอย 3 · เงินสด".to_owned(),
        ];
        let message = notice(
            "ออเดอร์ใหม่ค่ะ IM-20261022-0007",
            &details,
            vec![accept.clone(), reject.clone()],
        );
        limits::check(slice::from_ref(&message)).unwrap();

        let Body::Flex { alt_text, contents } = &message.body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "ออเดอร์ใหม่ค่ะ IM-20261022-0007");
        let FlexContainer::Bubble(card) = contents else {
            panic!("not a bubble");
        };
        let texts: Vec<_> = card
            .body
            .as_ref()
            .unwrap()
            .contents
            .iter()
            .map(|component| match component {
                Component::Text(text) => text.text.as_str(),
                other => panic!("not text: {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            [
                "ออเดอร์ใหม่ค่ะ IM-20261022-0007",
                "ไข่ไก่เบอร์ 2 × 2 = ฿220",
                "ค่าส่ง ฿10 · รวม ฿230",
                "ส่งที่ ซอย 3 · เงินสด",
            ]
        );
        let buttons: Vec<_> = card
            .footer
            .as_ref()
            .expect("buttons")
            .contents
            .iter()
            .map(|component| match component {
                Component::Button(button) => (&button.action, button.style),
                other => panic!("not a button: {other:?}"),
            })
            .collect();
        assert_eq!(
            buttons,
            [
                (&accept, Some(ButtonStyle::Primary)),
                (&reject, Some(ButtonStyle::Secondary)),
            ]
        );

        // Without buttons, no empty footer
        let plain = notice("ร้านป้าแดงรับออเดอร์แล้วค่ะ", &[], Vec::new());
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &plain.body
        else {
            panic!("not a bubble");
        };
        assert_eq!(card.footer, None);
        limits::check(&[plain]).unwrap();
    }

    /// The notice's lines, and its buttons.
    fn notice_parts(message: &Message) -> (Vec<&str>, Vec<&Action>) {
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &message.body
        else {
            panic!("not a bubble");
        };
        let lines = card.body.as_ref().unwrap().contents.iter();
        let lines = lines.map(|component| match component {
            Component::Text(text) => text.text.as_str(),
            other => panic!("not text: {other:?}"),
        });
        let buttons = card.footer.as_ref().unwrap().contents.iter();
        let buttons = buttons.map(|component| match component {
            Component::Button(button) => &button.action,
            other => panic!("not a button: {other:?}"),
        });
        (lines.collect(), buttons.collect())
    }

    fn privacy_facts<'a>(text: &'a str, url: Option<&'a str>) -> PrivacyFacts<'a> {
        PrivacyFacts {
            text,
            version: "2026-10",
            url,
            ai_providers: vec![AiProvider::Claude],
            ai_message_days: 30,
            slip_days: Some(90),
        }
    }

    fn privacy_notice(facts: PrivacyFacts<'_>) -> PrivacyNotice<'_> {
        PrivacyNotice {
            facts,
            updated: false,
            accept: Action::postback(labels::ACCEPT.clone(), "onboarding.accept?v=42"),
        }
    }

    /// FR-ONB-02, NFR-PRIV-02: the operator's text, then what is collected, how long it is
    /// kept, who receives it and the user's rights, and a button that accepts it.
    #[test]
    fn the_privacy_notice_says_what_happens_to_ones_data() {
        let t = templates(PoliteParticle::Kha);
        let facts = privacy_facts("ตลาดนี้ดูแลโดยคณะกรรมการหมู่บ้าน", None);
        let message = t.privacy_notice(&privacy_notice(facts));
        let (lines, buttons) = notice_parts(&message);
        assert_eq!(lines[0], "ประกาศความเป็นส่วนตัว");
        assert!(lines[1].contains("กด \"ยอมรับ\""));
        assert_eq!(lines[2], "ตลาดนี้ดูแลโดยคณะกรรมการหมู่บ้าน");
        assert!(lines[3].starts_with("ข้อมูลที่เก็บ:") && lines[3].contains("รูปสลิป"));
        assert!(lines[4].starts_with("ระยะเวลาเก็บ:"));
        assert!(lines[4].contains("AI เก็บ 30 วัน") && lines[4].contains("สลิปเก็บ 90 วัน"));
        assert!(lines[5].starts_with("ผู้ที่ได้รับข้อมูล: LINE"));
        assert!(lines[5].contains("Anthropic (Claude) ผู้ให้บริการ AI"));
        assert!(lines[5].contains("ผู้ซื้อหรือร้านค้าในคำสั่งซื้อเดียวกัน"));
        assert!(lines[6].contains("\"ข้อมูลของฉัน\"") && lines[6].contains("\"ลบบัญชี\""));
        assert_eq!(lines[7], "ฉบับที่ 2026-10");
        let accept = Action::postback(labels::ACCEPT.clone(), "onboarding.accept?v=42");
        assert_eq!(buttons, [&accept]);
        let help = &message.quick_reply.as_ref().unwrap().items;
        assert_eq!(help.len(), 1);
        assert_eq!(
            help[0].action,
            Action::message(labels::HELP.clone(), "ช่วยเหลือ")
        );
        limits::check(&[message]).unwrap();
    }

    #[test]
    fn the_privacy_notice_names_what_is_configured() {
        let t = templates(PoliteParticle::Kha);
        let url = "https://market.example.com/privacy";
        let both = PrivacyNotice {
            updated: true,
            ..privacy_notice(PrivacyFacts {
                ai_providers: vec![AiProvider::Gpt, AiProvider::Gemini],
                ..privacy_facts("ประกาศ", Some(url))
            })
        };
        let message = t.privacy_notice(&both);
        let (lines, buttons) = notice_parts(&message);
        assert_eq!(lines[0], "ประกาศความเป็นส่วนตัวฉบับใหม่");
        assert!(lines[1].contains("อีกครั้ง"));
        assert!(lines[5].contains("OpenAI (GPT) และ Google (Gemini) ผู้ให้บริการ AI"));
        let full = Action::Uri {
            label: labels::FULL_NOTICE.clone(),
            uri: url.to_owned(),
        };
        assert_eq!(buttons[1], &full);

        // With the AI off, or only the mock, and PromptPay off, neither is mentioned
        for ai_providers in [vec![], vec![AiProvider::Mock]] {
            let neither = privacy_notice(PrivacyFacts {
                ai_providers,
                slip_days: None,
                ..privacy_facts("ประกาศ", None)
            });
            let message = t.privacy_notice(&neither);
            let (lines, _) = notice_parts(&message);
            for line in &lines[3..] {
                assert!(!line.contains("AI") && !line.contains("สลิป"), "{line}");
            }
        }
    }

    /// The longest notice the config takes, in Thai, still fits one card (FR-LINE-11).
    #[test]
    fn the_longest_privacy_notice_fits() {
        let t = templates(PoliteParticle::Khrap);
        let longest = "ข".repeat(crate::config::NOTICE_TEXT_CHARS);
        let url = format!(
            "https://market.example.com/{}",
            "p".repeat(limits::URI_CHARS - 27)
        );
        let notice = PrivacyNotice {
            updated: true,
            ..privacy_notice(PrivacyFacts {
                ai_providers: vec![AiProvider::Claude, AiProvider::Gemini],
                ..privacy_facts(&longest, Some(&url))
            })
        };
        limits::check(&[t.privacy_notice(&notice)]).unwrap();
    }

    /// FR-ONB-01, FR-ONB-08
    #[test]
    fn the_welcome_names_the_village_and_myid_gives_the_id_alone() {
        let t = templates(PoliteParticle::Kha);
        let welcome = t.welcome(Some("ป้าแดง"), "หมู่บ้านไอคอน");
        assert!(text(&welcome).starts_with("สวัสดีค่ะ คุณป้าแดง ยินดีต้อนรับสู่ IconMarket"));
        assert!(text(&welcome).contains("ตลาดออนไลน์ของหมู่บ้านไอคอน"));
        assert!(text(&t.welcome(None, "หมู่บ้านไอคอน")).starts_with("สวัสดีค่ะ ยินดีต้อนรับ"));
        assert!(text(&t.welcome_back(None, "หมู่บ้านไอคอน")).contains("หมู่บ้านไอคอน"));

        let id = "U0123456789abcdef0123456789abcdef";
        let my_id = t.my_id(id);
        assert_eq!(text(&my_id[1]), id);
        limits::check(&my_id).unwrap();
    }

    /// FR-ONB-03: how many tries are left, then until when attempts are blocked.
    #[test]
    fn join_code_answers_say_what_is_left() {
        let t = templates(PoliteParticle::Kha);
        assert!(text(&t.join_code_wrong(2)).contains("ลองได้อีก 2 ครั้ง"));
        let until = Utc.with_ymd_and_hms(2026, 10, 24, 9, 10, 0).unwrap();
        assert!(text(&t.join_code_blocked(until)).contains("ตั้งแต่ 24 ต.ค. 2569 16:10"));
        assert!(
            text(&t.ask_join_code("หมู่บ้านไอคอน", Some("ลุงชัย 081-234-5678")))
                .contains("สอบถามได้ที่ ลุงชัย 081-234-5678")
        );
    }

    /// AC (FR-ONB-04): an invalid phone number is explained with the formats taken, and how to
    /// go without one, or remove the one given (NFR-PRIV-04).
    #[test]
    fn an_invalid_phone_is_explained_with_the_formats_taken() {
        let t = templates(PoliteParticle::Kha);
        let invalid = t.phone_invalid(false);
        let invalid = text(&invalid);
        assert!(invalid.starts_with("ขอโทษค่ะ เบอร์โทรนี้ไม่ถูกต้อง"));
        assert!(invalid.contains("มือถือ 10 หลัก") && invalid.contains("06 08 หรือ 09"));
        assert!(invalid.contains("0812345678"));
        assert!(invalid.contains("เบอร์บ้าน 9 หลัก") && invalid.contains("021234567"));
        assert!(invalid.contains("กด \"ข้าม\""));
        assert!(text(&t.phone_invalid(true)).contains("กด \"ลบเบอร์โทร\""));
    }

    /// FR-ONB-04, FR-ONB-05: the first time says why the address is asked for; a correction
    /// shows what is there now, and how to remove an optional value.
    #[test]
    fn address_prompts_say_why_and_what_is_there_now() {
        let t = templates(PoliteParticle::Kha);
        assert!(text(&t.ask_zone(true, true, None)).starts_with("ขอที่อยู่สำหรับส่งของก่อนนะคะ"));
        let correcting = t.ask_zone(false, true, Some("ซอย 3"));
        assert!(text(&correcting).starts_with("บ้านอยู่โซนไหนคะ เลือกจากปุ่ม"));
        assert!(text(&correcting).ends_with("(ตอนนี้: ซอย 3)"));
        assert!(text(&t.ask_zone(false, false, None)).contains("พิมพ์มาได้เลยค่ะ"));
        assert!(text(&t.ask_landmark(100, None)).contains("ไม่เกิน 100 ตัวอักษร"));
        assert!(text(&t.ask_phone(None)).contains("กด \"ข้าม\""));
        let phone = t.ask_phone(Some("081-234-5678"));
        assert!(text(&phone).contains("กด \"ลบเบอร์โทร\""));
        assert!(text(&phone).ends_with("(ตอนนี้: 081-234-5678)"));
        assert!(text(&t.ask_location(false)).contains("กด \"ข้าม\""));
        assert!(text(&t.ask_location(true)).contains("กด \"ลบตำแหน่ง\""));
        assert_eq!(address_line("ซอย 3", "45/2", None), "ซอย 3 บ้านเลขที่ 45/2");
        assert_eq!(
            address_line("ซอย 3", "45/2", Some("รั้วสีเขียว")),
            "ซอย 3 บ้านเลขที่ 45/2 (รั้วสีเขียว)"
        );
    }

    /// FR-ONB-05, FR-LINE-11: "ข้อมูลของฉัน" with the longest values the flows take still fits
    /// one card.
    #[test]
    fn my_data_with_everything_fits() {
        let t = templates(PoliteParticle::Khrap);
        let (long, longer) = ("ข".repeat(100), "ข".repeat(300));
        let data = MyData {
            display_name: Some(&long),
            has_picture: true,
            line_user_id: Some("U0123456789abcdef0123456789abcdef"),
            consent: Some((&long, Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap())),
            zone: Some(&long),
            house_number: Some(&long),
            landmark_note: Some(&long),
            phone: Some("0812345678"),
            location: Some((13.75, 100.5)),
            shop: Some(MyShop {
                phone: Some("053123456"),
                promptpay_id: Some("1234567890123"),
                pickup_address: Some(&longer),
                pickup_location: Some((13.7, 100.6)),
            }),
            orders_with_address: 1_000,
            slips: 1_000,
            retention: Retention {
                slip_days: 90,
                webhook_event_days: 14,
                push_log_days: 90,
                ai_message_days: Some(30),
            },
            edit: vec![
                Action::postback(labels::EDIT_ADDRESS.clone(), "my_data.edit?part=address"),
                Action::postback(labels::EDIT_PHONE.clone(), "my_data.edit?part=phone"),
                Action::postback(labels::EDIT_LOCATION.clone(), "my_data.edit?part=location"),
            ],
        };
        let card = t.my_data(&data);
        limits::check(slice::from_ref(&card)).unwrap();
        let Body::Flex { alt_text, .. } = &card.body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "ข้อมูลของฉัน");
    }

    /// FR-ADM-05, FR-LINE-11: a full page of "หมวดหมู่", with the longest names and every
    /// button, fits one message; the confirmation of a deletion can be cancelled.
    #[test]
    fn a_full_page_of_categories_fits() {
        let t = templates(PoliteParticle::Kha);
        let name = "ข".repeat(Label::MAX_CHARS);
        let action = |label: &Label| Action::Postback {
            label: label.clone(),
            data: format!("categories.up?c={}&n={}", i64::MAX, i64::MAX),
            display_text: Some(label.as_str().to_owned()),
        };
        let cards: Vec<_> = (1..=CATEGORIES_PER_PAGE + 1)
            .map(|rank| CategoryCard {
                rank,
                name: &name,
                is_active: rank % 2 == 0,
                products: 99_999,
                up: Some(action(&labels::MOVE_UP)),
                down: Some(action(&labels::MOVE_DOWN)),
                rename: action(&labels::RENAME),
                toggle: action(&labels::DEACTIVATE),
                delete: action(&labels::DELETE),
            })
            .collect();
        let more = |next| format!("categories?page={next}");
        let page = t.categories(&cards, 0, more, add_category());
        limits::check(slice::from_ref(&page)).unwrap();
        let Body::Flex { alt_text, .. } = &page.body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "หมวดหมู่");

        let confirm = t.confirm_delete_category(&name, confirm_delete());
        let items = &confirm.quick_reply.as_ref().unwrap().items;
        let labels: Vec<_> = items
            .iter()
            .map(|item| item.action.label().as_str())
            .collect();
        assert_eq!(labels, ["ยืนยันลบ", "ยกเลิก"]);
    }

    /// The buttons of a product's card in "สินค้าของฉัน", as many as it has, each sending a
    /// postback as long as a product's can be.
    fn my_product_buttons() -> Vec<Vec<Action>> {
        let action = |label: &Label| Action::Postback {
            label: label.clone(),
            data: format!("my_products.part?id={}&p=description", i64::MAX),
            display_text: Some(label.as_str().to_owned()),
        };
        vec![
            vec![action(&labels::RESTOCK), action(&labels::EDIT_PRICE)],
            vec![action(&labels::CHANGE_PHOTOS), action(&labels::EDIT)],
            vec![action(&labels::HIDE_PRODUCT), action(&labels::DELETE)],
        ]
    }

    /// Eggs, as their seller sees them: for sale, 30 left, with every button.
    fn eggs<'a>(name: &'a str, photo: Option<&'a Url>) -> MyProductCard<'a> {
        MyProductCard {
            name,
            price: Money::from_baht(110),
            unit: "แผง",
            category: "ไข่/นม",
            stock: Some(30),
            ready_on: None,
            status: product::Status::Active,
            photo,
            photos: 1,
            buttons: my_product_buttons(),
            primary: false,
        }
    }

    /// FR-PROD-05, FR-SRCH-03, FR-LINE-11: a full page of "สินค้าของฉัน", with the longest
    /// values a product takes and every button, fits in one carousel with "ดูเพิ่มเติม".
    #[test]
    fn a_full_page_of_my_products_fits() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit) = ("ข".repeat(80), "ข".repeat(20));
        let category = "ข".repeat(Label::MAX_CHARS);
        let photo =
            Url::parse("https://iconmarket.example.com/img/p/6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b")
                .unwrap();
        let cards: Vec<_> = (0..=PRODUCTS_PER_PAGE)
            .map(|_| MyProductCard {
                unit: &unit,
                category: &category,
                price: Money::from_baht(100_000),
                stock: Some(99_999),
                ready_on: Some(ready_day()),
                photos: 3,
                ..eggs(&name, Some(&photo))
            })
            .collect();
        let more = |next| format!("my_products?page={next}");
        let page = t.my_products(&cards, 0, more, add_product());
        limits::check(slice::from_ref(&page)).unwrap();
        let Body::Flex {
            alt_text,
            contents: FlexContainer::Carousel(carousel),
        } = &page.body
        else {
            panic!("not a carousel");
        };
        assert_eq!(alt_text, "สินค้าของฉัน");
        assert_eq!(carousel.contents.len(), PRODUCTS_PER_PAGE + 1);
        let texts = card_texts(&carousel.contents[0]);
        assert_eq!(texts[1], format!("฿100,000 / {unit}"));
        assert!(texts.contains(&format!("99,999 {unit}")), "{texts:?}");
        assert!(texts.contains(&"3 รูป".to_owned()), "{texts:?}");
        limits::check(&[t.my_product(&cards[0])]).unwrap();
    }

    /// A postback button labelled `label` sending `data`, as browsing's are.
    fn postback(label: &Label, data: String) -> Action {
        Action::Postback {
            label: label.clone(),
            data,
            display_text: Some(label.as_str().to_owned()),
        }
    }

    /// FR-SRCH-03, FR-SRCH-07, FR-LINE-11: a full page of a product list, with the longest values
    /// a product and its shop take, the longest keyword and every button, fits in one carousel
    /// with "ดูเพิ่มเติม", its cards all the size of the "ดูเพิ่มเติม" bubble.
    #[test]
    fn a_full_page_of_products_fits() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let keyword = "ข".repeat(80);
        let photo =
            Url::parse("https://iconmarket.example.com/img/p/6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b")
                .unwrap();
        let max = i64::MAX;
        let cards: Vec<_> = (0..PRODUCTS_PER_PAGE)
            .map(|_| ProductCard {
                name: &name,
                price: Money::from_baht(100_000),
                unit: &unit,
                shop: &shop,
                availability: Availability::Left(99_999),
                ready_on: Some(ready_day()),
                photo: &photo,
                add_to_cart: Some(postback(
                    &labels::ADD_TO_CART,
                    format!("cart.add?product={max}"),
                )),
                view_shop: postback(&labels::VIEW_SHOP, format!("browse.front?shop={max}")),
                details: postback(
                    &Label::fit(name.as_str()),
                    format!("browse.product?product={max}"),
                ),
            })
            .collect();
        let more = format!("browse.search?pa=6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b&page={max}");
        let next = vec![postback(&labels::BUY, "buy".to_owned())];
        let page = t.product_list(Browsed::Search(&keyword), &cards, Some(more), next);
        limits::check(slice::from_ref(&page)).unwrap();
        let Body::Flex {
            alt_text,
            contents: FlexContainer::Carousel(carousel),
        } = &page.body
        else {
            panic!("not a carousel");
        };
        assert_eq!(alt_text, &format!("ผลการค้นหา \"{keyword}\""));
        assert_eq!(carousel.contents.len(), PRODUCTS_PER_PAGE + 1);
        assert!(
            carousel
                .contents
                .iter()
                .all(|card| card.size == Some(BubbleSize::Kilo))
        );
        assert_eq!(
            card_texts(&carousel.contents[0]),
            [
                name.clone(),
                format!("฿100,000 / {unit}"),
                shop.clone(),
                "เหลือ 99999".to_owned(),
                "สั่งล่วงหน้า · พร้อม 31 ธ.ค. 2569".to_owned()
            ]
        );
    }

    /// Appendix A.2, FR-CART-01: how many is asked as in the sample, and the line put in the cart
    /// is shown as in it, with its total.
    #[test]
    fn the_cart_speaks_as_in_appendix_a2() {
        let t = templates(PoliteParticle::Kha);
        let ask = t.ask_cart_quantity("ไข่ไก่เบอร์ 2", "แผง", None, 0, false);
        assert_eq!(text(&ask), "\"ไข่ไก่เบอร์ 2\" เอากี่แผงคะ");
        let added = t.added_to_cart("ไข่ไก่เบอร์ 2", 2, "แผง", Money::from_baht(220), None, false);
        assert_eq!(text(&added), "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 2 แผง = ฿220");
        let more = t.added_to_cart("ไข่ไก่เบอร์ 2", 5, "แผง", Money::from_baht(550), None, true);
        assert_eq!(
            text(&more),
            "ใส่เพิ่มแล้วค่ะ ตอนนี้ในตะกร้ามี ไข่ไก่เบอร์ 2 × 5 แผง = ฿550"
        );
    }

    /// FR-CART-01: a quantity over what is left says how many are left, and, adding to a line,
    /// what it holds and how many more fit; over 99, that a line holds at most 99.
    #[test]
    fn too_many_for_the_cart_says_what_is_left() {
        let t = templates(PoliteParticle::Kha);
        let name = "ไข่ไก่เบอร์ 2";
        assert_eq!(
            text(&t.quantity_too_many(name, "แผง", Some(8), 99, 0)),
            "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
        );
        assert_eq!(
            text(&t.quantity_too_many(name, "แผง", Some(8), 99, 2)),
            "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง ในตะกร้ามีแล้ว 2 แผง จึงใส่เพิ่มได้อีกไม่เกิน 6 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
        );
        assert_eq!(
            text(&t.quantity_too_many(name, "แผง", Some(8), 99, 8)),
            "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง ในตะกร้ามีแล้ว 8 แผง จึงใส่เพิ่มไม่ได้แล้วค่ะ"
        );
        for left in [None, Some(99), Some(500)] {
            assert_eq!(
                text(&t.quantity_too_many(name, "แผง", left, 99, 0)),
                "ขอโทษค่ะ ใส่ตะกร้าได้รายการละไม่เกิน 99 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
            );
        }
    }

    /// FR-CART-03: a line that can't be ordered says why.
    #[test]
    fn a_cart_line_says_why_it_cant_be_ordered() {
        let flags = [
            cart::State::Orderable,
            cart::State::SoldOut,
            cart::State::ShopClosed,
            cart::State::Unavailable,
            cart::State::NotEnough(3),
        ]
        .map(|state| cart_problem(state, "แผง"));
        assert_eq!(
            flags,
            [
                None,
                Some("หมด".to_owned()),
                Some("ร้านปิด".to_owned()),
                Some("ตอนนี้ไม่มีขาย".to_owned()),
                Some("เหลือแค่ 3 แผง".to_owned()),
            ]
        );
    }

    /// FR-CART-03, FR-LINE-11: a full cart fits, with the longest values it takes: its 20 lines
    /// spread over a page of shops, then a "ดูเพิ่มเติม", or all of one shop.
    #[test]
    fn a_full_cart_fits() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let max = i64::MAX;
        let line = |state| CartLine {
            name: &name,
            unit: &unit,
            price: Money::from_baht(100_000),
            quantity: 99,
            total: Money::from_baht(100_000).times(99),
            ready_on: Some(ready_day()),
            state,
            buttons: vec![
                postback(
                    &labels::CHANGE_QUANTITY,
                    format!("cart.quantity?product={max}"),
                ),
                postback(&labels::REMOVE, format!("cart.remove?product={max}")),
            ],
        };
        let shop_of = |lines: usize| CartShop {
            name: &shop,
            lines: (0..lines)
                .map(|_| line(cart::State::NotEnough(98)))
                .collect(),
            subtotal: Money::from_baht(100_000).times(99 * 20),
            excluded: true,
            checkout: Some(postback(
                &labels::ORDER_FROM_SHOP,
                format!("checkout?shop={max}"),
            )),
        };
        let more = |next| format!("cart?page={next}");
        let next = vec![postback(&labels::CONTINUE_SHOPPING, "buy".to_owned())];

        let spread: Vec<_> = (0..=CART_SHOPS_PER_PAGE).map(|_| shop_of(2)).collect();
        let page = t.cart(&spread, 0, more, next.clone());
        limits::check(slice::from_ref(&page)).unwrap();
        let Body::Flex {
            alt_text,
            contents: FlexContainer::Carousel(carousel),
        } = &page.body
        else {
            panic!("not a carousel");
        };
        assert_eq!(alt_text, "ตะกร้า");
        assert_eq!(carousel.contents.len(), CART_SHOPS_PER_PAGE + 1);
        let texts = card_texts(&carousel.contents[0]);
        assert_eq!(texts[0], format!("ร้าน{shop}"));
        assert_eq!(texts[1], name);
        assert_eq!(texts[2], format!("99 {unit} × ฿100,000"));
        assert_eq!(texts[3], "฿9,900,000");
        assert_eq!(texts[4], "สั่งล่วงหน้า · พร้อม 31 ธ.ค. 2569");
        assert_eq!(texts[5], format!("เหลือแค่ 98 {unit}"));

        let one = [shop_of(20)];
        limits::check(&[t.cart(&one, 0, more, next)]).unwrap();
    }

    /// The order of Appendix A.2: 2 แผง of eggs, delivered for ฿10 to ซอย 3 and paid in cash.
    fn egg_order() -> OrderDetails<'static> {
        OrderDetails {
            shop: "ร้านป้าแดง",
            lines: vec![OrderLine {
                name: "ไข่ไก่เบอร์ 2",
                unit: "แผง",
                price: Money::from_baht(110),
                quantity: 2,
                total: Money::from_baht(220),
                ready_on: None,
            }],
            subtotal: Money::from_baht(220),
            delivery_fee: Money::from_baht(10),
            total: Money::from_baht(230),
            fulfillment: order::Fulfillment::Delivery,
            address: Some(address_line("ซอย 3", "45/2", None)),
            payment: order::PaymentMethod::Cash,
            note: None,
        }
    }

    /// Appendix A.2, FR-CART-04, FR-CART-08: checkout asks as in the sample, its summary shows
    /// the items, subtotal, delivery fee and total, how the order is received and paid, and the
    /// order placed says its number and status.
    #[test]
    fn checkout_speaks_as_in_appendix_a2() {
        let t = templates(PoliteParticle::Kha);
        assert_eq!(
            text(&t.ask_pickup_or_delivery()),
            "รับเองที่ร้าน หรือให้ร้านไปส่งคะ"
        );
        assert_eq!(
            text(&t.confirm_delivery_address("ซอย 3 บ้านเลขที่ 45/2", false)),
            "ส่งที่ ซอย 3 บ้านเลขที่ 45/2 ใช่ไหมคะ"
        );
        assert_eq!(text(&t.ask_payment_method()), "ชำระเงินแบบไหนคะ");
        assert_eq!(text(&t.ask_order_note()), "มีอะไรฝากถึงร้านไหมคะ พิมพ์ได้เลย");

        let order = egg_order();
        let summary = t.checkout_summary(&order, &[]);
        let Body::Flex {
            alt_text,
            contents: FlexContainer::Bubble(card),
        } = &summary.body
        else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "สรุปคำสั่งซื้อ รวม ฿230");
        assert_eq!(
            card_texts(card),
            [
                "สรุปคำสั่งซื้อ",
                "ร้านป้าแดง",
                "ไข่ไก่เบอร์ 2",
                "2 แผง × ฿110",
                "฿220",
                "ค่าสินค้า",
                "฿220",
                "ค่าส่ง",
                "฿10",
                "รวม",
                "฿230",
                "รับของ",
                "ส่งที่ ซอย 3 บ้านเลขที่ 45/2",
                "ชำระเงิน",
                "เงินสดตอนรับของ",
            ]
        );

        let placed = t.order_placed(
            "IM-20261022-0007",
            order::Status::Pending,
            &order,
            false,
            &[],
        );
        let Body::Flex { alt_text, .. } = &placed.body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "สั่งซื้อเรียบร้อยค่ะ เลขที่ IM-20261022-0007 · รอร้านยืนยัน");
    }

    /// FR-CART-05, FR-CART-06, AC-03: what changed since a summary is said line by line, and
    /// "สินค้าหมดแล้ว" first when a product sold out.
    #[test]
    fn what_changed_at_checkout_is_said() {
        let t = templates(PoliteParticle::Kha);
        assert_eq!(
            text(&t.checkout_changed(&CHANGES, false)),
            "ขอโทษค่ะ สินค้าหมดแล้ว จึงยังไม่ได้สั่งซื้อ\n\
             • \"ไข่ไก่เบอร์ 2\" หมดแล้ว\n\
             • \"นมสด\" เหลือแค่ 1 ขวด\n\
             • \"ผักบุ้ง\" ตอนนี้ไม่มีขาย\n\
             • ร้านป้าแดงไม่รับออเดอร์ตอนนี้\n\
             • \"ไข่ไก่เบอร์ 0\" ราคาเปลี่ยนจาก ฿110 / แผง เป็น ฿120 / แผง\n\
             • ค่าส่งเปลี่ยนจาก ฿10 เป็น ฿20\n\
             ดูตะกร้าหรือเลือกซื้อต่อได้เลยค่ะ"
        );
        assert_eq!(
            text(&t.checkout_changed(&CHANGES[4..], true)),
            "ขอโทษค่ะ ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ จึงยังไม่ได้สั่งซื้อ\n\
             • \"ไข่ไก่เบอร์ 0\" ราคาเปลี่ยนจาก ฿110 / แผง เป็น ฿120 / แผง\n\
             • ค่าส่งเปลี่ยนจาก ฿10 เป็น ฿20\n\
             ตรวจสรุปใหม่ด้านล่าง แล้วกด \"ยืนยันสั่งซื้อ\" อีกครั้งนะคะ"
        );
    }

    /// FR-PROD-11: a pre-order says when it is ready wherever it is ordered: asking how many,
    /// in the cart, at checkout and on the order, and the seller is told too; a change of its
    /// ready date since checkout's summary is said.
    #[test]
    fn pre_orders_say_when_they_are_ready() {
        let t = templates(PoliteParticle::Kha);
        let ready = NaiveDate::from_ymd_opt(2026, 11, 5);
        let ask = t.ask_cart_quantity("ขนมเปี๊ยะ", "กล่อง", ready, 0, false);
        assert_eq!(
            text(&ask),
            "\"ขนมเปี๊ยะ\" (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569) เอากี่กล่องคะ"
        );
        let added = t.added_to_cart("ขนมเปี๊ยะ", 2, "กล่อง", Money::from_baht(100), ready, false);
        assert_eq!(
            text(&added),
            "ใส่ตะกร้าแล้วค่ะ ขนมเปี๊ยะ × 2 กล่อง = ฿100 (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569)"
        );
        let changed = t.quantity_changed("ขนมเปี๊ยะ", 3, "กล่อง", Money::from_baht(150), ready);
        assert_eq!(
            text(&changed),
            "แก้จำนวนแล้วค่ะ ขนมเปี๊ยะ × 3 กล่อง = ฿150 (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569)"
        );

        let mut order = egg_order();
        order.lines[0].ready_on = ready;
        let summary = t.checkout_summary(&order, &[]);
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &summary.body
        else {
            panic!("not a card");
        };
        let texts = card_texts(card);
        let at = texts.iter().position(|text| text == "฿220").unwrap();
        assert_eq!(texts[at + 1], "สั่งล่วงหน้า · พร้อม 5 พ.ย. 2569");

        let mut new = new_order(order::Fulfillment::Pickup);
        new.lines[1].ready_on = ready;
        let notice = t.new_order_notice("IM-20261022-0007", &new, Vec::new());
        let lines = notice_lines(&notice);
        assert_eq!(lines[1], "ไข่ไก่เบอร์ 2 × 2 แผง = ฿220");
        assert_eq!(
            lines[2],
            "ขนมครก × 1 กล่อง = ฿20 (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569)"
        );

        assert_eq!(
            text(&t.checkout_changed(&READY_CHANGES, true)),
            "ขอโทษค่ะ ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ จึงยังไม่ได้สั่งซื้อ\n\
             • \"ขนมเปี๊ยะ\" เปลี่ยนวันพร้อมเป็น 7 พ.ย. 2569\n\
             • \"ขนมไหว้พระจันทร์\" ไม่ต้องสั่งล่วงหน้าแล้ว\n\
             ตรวจสรุปใหม่ด้านล่าง แล้วกด \"ยืนยันสั่งซื้อ\" อีกครั้งนะคะ"
        );
    }

    /// FR-PROD-11: the seller is asked for the ready date with the range it may be in, and told
    /// why one isn't taken.
    #[test]
    fn ready_dates_are_asked_for() {
        let t = templates(PoliteParticle::Kha);
        let latest = NaiveDate::from_ymd_opt(2027, 1, 23).unwrap();
        assert_eq!(
            text(&t.ask_product_ready_on(latest, Some("5 พ.ย. 2569"))),
            "ถ้ารับสั่งล่วงหน้า ของจะพร้อมวันไหนคะ กดเลือกวัน หรือพิมพ์วันที่ เช่น 15 พ.ย. ได้ตั้งแต่พรุ่งนี้ถึง 23 ม.ค. 2570 ผู้ซื้อจะเห็นวันนี้ที่สินค้า ถ้าไม่ได้สั่งล่วงหน้า กด \"ไม่สั่งล่วงหน้า\" ได้เลยค่ะ (ตอนนี้: 5 พ.ย. 2569)"
        );
        assert_eq!(
            text(&t.ready_day_too_late(latest)),
            "ขอโทษค่ะ ตั้งวันพร้อมได้ไม่เกิน 23 ม.ค. 2570 กดเลือกวันหรือพิมพ์ใหม่ได้เลยค่ะ"
        );
        assert_eq!(ready_line(latest), "สั่งล่วงหน้า · พร้อม 23 ม.ค. 2570");
    }

    /// Table 4.6-A, FR-PAY-02: an order's status, payment method and fulfillment as the buyer
    /// reads them.
    #[test]
    fn orders_read_as_table_4_6_a() {
        let statuses = [
            order::Status::Pending,
            order::Status::Accepted,
            order::Status::Ready,
            order::Status::Delivering,
            order::Status::Completed,
            order::Status::Rejected,
            order::Status::Cancelled,
            order::Status::Expired,
        ]
        .map(order_status_name);
        assert_eq!(
            statuses,
            [
                "รอร้านยืนยัน",
                "ร้านรับออเดอร์แล้ว",
                "พร้อมให้รับ",
                "กำลังไปส่ง",
                "สำเร็จ",
                "ร้านไม่รับออเดอร์",
                "ยกเลิกแล้ว",
                "หมดเวลา",
            ]
        );
        let payments = [order::PaymentMethod::Cash, order::PaymentMethod::PromptPay];
        assert_eq!(payments.map(payment_name), ["เงินสดตอนรับของ", "โอนพร้อมเพย์"]);
        let fulfillments = [order::Fulfillment::Pickup, order::Fulfillment::Delivery];
        assert_eq!(fulfillments.map(fulfillment_name), ["รับเองที่ร้าน", "ส่งถึงบ้าน"]);
    }

    /// FR-CART-07, FR-LINE-11: the largest order fits, as checkout's summary and as its card:
    /// 20 lines with the longest values they take, the longest address and note, and the
    /// longest buttons.
    #[test]
    fn a_full_order_fits() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let (zone, house_number, landmark) = ("ข".repeat(50), "ข".repeat(30), "ข".repeat(100));
        let note = "ข".repeat(200);
        let max = i64::MAX;
        let order = OrderDetails {
            shop: &shop,
            lines: (0..20)
                .map(|_| OrderLine {
                    name: &name,
                    unit: &unit,
                    price: Money::from_baht(100_000),
                    quantity: 99,
                    total: Money::from_baht(100_000).times(99),
                    ready_on: Some(ready_day()),
                })
                .collect(),
            subtotal: Money::from_baht(100_000).times(99 * 20),
            delivery_fee: Money::from_baht(10_000),
            total: Money::from_baht(100_000).times(99 * 20),
            fulfillment: order::Fulfillment::Delivery,
            address: Some(address_line(&zone, &house_number, Some(&landmark))),
            payment: order::PaymentMethod::PromptPay,
            note: Some(&note),
        };
        let summary = [
            postback(
                &labels::CONFIRM_ORDER,
                "confirm?k=checkout&pa=6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b".to_owned(),
            ),
            postback(&labels::EDIT, format!("checkout?shop={max}")),
            Action::message(labels::CANCEL.clone(), labels::CANCEL.as_str()),
        ];
        limits::check(&[t.checkout_summary(&order, &summary)]).unwrap();
        let cancel = [postback(
            &labels::CANCEL_ORDER,
            format!("order.cancel?order={max}"),
        )];
        let placed = t.order_placed(
            "IM-20261022-9999",
            order::Status::Pending,
            &order,
            true,
            &cancel,
        );
        limits::check(&[placed]).unwrap();
    }

    /// Appendix A.4: the seller is told of a new order, and accepts it, as in the sample; the
    /// buyer is told the shop is preparing it.
    #[test]
    fn orders_are_handled_as_in_appendix_a4() {
        let t = templates(PoliteParticle::Kha);
        let number = "IM-20261022-0007";
        let mut eggs = new_order(order::Fulfillment::Delivery);
        eggs.lines.truncate(1);
        eggs.total = Money::from_baht(230);
        let notice = t.new_order_notice(number, &eggs, Vec::new());
        assert_eq!(
            notice_lines(&notice),
            [
                "ออเดอร์ใหม่ค่ะ IM-20261022-0007",
                "ไข่ไก่เบอร์ 2 × 2 แผง = ฿220",
                "ค่าส่ง ฿10 · รวม ฿230",
                "ส่งที่ ซอย 3 · เงินสดตอนรับของ",
                "ลูกค้า: สมชาย",
            ]
        );
        let accepted = t.order_accepted(number, &contact(true), Vec::new());
        assert_eq!(
            notice_lines(&accepted),
            [
                "รับออเดอร์ IM-20261022-0007 แล้วค่ะ",
                "ส่งที่ ซอย 3 บ้านเลขที่ 45/2 (รั้วสีเขียว)",
                "ลูกค้า: สมชาย",
                "โทร 081-234-5678",
                "ฝากถึงร้าน: ไม่ใส่ถุง",
            ]
        );
        let picked_up = t.order_accepted(number, &contact(false), Vec::new());
        assert_eq!(notice_lines(&picked_up)[1], "รับเองที่ร้าน");
        let told = t.order_accepted_notice("ร้านป้าแดง", number, None, Vec::new());
        assert_eq!(
            notice_lines(&told),
            ["ร้านป้าแดงรับออเดอร์ IM-20261022-0007 แล้วค่ะ กำลังเตรียมของ"]
        );
    }

    /// FR-PAY-03: the buyer of an order paid by PromptPay is told, as it is accepted, the
    /// shop's PromptPay ID, its name and the exact amount; if the shop took its ID away since,
    /// to ask the shop for it.
    #[test]
    fn a_promptpay_order_accepted_says_what_to_transfer_where() {
        let t = templates(PoliteParticle::Kha);
        let number = "IM-20261022-0007";
        let shop = "ร้านป้าแดง";
        let told = t.order_accepted_notice(shop, number, Some(&due(Some("0812345678"))), vec![]);
        assert_eq!(
            notice_lines(&told),
            [
                "ร้านป้าแดงรับออเดอร์ IM-20261022-0007 แล้วค่ะ กำลังเตรียมของ",
                "โอนพร้อมเพย์ ฿230",
                "เลขพร้อมเพย์: 081-234-5678",
                "ชื่อร้าน: ร้านป้าแดง",
                "โอนแล้วกด \"ส่งสลิป\" แล้วส่งรูปสลิปในแชตนี้ค่ะ",
            ]
        );
        let citizen =
            t.order_accepted_notice(shop, number, Some(&due(Some("1101700203450"))), vec![]);
        assert_eq!(notice_lines(&citizen)[2], "เลขพร้อมเพย์: 1-1017-00203-45-0");
        let gone = t.order_accepted_notice(shop, number, Some(&due(None)), vec![]);
        assert_eq!(
            notice_lines(&gone),
            [
                "ร้านป้าแดงรับออเดอร์ IM-20261022-0007 แล้วค่ะ กำลังเตรียมของ",
                "โอนพร้อมเพย์ ฿230",
                "ร้านยังไม่มีเลขพร้อมเพย์ สอบถามเลขที่จะโอนกับร้านได้โดยตรงค่ะ",
            ]
        );
    }

    /// FR-PAY-05, FR-UI-14: the buyer is asked for the slip, or whether the photo they sent is
    /// it, and told it went to the seller; the seller gets it with the amount to look for and how
    /// long its link lasts (AC-14); and the buyer is told when the seller didn't get the money.
    #[test]
    fn a_slip_is_asked_for_sent_and_answered() {
        let t = templates(PoliteParticle::Kha);
        let (number, total) = ("IM-20261022-0007", Money::from_baht(230));
        assert_eq!(
            text(&t.ask_slip(number, total)),
            "ส่งรูปสลิปโอนเงินค่าออเดอร์ IM-20261022-0007 ยอด ฿230 มาในแชตนี้ได้เลยค่ะ"
        );
        let is_it = t.is_it_the_slip(number, total);
        assert_eq!(
            text(&is_it),
            "ได้รับรูปแล้วค่ะ รูปนี้เป็นสลิปโอนเงินค่าออเดอร์ IM-20261022-0007 ยอด ฿230 ใช่ไหมคะ ถ้าใช่ ระบบจะส่งให้ร้านตรวจสอบค่ะ"
        );
        let answers: Vec<_> = is_it.quick_reply.unwrap().items;
        let answers: Vec<_> = answers
            .iter()
            .map(|item| item.action.label().as_str())
            .collect();
        assert_eq!(answers, ["ใช่", "ไม่ใช่"]);
        assert_eq!(
            notice_lines(&t.slip_sent(number, Vec::new())),
            [
                "ส่งสลิปออเดอร์ IM-20261022-0007 ให้ร้านแล้วค่ะ",
                "รอร้านตรวจสอบยอดเงินค่ะ สถานะการชำระเงิน: รอร้านตรวจสอบ",
            ]
        );
        assert_eq!(
            notice_lines(&t.slip_received_notice(number, total, Some("สมชาย"), 24, None, vec![])),
            [
                "ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ",
                "ยอดออเดอร์ ฿230",
                "ลูกค้า: สมชาย",
                "ตรวจดูว่าเงินเข้าบัญชีแล้ว แล้วกดตอบด้านล่างได้เลยค่ะ รูปสลิปเปิดดูได้ภายใน 24 ชั่วโมง",
            ]
        );
        let t = templates(PoliteParticle::Khrap);
        assert_eq!(
            notice_lines(&t.slip_not_received(number, Vec::new())),
            [
                "บันทึกว่ายังไม่ได้รับเงินออเดอร์ IM-20261022-0007ครับ",
                "แจ้งลูกค้าให้ตรวจสอบการโอนแล้วครับ",
            ]
        );
        assert_eq!(
            notice_lines(&t.slip_not_received_notice("ป้าแดง", number, total, Vec::new())),
            [
                "ร้านป้าแดงแจ้งว่ายังไม่ได้รับเงินค่าออเดอร์ IM-20261022-0007ครับ",
                "ยอดที่ต้องโอน ฿230",
                "ตรวจสอบการโอนอีกครั้ง หรือสอบถามร้านได้โดยตรงครับ",
            ]
        );
    }

    /// FR-PAY-06: what the AI read from a slip shows beside the order's total, what doesn't match
    /// in red and bold, and is said to be advice; without a reading the notice is as it was.
    #[test]
    fn what_the_ai_read_from_a_slip_shows_beside_the_total() {
        let t = templates(PoliteParticle::Kha);
        let (number, total) = ("IM-20261022-0007", Money::from_baht(230));
        let [matching, mismatching, unread] = slip_readings();
        let warning = Some(WARNING_COLOR);
        let advice = (
            "ระบบอ่านจากสลิปอัตโนมัติ อาจผิดพลาดได้ ตรวจยอดในบัญชีก่อนกดตอบนะคะ",
            Some(MUTED),
            false,
        );
        let check = (
            "ตรวจดูว่าเงินเข้าบัญชีแล้ว แล้วกดตอบด้านล่างได้เลยค่ะ รูปสลิปเปิดดูได้ภายใน 24 ชั่วโมง",
            None,
            false,
        );
        let notice = |reading| t.slip_received_notice(number, total, None, 24, reading, vec![]);
        assert_eq!(
            toned_lines(&notice(Some(&matching))),
            [
                ("ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ", None, true),
                ("ยอดออเดอร์ ฿230", None, false),
                ("ยอดในสลิป ฿230 ✓ ตรงกับยอดออเดอร์", None, false),
                ("โอนเมื่อ 25 ต.ค. 2569 09:12", None, false),
                ("เลขอ้างอิง 016298091203", Some(MUTED), false),
                advice,
                check,
            ]
        );
        assert_eq!(
            toned_lines(&notice(Some(&mismatching))),
            [
                ("ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ", None, true),
                ("ยอดออเดอร์ ฿230", None, false),
                ("ยอดในสลิป ฿200.50 ⚠️ ไม่ตรงกับยอดออเดอร์", warning, true),
                (
                    "โอนเมื่อ 25 ต.ค. 2569 09:12 ⚠️ ไม่ตรงกับช่วงเวลาของออเดอร์",
                    warning,
                    true,
                ),
                advice,
                check,
            ]
        );
        assert_eq!(
            toned_lines(&notice(Some(&unread))),
            [
                ("ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ", None, true),
                ("ยอดออเดอร์ ฿230", None, false),
                ("⚠️ อ่านยอดในสลิปไม่ได้", warning, true),
                advice,
                check,
            ]
        );
        // Unread, as with the AI off, the notice is as it was
        assert_eq!(
            toned_lines(&notice(None)),
            [
                ("ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ", None, true),
                ("ยอดออเดอร์ ฿230", None, false),
                check,
            ]
        );
        // The headline is its preview
        let Body::Flex { alt_text, .. } = &notice(Some(&mismatching)).body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, "ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261022-0007 แล้วค่ะ");
    }

    /// 30 ต.ค. 2569 20:00 in Bangkok, when the daily summary goes by default
    fn summary_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 30, 13, 0, 0).unwrap()
    }

    /// A shop's day: 5 orders placed, 3 completed for ฿1,450, most of it ขนมครก
    fn summary_day() -> Day {
        let best = TopProduct {
            product_id: 1,
            name: "ขนมครก".to_owned(),
            unit: "กล่อง".to_owned(),
            quantity: 30,
            value: Money::from_baht(600),
        };
        Day {
            placed: 5,
            completed: 3,
            completed_value: Money::from_baht(1_450),
            top: vec![best],
        }
    }

    /// FR-NOT-06: the day's orders and sales and its best seller, then the AI's comment as an
    /// aside, if any.
    #[test]
    fn the_daily_summary_shows_the_day_then_the_ai_comment_aside() {
        let t = templates(PoliteParticle::Kha);
        let headline = ("สรุปยอดขายวันนี้ค่ะ 30 ต.ค. 2569", None, true);
        let comment = "ขนมครกขายดีกว่าเมื่อวานเยอะเลยค่ะ";
        let notice = t.daily_summary_notice(summary_time(), &summary_day(), Some(comment), vec![]);
        assert_eq!(
            toned_lines(&notice),
            [
                headline,
                ("ออเดอร์ใหม่ 5 รายการ", None, false),
                ("ขายสำเร็จ 3 รายการ รวม ฿1,450", None, false),
                ("ขายดีสุด: ขนมครก 30 กล่อง", None, false),
                ("ผู้ช่วย AI: ขนมครกขายดีกว่าเมื่อวานเยอะเลยค่ะ", Some(MUTED), false),
            ]
        );
        // The headline is its preview
        let Body::Flex { alt_text, .. } = &notice.body else {
            panic!("not a card");
        };
        assert_eq!(alt_text, headline.0);

        // Nothing completed yet, and no comment, as with the AI off
        let quiet = Day {
            placed: 1,
            completed: 0,
            completed_value: Money::ZERO,
            top: Vec::new(),
        };
        assert_eq!(
            toned_lines(&t.daily_summary_notice(summary_time(), &quiet, None, vec![])),
            [
                headline,
                ("ออเดอร์ใหม่ 1 รายการ", None, false),
                ("ขายสำเร็จ 0 รายการ รวม ฿0", None, false),
            ]
        );
    }

    /// FR-PAY-07: the seller is asked whether they got the money of an order handed over
    /// unpaid, as they hand it over or as its buyer says they received it.
    #[test]
    fn the_seller_is_asked_whether_they_got_the_money() {
        let t = templates(PoliteParticle::Khrap);
        let number = "IM-20261022-0007";
        let total = Money::from_baht(230);
        assert_eq!(
            notice_lines(&t.ask_paid(number, total, false, Vec::new())),
            [
                "ออเดอร์ IM-20261022-0007 สำเร็จแล้วครับ",
                "ได้รับเงิน ฿230 แล้วหรือยังครับ",
            ]
        );
        assert_eq!(
            notice_lines(&t.ask_paid(number, total, true, Vec::new())),
            [
                "ลูกค้าแจ้งว่าได้รับออเดอร์ IM-20261022-0007 แล้วครับ",
                "ได้รับเงิน ฿230 แล้วหรือยังครับ",
            ]
        );
        assert_eq!(
            notice_lines(&t.left_unpaid(number, Vec::new())),
            [
                "ออเดอร์ IM-20261022-0007 ยังค้างชำระครับ",
                "ดูได้ที่ \"ออเดอร์ร้าน\" > \"ค้างชำระ\" ได้รับเงินแล้วกด \"ได้รับเงินแล้ว\"ครับ",
            ]
        );
    }

    /// FR-LINE-11: the notices and cards of acting on an order fit with the longest values they
    /// take: an order of 20 lines, the longest names, address, note and reason, and the largest
    /// IDs in their buttons.
    #[test]
    fn order_notices_with_everything_fit() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let (zone, house_number, landmark) = ("ข".repeat(50), "ข".repeat(30), "ข".repeat(100));
        let (buyer, note, reason) = ("ข".repeat(100), "ข".repeat(200), "ข".repeat(300));
        let message = "ข".repeat(MESSAGE_CHARS);
        let number = "IM-20261022-9999";
        let max = i64::MAX;
        let button = |label: &Label, action: &str| postback(label, format!("{action}?order={max}"));
        let new = NewOrder {
            lines: (0..20)
                .map(|_| OrderLine {
                    name: &name,
                    unit: &unit,
                    price: Money::from_baht(100_000),
                    quantity: 99,
                    total: Money::from_baht(100_000).times(99),
                    ready_on: Some(ready_day()),
                })
                .collect(),
            delivery_fee: Money::from_baht(10_000),
            total: Money::from_baht(100_000).times(99 * 20),
            fulfillment: order::Fulfillment::Delivery,
            payment: order::PaymentMethod::PromptPay,
            buyer: Some(&buyer),
            zone: Some(&zone),
        };
        let decide = vec![
            button(&labels::ACCEPT_ORDER, "order.accept"),
            button(&labels::REJECT_ORDER, "order.reject"),
        ];
        let contact = Contact {
            buyer: Some(&buyer),
            address: Some(address_line(&zone, &house_number, Some(&landmark))),
            phone: Some("0812345678"),
            note: Some(&note),
        };
        let request = vec![
            button(&labels::MARK_DELIVERING, "order.delivering"),
            button(&labels::CANCEL_ORDER, "order.grant_cancel"),
            button(&labels::KEEP_ORDER, "order.keep"),
        ];
        let carry_on = || vec![postback(&labels::CONTINUE_SHOPPING, "buy".to_owned())];
        let found = FoundOrder {
            number,
            status: order::Status::Delivering,
            shop: &shop,
            buyer: Some(&buyer),
            total: Money::from_baht(100_000).times(99 * 20),
            payment: order::PaymentMethod::PromptPay,
            payment_status: order::PaymentStatus::AwaitingConfirmation,
            placed_at: Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap(),
            rating: Some(OrderRating {
                stars: 5,
                comment: Some(&note),
            }),
        };
        let reasons = [
            &labels::REASON_SOLD_OUT,
            &labels::REASON_CANT_DELIVER,
            &labels::REASON_SHOP_CLOSED,
            &labels::REASON_OTHER,
        ]
        .map(|label| Action::message(label.clone(), label.as_str()));
        for message in [
            t.new_order_notice(number, &new, decide),
            t.order_accepted(number, &contact, request.clone()),
            t.cancellation_requested_notice(number, request),
            t.order_cancelled_notice(&shop, number, true, &reason, true, carry_on()),
            t.order_rejected_notice(&shop, number, &reason, carry_on()),
            t.order_cancelled_by_admin_notice(
                number,
                &reason,
                true,
                vec![button(&labels::MARK_REFUNDED, "order.refunded")],
            ),
            t.order_accepted_notice(
                &shop,
                number,
                Some(&due(Some("004999012345678"))),
                vec![
                    button(&labels::SEND_SLIP, "order.send_slip"),
                    button(&labels::REQUEST_CANCEL, "order.request_cancel"),
                ],
            ),
            t.slip_received_notice(
                number,
                Money::from_baht(100_000).times(99 * 20),
                Some(&buyer),
                u32::MAX,
                Some(&SlipReading {
                    amount: Some((Money::from_satang(i64::MAX), false)),
                    paid_at: Some((DateTime::<Utc>::MAX_UTC, false)),
                    reference: Some(&"9".repeat(40)),
                }),
                vec![
                    button(&labels::MARK_PAID, "order.paid"),
                    postback(
                        &labels::NOT_PAID,
                        format!("order.not_paid?order={max}&slip={max}"),
                    ),
                ],
            ),
            t.slip_not_received_notice(
                &shop,
                number,
                Money::from_baht(100_000).times(99 * 20),
                vec![
                    button(&labels::SEND_SLIP, "order.send_slip"),
                    button(&labels::CONFIRM_RECEIPT, "order.received"),
                    button(&labels::REQUEST_CANCEL, "order.request_cancel"),
                ],
            ),
            t.order_found(
                &found,
                vec![
                    button(&labels::MARK_PAID, "order.paid"),
                    button(&labels::CANCEL_ORDER, "order.admin_cancel"),
                ],
            ),
            t.ask_order_reason(number, order::Status::Cancelled, true, reasons.to_vec()),
            t.relayed_message_notice(
                number,
                OrderParty::Shop(&shop),
                &message,
                vec![
                    button(&labels::MESSAGE_SHOP, "order.message_shop"),
                    button(&labels::ORDER_DETAILS, "order.view"),
                ],
            ),
            t.relayed_message_notice(
                number,
                OrderParty::Buyer(Some(&buyer)),
                &message,
                vec![
                    button(&labels::MESSAGE_BUYER, "order.message_buyer"),
                    button(&labels::ORDER_DETAILS, "order.view"),
                ],
            ),
            t.message_relayed(
                number,
                OrderParty::Buyer(Some(&buyer)),
                vec![
                    button(&labels::MARK_COMPLETED, "order.complete"),
                    button(&labels::MARK_PAID, "order.paid"),
                    button(&labels::CANCEL_ORDER, "order.grant_cancel"),
                    button(&labels::KEEP_ORDER, "order.keep"),
                    button(&labels::MESSAGE_BUYER, "order.message_buyer"),
                ],
            ),
            t.message_unreachable(
                number,
                OrderParty::Shop(&shop),
                vec![
                    button(&labels::SEND_SLIP, "order.send_slip"),
                    button(&labels::CONFIRM_RECEIPT, "order.received"),
                    button(&labels::REQUEST_CANCEL, "order.request_cancel"),
                ],
            ),
            t.ask_stars(
                number,
                &shop,
                (1..=5)
                    .rev()
                    .map(|stars| {
                        let rate = format!("order.rate?order={max}&stars={stars}");
                        postback(&stars_label(stars), rate)
                    })
                    .collect(),
            ),
            t.ask_rating_comment(
                number,
                5,
                200,
                Action::message(labels::SKIP.clone(), labels::SKIP.as_str()),
            ),
            t.rating_saved(number, carry_on()),
            t.already_rated(number, 5, carry_on()),
        ] {
            limits::check(slice::from_ref(&message)).unwrap();
        }
    }

    /// FR-ORD-10, FR-LINE-11: a full page of "คำสั่งซื้อของฉัน" and of "ออเดอร์ร้าน" fits, and
    /// an order in full, with the longest values their cards take: 20 lines, the longest names,
    /// addresses and note, a location shared, and the largest IDs in their buttons; or, once
    /// completed and rated, the longest comment (FR-ORD-14). A page mixing both is no larger
    /// than one of the larger kind.
    #[test]
    fn a_full_page_of_orders_fits() {
        /// An order of 20 lines, each `name` by `unit`, with `contact` and `buttons`: on its
        /// way, or completed if `rating`.
        fn order_card<'a>(
            name: &'a str,
            unit: &'a str,
            shop: Option<&'a str>,
            contact: OrderContact<'a>,
            rating: Option<OrderRating<'a>>,
            buttons: Vec<Action>,
        ) -> OrderCard<'a> {
            let status = match rating {
                Some(_) => order::Status::Completed,
                None => order::Status::Delivering,
            };
            OrderCard {
                number: "IM-20261022-9999",
                status,
                placed_at: Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap(),
                shop,
                lines: (0..20)
                    .map(|_| OrderLine {
                        name,
                        unit,
                        price: Money::from_baht(100_000),
                        quantity: 99,
                        total: Money::from_baht(100_000).times(99),
                        ready_on: Some(ready_day()),
                    })
                    .collect(),
                subtotal: Money::from_baht(100_000).times(99 * 20),
                delivery_fee: Money::from_baht(10_000),
                total: Money::from_baht(100_000).times(99 * 20),
                fulfillment: order::Fulfillment::Delivery,
                payment: order::PaymentMethod::PromptPay,
                payment_status: order::PaymentStatus::AwaitingConfirmation,
                unpaid: true,
                contact,
                rating,
                buttons,
            }
        }
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let (zone, house_number, landmark) = ("ข".repeat(50), "ข".repeat(30), "ข".repeat(100));
        let (buyer, note, pickup) = ("ข".repeat(100), "ข".repeat(200), "ข".repeat(200));
        let max = i64::MAX;
        let button = |label: &Label, action: &str| postback(label, format!("{action}?order={max}"));
        let location = Some((-89.123_456_789, -179.123_456_789));
        let seller = || {
            let contact = Contact {
                buyer: Some(&buyer),
                address: Some(address_line(&zone, &house_number, Some(&landmark))),
                phone: Some("0812345678"),
                note: Some(&note),
            };
            let buttons = vec![
                button(&labels::MARK_COMPLETED, "order.complete"),
                button(&labels::MARK_PAID, "order.paid"),
                button(&labels::CANCEL_ORDER, "order.grant_cancel"),
                button(&labels::KEEP_ORDER, "order.keep"),
                button(&labels::MESSAGE_BUYER, "order.message_buyer"),
                button(&labels::ORDER_DETAILS, "order.view"),
            ];
            let contact = OrderContact::BuyerFull { contact, location };
            order_card(&name, &unit, None, contact, None, buttons)
        };
        let buyer_card = || {
            let contact = OrderContact::Shop {
                pickup: Some(&pickup),
                location,
                phone: Some("0812345678"),
                promptpay: Some("004999012345678"),
            };
            let buttons = vec![
                button(&labels::SEND_SLIP, "order.send_slip"),
                button(&labels::CONFIRM_RECEIPT, "order.received"),
                button(&labels::REQUEST_CANCEL, "order.request_cancel"),
                button(&labels::MESSAGE_SHOP, "order.message_shop"),
                button(&labels::ORDER_DETAILS, "order.view"),
            ];
            order_card(&name, &unit, Some(&shop), contact, None, buttons)
        };
        // Completed, its contact no longer shown (FR-ORD-11), and rated
        let rated = || {
            Some(OrderRating {
                stars: 5,
                comment: Some(&note),
            })
        };
        let rated_seller = || {
            let buttons = vec![
                button(&labels::MARK_PAID, "order.paid"),
                button(&labels::ORDER_DETAILS, "order.view"),
            ];
            order_card(&name, &unit, None, OrderContact::None, rated(), buttons)
        };
        let rated_buyer = || {
            let buttons = vec![button(&labels::ORDER_DETAILS, "order.view")];
            order_card(
                &name,
                &unit,
                Some(&shop),
                OrderContact::None,
                rated(),
                buttons,
            )
        };
        let more = Some(format!("shop_orders?filter=unpaid&page={max}"));
        let filters: Vec<Action> = [
            &labels::FILTER_NEW,
            &labels::FILTER_ACTIVE,
            &labels::FILTER_UNPAID,
            &labels::FILTER_DONE,
            &labels::MY_ORDERS,
            &labels::MENU,
        ]
        .into_iter()
        .map(|label| postback(label, "shop_orders?filter=unpaid".to_owned()))
        .collect();
        let sellers: [Vec<OrderCard<'_>>; 2] = [
            (0..ORDERS_PER_PAGE).map(|_| seller()).collect(),
            (0..ORDERS_PER_PAGE).map(|_| rated_seller()).collect(),
        ];
        for sellers in &sellers {
            let page = t.shop_orders(&labels::FILTER_DONE, sellers, more.clone(), filters.clone());
            limits::check(&page).unwrap();
        }
        let buyers: [Vec<OrderCard<'_>>; 2] = [
            (0..ORDERS_PER_PAGE).map(|_| buyer_card()).collect(),
            (0..ORDERS_PER_PAGE).map(|_| rated_buyer()).collect(),
        ];
        for buyers in &buyers {
            let page = t.my_orders(buyers, more.clone(), filters.clone());
            limits::check(slice::from_ref(&page)).unwrap();
        }
        for card in [seller(), buyer_card(), rated_seller(), rated_buyer()] {
            let full = t.order_details(&card, filters.clone());
            limits::check(slice::from_ref(&full)).unwrap();
        }
    }

    /// FR-ORD-11: the seller's card of an order shows only what its contact says of the buyer,
    /// and the buyer's only what it says of the shop.
    #[test]
    fn an_orders_card_shows_only_its_contact() {
        let t = templates(PoliteParticle::Kha);
        let card = |shop, fulfillment, contact| OrderCard {
            number: "IM-20261022-0007",
            status: order::Status::Accepted,
            placed_at: Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap(),
            shop,
            lines: vec![OrderLine {
                name: "ไข่ไก่เบอร์ 2",
                unit: "แผง",
                price: Money::from_baht(110),
                quantity: 2,
                total: Money::from_baht(220),
                ready_on: None,
            }],
            subtotal: Money::from_baht(220),
            delivery_fee: Money::from_baht(10),
            total: Money::from_baht(230),
            fulfillment,
            payment: order::PaymentMethod::Cash,
            payment_status: order::PaymentStatus::Unpaid,
            unpaid: false,
            contact,
            rating: None,
            buttons: Vec::new(),
        };
        let delivered = order::Fulfillment::Delivery;
        let pending = OrderContact::Buyer {
            name: Some("สมชาย"),
            zone: Some("ซอย 3"),
        };
        let pending = t.order_details(&card(None, delivered, pending), Vec::new());
        let Body::Flex {
            contents: FlexContainer::Bubble(pending),
            ..
        } = &pending.body
        else {
            panic!("not a card");
        };
        assert_eq!(
            card_texts(pending),
            [
                "ออเดอร์ IM-20261022-0007",
                "สถานะ",
                "ร้านรับออเดอร์แล้ว",
                "สั่งเมื่อ",
                "22 ต.ค. 2569 14:30",
                "ไข่ไก่เบอร์ 2",
                "2 แผง × ฿110",
                "฿220",
                "ค่าสินค้า",
                "฿220",
                "ค่าส่ง",
                "฿10",
                "รวม",
                "฿230",
                "รับของ",
                "ส่งที่ ซอย 3",
                "ชำระเงิน",
                "เงินสดตอนรับของ · ยังไม่ชำระ",
                "ลูกค้า",
                "สมชาย",
            ]
        );
        let shop = OrderContact::Shop {
            pickup: Some("หน้าวัด"),
            location: None,
            phone: None,
            promptpay: None,
        };
        let pickup = t.order_details(
            &card(Some("ร้านป้าแดง"), order::Fulfillment::Pickup, shop),
            Vec::new(),
        );
        let Body::Flex {
            contents: FlexContainer::Bubble(pickup),
            ..
        } = &pickup.body
        else {
            panic!("not a card");
        };
        let texts = card_texts(pickup);
        assert_eq!(texts[5], "ร้านป้าแดง");
        assert_eq!(
            texts[texts.len() - 6..],
            [
                "รับของ",
                "รับเองที่ร้าน",
                "ที่รับของ",
                "หน้าวัด",
                "ชำระเงิน",
                "เงินสดตอนรับของ · ยังไม่ชำระ"
            ]
        );
        assert!(pickup.footer.is_none());

        // FR-PAY-03: the shop's PromptPay ID, while the buyer is yet to transfer
        let shop = OrderContact::Shop {
            pickup: None,
            location: None,
            phone: Some("0898765432"),
            promptpay: Some("0812345678"),
        };
        let mut transfer = card(Some("ร้านป้าแดง"), delivered, shop);
        transfer.payment = order::PaymentMethod::PromptPay;
        let transfer = t.order_details(&transfer, Vec::new());
        let Body::Flex {
            contents: FlexContainer::Bubble(transfer),
            ..
        } = &transfer.body
        else {
            panic!("not a card");
        };
        let texts = card_texts(transfer);
        assert_eq!(
            texts[texts.len() - 6..],
            [
                "ชำระเงิน",
                "โอนพร้อมเพย์ · ยังไม่ชำระ",
                "เบอร์ร้าน",
                "089-876-5432",
                "พร้อมเพย์",
                "081-234-5678"
            ]
        );
    }

    /// FR-PAY-08, FR-PAY-07: an order's card says how far it is paid once it is to be: not while
    /// it is yet to be accepted, or once called off unpaid; "ค้างชำระ" on its seller's card once
    /// handed over unpaid.
    #[test]
    fn an_orders_card_says_how_far_it_is_paid() {
        use order::PaymentStatus::{Paid, RefundDue, Refunded, Unpaid};
        use order::Status::{Cancelled, Completed, Delivering, Pending};
        let t = templates(PoliteParticle::Kha);
        for (status, payment, unpaid, shown) in [
            (Pending, Unpaid, false, "เงินสดตอนรับของ"),
            (Cancelled, Unpaid, false, "เงินสดตอนรับของ"),
            (Delivering, Unpaid, false, "เงินสดตอนรับของ · ยังไม่ชำระ"),
            (Completed, Unpaid, true, "เงินสดตอนรับของ · ค้างชำระ"),
            (Completed, Unpaid, false, "เงินสดตอนรับของ · ยังไม่ชำระ"),
            (Completed, Paid, false, "เงินสดตอนรับของ · ชำระแล้ว"),
            (Cancelled, RefundDue, false, "เงินสดตอนรับของ · รอคืนเงิน"),
            (Cancelled, Refunded, false, "เงินสดตอนรับของ · คืนเงินแล้ว"),
        ] {
            let card = OrderCard {
                number: "IM-20261022-0007",
                status,
                placed_at: Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap(),
                shop: None,
                lines: Vec::new(),
                subtotal: Money::from_baht(220),
                delivery_fee: Money::ZERO,
                total: Money::from_baht(220),
                fulfillment: order::Fulfillment::Pickup,
                payment: order::PaymentMethod::Cash,
                payment_status: payment,
                unpaid,
                contact: OrderContact::None,
                rating: None,
                buttons: Vec::new(),
            };
            let details = t.order_details(&card, Vec::new());
            let Body::Flex {
                contents: FlexContainer::Bubble(details),
                ..
            } = &details.body
            else {
                panic!("not a card");
            };
            let texts = card_texts(details);
            assert_eq!(texts.last().unwrap(), shown, "{status:?}, {payment:?}");
        }
    }

    /// FR-SRCH-07: a product's details show its card and every other photo, as many as a carousel
    /// holds, with the longest values it takes.
    #[test]
    fn a_products_details_fit_with_every_photo() {
        let t = templates(PoliteParticle::Kha);
        let (name, unit, shop) = ("ข".repeat(80), "ข".repeat(20), "ข".repeat(50));
        let description = "ข".repeat(500);
        let photos: Vec<Url> = (0..20)
            .map(|i| {
                Url::parse(&format!(
                    "https://iconmarket.example.com/img/p/6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a{i:02}"
                ))
                .unwrap()
            })
            .collect();
        let max = i64::MAX;
        let details = |photos| ProductDetails {
            name: &name,
            price: Money::from_baht(100_000),
            unit: &unit,
            description: Some(&description),
            shop: &shop,
            availability: Availability::Orderable,
            ready_on: Some(ready_day()),
            photos,
            buttons: vec![
                postback(&labels::ADD_TO_CART, format!("cart.add?product={max}")),
                postback(&labels::VIEW_SHOP, format!("browse.front?shop={max}")),
                postback(&labels::REPORT, format!("report?p={max}")),
            ],
        };
        let next = vec![postback(&labels::BUY, "buy".to_owned())];

        let shown = t.product_details(&details(&photos), next.clone());
        limits::check(slice::from_ref(&shown)).unwrap();
        let Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } = &shown.body
        else {
            panic!("not a carousel");
        };
        assert_eq!(carousel.contents.len(), limits::BUBBLES_PER_CAROUSEL);
        assert_eq!(
            card_texts(&carousel.contents[0])[..5],
            [
                name.clone(),
                format!("฿100,000 / {unit}"),
                "สั่งได้".to_owned(),
                "สั่งล่วงหน้า · พร้อม 31 ธ.ค. 2569".to_owned(),
                description.clone()
            ]
        );

        // One photo, one card
        let one = t.product_details(&details(&photos[..1]), next);
        let Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } = &one.body
        else {
            panic!("not one card");
        };
        assert_eq!(card.footer.as_ref().unwrap().contents.len(), 3);
    }

    /// FR-SRCH-06, FR-LINE-11: a full page of "ร้านค้าทั้งหมด", with the longest values a shop
    /// takes, fits in one carousel with "ดูเพิ่มเติม".
    #[test]
    fn a_full_page_of_shops_fits() {
        let t = templates(PoliteParticle::Kha);
        let (name, description, hours) = ("ข".repeat(50), "ข".repeat(300), "ข".repeat(50));
        let photo =
            Url::parse("https://iconmarket.example.com/img/p/6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b")
                .unwrap();
        let max = i64::MAX;
        let fronts: Vec<_> = (0..SHOPS_PER_PAGE)
            .map(|_| ShopFront {
                name: &name,
                description: &description,
                photo: Some(&photo),
                is_open: false,
                opening_hours: Some(&hours),
                minimum_order: Some(Money::from_baht(100_000)),
                delivery_fee: Money::from_baht(1_000),
                rating: Some(ShopRating {
                    tenths: 50,
                    count: 1_000_000,
                }),
                products: postback(&labels::VIEW_PRODUCTS, format!("browse.shop?shop={max}")),
                ..shop_front()
            })
            .collect();
        let more = format!("browse.shops?page={max}");
        let next = vec![postback(&labels::BUY, "buy".to_owned())];
        let page = t.shop_list(Browsed::Shops, &fronts, Some(more), next);
        limits::check(slice::from_ref(&page)).unwrap();
        let Body::Flex {
            alt_text,
            contents: FlexContainer::Carousel(carousel),
        } = &page.body
        else {
            panic!("not a carousel");
        };
        assert_eq!(alt_text, "ร้านค้าทั้งหมด");
        assert_eq!(carousel.contents.len(), SHOPS_PER_PAGE + 1);
    }

    /// FR-SRCH-07: a card says whether the product can be ordered, in the colour of an open
    /// shop if it can.
    #[test]
    fn availability_is_coloured_by_whether_it_can_be_ordered() {
        for (of, color) in [
            (Availability::Left(5), OPEN_COLOR),
            (Availability::Orderable, OPEN_COLOR),
            (Availability::SoldOut, CLOSED_COLOR),
            (Availability::ShopClosed, CLOSED_COLOR),
        ] {
            let Component::Text(text) = availability_line(of) else {
                panic!("not text");
            };
            assert_eq!(text.color.as_deref(), Some(color), "{of:?}");
        }
    }

    /// FR-PROD-05 – FR-PROD-07: a product's card says whether buyers see it and whether it is
    /// sold out, and makes "เติมของ" the primary button of a sold-out one.
    #[test]
    fn a_products_card_says_whether_buyers_can_order_it() {
        use product::Status::{Active, Held, Hidden, Removed};
        for (status, stock, state) in [
            (Active, Some(30), "ลงขายอยู่"),
            (Active, None, "ลงขายอยู่"),
            (Active, Some(0), "หมด"),
            (Hidden, Some(0), "ซ่อนอยู่ (ผู้ซื้อไม่เห็น)"),
            (Held, Some(30), "รอตรวจ (ผู้ซื้อยังไม่เห็น)"),
            (Removed, Some(30), "ถูกนำออก (ผู้ซื้อไม่เห็น)"),
        ] {
            let card = MyProductCard {
                status,
                stock,
                ..eggs("ไข่ไก่เบอร์ 2", None)
            };
            let texts = card_texts(&my_product_card(&card));
            assert_eq!(texts[2], state, "{status:?} {stock:?}");
        }

        let styles = |primary: bool| -> Vec<Option<ButtonStyle>> {
            let card = MyProductCard {
                primary,
                ..eggs("ไข่ไก่เบอร์ 2", None)
            };
            let bubble = my_product_card(&card);
            let mut styles = Vec::new();
            for row in &bubble.footer.unwrap().contents {
                let Component::Box(row) = row else {
                    panic!("not a row: {row:?}");
                };
                for button in &row.contents {
                    let Component::Button(button) = button else {
                        panic!("not a button: {button:?}");
                    };
                    styles.push(button.style);
                }
            }
            styles
        };
        let secondary = Some(ButtonStyle::Secondary);
        assert_eq!(styles(false), [secondary; 6]);
        let mut first_primary = [secondary; 6];
        first_primary[0] = Some(ButtonStyle::Primary);
        assert_eq!(styles(true), first_primary);
    }

    #[test]
    fn dates_use_the_configured_time_zone() {
        let at = Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap();
        let t = templates(PoliteParticle::Kha);
        assert_eq!(t.date_time(at), "22 ต.ค. 2569 14:30");
        assert_eq!(t.date(at), "22 ต.ค. 2569");
    }
}
