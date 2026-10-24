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
use crate::config::{AiProvider, AppConfig, PoliteParticle};
use crate::dispatcher::menu::{Item, Tab};
use crate::line::flex::{
    Bubble, BubbleSize, Button, ButtonHeight, ButtonStyle, Carousel, Component, FlexBox,
    FlexContainer, Justify, Separator, Size, Spacing, Text, Weight,
};
use crate::line::{Action, Message, QuickReply, QuickReplyItem, limits};
use crate::thai::{self, Label, NumberError};
use crate::users::access::Denied;

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
        let ai: Vec<_> = privacy
            .ai_providers
            .iter()
            .filter_map(|&provider| ai_provider_name(provider))
            .collect();
        let (my_data, delete) = (labels::MY_DATA.as_str(), labels::DELETE_ACCOUNT.as_str());

        let mut collected =
            "ข้อมูลที่เก็บ: ชื่อและรูปโปรไฟล์ LINE, ที่อยู่สำหรับส่งของ, เบอร์โทรและตำแหน่ง (ไม่บังคับ), คำสั่งซื้อ"
                .to_owned();
        let mut kept = "ระยะเวลาเก็บ: ข้อมูลบัญชีและที่อยู่เก็บจนกว่าคุณจะลบบัญชี".to_owned();
        let mut receives = "ผู้ที่ได้รับข้อมูล: LINE ผู้ให้บริการแชตนี้".to_owned();
        if let Some(days) = privacy.slip_days {
            collected.push_str(", รูปสลิปโอนเงิน");
            kept.push_str(&format!(", รูปสลิปเก็บ {days} วันหลังคำสั่งซื้อจบ"));
        }
        if !ai.is_empty() {
            let (names, days) = (ai.join(" และ "), privacy.ai_message_days);
            collected.push_str(", ข้อความและรูปที่ส่งถึงผู้ช่วย AI");
            kept.push_str(&format!(", ข้อความที่คุยกับผู้ช่วย AI เก็บ {days} วัน"));
            receives.push_str(&format!(
                ", {names} ผู้ให้บริการ AI ซึ่งได้รับเฉพาะข้อความและรูปที่จำเป็นต่อการตอบ"
            ));
        }
        collected.push_str(" เพื่อใช้ซื้อขายและส่งของกันในหมู่บ้าน");
        kept.push_str(", ประวัติคำสั่งซื้อเก็บไว้ แต่ลบข้อมูลที่ระบุตัวคุณออกเมื่อลบบัญชี");
        receives.push_str(", และผู้ซื้อหรือร้านค้าในคำสั่งซื้อเดียวกัน ซึ่งเห็นเฉพาะข้อมูลสำหรับส่งของและติดต่อ");
        let details = [
            intro,
            privacy.text.to_owned(),
            collected,
            kept,
            receives,
            format!(
                "สิทธิของคุณ: ดูและแก้ไขข้อมูลได้ที่ \"{my_data}\" ถอนความยินยอมและลบข้อมูลได้ที่ \"{delete}\""
            ),
            format!("ฉบับที่ {}", privacy.version),
        ];

        let mut buttons = vec![privacy.accept.clone()];
        if let Some(url) = privacy.url {
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
}

/// What "ข้อมูลของฉัน" shows for a value the user hasn't given
const NOT_GIVEN: &str = "ยังไม่ได้ให้ไว้";
/// What it shows for one that LINE didn't give
const NONE: &str = "ไม่มี";

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

/// The privacy notice as shown before consent (FR-ONB-02, NFR-PRIV-02).
pub struct PrivacyNotice<'a> {
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
    /// Whether the user consented to an earlier version, so is asked again (NFR-PRIV-01)
    pub updated: bool,
    /// The button that consents to this version
    pub accept: Action,
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

    fn privacy_notice<'a>(text: &'a str, url: Option<&'a str>) -> PrivacyNotice<'a> {
        PrivacyNotice {
            text,
            version: "2026-10",
            url,
            ai_providers: vec![AiProvider::Claude],
            ai_message_days: 30,
            slip_days: Some(90),
            updated: false,
            accept: Action::postback(labels::ACCEPT.clone(), "onboarding.accept?v=42"),
        }
    }

    /// FR-ONB-02, NFR-PRIV-02: the operator's text, then what is collected, how long it is
    /// kept, who receives it and the user's rights, and a button that accepts it.
    #[test]
    fn the_privacy_notice_says_what_happens_to_ones_data() {
        let t = templates(PoliteParticle::Kha);
        let message = t.privacy_notice(&privacy_notice("ตลาดนี้ดูแลโดยคณะกรรมการหมู่บ้าน", None));
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
            ai_providers: vec![AiProvider::Gpt, AiProvider::Gemini],
            updated: true,
            ..privacy_notice("ประกาศ", Some(url))
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
            let neither = PrivacyNotice {
                ai_providers,
                slip_days: None,
                ..privacy_notice("ประกาศ", None)
            };
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
            ai_providers: vec![AiProvider::Claude, AiProvider::Gemini],
            updated: true,
            ..privacy_notice(&longest, Some(&url))
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

    #[test]
    fn dates_use_the_configured_time_zone() {
        let at = Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap();
        let t = templates(PoliteParticle::Kha);
        assert_eq!(t.date_time(at), "22 ต.ค. 2569 14:30");
        assert_eq!(t.date(at), "22 ต.ค. 2569");
    }
}
