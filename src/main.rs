#[macro_use] extern crate rocket;
use rocket::form::Form;
use rocket::response::{Debug, Redirect};
use rocket::serde::{Serialize, Deserialize, json::Json};
use rocket_dyn_templates::{context, Template};
use rocket_sync_db_pools::database;
use serde_json::Value as JsonValue;
use diesel::sql_types::Text as SqlText;

use diesel::prelude::*;
use lazy_static::lazy_static;

lazy_static! {}

#[database("diesel")]
struct Db(diesel::SqliteConnection);

type Result<T, E = Debug<diesel::result::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Deserialize, Serialize, Queryable, Insertable)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = knowledge_sources)]
struct KnowledgeSource {
    name: String,
    url: Option<String>,
    croissant_metadata: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize, Queryable, Insertable)]
#[serde(crate = "rocket::serde")]
#[diesel(table_name = knowledge_source_mappings)]
struct KnowledgeSourceMapping {
    source_name: String,
    key: String,
    answer: JsonValue,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldEntry {
    path: String,
    display_path: String,
    value: String,
    source_url: String,
    confidence_display: String,
    confidence_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldGroup {
    name: String,
    fields: Vec<FieldEntry>,
}

table! {
    knowledge_sources (name) {
        name -> diesel::sql_types::Text,
        url -> diesel::sql_types::Nullable<diesel::sql_types::Text>,
        croissant_metadata -> diesel::sql_types::Json,
    }
}

table! {
    knowledge_source_mappings (source_name, key) {
        source_name -> diesel::sql_types::Text,
        key -> diesel::sql_types::Text,
        answer -> diesel::sql_types::Json,
    }
}

// Recursively strip {value, source_url, confidence} envelopes, returning clean Croissant JSON
fn strip_envelopes(val: &JsonValue) -> JsonValue {
    match val {
        JsonValue::Object(map) => {
            if map.contains_key("value") && map.contains_key("source_url") && map.contains_key("confidence") {
                strip_envelopes(&map["value"])
            } else {
                let new_map: serde_json::Map<String, JsonValue> = map.iter()
                    .map(|(k, v)| (k.clone(), strip_envelopes(v)))
                    .collect();
                JsonValue::Object(new_map)
            }
        }
        JsonValue::Array(arr) => JsonValue::Array(arr.iter().map(strip_envelopes).collect()),
        _ => val.clone(),
    }
}

// Format a dot-path like "distribution.0.name" into "distribution[1].name"
fn format_display_path(path: &str) -> String {
    let mut result = String::new();
    for (i, part) in path.split('.').enumerate() {
        if let Ok(n) = part.parse::<usize>() {
            result.push_str(&format!("[{}]", n + 1));
        } else {
            if i > 0 { result.push('.'); }
            result.push_str(part);
        }
    }
    result
}

// Determine the section group name from a field path
fn get_group(path: &str) -> String {
    let mut parts = path.splitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(_), None) => "Dataset".to_string(),
        (Some(top), Some(next)) => {
            let capitalized = {
                let mut c = top.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            };
            if let Ok(n) = next.parse::<usize>() {
                format!("{} [{}]", capitalized, n + 1)
            } else {
                capitalized
            }
        }
        _ => "Other".to_string(),
    }
}

// Walk the JSON tree and collect all fields that have the envelope format
fn extract_enveloped_fields(val: &JsonValue, path: &str, fields: &mut Vec<FieldEntry>) {
    match val {
        JsonValue::Object(map) => {
            let is_envelope = !path.is_empty()
                && map.contains_key("value")
                && map.contains_key("source_url")
                && map.contains_key("confidence");

            if is_envelope {
                let value = match map.get("value") {
                    Some(JsonValue::String(s)) => s.clone(),
                    Some(JsonValue::Null) | None => String::new(),
                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                };
                let source_url = map.get("source_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let confidence = map.get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let confidence_label = if confidence >= 0.9 { "high" }
                    else if confidence >= 0.7 { "medium" }
                    else if confidence > 0.0 { "low" }
                    else { "unknown" }.to_string();

                fields.push(FieldEntry {
                    path: path.to_string(),
                    display_path: format_display_path(path),
                    value,
                    source_url,
                    confidence_display: format!("{:.0}%", confidence * 100.0),
                    confidence_label,
                });
            } else {
                for (key, child) in map {
                    if key.starts_with('@') { continue; }
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", path, key)
                    };
                    extract_enveloped_fields(child, &child_path, fields);
                }
            }
        }
        JsonValue::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_path = format!("{}.{}", path, i);
                extract_enveloped_fields(item, &child_path, fields);
            }
        }
        _ => {}
    }
}

