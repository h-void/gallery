use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct CandidateQuery {
    pub(crate) status: Option<String>,
    pub(crate) hide_grouped: Option<bool>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct GroupQuery {
    pub(crate) status: Option<String>,
    pub(crate) sample_limit: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    pub(crate) status: Option<String>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct OperationHistoryQuery {
    pub(crate) limit: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct TagsQuery {
    pub(crate) artist_id: i64,
}

#[derive(Deserialize)]
pub(crate) struct FoldersQuery {
    pub(crate) artist_id: i64,
}

#[derive(Deserialize)]
pub(crate) struct ItemsQuery {
    pub(crate) artist_id: Option<i64>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    pub(crate) sort: Option<String>,
    pub(crate) media_type: Option<String>,
    pub(crate) tag_id: Option<i64>,
    pub(crate) tags: Option<String>,
    pub(crate) folder: Option<String>,
    pub(crate) date_from: Option<String>,
    pub(crate) date_to: Option<String>,
    pub(crate) image_only: Option<bool>,
    pub(crate) untagged: Option<bool>,
    pub(crate) duplicates_only: Option<bool>,
    /// Search filter, handled natively (raw substring on file_name/folder_name/
    /// file_path + pinyin on item tag names); mirrors `app/api/items.py`.
    pub(crate) search: Option<String>,
    pub(crate) search_tags_only: Option<bool>,
    pub(crate) archive_only: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct TagSearchQuery {
    pub(crate) artist_id: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct CharactersQuery {
    pub(crate) search: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CharacterSummaryQuery {
    pub(crate) artist_id: Option<i64>,
    pub(crate) model_repo_id: Option<String>,
    pub(crate) model_variant: Option<String>,
    pub(crate) model_file: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ArtistReferenceScoreRequest {
    pub(crate) dino_embedding: Vec<f32>,
    pub(crate) wd14_embedding: Vec<f32>,
    pub(crate) dino_weight: Option<f64>,
    pub(crate) wd14_weight: Option<f64>,
    pub(crate) limit: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct ReferenceQuery {
    pub(crate) limit: Option<i64>,
}
