//! The main menu (FR-UI-05, §3.1.2): the functions of the rich menus, a tab for each role, and
//! the message `เมนู` shows for clients without a rich menu. The
//! [rich menus](crate::notifications::RichMenus) use the same items and postbacks, so a function
//! is reached the same way from either.

use super::postback::Postback;
use crate::line::Message;
use crate::notifications::templates;
use crate::users::access::Actor;
use crate::users::roles::Roles;

/// The action of the postback a rich menu's tab sends when tapped (FR-UI-10). LINE has already
/// switched to the tab's menu by then, so it needs no answer.
pub const SWITCH_TAB: &str = "tab";

/// A tab of the menu: one per role, shown to the users who hold it (FR-UI-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// Every user's, and the default rich menu
    Buyer,
    Seller,
    Admin,
}

impl Tab {
    /// Its name in postbacks and in the names of the rich menus.
    pub fn key(self) -> &'static str {
        match self {
            Self::Buyer => "buyer",
            Self::Seller => "seller",
            Self::Admin => "admin",
        }
    }

    /// The postback its tab in a rich menu sends.
    pub fn switch_postback(self) -> Postback {
        Postback::new(SWITCH_TAB).with_name("to", self.key())
    }

    /// Its functions, in the order of §3.1.2.
    pub fn items(self) -> [Item; 6] {
        use Item::*;
        match self {
            Self::Buyer => [Buy, Search, Cart, MyOrders, MyShop, Help],
            Self::Seller => [
                ShopOrders,
                AddProduct,
                MyProducts,
                OpenClose,
                Sales,
                ShopSettings,
            ],
            Self::Admin => [
                ApproveShops,
                ReviewQueue,
                Users,
                Categories,
                Stats,
                Announcements,
            ],
        }
    }

    /// The tabs for `roles`.
    pub fn of(roles: Roles) -> impl Iterator<Item = Self> {
        [
            (Self::Buyer, true),
            (Self::Seller, roles.seller),
            (Self::Admin, roles.admin),
        ]
        .into_iter()
        .filter_map(|(tab, held)| held.then_some(tab))
    }
}

/// A function of the menu. Each is a postback naming its action; the dispatcher routes it
/// to the module that owns the function, which checks the user may use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    // Buyer
    Buy,
    Search,
    Cart,
    MyOrders,
    /// "เปิดร้าน" until the user has a shop, then "ร้านของฉัน"
    MyShop,
    Help,
    // Seller
    ShopOrders,
    AddProduct,
    MyProducts,
    OpenClose,
    Sales,
    ShopSettings,
    // Admin
    ApproveShops,
    ReviewQueue,
    Users,
    Categories,
    Stats,
    Announcements,
}

impl Item {
    /// Its postback's action.
    pub fn action(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Search => "search",
            Self::Cart => "cart",
            Self::MyOrders => "my_orders",
            Self::MyShop => "my_shop",
            Self::Help => "help",
            Self::ShopOrders => "shop_orders",
            Self::AddProduct => "add_product",
            Self::MyProducts => "my_products",
            Self::OpenClose => "open_close",
            Self::Sales => "sales",
            Self::ShopSettings => "shop_settings",
            Self::ApproveShops => "approve_shops",
            Self::ReviewQueue => "review_queue",
            Self::Users => "users",
            Self::Categories => "categories",
            Self::Stats => "stats",
            Self::Announcements => "announcements",
        }
    }

    /// The item whose postback has `action`.
    pub fn from_action(action: &str) -> Option<Self> {
        [Tab::Buyer, Tab::Seller, Tab::Admin]
            .into_iter()
            .flat_map(Tab::items)
            .find(|item| item.action() == action)
    }

    pub fn postback(self) -> Postback {
        Postback::new(self.action())
    }
}