fn group_fields(fields: Vec<FieldEntry>) -> Vec<FieldGroup> {
    let mut groups: Vec<FieldGroup> = Vec::new();
    for field in fields {
        let group_name = get_group(&field.path);
        if let Some(group) = groups.iter_mut().find(|g| g.name == group_name) {
            group.fields.push(field);
        } else {
            groups.push(FieldGroup { name: group_name, fields: vec![field] });
        }
    }
    groups
}

// Navigate to the envelope at the given dot-path and update its value/source/confidence
fn set_envelope_at_path(val: &mut JsonValue, path: &[&str], new_value_str: &str, new_source_url: &str) {
    if path.is_empty() { return; }

    if path.len() == 1 {
        if let JsonValue::Object(map) = val {
            if let Some(JsonValue::Object(env)) = map.get_mut(path[0]) {
                let typed: JsonValue = serde_json::from_str(new_value_str)
                    .unwrap_or_else(|_| JsonValue::String(new_value_str.to_string()));
                env.insert("value".to_string(), typed);
                env.insert("source_url".to_string(), JsonValue::String(new_source_url.to_string()));
                env.insert("confidence".to_string(), serde_json::json!(1.0));
            }
        }
        return;
    }

    match val {
        JsonValue::Object(map) => {
            if let Some(child) = map.get_mut(path[0]) {
                set_envelope_at_path(child, &path[1..], new_value_str, new_source_url);
            }
        }
        JsonValue::Array(arr) => {
            if let Ok(idx) = path[0].parse::<usize>() {
                if let Some(child) = arr.get_mut(idx) {
                    set_envelope_at_path(child, &path[1..], new_value_str, new_source_url);
                }
            }
        }
        _ => {}
    }
}

#[get("/knowledge_source/names")]
async fn names(db: Db) -> Result<Json<Vec<String>>> {
    let ids = db.run(move |conn| {
        knowledge_sources::table.select(knowledge_sources::name).load::<String>(conn)
    }).await?;
    Ok(Json(ids))
}

// Returns clean (envelope-stripped) Croissant JSON for download
#[get("/knowledge_source/<name>")]
async fn knowledge_source(db: Db, name: String) -> Result<JsonValue> {
    let ks: KnowledgeSource = db.run(move |conn| {
        knowledge_sources::table
            .filter(knowledge_sources::name.eq(name))
            .first(conn)
    }).await?;
    Ok(strip_envelopes(&ks.croissant_metadata))
}

#[get("/")]
async fn index(db: Db) -> Result<Template> {
    let results: Vec<KnowledgeSource> = db.run(move |conn| {
        knowledge_sources::table.load(conn)
    }).await?;

    let items: Vec<JsonValue> = results.iter().map(|ks| {
        serde_json::json!({
            "name": ks.name,
            "url": ks.url,
            "clean_metadata": strip_envelopes(&ks.croissant_metadata),
        })
    }).collect();

    Ok(Template::render("index", context! {
        title: "Home",
        items: items,
    }))
}

#[get("/update/<name>")]
async fn update_view(db: Db, name: String) -> Result<Template> {
    let name2 = name.clone();

    let ks: KnowledgeSource = db.run(move |conn| {
        knowledge_sources::table
            .filter(knowledge_sources::name.eq(name2))
            .first(conn)
    }).await?;

    let mut all_fields: Vec<FieldEntry> = Vec::new();
    extract_enveloped_fields(&ks.croissant_metadata, "", &mut all_fields);
    let total_fields = all_fields.len();
    let groups = group_fields(all_fields);

    Ok(Template::render("update", context! {
        title: "Update",
        item: ks,
        groups: groups,
        total_fields: total_fields,
    }))
}

