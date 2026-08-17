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
use rocket::fairing::AdHoc;
use rocket::form::Form;
use rocket::response::{Debug, Redirect};
use rocket::serde::{Serialize, json::Json};
use rocket_dyn_templates::{Template, context};
use rocket_sync_db_pools::database;
use serde_json::Value as JsonValue;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

mod db_model;
mod db_schema;

#[database("diesel")]
struct Db(diesel::SqliteConnection);

type Result<T, E = Debug<diesel::result::Error>> = std::result::Result<T, E>;

// Croissant / schema.org controlled vocabularies
const VALID_TYPES: &[&str] = &[
    "sc:Dataset",
    "cr:FileObject",
    "cr:FileSet",
    "cr:RecordSet",
    "cr:Field",
    "sc:Organization",
    "sc:Person",
    "sc:CreativeWork",
];
const VALID_DATA_TYPES: &[&str] = &[
    "sc:Text",
    "sc:Integer",
    "sc:Float",
    "sc:Boolean",
    "sc:Date",
    "sc:Time",
    "sc:DateTime",
    "sc:URL",
];
const CROISSANT_CONFORMS_TO: &str = "http://mlcommons.org/croissant/1.0";

// Croissant files are JSON-LD documents (they carry an @context), so they're
// exported as .jsonld, not .json.
const FINAL_CROISSANTS_DIR: &str = "/Users/ekcarter/Desktop/robo-croissant-reviews/final_croissants";

fn write_jsonld_file(path: &std::path::Path, metadata: &JsonValue) {
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!("final_croissants: could not create {}: {}", dir.display(), e);
            return;
        }
    }
    match serde_json::to_string_pretty(metadata) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                warn!("final_croissants: could not write {}: {}", path.display(), e);
            }
        }
        Err(e) => warn!("final_croissants: could not serialize {}: {}", path.display(), e),
    }
}

// Every save (per-field, full-JSON, or reconcile commit) gets its own
// timestamped snapshot, so no save is ever silently overwritten.
fn write_croissant_snapshot(name: &str, metadata: &JsonValue) {
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = std::path::Path::new(FINAL_CROISSANTS_DIR).join(format!("{}_{}.jsonld", name, ts));
    write_jsonld_file(&path, metadata);
}

// The single canonical "current final" file for a KB — overwritten only by
// whole-document actions (full-JSON save, reconcile commit), not by
// incremental per-field saves. This is what "export full JSON" now means:
// saving IS exporting, straight to the correct Desktop directory.
fn write_final_croissant(name: &str, metadata: &JsonValue) {
    let path = std::path::Path::new(FINAL_CROISSANTS_DIR).join(format!("{}.jsonld", name));
    write_jsonld_file(&path, metadata);
}

// Whole-document replaces (full-JSON save, reconcile commit) can reorder or
// resize arrays like distribution/recordSet, leaving kb_links — and the
// review grid / validation issues built from it — silently stale. Run
// resync_kb_links.py synchronously (DB-only, fast) so it's caught immediately.
async fn resync_kb_links_blocking(name: &str) -> String {
    match rocket::tokio::process::Command::new("python3")
        .args(["scripts/resync_kb_links.py", "--kb", name])
        .output()
        .await
    {
        Ok(out) if out.status.success() => "Synced review fields to the new document.".to_string(),
        Ok(out) => {
            warn!("resync_kb_links.py --kb {} failed: {}", name, String::from_utf8_lossy(&out.stderr));
            "Field resync step failed — check server logs.".to_string()
        }
        Err(e) => {
            warn!("could not run resync_kb_links.py: {}", e);
            "Could not run field resync step — check server logs.".to_string()
        }
    }
}

