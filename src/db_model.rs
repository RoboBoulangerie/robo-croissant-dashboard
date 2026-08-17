use super::db_schema;
use diesel::{AsChangeset, Insertable, Queryable, QueryableByName};
use rocket::serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Serialize, Queryable, QueryableByName)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::runs)]
pub(crate) struct Run {
    pub id: i32,
    pub label: String,
    pub run_date: Option<String>,
    pub model: Option<String>,
    pub file_hash: String,
    pub imported_at: String,
}

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
    pub reviewed: bool,
    pub auto_reviewed: bool,
}

#[derive(Debug, Clone, Serialize, Queryable)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::validation_issues)]
pub(crate) struct ValidationIssue {
    pub id: i32,
    pub kb_name: String,
    pub issue_type: String,
    pub path: String,
    pub value: String,
    pub detail: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Queryable, Insertable, AsChangeset)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::staged_kb_versions)]
pub(crate) struct StagedKbVersion {
    pub kb_name: String,
    pub source_label: String,
    pub croissant_metadata: JsonValue,
    pub staged_at: String,
}

#[derive(Debug, Clone, Serialize, Queryable)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = db_schema::staged_kb_issues)]
pub(crate) struct StagedKbIssue {
    pub kb_name: String,
    pub issue_type: String,
    pub path: String,
    pub value: String,
    pub detail: String,
}
