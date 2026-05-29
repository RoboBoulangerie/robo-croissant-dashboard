#[macro_use]
extern crate rocket;
#[macro_use]
extern crate log;

use capitalize::Capitalize;
use diesel::debug_query;
use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use json_dotpath::DotPaths;
use regex::Regex;
use rocket::form::Form;
use rocket::response::{Debug, Redirect};
use rocket::serde::{Serialize, json::Json};
use rocket_dyn_templates::{Template, context};
use rocket_sync_db_pools::database;
use serde_json::Value as JsonValue;

mod db_model;
mod db_schema;

#[database("diesel")]
struct Db(diesel::SqliteConnection);

type Result<T, E = Debug<diesel::result::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldEntry {
    path: String,
    value: String,
    url: String,
    confidence_display: String,
    confidence_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldGroup {
    name: String,
    fields: Vec<FieldEntry>,
}

// Determine the section group name from a field path
fn get_group(path: &str) -> String {
    let re = Regex::new(r"\[[0-9]+\]").unwrap();
    let mut parts = path.splitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(_), None) => "Dataset".to_string(),
        (Some(top), Some(next)) => match (re.is_match(top), re.is_match(next)) {
            (true, true) => {
                format!("{} - {}", top.capitalize_first_only(), next.capitalize_first_only(),)
            }
            (true, false) => {
                format!("{}", top.capitalize_first_only())
            }

            (false, true) => format!("{} - {}", top.capitalize_first_only(), next.capitalize_first_only()),
            (false, false) => top.capitalize_first_only(),
        },
        _ => "Other".to_string(),
    }
}

fn group_fields(fields: Vec<FieldEntry>) -> Vec<FieldGroup> {
    let mut groups: Vec<FieldGroup> = Vec::new();
    for field in fields {
        let group_name = get_group(&field.path);
        if let Some(group) = groups.iter_mut().find(|g| g.name == group_name) {
            group.fields.push(field);
        } else {
            groups.push(FieldGroup {
                name: group_name,
                fields: vec![field],
            });
        }
    }
    groups
}

#[get("/knowledge_base/names")]
async fn names(db: Db) -> Result<Json<Vec<String>>> {
    let ids = db
        .run(move |conn| db_schema::knowledge_bases::table.select(db_schema::knowledge_bases::name).load::<String>(conn))
        .await?;
    Ok(Json(ids))
}

// Returns clean (envelope-stripped) Croissant JSON for download
#[get("/knowledge_base/<name>")]
async fn knowledge_base(db: Db, name: String) -> Result<JsonValue> {
    let ks: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name)).first(conn))
        .await?;
    Ok(ks.croissant_metadata)
}

#[get("/")]
async fn index(db: Db) -> Result<Template> {
    let results: Vec<db_model::KnowledgeBase> = db.run(move |conn| db_schema::knowledge_bases::table.load(conn)).await?;

    let items: Vec<JsonValue> = results
        .iter()
        .map(|ks| {
            serde_json::json!({
                "name": ks.name,
                "url": ks.url,
                "croissant_metadata": ks.croissant_metadata,
            })
        })
        .collect();

    Ok(Template::render(
        "index",
        context! {
            title: "Home",
            items: items,
        },
    ))
}

#[get("/update/<name>")]
async fn update_view(db: Db, name: String) -> Result<Template> {
    let name1 = name.clone();
    let ks: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name1)).first(conn))
        .await?;

    let name2 = name.clone();
    let links: Vec<db_model::KnowledgeBaseLink> = db
        .run(move |conn| {
            db_schema::kb_links::table
                .filter(db_schema::kb_links::kb_name.eq(name2))
                // .order_by(db_schema::kb_links::path.asc())
                .load::<db_model::KnowledgeBaseLink>(conn)
        })
        .await?;

    let all_fields: Vec<FieldEntry> = links
        .iter()
        .map(|l| FieldEntry {
            path: l.path.to_string(),
            value: {
                if let Some(a) = serde_json::from_str(l.value.as_str()).ok() {
                    match a {
                        JsonValue::Array(s) => {
                            let asdf: Vec<String> = s.iter().map(|v| v.to_string()).collect();
                            asdf.join(", ")
                        }
                        _ => l.value.to_string(),
                    }
                } else {
                    l.value.to_string()
                }
            },
            url: l.url.to_string(),
            confidence_display: format!("{:.0}%", l.confidence * 100.0),
            confidence_label: match l.confidence {
                i if i >= 0.9 => "high".to_string(),
                i if i >= 0.7 => "medium".to_string(),
                i if i > 0.0 => "low".to_string(),
                _ => "unknown".to_string(),
            },
        })
        .collect();

    let total_fields = all_fields.len();
    let groups = group_fields(all_fields);

    Ok(Template::render(
        "update",
        context! {
            title: "Update",
            item: ks,
            groups: groups,
            total_fields: total_fields,
        },
    ))
}