// Validation does live URL checks and can be slow, so it's kicked off in the
// background instead of blocking the response.
fn spawn_background_validate(name: &str) {
    if let Err(e) = rocket::tokio::process::Command::new("python3")
        .args(["scripts/validate_all.py", "--kb", name])
        .spawn()
    {
        warn!("could not start validate_all.py --kb {}: {}", name, e);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct RecordSetGroup {
    schema_summary: String,
    field_count: usize,
    members: Vec<String>,
    is_duplicate: bool,
    has_placeholders: bool,
}

fn build_schema_groups(metadata: &JsonValue) -> Vec<RecordSetGroup> {
    let record_sets = match metadata.get("recordSet").and_then(|v| v.as_array()) {
        Some(rs) => rs,
        None => return vec![],
    };
    let placeholder_re = Regex::new(r"(?i)^column_\d+$").unwrap();
    // (fingerprint, members, field_count, has_placeholders)
    let mut groups: Vec<(Vec<String>, Vec<String>, usize, bool)> = Vec::new();
    for rs in record_sets {
        let label = rs.get("@id")
            .or_else(|| rs.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed)")
            .to_string();
        let fields = rs.get("field").and_then(|v| v.as_array());
        let fingerprint: Vec<String> = fields
            .map(|fs| {
                fs.iter()
                    .filter_map(|f| f.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let has_ph = fingerprint.iter().any(|n| placeholder_re.is_match(n));
        let field_count = fingerprint.len();
        if let Some(g) = groups.iter_mut().find(|(fp, _, _, _)| *fp == fingerprint) {
            g.1.push(label);
            g.3 = g.3 || has_ph;
        } else {
            groups.push((fingerprint, vec![label], field_count, has_ph));
        }
    }
    groups
        .into_iter()
        .map(|(fingerprint, members, field_count, has_placeholders)| {
            let is_duplicate = members.len() > 1;
            let preview: Vec<&str> = fingerprint.iter().take(4).map(|s| s.as_str()).collect();
            let schema_summary = if preview.is_empty() {
                "(no fields defined)".to_string()
            } else if fingerprint.len() > 4 {
                format!("{}, …", preview.join(", "))
            } else {
                preview.join(", ")
            };
            RecordSetGroup { schema_summary, field_count, members, is_duplicate, has_placeholders }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldEntry {
    path: String,
    value: String,
    url: String,
    confidence_display: String,
    confidence_label: String,
    pre_accepted: bool,
    pre_auto_reviewed: bool,
    issue_type: String,
    issue_detail: String,
    run_changed: bool,
}

#[derive(diesel::QueryableByName)]
struct RawPath {
    #[diesel(sql_type = diesel::sql_types::Text)]
    path: String,
}

#[derive(diesel::QueryableByName)]
struct RawCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    c: i64,
}

#[derive(diesel::QueryableByName)]
struct IssueRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    issue_type: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    path: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct FieldGroup {
    name: String,
    fields: Vec<FieldEntry>,
}

static GROUP_IDX_RE: OnceLock<Regex> = OnceLock::new();

// Determine the section group name from a field path
fn get_group(path: &str) -> String {
    let re = GROUP_IDX_RE.get_or_init(|| Regex::new(r"\[[0-9]+\]").unwrap());
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

static NATURAL_SORT_RE: OnceLock<Regex> = OnceLock::new();

// Pad numeric indices in bracket notation so string sort == numeric sort.
// e.g. "distribution[10]" → "distribution[00000010]"
fn natural_sort_key(s: &str) -> String {
    let re = NATURAL_SORT_RE.get_or_init(|| Regex::new(r"\[(\d+)\]").unwrap());
    re.replace_all(s, |caps: &regex::Captures| {
        format!("[{:08}]", caps[1].parse::<u64>().unwrap_or(0))
    })
    .to_string()
}

fn path_to_anchor(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
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

#[derive(diesel::QueryableByName)]
struct KbUrlRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    kb_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    url: String,
}

#[derive(diesel::QueryableByName)]
struct KbReviewRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    kb_name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    total: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    reviewed: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    auto_reviewed_count: i64,
}

#[derive(diesel::QueryableByName)]
struct KbIssueRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    kb_name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    total_issues: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    error_count: i64,
}

#[get("/")]
async fn index(db: Db) -> Result<Template> {
    let working_kbs: Vec<(String, Option<String>)> = db
        .run(|conn| {
            db_schema::knowledge_bases::table
                .select((db_schema::knowledge_bases::name, db_schema::knowledge_bases::url))
                .load(conn)
        })
        .await?;

    let working_names: HashSet<String> = working_kbs.iter().map(|(n, _)| n.clone()).collect();

    // Load runs sorted newest-first (cheap — typically < 20 rows)
    let all_runs: Vec<db_model::Run> = db
        .run(|conn| {
            diesel::sql_query(
                "SELECT * FROM runs ORDER BY COALESCE(run_date, imported_at) DESC",
            )
            .load::<db_model::Run>(conn)
        })
        .await
        .unwrap_or_default();

    // Only hit run_links when runs actually exist
    let (all_run_kb_names, kb_url_map, kb_latest_run) = if all_runs.is_empty() {
        (Vec::new(), HashMap::new(), HashMap::new())
    } else {
        // Query 1: distinct KB names — index-only scan on (kb_name, path)
        let kb_names: Vec<String> = db
            .run(|conn| {
                diesel::sql_query("SELECT DISTINCT kb_name AS path FROM run_links ORDER BY kb_name")
                    .load::<RawPath>(conn)
            })
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.path)
            .collect();

        // Query 2: URLs — index seek on path='url', fast
        let url_rows: Vec<KbUrlRow> = db
            .run(|conn| {
                diesel::sql_query(
                    "SELECT kb_name, MIN(value) as url FROM run_links \
                     WHERE path = 'url' GROUP BY kb_name",
                )
                .load(conn)
            })
            .await
            .unwrap_or_default();
        let url_map: HashMap<String, String> =
            url_rows.into_iter().map(|r| (r.kb_name, r.url)).collect();

        // Query 3: latest run per KB — one query per run via primary key (run_id, kb_name, path)
        // Runs are already sorted newest-first; first occurrence wins.
        let mut latest_run_map: HashMap<String, (String, Option<String>)> = HashMap::new();
        for run in &all_runs {
            let run_id = run.id;
            let kb_names_in_run: Vec<String> = db
                .run(move |conn| {
                    diesel::sql_query(
                        "SELECT DISTINCT kb_name as path FROM run_links WHERE run_id = ?",
                    )
                    .bind::<diesel::sql_types::Integer, _>(run_id)
                    .load::<RawPath>(conn)
                })
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.path)
                .collect();
            for kb in kb_names_in_run {
                latest_run_map
                    .entry(kb)
                    .or_insert_with(|| (run.label.clone(), run.run_date.clone()));
            }
        }

        (kb_names, url_map, latest_run_map)
    };

    // Per-KB review completion (fields reviewed / total fields)
    let review_stats: HashMap<String, (i64, i64, i64)> = db
        .run(|conn| {
            diesel::sql_query(
                "SELECT kb_name, COUNT(*) as total, \
                 SUM(CASE WHEN reviewed=1 OR auto_reviewed=1 THEN 1 ELSE 0 END) as reviewed, \
                 SUM(CASE WHEN auto_reviewed=1 THEN 1 ELSE 0 END) as auto_reviewed_count \
                 FROM kb_links GROUP BY kb_name",
            )
            .load::<KbReviewRow>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.kb_name, (r.total, r.reviewed, r.auto_reviewed_count)))
        .collect();

    // KBs with a staged version awaiting reconciliation (handles missing table gracefully)
    let staged_kb_names: HashSet<String> = db
        .run(|conn| {
            db_schema::staged_kb_versions::table
                .select(db_schema::staged_kb_versions::kb_name)
                .load::<String>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    // Per-KB validation issue counts (handles missing table gracefully)
    let issue_stats: HashMap<String, (i64, i64)> = db
        .run(|conn| {
            diesel::sql_query(
                "SELECT kb_name, COUNT(*) as total_issues, \
                 SUM(CASE WHEN issue_type IN \
                   ('repeated_content_url','duplicate_distribution_id','broken_source_reference',\
                    'missing_required_field','missing_field_source') \
                 THEN 1 ELSE 0 END) as error_count \
                 FROM validation_issues GROUP BY kb_name",
            )
            .load::<KbIssueRow>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.kb_name, (r.total_issues, r.error_count)))
        .collect();

    let mut items: Vec<JsonValue> = working_kbs
        .iter()
        .map(|(name, url)| {
            let run_url = kb_url_map.get(name).cloned();
            let (latest_label, latest_date) = kb_latest_run
                .get(name)
                .map(|(l, d)| (Some(l.clone()), d.clone()))
                .unwrap_or((None, None));
            let (total_fields, reviewed_fields, auto_reviewed_count) =
                review_stats.get(name).copied().unwrap_or((0, 0, 0));
            let (total_issues, error_count) =
                issue_stats.get(name).copied().unwrap_or((0, 0));
            let pct = if total_fields > 0 { reviewed_fields * 100 / total_fields } else { 0 };
            let has_been_validated = auto_reviewed_count > 0 || total_issues > 0;
            let has_staged_version = staged_kb_names.contains(name);
            serde_json::json!({
                "name": name,
                "url": url.as_ref().or(run_url.as_ref()),
                "in_working_db": true,
                "latest_run_label": latest_label,
                "latest_run_date": latest_date,
                "total_fields": total_fields,
                "reviewed_fields": reviewed_fields,
                "pct_reviewed": pct,
                "total_issues": total_issues,
                "error_count": error_count,
                "has_been_validated": has_been_validated,
                "has_staged_version": has_staged_version,
            })
        })
        .collect();

    for kb_name in &all_run_kb_names {
        if !working_names.contains(kb_name) {
            let (latest_label, latest_date) = kb_latest_run
                .get(kb_name)
                .map(|(l, d)| (Some(l.clone()), d.clone()))
                .unwrap_or((None, None));
            items.push(serde_json::json!({
                "name": kb_name,
                "url": kb_url_map.get(kb_name),
                "in_working_db": false,
                "latest_run_label": latest_label,
                "latest_run_date": latest_date,
            }));
        }
    }

    items.sort_by_key(|i| i["name"].as_str().unwrap_or("").to_lowercase());

    Ok(Template::render(
        "index",
        context! {
            title: "Home",
            items: items,
        },
    ))
}

const PAGE_SIZE: i64 = 500;

#[get("/update/<name>?<page>")]
async fn update_view(db: Db, name: String, page: Option<i64>) -> Result<Template> {
    let current_page = page.unwrap_or(0).max(0);
    let offset = current_page * PAGE_SIZE;

    let name1 = name.clone();
    let ks: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name1)).first(conn))
        .await?;

    let name2 = name.clone();
    let total_fields: i64 = db
        .run(move |conn| {
            db_schema::kb_links::table
                .filter(db_schema::kb_links::kb_name.eq(name2))
                .count()
                .get_result(conn)
        })
        .await?;

    let name3 = name.clone();
    let links: Vec<db_model::KnowledgeBaseLink> = db
        .run(move |conn| {
            db_schema::kb_links::table
                .filter(db_schema::kb_links::kb_name.eq(name3))
                .load::<db_model::KnowledgeBaseLink>(conn)
        })
        .await?;

    // Load all validation issues for this KB and build a path→(type,detail) map
    let name4 = name.clone();
    let issue_pairs: Vec<(String, String, String)> = db
        .run(move |conn| {
            db_schema::validation_issues::table
                .filter(db_schema::validation_issues::kb_name.eq(name4))
                .select((
                    db_schema::validation_issues::path,
                    db_schema::validation_issues::issue_type,
                    db_schema::validation_issues::detail,
                ))
                .load::<(String, String, String)>(conn)
        })
        .await
        .unwrap_or_default();

    let mut issues_map: HashMap<String, (String, String)> = HashMap::new();
    for (path, itype, detail) in issue_pairs {
        issues_map.entry(path).or_insert((itype, detail));
    }

    let mut page_fields: Vec<FieldEntry> = links
        .iter()
        .map(|l| {
            let (issue_type, issue_detail) = issues_map
                .get(&l.path)
                .map(|(t, d)| (t.clone(), d.clone()))
                .unwrap_or_else(|| (String::new(), String::new()));
            FieldEntry {
                path: l.path.to_string(),
                value: {
                    if let Some(a) = serde_json::from_str(l.value.as_str()).ok() {
                        match a {
                            JsonValue::Array(s) => {
                                let parts: Vec<String> = s.iter().map(|v| v.to_string()).collect();
                                parts.join(", ")
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
                pre_accepted: l.reviewed,
                pre_auto_reviewed: l.auto_reviewed,
                issue_type,
                issue_detail,
                run_changed: false,
            }
        })
        .collect();

    page_fields.sort_by_cached_key(|f| natural_sort_key(&f.path));

    // Paths that have different values across runs — used for change indicator
    let name6 = name.clone();
    let changed_paths: std::collections::HashSet<String> = db
        .run(move |conn| {
            diesel::sql_query(
                "SELECT path FROM run_links WHERE kb_name = ? \
                 GROUP BY path HAVING COUNT(DISTINCT value) > 1",
            )
            .bind::<diesel::sql_types::Text, _>(name6)
            .load::<RawPath>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.path)
        .collect();

    let name5 = name.clone();
    let total_reviewed_db: i64 = db
        .run(move |conn| {
            db_schema::kb_links::table
                .filter(db_schema::kb_links::kb_name.eq(name5))
                .filter(
                    db_schema::kb_links::reviewed
                        .eq(true)
                        .or(db_schema::kb_links::auto_reviewed.eq(true)),
                )
                .count()
                .get_result(conn)
        })
        .await?;

    let total_pages = (total_fields + PAGE_SIZE - 1) / PAGE_SIZE;

    // Annotate each field with whether it changed across runs, then paginate
    // after natural sort so pages never split a distribution group
    let page_fields: Vec<FieldEntry> = page_fields
        .into_iter()
        .map(|mut f| {
            f.run_changed = changed_paths.contains(&f.path);
            f
        })
        .skip(offset as usize)
        .take(PAGE_SIZE as usize)
        .collect();

    let page_field_count = page_fields.len();
    let groups = group_fields(page_fields);
    let schema_groups = build_schema_groups(&ks.croissant_metadata);
    let total_record_sets: usize = schema_groups.iter().map(|g| g.members.len()).sum();

    Ok(Template::render(
        "update",
        context! {
            title: "Update",
            item: ks,
            groups: groups,
            schema_groups: schema_groups,
            total_record_sets: total_record_sets,
            page_field_count: page_field_count,
            total_fields: total_fields,
            total_reviewed_db: total_reviewed_db,
            current_page: current_page,
            total_pages: total_pages,
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
    let metadata_for_export = metadata.clone();
    let name2 = name.clone();

    db.run(move |conn| {
        diesel::update(db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name)))
            .set(db_schema::knowledge_bases::croissant_metadata.eq(metadata))
            .execute(conn)
    })
    .await?;
    write_croissant_snapshot(&name2, &metadata_for_export);
    write_final_croissant(&name2, &metadata_for_export);
    resync_kb_links_blocking(&name2).await;
    spawn_background_validate(&name2);

    Ok(Redirect::to(uri!(index)))
}

#[derive(FromForm)]
struct UpdateFieldsForm {
    fields_json: String,
    accepted_json: Option<String>,
}

#[post("/update/<name>/fields?<page>", data = "<form>")]
async fn update_fields(db: Db, name: String, page: Option<i64>, form: Form<UpdateFieldsForm>) -> Result<Redirect> {
    info!("{}", form.fields_json);
    let updates: Vec<JsonValue> = serde_json::from_str(&form.fields_json).map_err(|e| Debug(diesel::result::Error::DeserializationError(Box::new(e))))?;
    let metadata_changed = !updates.is_empty();

    let name1 = name.clone();
    let mut kb: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name1)).first(conn))
        .await?;

    for update in updates {
        let path = update["path"].as_str().unwrap_or("").to_string();
        let url = update["url"].as_str().unwrap_or("").to_string();
        let value = update["value"].as_str().unwrap_or("").to_string();

        let name2 = name.clone();
        let path2 = path.clone();
        let existing: Option<db_model::KnowledgeBaseLink> = db
            .run(move |conn| {
                db_schema::kb_links::table
                    .filter(db_schema::kb_links::kb_name.eq(name2))
                    .filter(db_schema::kb_links::path.eq(path2))
                    .first(conn)
                    .optional()
            })
            .await?;

        match existing {
            Some(mut link) => {
                link.url = url.clone();
                link.value = value.clone();
                link.reviewed = true;
                let update_link = diesel::update(
                    db_schema::kb_links::table
                        .filter(db_schema::kb_links::dsl::kb_name.eq(link.kb_name.clone()))
                        .filter(db_schema::kb_links::dsl::path.eq(link.path.clone())),
                )
                .set(link);
                debug!("{}", debug_query::<Sqlite, _>(&update_link).to_string());
                let num_updated = db.run(move |conn| update_link.execute(conn)).await?;
                debug!("num_kb_links_updated: {}", num_updated);
            }
            None => {
                let new_link = db_model::KnowledgeBaseLink {
                    kb_name: name.clone(),
                    path: path.clone(),
                    value: value.clone(),
                    url: url.clone(),
                    confidence: 0.0,
                    reviewed: true,
                    auto_reviewed: false,
                };
                db.run(move |conn| {
                    diesel::insert_into(db_schema::kb_links::table)
                        .values(&new_link)
                        .execute(conn)
                })
                .await?;
                debug!("inserted new kb_link for path: {}", path);
            }
        }

        let mut cr_metadata_json = kb.croissant_metadata.clone();
        cr_metadata_json
            .dot_set(update["path"].as_str().unwrap_or(""), update["value"].as_str().unwrap_or(""))
            .map_err(|e| Debug(diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))))?;
        kb.croissant_metadata = cr_metadata_json;

        let name3 = name.clone();
        let update_kb = diesel::update(db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::dsl::name.eq(name3))).set(kb.clone());
        let num_update_kb = db.run(move |conn| update_kb.execute(conn)).await?;
        debug!("num_update_kb: {}", num_update_kb);
    }

    let accepted_paths: Vec<String> = form
        .accepted_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    for path in accepted_paths {
        let kb_name_clone = name.clone();
        let path2 = path.clone();
        let exists: bool = db
            .run(move |conn| {
                db_schema::kb_links::table
                    .filter(db_schema::kb_links::dsl::kb_name.eq(kb_name_clone))
                    .filter(db_schema::kb_links::dsl::path.eq(path2))
                    .count()
                    .get_result::<i64>(conn)
            })
            .await?
            > 0;

        if exists {
            let kb_name_clone2 = name.clone();
            db.run(move |conn| {
                diesel::update(
                    db_schema::kb_links::table
                        .filter(db_schema::kb_links::dsl::kb_name.eq(kb_name_clone2))
                        .filter(db_schema::kb_links::dsl::path.eq(path)),
                )
                .set(db_schema::kb_links::dsl::reviewed.eq(true))
                .execute(conn)
            })
            .await?;
        }
    }

    if metadata_changed {
        write_croissant_snapshot(&name, &kb.croissant_metadata);
    }

    Ok(Redirect::to(uri!(update_view(name = name, page = page))))
}

#[post("/validate/<name>")]
async fn validate(db: Db, name: String) -> Result<Json<serde_json::Value>> {
    let name1 = name.clone();
    let links: Vec<db_model::KnowledgeBaseLink> = db
        .run(move |conn| {
            db_schema::kb_links::table
                .filter(db_schema::kb_links::kb_name.eq(name1))
                .load(conn)
        })
        .await?;

    // Build path → link lookup and collect known distribution @id values
    let link_map: HashMap<String, &db_model::KnowledgeBaseLink> =
        links.iter().map(|l| (l.path.clone(), l)).collect();

    let dist_id_re    = Regex::new(r"^distribution\[\d+\]\.@id$").unwrap();
    let dist_pfx_re   = Regex::new(r"^(distribution\[\d+\])\.@id$").unwrap();
    let field_col_re  = Regex::new(r"^(recordSet\[\d+\]\.field\[\d+\])\.source\.extract\.column$").unwrap();
    let field_nm_re   = Regex::new(r"^(recordSet\[\d+\]\.field\[\d+\])\.name$").unwrap();
    let field_src_re  = Regex::new(r"^(recordSet\[\d+\]\.field\[\d+\])\.source\.file(?:Object|Set)\.@id$").unwrap();

    let dist_ids: HashSet<String> = links
        .iter()
        .filter(|l| dist_id_re.is_match(&l.path))
        .map(|l| l.value.clone())
        .collect();

    let mut auto_paths: Vec<String> = Vec::new();

    // Vocabulary and structural checks (no network)
    for link in &links {
        let path_last = link.path.split('.').last().unwrap_or(&link.path);

        match path_last {
            "@type" => {
                if VALID_TYPES.contains(&link.value.as_str()) {
                    auto_paths.push(link.path.clone());
                }
            }
            "dataType" => {
                if VALID_DATA_TYPES.contains(&link.value.as_str()) {
                    auto_paths.push(link.path.clone());
                }
            }
            "@id" => {
                if link.path.contains("source.fileObject.@id") {
                    // Cross-reference: must point to a known distribution
                    if dist_ids.contains(&link.value) {
                        auto_paths.push(link.path.clone());
                    }
                } else if !link.value.is_empty() {
                    auto_paths.push(link.path.clone());
                }
            }
            _ => {
                if link.path == "conformsTo" && link.value.starts_with(CROISSANT_CONFORMS_TO) {
                    auto_paths.push(link.path.clone());
                }
            }
        }
    }

    // URL checks — concurrent HEAD requests for every distribution contentUrl
    let dist_url_re = Regex::new(r"^(distribution\[\d+\])\.contentUrl$").unwrap();
    let url_tasks: Vec<(String, String, String)> = links
        .iter()
        .filter_map(|l| {
            dist_url_re.captures(&l.path).and_then(|c| c.get(1)).map(|m| {
                (l.path.clone(), m.as_str().to_string(), l.value.clone())
            })
        })
        .collect();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("robo-croissant-dashboard/0.1")
        .build()
        .map_err(|e| Debug(diesel::result::Error::DeserializationError(Box::new(e))))?;

    let head_futures: Vec<_> = url_tasks
        .into_iter()
        .map(|(path, prefix, url)| {
            let client = client.clone();
            async move {
                let resp = client.head(&url).send().await;
                (path, prefix, url, resp)
            }
        })
        .collect();

    let head_results = futures::future::join_all(head_futures).await;

    let mut failed_urls: Vec<String> = Vec::new();

    for (path, prefix, url, resp_result) in head_results {
        match resp_result {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                auto_paths.push(path);

                // name: compare URL filename to distribution[n].name value
                let url_filename = url.split('/').last().unwrap_or("").split('?').next().unwrap_or("");
                let name_path = format!("{}.name", prefix);
                if let Some(lnk) = link_map.get(&name_path) {
                    if !url_filename.is_empty() && lnk.value == url_filename {
                        auto_paths.push(name_path);
                    }
                }

                // encodingFormat: compare to Content-Type base type
                let fmt_path = format!("{}.encodingFormat", prefix);
                if let Some(lnk) = link_map.get(&fmt_path) {
                    let content_type = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !content_type.is_empty() && lnk.value == content_type {
                        auto_paths.push(fmt_path);
                    }
                }

                // contentSize: compare to Content-Length
                let size_path = format!("{}.contentSize", prefix);
                if let Some(lnk) = link_map.get(&size_path) {
                    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
                        if let Ok(cl_str) = cl.to_str() {
                            if lnk.value == cl_str {
                                auto_paths.push(size_path);
                            }
                        }
                    }
                }
            }
            _ => {
                failed_urls.push(url);
            }
        }
    }

    // ── Field column checks — verify name/extract.column against source file headers ──
    // Build dist @id → (contentUrl, encodingFormat) for text-delimited files only
    let mut dist_content_map: HashMap<String, (String, String)> = HashMap::new();
    for link in &links {
        if let Some(caps) = dist_pfx_re.captures(&link.path) {
            let pfx = caps.get(1).unwrap().as_str();
            let url = link_map.get(&format!("{}.contentUrl", pfx)).map(|l| l.value.as_str()).unwrap_or("");
            let fmt = link_map.get(&format!("{}.encodingFormat", pfx)).map(|l| l.value.as_str()).unwrap_or("");
            let url_lc = url.to_lowercase();
            let is_text = (url_lc.ends_with(".csv") || url_lc.ends_with(".tsv")
                           || fmt.contains("csv") || fmt.contains("tsv") || fmt.contains("tab"))
                          && !url_lc.ends_with(".gz") && !url_lc.ends_with(".zip")
                          && url.starts_with("http");
            if is_text {
                dist_content_map.insert(link.value.clone(), (url.to_string(), fmt.to_string()));
            }
        }
    }

    // Build field prefix → (column_value, name_value, dist_id_value)
    let mut field_meta: HashMap<String, (Option<String>, Option<String>, Option<String>)> = HashMap::new();
    for link in &links {
        if let Some(caps) = field_col_re.captures(&link.path) {
            let pfx = caps.get(1).unwrap().as_str().to_string();
            field_meta.entry(pfx).or_default().0 = Some(link.value.clone());
        } else if let Some(caps) = field_nm_re.captures(&link.path) {
            let pfx = caps.get(1).unwrap().as_str().to_string();
            field_meta.entry(pfx).or_default().1 = Some(link.value.clone());
        } else if let Some(caps) = field_src_re.captures(&link.path) {
            let pfx = caps.get(1).unwrap().as_str().to_string();
            let entry = field_meta.entry(pfx).or_default();
            if entry.2.is_none() { entry.2 = Some(link.value.clone()); }
        }
    }

    // Collect unique file URLs to fetch headers for
    let mut seen_file_urls: HashSet<String> = HashSet::new();
    let mut file_url_tasks: Vec<(String, String)> = Vec::new();
    for (_, _, dist_id) in field_meta.values() {
        if let Some(id) = dist_id {
            if let Some((url, fmt)) = dist_content_map.get(id) {
                if seen_file_urls.insert(url.clone()) {
                    file_url_tasks.push((url.clone(), fmt.clone()));
                }
            }
        }
    }

    // GET the first 8 KB of each file to read the header row
    let header_futures: Vec<_> = file_url_tasks.into_iter().map(|(url, fmt)| {
        let client = client.clone();
        async move {
            let cols: Option<Vec<String>> = async {
                let resp = client.get(&url)
                    .header("Range", "bytes=0-8191")
                    .send().await.ok()?;
                if !resp.status().is_success() && resp.status().as_u16() != 206 { return None; }
                let bytes = resp.bytes().await.ok()?;
                let text = std::str::from_utf8(&bytes).ok()?;
                let first_line = text.lines().next()?;
                let is_tsv = fmt.contains("tab") || fmt.contains("tsv") || first_line.contains('\t');
                let delim = if is_tsv { '\t' } else { ',' };
                Some(first_line.split(delim).map(|c| c.trim().trim_matches('"').to_string()).collect())
            }.await;
            (url, cols)
        }
    }).collect();

    let file_headers: HashMap<String, Option<Vec<String>>> =
        futures::future::join_all(header_futures).await.into_iter().collect();

    // Approve field names and extract.column values that appear in the file header
    for (prefix, (column, field_name, dist_id)) in &field_meta {
        let Some(id) = dist_id else { continue };
        let Some((url, _)) = dist_content_map.get(id) else { continue };
        let Some(Some(cols)) = file_headers.get(url) else { continue };

        if let Some(col) = column {
            if cols.contains(col) {
                auto_paths.push(format!("{}.source.extract.column", prefix));
            }
        }
        if let Some(nm) = field_name {
            if cols.contains(nm) {
                auto_paths.push(format!("{}.name", prefix));
            }
        }
    }

    // Deduplicate and batch-update in chunks of 500 (SQLite variable limit)
    auto_paths.sort();
    auto_paths.dedup();
    let auto_count = auto_paths.len();

    for chunk in auto_paths.chunks(500) {
        let kb_name_clone = name.clone();
        let chunk_vec: Vec<String> = chunk.to_vec();
        db.run(move |conn| {
            diesel::update(
                db_schema::kb_links::table
                    .filter(db_schema::kb_links::dsl::kb_name.eq(kb_name_clone))
                    .filter(db_schema::kb_links::dsl::path.eq_any(chunk_vec)),
            )
            .set(db_schema::kb_links::dsl::auto_reviewed.eq(true))
            .execute(conn)
        })
        .await?;
    }

    Ok(Json(serde_json::json!({
        "auto_reviewed": auto_count,
        "failed_urls": failed_urls,
    })))
}

#[get("/issues")]
async fn issues_view(db: Db) -> Result<Template> {
    let issues: Vec<db_model::ValidationIssue> = db
        .run(|conn| {
            db_schema::validation_issues::table
                .order_by((
                    db_schema::validation_issues::kb_name.asc(),
                    db_schema::validation_issues::issue_type.asc(),
                    db_schema::validation_issues::path.asc(),
                ))
                .load(conn)
        })
        .await
        .unwrap_or_default();

    let total = issues.len();

    // Build summary by issue type
    let mut type_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for iss in &issues {
        *type_counts.entry(iss.issue_type.clone()).or_default() += 1;
    }
    let by_type: Vec<serde_json::Value> = type_counts
        .iter()
        .map(|(t, count)| {
            let (label, severity) = issue_type_meta(t);
            serde_json::json!({ "issue_type": t, "count": count, "label": label, "severity": severity })
        })
        .collect();

    // Group issues by KB for collapsible sections
    let mut kb_groups: Vec<serde_json::Value> = Vec::new();
    let mut cur_kb: Option<String> = None;
    let mut cur_issues: Vec<serde_json::Value> = Vec::new();

    for iss in &issues {
        if cur_kb.as_deref() != Some(&iss.kb_name) {
            if let Some(kb) = cur_kb.take() {
                let count = cur_issues.len();
                kb_groups.push(serde_json::json!({ "kb_name": kb, "count": count, "issues": cur_issues }));
                cur_issues = Vec::new();
            }
            cur_kb = Some(iss.kb_name.clone());
        }
        let (label, severity) = issue_type_meta(&iss.issue_type);
        cur_issues.push(serde_json::json!({
            "issue_type": iss.issue_type,
            "issue_label": label,
            "severity": severity,
            "path": iss.path,
            "value": iss.value,
            "detail": iss.detail,
        }));
    }
    if let Some(kb) = cur_kb {
        let count = cur_issues.len();
        kb_groups.push(serde_json::json!({ "kb_name": kb, "count": count, "issues": cur_issues }));
    }

    Ok(Template::render(
        "issues",
        context! {
            title: "Validation Issues",
            total: total,
            by_type: by_type,
            kb_groups: kb_groups,
        },
    ))
}

fn issue_type_meta(issue_type: &str) -> (&'static str, &'static str) {
    match issue_type {
        "repeated_content_url"        => ("Repeated contentUrl",              "error"),
        "duplicate_distribution_id"   => ("Duplicate distribution @id",       "error"),
        "broken_source_reference"     => ("Broken source reference",          "error"),
        "missing_required_field"      => ("Missing required field",           "error"),
        "missing_field_source"        => ("Field missing source",             "error"),
        "relative_content_url"        => ("Relative contentUrl",              "warning"),
        "wrong_distribution_type"     => ("Wrong distribution @type",         "warning"),
        "format_extension_mismatch"   => ("Format/extension mismatch",        "warning"),
        "placeholder_value"           => ("Placeholder value",                "warning"),
        "url_error_response"          => ("URL error response",               "warning"),
        "placeholder_field_name"      => ("Placeholder field name",           "warning"),
        "inline_data_rows"            => ("Inline data rows (use examples)",  "warning"),
        "zip_without_inner_file"      => ("Archive missing inner file",       "warning"),
        "tabular_file_missing_recordset" => ("Tabular file missing RecordSet", "warning"),
        "provenance_noncompliant"     => ("Non-compliant dct:provenance",     "warning"),
        "license_is_terms_page"       => ("License URL is a terms page",      "warning"),
        "invalid_checksum_format"     => ("Invalid checksum format",          "warning"),
        "url_unreachable"             => ("URL unreachable",                  "info"),
        _                             => ("Unknown issue",                    "info"),
    }
}

// ── Fix suggestion helpers ────────────────────────────────────────────────────

fn parse_bracket_index(path: &str) -> Option<usize> {
    let s = path.find('[')? + 1;
    let e = path.find(']')?;
    path[s..e].parse().ok()
}

fn infer_inner_filename(url: &str) -> String {
    let base = url.split('/').last().unwrap_or(url);
    let base = base.strip_suffix(".zip")
        .or_else(|| base.strip_suffix(".tar.gz"))
        .or_else(|| base.strip_suffix(".tgz"))
        .or_else(|| base.strip_suffix(".gz"))
        .unwrap_or(base);
    for ext in &["_tsv", "_csv", "_json", "_jsonl", "_ndjson"] {
        if let Some(s) = base.strip_suffix(ext) {
            return format!("{}.{}", s, &ext[1..]);
        }
    }
    for ext in &[".tsv", ".csv", ".json", ".jsonl"] {
        if base.ends_with(ext) { return base.to_string(); }
    }
    format!("{}.tsv", base)
}

fn inner_ext_to_mime(filename: &str) -> &'static str {
    if filename.ends_with(".tsv") { "text/tab-separated-values" }
    else if filename.ends_with(".csv") { "text/csv" }
    else if filename.ends_with(".json") { "application/json" }
    else if filename.ends_with(".jsonl") || filename.ends_with(".ndjson") { "application/jsonlines" }
    else { "application/octet-stream" }
}