#[derive(FromForm)]
struct UpdateFullJson {
    croissant_metadata: String,
}

#[post("/update/<name>", data = "<form>")]
async fn update(db: Db, name: String, form: Form<UpdateFullJson>) -> Result<Redirect> {
    let metadata: JsonValue = serde_json::from_str(&form.croissant_metadata).map_err(|e| {
        Debug(diesel::result::Error::DeserializationError(Box::new(e)))
    })?;

    db.run(move |conn| {
        diesel::update(knowledge_sources::table.filter(knowledge_sources::name.eq(name)))
            .set(knowledge_sources::croissant_metadata.eq(metadata))
            .execute(conn)
    }).await?;

    Ok(Redirect::to(uri!(index)))
}

#[derive(FromForm)]
struct UpdateFieldsForm {
    fields_json: String,
}

#[post("/update/<name>/fields", data = "<form>")]
async fn update_fields(db: Db, name: String, form: Form<UpdateFieldsForm>) -> Result<Redirect> {
    let updates: Vec<JsonValue> = serde_json::from_str(&form.fields_json).map_err(|e| {
        Debug(diesel::result::Error::DeserializationError(Box::new(e)))
    })?;

    let name_for_redirect = name.clone();

    db.run(move |conn| -> diesel::QueryResult<()> {
        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS corrections (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                knowledge_source TEXT NOT NULL,
                field_path TEXT NOT NULL,
                original_value TEXT,
                corrected_value TEXT,
                corrected_source_url TEXT,
                corrected_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )"
        ).execute(conn)?;

        let ks: KnowledgeSource = knowledge_sources::table
            .filter(knowledge_sources::name.eq(&name))
            .first(conn)?;

        let mut metadata = ks.croissant_metadata.clone();

        for update in &updates {
            let path = match update["path"].as_str() { Some(p) => p, None => continue };
            let new_value_str = update["value"].as_str().unwrap_or("");
            let new_source = update["source_url"].as_str().unwrap_or("");
            let original_value = match update.get("original_value") {
                Some(JsonValue::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };

            let path_parts: Vec<&str> = path.split('.').collect();
            set_envelope_at_path(&mut metadata, &path_parts, new_value_str, new_source);

            diesel::sql_query(
                "INSERT INTO corrections (knowledge_source, field_path, original_value, corrected_value, corrected_source_url) VALUES (?, ?, ?, ?, ?)"
            )
            .bind::<SqlText, _>(name.clone())
            .bind::<SqlText, _>(path.to_string())
            .bind::<SqlText, _>(original_value)
            .bind::<SqlText, _>(new_value_str.to_string())
            .bind::<SqlText, _>(new_source.to_string())
            .execute(conn)?;

            // Keep knowledge_source_mappings in sync for key fields
            const KEY_FIELDS: &[&str] = &["name", "description", "version", "citation", "license", "keywords"];
            if KEY_FIELDS.contains(&path) {
                let typed: JsonValue = serde_json::from_str(new_value_str)
                    .unwrap_or_else(|_| JsonValue::String(new_value_str.to_string()));
                let new_answer = serde_json::json!({
                    "value": typed,
                    "source_url": new_source,
                    "confidence": 1.0
                });
                diesel::update(
                    knowledge_source_mappings::table
                        .filter(knowledge_source_mappings::source_name.eq(&name))
                        .filter(knowledge_source_mappings::key.eq(path))
                )
                .set(knowledge_source_mappings::answer.eq(new_answer))
                .execute(conn)?;
            }
        }

        diesel::update(knowledge_sources::table.filter(knowledge_sources::name.eq(&name)))
            .set(knowledge_sources::croissant_metadata.eq(metadata))
            .execute(conn)?;

        Ok(())
    }).await?;

    Ok(Redirect::to(format!("/update/{}", name_for_redirect)))
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .attach(Db::fairing())
        .attach(Template::fairing())
        .mount("/", routes![index, knowledge_source, names, update_view, update, update_fields])
}
