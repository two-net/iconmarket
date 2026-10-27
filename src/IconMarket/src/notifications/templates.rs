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
use url::Url;

use crate::catalog::images::Rejected;
use crate::catalog::products::Availability;
use crate::config::{AiProvider, AppConfig, PoliteParticle};
use crate::dispatcher::menu::{Item, Tab};
use crate::line::flex::{
    AspectMode, Bubble, BubbleSize, Button, ButtonHeight, ButtonStyle, Carousel, Component,
    FlexBox, FlexContainer, Hero, Image, Justify, Separator, Size, Spacing, Text, Weight,
};
use crate::line::{Action, Message, QuickReply, QuickReplyItem, limits};
use crate::payments;
use crate::persistence::entity::product;
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

    /// A page of "ร้านค้าทั้งหมด" (FR-SRCH-06): a card for each of `fronts`, as buyers see shops
    /// (FR-SHOP-09), and, when more follow, "ดูเพิ่มเติม" sending the postback `more`; `next`
    /// are its Quick Replies.
    pub fn shop_list(
        &self,
        fronts: &[ShopFront<'_>],
        more: Option<String>,
        next: Vec<Action>,
    ) -> Message {
        let bubbles = fronts
            .iter()
            .map(|front| self.shop_front_bubble(front))
            .collect();
        Message::flex(headline(Browsed::Shops), carousel(bubbles, more))
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
        };
        Message::text(text).with_quick_reply(QuickReply::new(suggestions))
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
                    lines.push(
                        "- หรือส่งรูปสินค้าพร้อมข้อความสั้นๆ เช่น \"ขนมครก กล่องละ 20 มี 30 กล่อง\" ผู้ช่วยจะร่างให้ตรวจก่อนลงขาย"
                            .to_owned(),
                    );
                }
                lines.push(format!(
                    "- {}: แก้ราคา เติมจำนวน ซ่อน หรือลบสินค้า",
                    quoted(&labels::MY_PRODUCTS)
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

    /// The product before it is listed (FR-PROD-01): `preview`, whose buttons list it or correct
    /// a part.
    pub fn product_preview(&self, preview: &ProductPreview<'_>) -> Vec<Message> {
        let (kha, kha_question) = (self.kha(), self.kha_question());
        let text = format!(
            "ตรวจสินค้าอีกครั้งนะ{kha_question} ถ้าถูกต้อง กด \"{}\" แล้วลูกค้าจะเห็นสินค้าทันที ถ้าต้องการแก้ กด \"{}\" ได้เลย{kha}",
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
        let details = FlexBox {
            spacing: Some(Spacing::Sm),
            ..FlexBox::vertical([
                row("หมวดหมู่", preview.category),
                row("จำนวน", &stock_line(preview.stock, preview.unit)),
                row("รูปสินค้า", &photos),
            ])
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
/// The colours of whether a shop is open, on its card
const OPEN_COLOR: &str = "#06C755";
const CLOSED_COLOR: &str = "#D93025";

/// A shop's card as buyers see it (FR-SHOP-09). #45 adds its rating, once ratings are on
/// (FR-ORD-14).
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
    /// "ดูสินค้า"
    pub products: Action,
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
    /// Its first photo's `/img/p` link
    pub photo: Option<&'a Url>,
    /// How many photos it has
    pub photos: usize,
    pub buttons: Vec<Action>,
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
}

/// A product's card as buyers see it in a list (FR-SRCH-07).
pub struct ProductCard<'a> {
    pub name: &'a str,
    pub price: Money,
    pub unit: &'a str,
    /// Its shop's name
    pub shop: &'a str,
    pub availability: Availability,
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
    /// Its photos' `/img/p` links, in order, or the placeholder's alone
    pub photos: &'a [Url],
    /// "ใส่ตะกร้า" first while it can be ordered, then "ดูร้าน" and "แจ้งปัญหา"
    pub buttons: Vec<Action>,
}

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
    let body = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical([
            name.into(),
            price.into(),
            shop.into(),
            availability_line(card.availability),
        ])
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
    let details = FlexBox {
        spacing: Some(Spacing::Sm),
        ..FlexBox::vertical([
            row("จำนวน", &stock_line(card.stock, card.unit)),
            row("หมวดหมู่", card.category),
            row("รูปสินค้า", &photos),
        ])
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
    let headline_text = Text {
        weight: Some(Weight::Bold),
        wrap: Some(true),
        ..Text::new(headline)
    };
    let lines = details.iter().map(|line| {
        let text = Text {
            size: Some(Size::Sm),
            wrap: Some(true),
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
    let card = Bubble {
        body: Some(body),
        footer,
        ..Bubble::default()
    };
    Message::flex(headline, card)
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
        /// Removes an optional part while correcting it
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
        /// Puts a product in the cart (FR-SRCH-07, #29)
        ADD_TO_CART = "ใส่ตะกร้า",
        /// A product's shop, from its card (FR-SRCH-07)
        VIEW_SHOP = "ดูร้าน",

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
            text(&t.ask_product_photos(0, 3, false)).to_owned(),
            text(&t.ask_product_photos(0, 3, true)).to_owned(),
            text(&t.ask_product_photos(1, 3, false)).to_owned(),
            text(&t.ask_product_photos(2, 3, true)).to_owned(),
            text(&t.ask_product_photos(3, 3, true)).to_owned(),
            text(&t.listing_offered(2)).to_owned(),
            text(&t.listing_offered(0)).to_owned(),
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
            text(&t.product_preview(&product_preview())[0]).to_owned(),
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
            text(&t.product_deleted("ไข่ไก่เบอร์ 2", true)).to_owned(),
            text(&t.product_deleted("ไข่ไก่เบอร์ 2", false)).to_owned(),
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
        texts
    }

    const BROWSED: [Browsed<'static>; 5] = [
        Browsed::Category("ขนม/ของหวาน"),
        Browsed::Shop("ร้านป้าแดง"),
        Browsed::NewToday,
        Browsed::Search("ขนมครก"),
        Browsed::Shops,
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
            photo: None,
            photos: 0,
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
            photo: Some(&photo),
            photos: 3,
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
                "เหลือ 99999".to_owned()
            ]
        );
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
            card_texts(&carousel.contents[0])[..4],
            [
                name.clone(),
                format!("฿100,000 / {unit}"),
                "สั่งได้".to_owned(),
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
                products: postback(&labels::VIEW_PRODUCTS, format!("browse.shop?shop={max}")),
                ..shop_front()
            })
            .collect();
        let more = format!("browse.shops?page={max}");
        let next = vec![postback(&labels::BUY, "buy".to_owned())];
        let page = t.shop_list(&fronts, Some(more), next);
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