/// The main menu for `actor`'s roles, as a message (FR-UI-05).
pub fn main_menu(actor: &Actor) -> Message {
    templates::main_menu(Tab::of(actor.roles()), actor.shop.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line::flex::{Component, FlexContainer};
    use crate::line::{Action, Body, limits};

    fn menu(seller: bool, admin: bool, has_shop: bool) -> Message {
        templates::main_menu(Tab::of(Roles { seller, admin }), has_shop)
    }

    /// Each card's title, and the label and postback data of each of its buttons.
    fn cards(message: &Message) -> Vec<(String, Vec<(String, String)>)> {
        let Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } = &message.body
        else {
            panic!("not a carousel: {message:?}");
        };
        let button = |component: &Component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback {
                    label,
                    data,
                    display_text,
                } => {
                    assert_eq!(display_text.as_deref(), Some(label.as_str()));
                    (label.as_str().to_owned(), data.clone())
                }
                action => panic!("not a postback: {action:?}"),
            },
            component => panic!("not a button: {component:?}"),
        };
        carousel
            .contents
            .iter()
            .map(|bubble| {
                let title = match bubble
                    .header
                    .as_ref()
                    .map(|header| header.contents.as_slice())
                {
                    Some([Component::Text(text)]) => text.text.clone(),
                    header => panic!("no title: {header:?}"),
                };
                let body = bubble.body.as_ref().expect("buttons");
                (title, body.contents.iter().map(button).collect())
            })
            .collect()
    }

    fn titles(message: &Message) -> Vec<String> {
        cards(message).into_iter().map(|(title, _)| title).collect()
    }

    /// FR-UI-05, FR-UI-10: a card for each role the user holds, and only those.
    #[test]
    fn the_menu_has_a_card_for_each_role() {
        let menus = [
            (menu(false, false, false), vec!["ผู้ซื้อ"]),
            (menu(true, false, true), vec!["ผู้ซื้อ", "ผู้ขาย"]),
            (menu(false, true, false), vec!["ผู้ซื้อ", "ผู้ดูแล"]),
            (menu(true, true, true), vec!["ผู้ซื้อ", "ผู้ขาย", "ผู้ดูแล"]),
        ];
        for (message, expected) in menus {
            assert_eq!(titles(&message), expected);
            limits::check(&[message]).unwrap();
        }
    }

    #[test]
    fn every_button_is_the_postback_of_its_item() {
        let cards = cards(&menu(true, true, true));
        for ((_, buttons), tab) in cards.iter().zip([Tab::Buyer, Tab::Seller, Tab::Admin]) {
            let items: Vec<_> = buttons
                .iter()
                .map(|(_, data)| {
                    let postback = Postback::parse(data).unwrap();
                    Item::from_action(postback.action()).unwrap()
                })
                .collect();
            assert_eq!(items, tab.items());
        }
        let labels: Vec<_> = cards[1].1.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "ออเดอร์ร้าน",
                "เพิ่มสินค้า",
                "สินค้าของฉัน",
                "เปิด/ปิดร้าน",
                "ยอดขาย",
                "ตั้งค่าร้าน"
            ]
        );
    }

    #[test]
    fn open_a_shop_becomes_my_shop_once_there_is_one() {
        let my_shop = |has_shop| cards(&menu(false, false, has_shop))[0].1[4].0.clone();
        assert_eq!(my_shop(false), "เปิดร้าน");
        assert_eq!(my_shop(true), "ร้านของฉัน");
    }

    #[test]
    fn every_item_has_its_own_action() {
        let items: Vec<_> = [Tab::Buyer, Tab::Seller, Tab::Admin]
            .into_iter()
            .flat_map(Tab::items)
            .collect();
        assert_eq!(items.len(), 18);
        for item in &items {
            assert_eq!(Item::from_action(item.action()), Some(*item));
            let data = item.postback().to_string();
            assert_eq!(Postback::parse(&data), Some(item.postback()));
        }
        assert_eq!(Item::from_action("confirm"), None);
        assert_eq!(Item::from_action(SWITCH_TAB), None);
    }

    #[test]
    fn a_tab_sends_its_own_postback() {
        for tab in [Tab::Buyer, Tab::Seller, Tab::Admin] {
            let data = tab.switch_postback().to_string();
            let postback = Postback::parse(&data).unwrap();
            assert_eq!(postback.action(), SWITCH_TAB);
            assert_eq!(postback.name("to"), Some(tab.key()));
        }
    }
}