fn slug_id(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

fn generate_fix_suggestions(metadata: &JsonValue, issues: &[IssueRow]) -> Vec<JsonValue> {
    let mut suggestions: Vec<JsonValue> = Vec::new();
    let empty_arr = vec![];
    let dists = metadata["distribution"].as_array().unwrap_or(&empty_arr);

    // Collect issue_types present to detect dependencies
    let has_zip_issues = issues.iter().any(|i| i.issue_type == "zip_without_inner_file");

    // ── zip_without_inner_file — shown first (must resolve before field sources) ──
    for iss in issues.iter().filter(|i| i.issue_type == "zip_without_inner_file") {
        let Some(dist_idx) = parse_bracket_index(&iss.path) else { continue };
        let zip_id = dists.get(dist_idx)
            .and_then(|d| d["@id"].as_str())
            .unwrap_or("?")
            .to_string();
        let inner_fname = infer_inner_filename(&iss.value);
        let inner_mime  = inner_ext_to_mime(&inner_fname);
        let inner_id    = format!("file_{}_inner", slug_id(&zip_id));
        suggestions.push(serde_json::json!({
            "id":          format!("zip_inner:{}", zip_id),
            "issue_id":    iss.id,
            "issue_type":  "zip_without_inner_file",
            "severity":    "warning",
            "title":       format!("Add inner FileObject for {}", zip_id),
            "description": "Defines a cr:FileObject for the file inside this archive so that recordSet fields can declare their source.",
            "fix_type":    "add_inner_file",
            "params": {
                "dist_index":      dist_idx,
                "zip_id":          zip_id.clone(),
                "inner_id":        inner_id.clone(),
                "inner_filename":  inner_fname.clone(),
                "encoding_format": inner_mime,
            },
            "editable_fields": [
                { "key": "inner_filename",  "label": "Inner filename",  "hint": "Filename inside the archive" },
                { "key": "inner_id",        "label": "FileObject @id",  "hint": "Unique identifier — must be referenced in field sources" },
                { "key": "encoding_format", "label": "MIME type",       "hint": "e.g. text/tab-separated-values" },
            ],
            "preview": serde_json::json!({
                "@id":           inner_id,
                "@type":         "cr:FileObject",
                "name":          inner_fname.clone(),
                "contentUrl":    inner_fname,
                "encodingFormat": inner_mime,
                "containedIn":   { "@id": zip_id },
            }),
        }));
    }

    // ── missing_field_source — one per recordSet ──────────────────────────────
    let record_sets = metadata["recordSet"].as_array().unwrap_or(&empty_arr);
    for iss in issues.iter().filter(|i| i.issue_type == "missing_field_source") {
        let Some(rs_idx) = parse_bracket_index(&iss.path) else { continue };
        let rs     = record_sets.get(rs_idx);
        let rs_id  = rs.and_then(|r| r["@id"].as_str()).unwrap_or("?").to_string();
        let n_miss = rs.and_then(|r| r["field"].as_array()).map(|f|
            f.iter().filter(|fld| fld.get("source").is_none()).count()
        ).unwrap_or(0);
        let dep_note = if has_zip_issues {
            " Apply 'Add inner FileObject' fixes first, then paste the resulting @id here."
        } else { "" };
        suggestions.push(serde_json::json!({
            "id":          format!("missing_source:rs:{}", rs_idx),
            "issue_id":    iss.id,
            "issue_type":  "missing_field_source",
            "severity":    "error",
            "title":       format!("Add source to {} fields in '{}'", n_miss, rs_id),
            "description": format!(
                "Fields need source.fileObject.@id and source.extract.column so consumers know which file and column each field comes from. The column name is taken from each field's name.{}",
                dep_note
            ),
            "fix_type":    "add_field_sources",
            "params": {
                "rs_index":      rs_idx,
                "inner_file_id": "",
            },
            "editable_fields": [
                { "key": "inner_file_id", "label": "Inner file @id",
                  "hint": "The @id of the cr:FileObject inside the archive (e.g. file_bindingdb_all_202606_tsv_zip_inner)" },
            ],
            "preview": serde_json::json!({
                "source": {
                    "fileObject": { "@id": "(enter inner file @id above)" },
                    "extract":    { "column": "(taken from field.name per field)" },
                }
            }),
        }));
    }

    // ── inline_data_rows — rename data→examples, grouped as one suggestion ───
    let inline_issues: Vec<_> = issues.iter()
        .filter(|i| i.issue_type == "inline_data_rows")
        .collect();
    if !inline_issues.is_empty() {
        let rs_indices: Vec<_> = inline_issues.iter()
            .filter_map(|i| parse_bracket_index(&i.path))
            .collect();
        let previews: Vec<_> = inline_issues.iter().map(|i| serde_json::json!({
            "path":  i.path.clone(),
            "value": i.value.clone(),
        })).collect();
        suggestions.push(serde_json::json!({
            "id":          "inline_data_rows:all",
            "issue_type":  "inline_data_rows",
            "severity":    "warning",
            "title":       format!("Rename `data` → `examples` in {} recordSet(s)", inline_issues.len()),
            "description": "These recordSets use `data` for sample rows, but the dataset has external files. In Croissant 1.1, `data` means the recordSet is fully self-contained — illustrative rows belong in `examples`.",
            "fix_type":    "rename_data_to_examples",
            "params":      { "rs_indices": rs_indices },
            "editable_fields": [],
            "preview":     previews,
        }));
    }

    suggestions
}

#[get("/runs")]
async fn runs_view(db: Db) -> Result<Template> {
    let runs: Vec<db_model::Run> = db
        .run(|conn| {
            db_schema::runs::table
                .order_by(db_schema::runs::run_date.asc())
                .load(conn)
        })
        .await
        .unwrap_or_default();

    #[derive(diesel::QueryableByName)]
    struct RunStats {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        run_id: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        kb_count: i32,
        #[diesel(sql_type = diesel::sql_types::Integer)]
        field_count: i32,
    }

    let stats: Vec<RunStats> = db
        .run(|conn| {
            diesel::sql_query(
                "SELECT run_id, \
                        COUNT(DISTINCT kb_name) AS kb_count, \
                        COUNT(*) AS field_count \
                 FROM run_links GROUP BY run_id",
            )
            .load(conn)
        })
        .await
        .unwrap_or_default();

    let stats_map: HashMap<i32, (i32, i32)> = stats
        .into_iter()
        .map(|s| (s.run_id, (s.kb_count, s.field_count)))
        .collect();

    let run_rows: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| {
            let (kb_count, field_count) = stats_map.get(&r.id).copied().unwrap_or((0, 0));
            serde_json::json!({
                "id": r.id,
                "label": r.label,
                "run_date": r.run_date,
                "model": r.model,
                "file_hash": &r.file_hash[..12],
                "imported_at": r.imported_at,
                "kb_count": kb_count,
                "field_count": field_count,
            })
        })
        .collect();

    Ok(Template::render("runs", context! {
        title: "Runs",
        runs: run_rows,
    }))
}

