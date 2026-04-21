//! Radarr-shape response + request DTOs.
//!
//! Shared DTOs (QualityProfile, Tag, SystemStatus, DownloadClientEntry,
//! TagBody, LookupQuery) live in crate::handlers::arr_shared.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── Radarr-compatible types ───────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrMovie {
    pub id: i64,
    pub title: String,
    pub sort_title: String,
    pub status: String,
    pub overview: String,
    pub images: Vec<RadarrImage>,
    pub remote_poster: String,
    pub year: i32,
    pub path: String,
    pub quality_profile_id: i32,
    pub monitored: bool,
    pub minimum_availability: String,
    pub runtime: i32,
    pub tmdb_id: i64,
    pub imdb_id: String,
    pub title_slug: String,
    pub certification: String,
    pub genres: Vec<String>,
    pub tags: Vec<i32>,
    pub added: String,
    pub ratings: RadarrRatings,
    pub has_file: bool,
    pub is_available: bool,
    pub folder_name: String,
    pub clean_title: String,
    pub root_folder_path: String,
    pub movie_file: Option<RadarrMovieFile>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrImage {
    pub cover_type: String,
    pub url: String,
    pub remote_url: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRatings {
    pub imdb: RadarrRatingValue,
    pub tmdb: RadarrRatingValue,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRatingValue {
    pub votes: i32,
    pub value: f64,
    #[serde(rename = "type")]
    pub rating_type: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RadarrMovieFile {
    pub id: i64,
    pub relative_path: String,
    pub size: i64,
    pub quality: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrRootFolder {
    pub id: i32,
    pub path: String,
    pub free_space: i64,
    pub accessible: bool,
    pub unmapped_folders: Vec<()>,
}

// ── Request types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddMovieBody {
    pub tmdb_id: Option<i64>,
    pub title: Option<String>,
    pub quality_profile_id: Option<i32>,
    pub monitored: Option<bool>,
    pub root_folder_path: Option<String>,
    pub minimum_availability: Option<String>,
    pub tags: Option<Vec<i32>>,
    pub add_options: Option<AddMovieOptions>,
    pub title_slug: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AddMovieOptions {
    pub search_for_movie: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UpdateMovieBody {
    pub id: i64,
    pub monitored: Option<bool>,
    pub tags: Option<Vec<i32>>,
    #[serde(flatten)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrCommandBody {
    pub name: Option<String>,
    pub movie_ids: Option<Vec<i64>>,
}
