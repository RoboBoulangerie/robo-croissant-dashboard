#[macro_use] extern crate rocket;
use rocket::form::Form;
use rocket::response::{Debug, Redirect};
use rocket::serde::{Serialize, Deserialize, json::Json};
use rocket_dyn_templates::{context, Template};
use rocket_sync_db_pools::database;
use serde_json::Value as JsonValue;

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
    croissant_metadata: JsonValue,
}

table! {
    knowledge_sources (name) {
        name -> diesel::sql_types::Text,
        croissant_metadata -> diesel::sql_types::Json,
    }
}

#[get("/knowledge_source/names")]
async fn names(db: Db) -> Result<Json<Vec<String>>> {
    let ids: Vec<String> = db.run(move |conn| {
        knowledge_sources::table.select(knowledge_sources::name).load(conn)
    }).await?;

    Ok(Json(ids))
}

#[get("/knowledge_source/<name>")]
async fn knowledge_source(db: Db, name: String) -> Result<JsonValue> {
    let ks: KnowledgeSource = db.run(move |conn| {
        knowledge_sources::table
            .filter(knowledge_sources::name.eq(name))
            .first(conn)
    }).await?;
    Ok(ks.croissant_metadata)
}

#[get("/")]
async fn index(db: Db) -> Result<Template> {

    let results: Vec<KnowledgeSource> = db.run(move |conn| {
        knowledge_sources::table.load(conn)
    }).await?;

    Ok(Template::render("index", context! {
        title: "Home",
        items: results,
    }))
}

#[derive(FromForm)]
struct UpdateKnowledgeSource {
    croissant_metadata: String,
}

#[get("/update/<name>")]
async fn update_view(db: Db, name: String) -> Result<Template> {
    let ks: KnowledgeSource = db.run(move |conn| {
        knowledge_sources::table
            .filter(knowledge_sources::name.eq(name))
            .first(conn)
    }).await?;

    Ok(Template::render("update", context! {
        title: "Update",
        item: ks,
    }))
}

#[post("/update/<name>", data = "<update>")]
async fn update(db: Db, name: String, update: Form<UpdateKnowledgeSource>) -> Result<Redirect> {
    let metadata: JsonValue = serde_json::from_str(&update.croissant_metadata).map_err(|e| {
        Debug(diesel::result::Error::DeserializationError(Box::new(e)))
    })?;

    db.run(move |conn| {
        diesel::update(knowledge_sources::table.filter(knowledge_sources::name.eq(name)))
            .set(knowledge_sources::croissant_metadata.eq(metadata))
            .execute(conn)
    }).await?;

    Ok(Redirect::to(uri!(index)))
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .attach(Db::fairing())
        .attach(Template::fairing())
        .mount("/", routes![index, knowledge_source, names, update_view, update])
}