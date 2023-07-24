use std::collections::HashMap;

use cedar_policy::{Authorizer, Request, Context, EntityUid};
use rusqlite::Connection;
// use lazy_static::lazy_static;

pub use crate::entity_sql_fetcher::*;

mod entity_sql_fetcher;

fn main() {
    let conn = Connection::open("cedar-db/example.db").expect("Connection failed");
    let table: EntityFetchTable<EntitySQLFetcher> = EntityFetchTable(HashMap::from([
        ("Users".parse().unwrap(), EntitySQLFetcher::new_default(&conn, "users", &["name", "age"])),
        ("Photos".parse().unwrap(), EntitySQLFetcher::new_default(&conn, "photos", &["title", "location"]))
    ]));

    let auth = Authorizer::new();
    let euid: EntityUid = "Users::\"0\"".parse().unwrap();
    let result = auth.is_authorized_parsed(
        &Request::new(Some(euid.clone()),
            Some("Actions::\"view\"".parse().unwrap()),
            Some("Photos::\"20\"".parse().unwrap()), Context::empty())
        , &"permit(principal, action, resource) when { principal.name == \"Alice\" && resource.title == \"Beach photo\" };".parse().unwrap(),
        &table);
    println!("Result {:?}", result);

    println!("Euid: {}", euid.to_json_implicit());
}
