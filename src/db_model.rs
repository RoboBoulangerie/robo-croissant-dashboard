use super::db_schema;
use diesel::{AsChangeset, Insertable, Queryable};
use rocket::serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Deserialize, Serialize, Queryable, Insertable, AsChangeset)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::knowledge_bases)]
pub(crate) struct KnowledgeBase {
    pub name: String,
    pub url: Option<String>,
    pub croissant_metadata: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize, Queryable, Insertable, AsChangeset)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::kb_links)]
pub(crate) struct KnowledgeBaseLink {
    pub kb_name: String,
    pub path: String,
    pub value: String,
    pub url: String,
    pub confidence: f32,
}