#[derive(FromForm)]
struct RunUpdateForm {
    label: String,
    model: String,
}

#[post("/runs/<id>", data = "<form>")]
async fn runs_update(db: Db, id: i32, form: Form<RunUpdateForm>) -> Result<Redirect> {
    let label = form.label.trim().to_string();
    let model = form.model.trim().to_string();
    let model_opt: Option<String> = if model.is_empty() { None } else { Some(model) };

    db.run(move |conn| {
        diesel::sql_query(
            "UPDATE runs SET label = ?, model = ? WHERE id = ?",
        )
        .bind::<diesel::sql_types::Text, _>(&label)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(&model_opt)
        .bind::<diesel::sql_types::Integer, _>(id)
        .execute(conn)
    })
    .await?;

    Ok(Redirect::to(uri!(runs_view)))
}

const COMPARE_PAGE_SIZE: i64 = 200;

#[get("/compare/<kb_name>?<page>")]
async fn compare_view(db: Db, kb_name: String, page: Option<i64>) -> Result<Template> {
    let current_page = page.unwrap_or(0).max(0);
    let offset = current_page * COMPARE_PAGE_SIZE;

    let runs: Vec<db_model::Run> = db
        .run(|conn| {
            diesel::sql_query(
                "SELECT * FROM runs \
                 ORDER BY COALESCE(run_date, imported_at) DESC",
            )
            .load::<db_model::Run>(conn)
        })
        .await
        .unwrap_or_default();

    if runs.is_empty() {
        return Ok(Template::render("compare", context! {
            title: format!("Compare — {}", kb_name),
            kb_name: kb_name,
            runs: Vec::<serde_json::Value>::new(),
            rows: Vec::<serde_json::Value>::new(),
            total_changed: 0i64,
            current_page: current_page,
            total_pages: 0i64,
        }));
    }

    #[derive(diesel::QueryableByName)]
    struct PathRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        path: String,
    }

    let kb1 = kb_name.clone();
    let total_changed: i64 = db
        .run(move |conn| {
            diesel::sql_query(
                "SELECT COUNT(*) AS c FROM (\
                    SELECT path FROM run_links WHERE kb_name = ? \
                    GROUP BY path \
                    HAVING COUNT(DISTINCT value) > 1 \
                ) sub",
            )
            .bind::<diesel::sql_types::Text, _>(&kb1)
            .load::<RawCount>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|r| r.c)
        .unwrap_or(0);

    let kb2 = kb_name.clone();
    let changed_paths: Vec<String> = db
        .run(move |conn| {
            diesel::sql_query(
                "SELECT path FROM run_links WHERE kb_name = ? \
                 GROUP BY path \
                 HAVING COUNT(DISTINCT value) > 1 \
                 ORDER BY path \
                 LIMIT ? OFFSET ?",
            )
            .bind::<diesel::sql_types::Text, _>(&kb2)
            .bind::<diesel::sql_types::BigInt, _>(COMPARE_PAGE_SIZE)
            .bind::<diesel::sql_types::BigInt, _>(offset)
            .load::<PathRow>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.path)
        .collect();

    // Load all (run_id, path, value) for these paths in this KB
    #[derive(diesel::QueryableByName)]
    struct RunValue {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        run_id: i32,
        #[diesel(sql_type = diesel::sql_types::Text)]
        path: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    let kb3 = kb_name.clone();
    let paths_clone = changed_paths.clone();
    let run_values: Vec<RunValue> = db
        .run(move |conn| {
            if paths_clone.is_empty() {
                return Ok::<Vec<RunValue>, diesel::result::Error>(Vec::new());
            }
            let rows: Vec<(i32, String, String)> = db_schema::run_links::table
                .filter(db_schema::run_links::kb_name.eq(&kb3))
                .filter(db_schema::run_links::path.eq_any(&paths_clone))
                .select((
                    db_schema::run_links::run_id,
                    db_schema::run_links::path,
                    db_schema::run_links::value,
                ))
                .order_by((db_schema::run_links::path.asc(), db_schema::run_links::run_id.asc()))
                .load::<(i32, String, String)>(conn)?;
            Ok(rows.into_iter().map(|(run_id, path, value)| RunValue { run_id, path, value }).collect())
        })
        .await
        .unwrap_or_default();

    // Pivot: path → run_id → value
    let run_ids: Vec<i32> = runs.iter().map(|r| r.id).collect();
    let mut pivot: HashMap<String, HashMap<i32, String>> = HashMap::new();
    for rv in run_values {
        pivot.entry(rv.path).or_default().insert(rv.run_id, rv.value);
    }

    let rows: Vec<serde_json::Value> = changed_paths
        .iter()
        .map(|path| {
            let path_map = pivot.get(path).cloned().unwrap_or_default();
            let values: Vec<serde_json::Value> = run_ids
                .iter()
                .map(|rid| {
                    match path_map.get(rid) {
                        Some(v) => serde_json::json!({ "present": true, "value": v }),
                        None => serde_json::json!({ "present": false, "value": "" }),
                    }
                })
                .collect();
            // Flag row as changed if values differ among present entries
            let present_vals: Vec<&str> = run_ids
                .iter()
                .filter_map(|rid| path_map.get(rid).map(|s| s.as_str()))
                .collect();
            let all_same = present_vals.windows(2).all(|w| w[0] == w[1]);
            serde_json::json!({
                "path": path,
                "anchor": path_to_anchor(path),
                "values": values,
                "all_same": all_same,
            })
        })
        .collect();

    let run_cols: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| serde_json::json!({
            "id": r.id,
            "label": r.label,
            "run_date": r.run_date,
            "model": r.model,
        }))
        .collect();

    let total_pages = (total_changed + COMPARE_PAGE_SIZE - 1) / COMPARE_PAGE_SIZE;

    Ok(Template::render("compare", context! {
        title: format!("Compare — {}", kb_name),
        kb_name: kb_name,
        runs: run_cols,
        rows: rows,
        total_changed: total_changed,
        current_page: current_page,
        total_pages: total_pages,
    }))
}

// ── Compare seek — redirect to correct page + anchor for a specific path ────
#[get("/compare/<kb_name>/seek?<path>")]
async fn compare_seek(db: Db, kb_name: String, path: String) -> Redirect {
    #[derive(diesel::QueryableByName)]
    struct PathRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        path: String,
    }
    let kb = kb_name.clone();
    let all_paths: Vec<String> = db
        .run(move |conn| {
            diesel::sql_query(
                "SELECT path FROM run_links WHERE kb_name = ? \
                 GROUP BY path HAVING COUNT(DISTINCT value) > 1 ORDER BY path",
            )
            .bind::<diesel::sql_types::Text, _>(&kb)
            .load::<PathRow>(conn)
        })
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.path)
        .collect();

    let mut sorted = all_paths;
    sorted.sort_by_cached_key(|p| natural_sort_key(p));

    let page = sorted.iter().position(|p| p == &path)
        .map(|idx| idx as i64 / COMPARE_PAGE_SIZE)
        .unwrap_or(0);

    let anchor = path_to_anchor(&path);
    let encoded_name = utf8_percent_encode(&kb_name, NON_ALPHANUMERIC).to_string();
    let target = format!("/compare/{}?page={}#field-{}", encoded_name, page, anchor);
    Redirect::to(target)
}

