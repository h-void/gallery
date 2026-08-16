use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::{json, Value};

use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
struct CharacterRow {
    id: i64,
    name: String,
    created_at: f64,
}

pub fn characters_response(conn: &Connection, search: Option<&str>) -> Result<Value> {
    Ok(json!({ "characters": list_characters(conn, search)? }))
}

pub fn character_response(conn: &Connection, character_id: i64) -> Result<Value> {
    Ok(json!({ "character": get_character(conn, character_id)? }))
}

fn list_characters(conn: &Connection, search: Option<&str>) -> Result<Vec<CharacterRow>> {
    let mut characters = if let Some(search) = search.filter(|value| !value.is_empty()) {
        let mut stmt = conn.prepare(
            "
            SELECT id, name, created_at
            FROM characters
            WHERE name LIKE ?
            ",
        )?;
        let pattern = format!("%{search}%");
        let rows = stmt
            .query_map([pattern], character_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT id, name, created_at
            FROM characters
            ",
        )?;
        let rows = stmt
            .query_map([], character_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    characters.sort_by(|left, right| natural_compare(&left.name, &right.name));
    Ok(characters)
}

fn get_character(conn: &Connection, character_id: i64) -> Result<Option<CharacterRow>> {
    conn.query_row(
        "SELECT id, name, created_at FROM characters WHERE id = ?",
        [character_id],
        character_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn character_from_row(row: &Row<'_>) -> rusqlite::Result<CharacterRow> {
    Ok(CharacterRow {
        id: row.get("id")?,
        name: row.get("name")?,
        created_at: row.get("created_at")?,
    })
}
