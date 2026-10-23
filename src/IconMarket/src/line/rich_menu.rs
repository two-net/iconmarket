//! Rich menus (§3.1.2): the menu object and the calls that create, link and switch menus.
//! Provisioning them at startup is #17's; each call here is tried once, and a failure is the
//! caller's to log.

use serde::{Deserialize, Serialize};

use super::client::{Client, TRANSFER_TIMEOUT};
use super::error::{Error, LimitError};
use super::limits::{self, RICH_MENU_IMAGE_BYTES, USERS_PER_BULK_LINK};
use super::message::Action;

/// A menu of tap areas over an image.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RichMenu {
    /// Of the image: 800 to 2500 wide, at least 250 high, and at least 1.45 times as wide
    /// as high
    pub size: Size,
    /// Shown open rather than collapsed
    pub selected: bool,
    /// For finding it again; users don't see it
    pub name: String,
    /// The text on the bar that opens it
    pub chat_bar_text: String,
    pub areas: Vec<Area>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// A tap area, in image pixels.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Area {
    pub bounds: Bounds,
    pub action: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Bounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A rich menu that exists at LINE.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RichMenuInfo {
    pub rich_menu_id: String,
    pub name: String,
}

/// A stable name for a rich menu, which `richmenuswitch` actions refer to.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alias {
    pub rich_menu_alias_id: String,
    pub rich_menu_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageType {
    Jpeg,
    Png,
}

impl ImageType {
    fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    rich_menu_id: String,
}

#[derive(Deserialize)]
struct Menus {
    richmenus: Vec<RichMenuInfo>,
}

#[derive(Deserialize)]
struct Aliases {
    aliases: Vec<Alias>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BulkLink<'a> {
    rich_menu_id: &'a str,
    user_ids: &'a [&'a str],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AliasRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    rich_menu_alias_id: Option<&'a str>,
    rich_menu_id: &'a str,
}

impl Client {
    /// Creates a rich menu, without its image yet, and returns its ID.
    pub async fn create_rich_menu(&self, menu: &RichMenu) -> Result<String, Error> {
        limits::check_rich_menu(menu)?;
        let request = self
            .http
            .post(self.api_url(&["v2", "bot", "richmenu"]))
            .json(menu);
        let created: Created = self.fetch(request).await?;
        Ok(created.rich_menu_id)
    }

    /// Sets a rich menu's image, once: LINE can't replace it.
    pub async fn upload_rich_menu_image(
        &self,
        rich_menu_id: &str,
        image_type: ImageType,
        image: Vec<u8>,
    ) -> Result<(), Error> {
        if image.len() > RICH_MENU_IMAGE_BYTES {
            return Err(Error::Limit(LimitError {
                path: "image".to_owned(),
                problem: format!("{} bytes, at most {RICH_MENU_IMAGE_BYTES}", image.len()),
            }));
        }
        let request = self
            .http
            .post(self.data_url(&["v2", "bot", "richmenu", rich_menu_id, "content"]))
            .header(reqwest::header::CONTENT_TYPE, image_type.mime())
            .timeout(TRANSFER_TIMEOUT)
            .body(image);
        self.send(request).await.map(drop)
    }

    pub async fn rich_menus(&self) -> Result<Vec<RichMenuInfo>, Error> {
        let request = self
            .http
            .get(self.api_url(&["v2", "bot", "richmenu", "list"]));
        let menus: Menus = self.fetch(request).await?;
        Ok(menus.richmenus)
    }

    pub async fn delete_rich_menu(&self, rich_menu_id: &str) -> Result<(), Error> {
        let request = self
            .http
            .delete(self.api_url(&["v2", "bot", "richmenu", rich_menu_id]));
        self.send(request).await.map(drop)
    }

    /// The menu of every user without a menu of their own.
    pub async fn set_default_rich_menu(&self, rich_menu_id: &str) -> Result<(), Error> {
        let url = self.api_url(&["v2", "bot", "user", "all", "richmenu", rich_menu_id]);
        self.send(self.http.post(url)).await.map(drop)
    }

    pub async fn link_rich_menu(&self, user_id: &str, rich_menu_id: &str) -> Result<(), Error> {
        let url = self.api_url(&["v2", "bot", "user", user_id, "richmenu", rich_menu_id]);
        self.send(self.http.post(url)).await.map(drop)
    }

    /// Back to the default menu.
    pub async fn unlink_rich_menu(&self, user_id: &str) -> Result<(), Error> {
        let url = self.api_url(&["v2", "bot", "user", user_id, "richmenu"]);
        self.send(self.http.delete(url)).await.map(drop)
    }

    /// Links a menu to many users, 500 to a request. LINE links them in the background, and
    /// skips users who aren't friends.
    pub async fn link_rich_menu_to_users(
        &self,
        rich_menu_id: &str,
        user_ids: &[&str],
    ) -> Result<(), Error> {
        for user_ids in user_ids.chunks(USERS_PER_BULK_LINK) {
            let request = self
                .http
                .post(self.api_url(&["v2", "bot", "richmenu", "bulk", "link"]))
                .json(&BulkLink {
                    rich_menu_id,
                    user_ids,
                });
            self.send(request).await?;
        }
        Ok(())
    }

    pub async fn create_rich_menu_alias(
        &self,
        alias_id: &str,
        rich_menu_id: &str,
    ) -> Result<(), Error> {
        let request = self
            .http
            .post(self.api_url(&["v2", "bot", "richmenu", "alias"]))
            .json(&AliasRequest {
                rich_menu_alias_id: Some(alias_id),
                rich_menu_id,
            });
        self.send(request).await.map(drop)
    }

    /// Points an alias at another menu.
    pub async fn update_rich_menu_alias(
        &self,
        alias_id: &str,
        rich_menu_id: &str,
    ) -> Result<(), Error> {
        let request = self
            .http
            .post(self.api_url(&["v2", "bot", "richmenu", "alias", alias_id]))
            .json(&AliasRequest {
                rich_menu_alias_id: None,
                rich_menu_id,
            });
        self.send(request).await.map(drop)
    }

    pub async fn delete_rich_menu_alias(&self, alias_id: &str) -> Result<(), Error> {
        let url = self.api_url(&["v2", "bot", "richmenu", "alias", alias_id]);
        self.send(self.http.delete(url)).await.map(drop)
    }

    pub async fn rich_menu_aliases(&self) -> Result<Vec<Alias>, Error> {
        let request = self
            .http
            .get(self.api_url(&["v2", "bot", "richmenu", "alias", "list"]));
        let aliases: Aliases = self.fetch(request).await?;
        Ok(aliases.aliases)
    }
}
