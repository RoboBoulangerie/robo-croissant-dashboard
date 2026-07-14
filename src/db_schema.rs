use diesel::table;

table! {
    runs (id) {
        id -> diesel::sql_types::Integer,
        label -> diesel::sql_types::Text,
        run_date -> diesel::sql_types::Nullable<diesel::sql_types::Text>,
        model -> diesel::sql_types::Nullable<diesel::sql_types::Text>,
        file_hash -> diesel::sql_types::Text,
        imported_at -> diesel::sql_types::Text,
    }
}

table! {
    run_links (run_id, kb_name, path) {
        run_id -> diesel::sql_types::Integer,
        kb_name -> diesel::sql_types::Text,
        path -> diesel::sql_types::Text,
        value -> diesel::sql_types::Text,
    }
}

table! {
    knowledge_bases (name) {
        name -> diesel::sql_types::Text,
        url -> diesel::sql_types::Nullable<diesel::sql_types::Text>,
        croissant_metadata -> diesel::sql_types::Json,
    }
}

table! {
    kb_links (kb_name, path) {
        kb_name -> diesel::sql_types::Text,
        path -> diesel::sql_types::Text,
        value -> diesel::sql_types::Text,
        url -> diesel::sql_types::Text,
        confidence -> diesel::sql_types::Float,
        reviewed -> diesel::sql_types::Bool,
        auto_reviewed -> diesel::sql_types::Bool,
    }
}

table! {
    validation_issues (id) {
        id -> diesel::sql_types::Integer,
        kb_name -> diesel::sql_types::Text,
        issue_type -> diesel::sql_types::Text,
        path -> diesel::sql_types::Text,
        value -> diesel::sql_types::Text,
        detail -> diesel::sql_types::Text,
        created_at -> diesel::sql_types::Text,
    }
}
