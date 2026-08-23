use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::tags::compare_tag_order;

#[derive(Clone, Serialize, Debug)]
pub(crate) struct ItemDetailTagRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    #[serde(skip_serializing)]
    pub(crate) sort_order: i64,
}

pub(crate) fn list_item_detail_tags(
    conn: &Connection,
    item_id: i64,
) -> Result<Vec<ItemDetailTagRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT t.id, t.name, t.sort_order
        FROM item_tags it
        JOIN tags t ON t.id = it.tag_id
        WHERE it.item_id=?
        ",
    )?;
    let mut tags = stmt
        .query_map([item_id], |row| {
            Ok(ItemDetailTagRow {
                id: row.get("id")?,
                name: row.get("name")?,
                sort_order: row.get("sort_order")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort_by(|left, right| {
        compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name)
    });
    Ok(tags)
}
