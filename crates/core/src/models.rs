use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub space_id: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub mime: Option<String>,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub updated_at: f64,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub content_version: i64,
    pub role: Option<String>,
    pub shared_with_you: Option<bool>,
}

impl Node {
    pub fn is_folder(&self) -> bool { self.kind == "folder" }
    pub fn is_shared_with_you(&self) -> bool { self.shared_with_you == Some(true) }
    pub fn can_edit(&self) -> bool { matches!(self.role.as_deref(), Some("owner" | "editor")) }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    pub name: String,
    pub root_id: String,
    pub role: String,
}

impl Space {
    pub fn can_edit(&self) -> bool { matches!(self.role.as_str(), "owner" | "editor") }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FolderListing {
    pub folder: Node,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Changes {
    pub cursor: i64,
    pub changed: Vec<Node>,
    pub removed: Vec<String>,
    pub more: bool,
    pub spaces: Vec<Space>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub username: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInConfig {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Uploaded {
    pub id: String,
    pub conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInNext { Done, Code }
