use diesel::table;

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
    }
}
