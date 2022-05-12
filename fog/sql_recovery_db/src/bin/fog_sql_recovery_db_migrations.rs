// Copyright (c) 2018-2022 The MobileCoin Foundation

//! A helper utility for running migrations on a database configured via
//! DATABASE_URL.

#[macro_use]
extern crate diesel_migrations;

use diesel::{prelude::*, PgConnection};
use diesel_migrations::embed_migrations;
use std::{
    env,
    io::{stdout, Write},
};

embed_migrations!("migrations/");

fn main() {
    let database_url = env::var("DATABASE_URL").expect("Missing DATABASE_URL environment variable");
    println!("Got DATABASE_URL={}", database_url);
    stdout().flush().expect("flush stdout");

    let conn = PgConnection::establish(&database_url).unwrap_or_else(|err| {
        panic!(
            "fog-sql-recovery-db-migrations cannot connect to PG database '{}': {}",
            database_url, err
        )
    });

    embedded_migrations::run(&conn).expect("Failed running migrations");

    println!("Done migrating Fog recovery DB!");
}