#[derive(FromForm)]
struct UpdateFullJson {
    croissant_metadata: String,
}

#[post("/update/<name>", data = "<form>")]
async fn update(db: Db, name: String, form: Form<UpdateFullJson>) -> Result<Redirect> {
    let metadata: JsonValue = serde_json::from_str(&form.croissant_metadata).map_err(|e| Debug(diesel::result::Error::DeserializationError(Box::new(e))))?;

    db.run(move |conn| {
        diesel::update(db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name)))
            .set(db_schema::knowledge_bases::croissant_metadata.eq(metadata))
            .execute(conn)
    })
    .await?;

    Ok(Redirect::to(uri!(index)))
}

#[derive(FromForm)]
struct UpdateFieldsForm {
    fields_json: String,
}

#[post("/update/<name>/fields", data = "<form>")]
async fn update_fields(db: Db, name: String, form: Form<UpdateFieldsForm>) -> Result<Redirect> {
    info!("{}", form.fields_json);
    let updates: Vec<JsonValue> = serde_json::from_str(&form.fields_json).map_err(|e| Debug(diesel::result::Error::DeserializationError(Box::new(e))))?;

    let name1 = name.clone();
    let mut kb: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name1)).first(conn))
        .await?;

    for update in updates {
        let path = update["path"].as_str().unwrap_or("").to_string();
        let url = update["url"].as_str().unwrap_or("").to_string();
        let value = update["value"].as_str().unwrap_or("").to_string();

        let name2 = name.clone();
        let mut link: db_model::KnowledgeBaseLink = db
            .run(move |conn| {
                db_schema::kb_links::table
                    .filter(db_schema::kb_links::kb_name.eq(name2))
                    .filter(db_schema::kb_links::path.eq(path.clone()))
                    .first(conn)
            })
            .await?;

        link.url = url.clone();
        link.value = value.clone();
        link.confidence = 1.0;

        let update_link = diesel::update(
            db_schema::kb_links::table
                .filter(db_schema::kb_links::dsl::kb_name.eq(link.kb_name.clone()))
                .filter(db_schema::kb_links::dsl::path.eq(link.path.clone())),
        )
        .set(link);
        debug!("{}", debug_query::<Sqlite, _>(&update_link).to_string());
        let num_kb_links_udpate = db.run(move |conn| update_link.execute(conn).unwrap()).await;
        debug!("num_kb_links_udpate: {}", num_kb_links_udpate);

        let mut cr_metadata_json = kb.croissant_metadata.clone();
        cr_metadata_json
            .dot_set(update["path"].as_str().unwrap_or(""), update["value"].as_str().unwrap_or(""))
            .unwrap();
        kb.croissant_metadata = cr_metadata_json;

        let name3 = name.clone();
        let update_kb = diesel::update(db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::dsl::name.eq(name3))).set(kb.clone());
        // debug!("{}", debug_query::<Sqlite, _>(&update_kb).to_string());
        let num_update_kb = db.run(move |conn| update_kb.execute(conn).unwrap()).await;
        debug!("num_update_kb: {}", num_update_kb);
    }

    Ok(Redirect::to(uri!(update_view(name.clone()))))
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .attach(Db::fairing())
        .attach(Template::fairing())
        .mount("/", routes![index, knowledge_base, names, update_view, update, update_fields])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_group() {
        // Single part path
        assert_eq!(get_group("name"), "Dataset");
        assert_eq!(get_group("description"), "Dataset");

        assert_eq!(get_group("distribution[0].name"), "Distribution[0]");
        assert_eq!(get_group("recordSet[0].field[0].name"), "RecordSet[0] - Field[0]");

        // Multi part path with non-numeric second part
        assert_eq!(get_group("distribution.name"), "Distribution");
        assert_eq!(get_group("recordSet.name"), "RecordSet");

        // Edge cases
        assert_eq!(get_group(""), "Dataset"); // splits into [""] -> (Some(""), None)
        assert_eq!(get_group("a.b.c.d"), "A"); // splits into ["a", "b", "c.d"] -> (Some("a"), Some("b")) -> capitalized "A"
    }
}