// ── Fix suggestion routes ─────────────────────────────────────────────────────

#[get("/suggest_fixes/<name>")]
async fn suggest_fixes(db: Db, name: String) -> Result<Json<Vec<serde_json::Value>>> {
    let name1 = name.clone();
    let ks: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table
            .filter(db_schema::knowledge_bases::name.eq(name1))
            .first(conn))
        .await?;

    let name2 = name.clone();
    let issues: Vec<IssueRow> = db
        .run(move |conn| {
            diesel::sql_query(
                "SELECT id, issue_type, path, value, detail \
                 FROM validation_issues \
                 WHERE kb_name = ? \
                   AND issue_type IN ('zip_without_inner_file','missing_field_source','inline_data_rows') \
                 ORDER BY \
                   CASE issue_type \
                     WHEN 'zip_without_inner_file' THEN 1 \
                     WHEN 'missing_field_source'   THEN 2 \
                     WHEN 'inline_data_rows'        THEN 3 \
                     ELSE 4 END, path",
            )
            .bind::<diesel::sql_types::Text, _>(&name2)
            .load::<IssueRow>(conn)
        })
        .await
        .unwrap_or_default();

    Ok(Json(generate_fix_suggestions(&ks.croissant_metadata, &issues)))
}

#[post("/apply_fix/<name>", format = "json", data = "<body>")]
async fn apply_fix(db: Db, name: String, body: Json<serde_json::Value>) -> Result<Json<serde_json::Value>> {
    let fix_type  = body["fix_type"].as_str().unwrap_or("").to_string();
    let params    = body["params"].clone();

    let name1 = name.clone();
    let mut kb: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table
            .filter(db_schema::knowledge_bases::name.eq(name1))
            .first(conn))
        .await?;

    let mut metadata = kb.croissant_metadata.clone();
    let mut issue_type_to_delete = String::new();
    let mut path_to_delete       = String::new();

    match fix_type.as_str() {
        // Rename recordSet[i].data → recordSet[i].examples
        "rename_data_to_examples" => {
            let indices: Vec<usize> = params["rs_indices"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_u64().map(|i| i as usize))
                .collect();
            if let Some(rs_arr) = metadata["recordSet"].as_array_mut() {
                for idx in &indices {
                    if let Some(rs) = rs_arr.get_mut(*idx) {
                        if let Some(obj) = rs.as_object_mut() {
                            if let Some(data) = obj.remove("data") {
                                obj.insert("examples".to_string(), data);
                            }
                        }
                    }
                }
            }
            issue_type_to_delete = "inline_data_rows".to_string();
        }

        // Insert a new FileObject for the file inside a compressed archive
        "add_inner_file" => {
            let inner_id       = params["inner_id"].as_str().unwrap_or("").to_string();
            let inner_filename = params["inner_filename"].as_str().unwrap_or("").to_string();
            let encoding       = params["encoding_format"].as_str().unwrap_or("").to_string();
            let zip_id         = params["zip_id"].as_str().unwrap_or("").to_string();

            if inner_id.is_empty() || inner_filename.is_empty() {
                return Ok(Json(serde_json::json!({ "error": "inner_id and inner_filename are required" })));
            }

            let new_file = serde_json::json!({
                "@id":           inner_id,
                "@type":         "cr:FileObject",
                "name":          inner_filename,
                "contentUrl":    inner_filename,
                "encodingFormat": encoding,
                "containedIn":   { "@id": zip_id },
            });

            if let Some(arr) = metadata["distribution"].as_array_mut() {
                arr.push(new_file);
            }
            issue_type_to_delete = "zip_without_inner_file".to_string();
            path_to_delete       = params["dist_index"]
                .as_u64()
                .map(|i| format!("distribution[{}].contentUrl", i))
                .unwrap_or_default();
        }

        // Add source to every field missing one in a recordSet
        "add_field_sources" => {
            let rs_idx       = params["rs_index"].as_u64().unwrap_or(0) as usize;
            let inner_file_id = params["inner_file_id"].as_str().unwrap_or("").to_string();

            if inner_file_id.is_empty() {
                return Ok(Json(serde_json::json!({
                    "error": "inner_file_id is required — enter the @id of the cr:FileObject inside the archive"
                })));
            }

            if let Some(rs_arr) = metadata["recordSet"].as_array_mut() {
                if let Some(rs) = rs_arr.get_mut(rs_idx) {
                    if let Some(fields) = rs["field"].as_array_mut() {
                        for field in fields.iter_mut() {
                            if field.get("source").is_none() {
                                let col = field["name"].as_str().unwrap_or("").to_string();
                                field["source"] = serde_json::json!({
                                    "fileObject": { "@id": inner_file_id },
                                    "extract":    { "column": col },
                                });
                            }
                        }
                    }
                }
            }
            issue_type_to_delete = "missing_field_source".to_string();
            path_to_delete       = format!("recordSet[{}]", rs_idx);
        }

        _ => {
            return Ok(Json(serde_json::json!({ "error": format!("Unknown fix_type: {}", fix_type) })));
        }
    }

    // Save updated metadata
    kb.croissant_metadata = metadata;
    let name2 = name.clone();
    let update_kb = diesel::update(db_schema::knowledge_bases::table
        .filter(db_schema::knowledge_bases::dsl::name.eq(name2)))
        .set(kb.clone());
    db.run(move |conn| update_kb.execute(conn)).await?;

    // Remove the resolved validation issue(s) so they don't re-appear in suggestions
    if !issue_type_to_delete.is_empty() {
        let name3     = name.clone();
        let itype     = issue_type_to_delete.clone();
        let ipath     = path_to_delete.clone();
        db.run(move |conn| {
            if ipath.is_empty() {
                diesel::sql_query(
                    "DELETE FROM validation_issues WHERE kb_name = ? AND issue_type = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&name3)
                .bind::<diesel::sql_types::Text, _>(&itype)
                .execute(conn)
            } else {
                diesel::sql_query(
                    "DELETE FROM validation_issues WHERE kb_name = ? AND issue_type = ? AND path = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&name3)
                .bind::<diesel::sql_types::Text, _>(&itype)
                .bind::<diesel::sql_types::Text, _>(&ipath)
                .execute(conn)
            }
        })
        .await
        .ok();
    }

    Ok(Json(serde_json::json!({ "ok": true, "fix_type": fix_type })))
}

// ── Reconcile routes ───────────────────────────────────────────────────────────

const RECONCILE_SKIP_KEYS: &[&str] = &["@context", "distribution", "recordSet"];

fn json_display(v: Option<&JsonValue>) -> String {
    match v {
        None | Some(JsonValue::Null) => String::new(),
        Some(JsonValue::String(s)) => s.clone(),
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn json_raw(v: Option<&JsonValue>) -> String {
    match v {
        None => "null".to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct ReconcileRow {
    key: String,
    current_present: bool,
    current_display: String,
    current_json: String,
    staged_present: bool,
    staged_display: String,
    staged_json: String,
    default_choice: String,
    is_provenance: bool,
}

#[get("/reconcile/<name>")]
async fn reconcile_view(db: Db, name: String) -> Result<Template> {
    let name1 = name.clone();
    let current: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table
            .filter(db_schema::knowledge_bases::name.eq(name1))
            .first(conn))
        .await?;

    let name2 = name.clone();
    let staged: Option<db_model::StagedKbVersion> = db
        .run(move |conn| db_schema::staged_kb_versions::table
            .filter(db_schema::staged_kb_versions::kb_name.eq(name2))
            .first(conn)
            .optional())
        .await?;

    let staged = match staged {
        Some(s) => s,
        None => {
            return Ok(Template::render(
                "reconcile",
                context! {
                    title: format!("Reconcile: {}", name),
                    kb_name: name,
                    no_staged_version: true,
                },
            ));
        }
    };

    let name3 = name.clone();
    let staged_issues: Vec<db_model::StagedKbIssue> = db
        .run(move |conn| db_schema::staged_kb_issues::table
            .filter(db_schema::staged_kb_issues::kb_name.eq(name3))
            .load(conn))
        .await
        .unwrap_or_default();

    let issue_banner: Vec<JsonValue> = staged_issues
        .iter()
        .map(|i| {
            let (label, severity) = issue_type_meta(&i.issue_type);
            serde_json::json!({
                "label": label,
                "severity": severity,
                "path": i.path,
                "value": i.value,
                "detail": i.detail,
            })
        })
        .collect();

    let current_obj = current.croissant_metadata.as_object().cloned().unwrap_or_default();
    let staged_obj = staged.croissant_metadata.as_object().cloned().unwrap_or_default();

    let mut keys: Vec<String> = current_obj.keys().chain(staged_obj.keys()).cloned().collect();
    keys.sort();
    keys.dedup();

    let mut rows: Vec<ReconcileRow> = Vec::new();
    for key in keys {
        if RECONCILE_SKIP_KEYS.contains(&key.as_str()) {
            continue;
        }
        let cur_val = current_obj.get(&key);
        let staged_val = staged_obj.get(&key);
        if cur_val == staged_val {
            continue; // identical — nothing to reconcile
        }
        let current_present = cur_val.is_some();
        let staged_present = staged_val.is_some();
        let default_choice = if !current_present && staged_present {
            "staged"
        } else {
            "current" // covers: missing from staged, and genuine conflicts (safe default)
        };
        rows.push(ReconcileRow {
            key: key.clone(),
            current_present,
            current_display: json_display(cur_val),
            current_json: json_raw(cur_val),
            staged_present,
            staged_display: json_display(staged_val),
            staged_json: json_raw(staged_val),
            default_choice: default_choice.to_string(),
            is_provenance: key == "dct:provenance",
        });
    }

    let current_dist_count = current_obj.get("distribution").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let staged_dist_count = staged_obj.get("distribution").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let current_rs_count = current_obj.get("recordSet").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let staged_rs_count = staged_obj.get("recordSet").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let dist_recordset_differs = current_obj.get("distribution") != staged_obj.get("distribution")
        || current_obj.get("recordSet") != staged_obj.get("recordSet");
    let dist_recordset_default = if current_dist_count == 0 && current_rs_count == 0 && (staged_dist_count > 0 || staged_rs_count > 0) {
        "staged"
    } else {
        "current"
    };

    Ok(Template::render(
        "reconcile",
        context! {
            title: format!("Reconcile: {}", name),
            kb_name: name,
            no_staged_version: false,
            source_label: staged.source_label,
            staged_at: staged.staged_at,
            rows: rows,
            issue_banner: issue_banner,
            current_dist_count: current_dist_count,
            staged_dist_count: staged_dist_count,
            current_rs_count: current_rs_count,
            staged_rs_count: staged_rs_count,
            dist_recordset_differs: dist_recordset_differs,
            dist_recordset_default: dist_recordset_default,
        },
    ))
}

#[post("/reconcile/<name>/commit", format = "json", data = "<body>")]
async fn reconcile_commit(db: Db, name: String, body: Json<serde_json::Value>) -> Result<Json<serde_json::Value>> {
    let name1 = name.clone();
    let current: db_model::KnowledgeBase = db
        .run(move |conn| db_schema::knowledge_bases::table
            .filter(db_schema::knowledge_bases::name.eq(name1))
            .first(conn))
        .await?;

    let name2 = name.clone();
    let staged: db_model::StagedKbVersion = db
        .run(move |conn| db_schema::staged_kb_versions::table
            .filter(db_schema::staged_kb_versions::kb_name.eq(name2))
            .first(conn))
        .await?;

    let mut final_doc = current.croissant_metadata.clone();
    let final_obj = match final_doc.as_object_mut() {
        Some(o) => o,
        None => return Ok(Json(serde_json::json!({ "error": "current metadata is not a JSON object" }))),
    };
    let staged_obj = staged.croissant_metadata.as_object().cloned().unwrap_or_default();

    let field_resolutions = body["field_resolutions"].as_object().cloned().unwrap_or_default();
    for (key, resolution) in field_resolutions.iter() {
        let source = resolution["source"].as_str().unwrap_or("current");
        match source {
            "current" => {} // already the base — nothing to do
            "staged" => {
                match staged_obj.get(key) {
                    Some(v) => { final_obj.insert(key.clone(), v.clone()); }
                    None => { final_obj.remove(key); }
                }
            }
            "edit" => {
                final_obj.insert(key.clone(), resolution["value"].clone());
            }
            "restructure" => {
                final_obj.remove(key);
                if let Some(merge_obj) = resolution["value"].as_object() {
                    for (mk, mv) in merge_obj.iter() {
                        final_obj.insert(mk.clone(), mv.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let dist_recordset_choice = body["dist_recordset_choice"].as_str().unwrap_or("current");
    if dist_recordset_choice == "staged" {
        match staged_obj.get("distribution") {
            Some(v) => { final_obj.insert("distribution".to_string(), v.clone()); }
            None => { final_obj.remove("distribution"); }
        }
        match staged_obj.get("recordSet") {
            Some(v) => { final_obj.insert("recordSet".to_string(), v.clone()); }
            None => { final_obj.remove("recordSet"); }
        }
    }

    let name3 = name.clone();
    let final_doc_for_export = final_doc.clone();
    db.run(move |conn| {
        diesel::update(db_schema::knowledge_bases::table.filter(db_schema::knowledge_bases::name.eq(name3)))
            .set(db_schema::knowledge_bases::croissant_metadata.eq(final_doc))
            .execute(conn)
    })
    .await?;
    write_croissant_snapshot(&name, &final_doc_for_export);
    write_final_croissant(&name, &final_doc_for_export);
    let resync_note = resync_kb_links_blocking(&name).await;
    spawn_background_validate(&name);

    Ok(Json(serde_json::json!({
        "ok": true,
        "next_steps": format!(
            "Committed. {} Validation is running in the background — reload in a few seconds to see updated issues.",
            resync_note
        ),
    })))
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .attach(Db::fairing())
        .attach(Template::fairing())
        .attach(AdHoc::try_on_ignite("Create run tables", |rocket| async {
            match Db::get_one(&rocket).await {
                Some(db) => {
                    db.run(|conn| {
                        use diesel::connection::SimpleConnection;
                        conn.batch_execute("
                            CREATE TABLE IF NOT EXISTS runs (
                                id INTEGER PRIMARY KEY AUTOINCREMENT,
                                label TEXT NOT NULL,
                                run_date TEXT,
                                model TEXT,
                                file_hash TEXT UNIQUE NOT NULL,
                                imported_at TEXT NOT NULL
                            );
                            CREATE TABLE IF NOT EXISTS run_links (
                                run_id INTEGER NOT NULL,
                                kb_name TEXT NOT NULL,
                                path TEXT NOT NULL,
                                value TEXT NOT NULL,
                                PRIMARY KEY (run_id, kb_name, path)
                            );
                            CREATE INDEX IF NOT EXISTS idx_run_links_kb_path
                                ON run_links(kb_name, path);
                            CREATE INDEX IF NOT EXISTS idx_kb_links_kb_name
                                ON kb_links(kb_name);
                            CREATE INDEX IF NOT EXISTS idx_validation_issues_kb_name
                                ON validation_issues(kb_name);
                            CREATE TABLE IF NOT EXISTS staged_kb_versions (
                                kb_name            TEXT NOT NULL PRIMARY KEY,
                                source_label       TEXT NOT NULL,
                                croissant_metadata TEXT NOT NULL,
                                staged_at          TEXT NOT NULL
                            );
                            CREATE TABLE IF NOT EXISTS staged_kb_issues (
                                kb_name    TEXT NOT NULL,
                                issue_type TEXT NOT NULL,
                                path       TEXT NOT NULL,
                                value      TEXT NOT NULL,
                                detail     TEXT NOT NULL
                            );
                            CREATE INDEX IF NOT EXISTS idx_staged_kb_issues_kb
                                ON staged_kb_issues(kb_name);
                        ").ok();
                    }).await;
                    Ok(rocket)
                }
                None => Err(rocket),
            }
        }))
        .mount("/", routes![index, knowledge_base, names, update_view, update, update_fields, validate, issues_view, runs_view, runs_update, compare_view, compare_seek, suggest_fixes, apply_fix, reconcile_view, reconcile_commit])
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
