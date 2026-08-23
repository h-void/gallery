use serde::Deserialize;

fn deserialize_bool_lenient<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct LenientBoolVisitor;

    impl<'de> serde::de::Visitor<'de> for LenientBoolVisitor {
        type Value = Option<bool>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean or string representation of a boolean")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v != 0))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v != 0))
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "t" | "yes" | "y" | "on" => Ok(Some(true)),
                "0" | "false" | "f" | "no" | "n" | "off" => Ok(Some(false)),
                "" => Ok(None),
                _ => Err(E::custom(format!("invalid boolean string: {}", v))),
            }
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(LenientBoolVisitor)
}

#[derive(Deserialize)]
pub(crate) struct CandidateQuery {
    pub(crate) status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) hide_grouped: Option<bool>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct ScanCandidateQuery {
    pub(crate) status: Option<String>,
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
    pub(crate) cursor: Option<String>,
    pub(crate) sort: Option<String>,
    pub(crate) media_type: Option<String>,
    pub(crate) tag_id: Option<i64>,
    pub(crate) tags: Option<String>,
    pub(crate) folder: Option<String>,
    pub(crate) date_from: Option<String>,
    pub(crate) date_to: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) image_only: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) untagged: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) duplicates_only: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) favorite_only: Option<bool>,
    /// Search filter, handled natively (raw substring on file_name/folder_name/
    /// file_path + pinyin on item tag names); mirrors `app/api/items.py`.
    pub(crate) search: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) search_tags_only: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub(crate) archive_only: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct TagSearchQuery {
    pub(crate) artist_id: Option<i64>,
    pub(crate) search: Option<String>,
    pub(crate) limit: Option<usize>,
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
