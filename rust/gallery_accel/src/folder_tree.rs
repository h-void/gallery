use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
pub(crate) struct FolderNode {
    path: String,
    name: String,
    item_count: i64,
    children: Vec<FolderNode>,
}

pub(crate) fn new_folder_node(path: &str, name: &str, item_count: i64) -> FolderNode {
    FolderNode {
        path: path.to_string(),
        name: name.to_string(),
        item_count,
        children: Vec::new(),
    }
}

pub(crate) fn folder_tree_from_paths(artist_path: &str, file_paths: Vec<String>) -> FolderNode {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut child_paths: HashMap<String, HashSet<String>> = HashMap::new();
    names.insert(String::new(), "全部文件夹".to_string());
    counts.insert(String::new(), file_paths.len() as i64);
    child_paths.insert(String::new(), HashSet::new());

    let artist = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let mut direct_counts: HashMap<String, i64> = HashMap::new();
    let mut last_dir = String::new();
    let mut last_folder = String::new();
    let mut last_count = 0i64;
    let mut has_last = false;

    for file_path in file_paths {
        let normalized_file: Cow<'_, str> = if file_path.contains('\\') {
            Cow::Owned(file_path.replace('\\', "/"))
        } else {
            Cow::Borrowed(file_path.as_str())
        };
        let file_dir = match normalized_file.rfind('/') {
            Some(index) => normalized_file[..index].trim_end_matches('/'),
            None => "",
        };
        if has_last && file_dir == last_dir {
            last_count += 1;
            continue;
        }
        if has_last && !last_folder.is_empty() && last_count > 0 {
            *direct_counts.entry(last_folder.clone()).or_insert(0) += last_count;
        }
        has_last = true;
        last_dir.clear();
        last_dir.push_str(file_dir);
        last_folder = relative_folder_from_normalized(&artist, file_dir);
        last_count = 1;
    }
    if has_last && !last_folder.is_empty() && last_count > 0 {
        *direct_counts.entry(last_folder).or_insert(0) += last_count;
    }

    for (folder, folder_count) in direct_counts {
        let mut current = String::new();
        for part in folder.split('/').filter(|part| !part.is_empty()) {
            let next_path = if current.is_empty() {
                part.to_string()
            } else {
                format!("{current}/{part}")
            };
            if let Some(existing_count) = counts.get_mut(&next_path) {
                *existing_count += folder_count;
            } else {
                names.insert(next_path.clone(), part.to_string());
                child_paths.insert(next_path.clone(), HashSet::new());
                child_paths
                    .entry(current)
                    .or_default()
                    .insert(next_path.clone());
                counts.insert(next_path.clone(), folder_count);
            }
            current = next_path;
        }
    }

    build_folder_node("", &names, &counts, &child_paths)
}

fn build_folder_node(
    path: &str,
    names: &HashMap<String, String>,
    counts: &HashMap<String, i64>,
    child_paths: &HashMap<String, HashSet<String>>,
) -> FolderNode {
    let mut children = child_paths
        .get(path)
        .map(|paths| paths.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    children.sort_by(|left, right| {
        let left_name = names.get(left).map(|value| value.as_str()).unwrap_or(left);
        let right_name = names
            .get(right)
            .map(|value| value.as_str())
            .unwrap_or(right);
        natural_compare(left_name, right_name)
    });

    FolderNode {
        path: path.to_string(),
        name: names.get(path).cloned().unwrap_or_default(),
        item_count: *counts.get(path).unwrap_or(&0),
        children: children
            .iter()
            .map(|child| build_folder_node(child, names, counts, child_paths))
            .collect(),
    }
}

pub(crate) fn normalize_folder(folder: &str) -> String {
    let trimmed = folder.trim_matches(|c| c == '/' || c == '\\');
    if trimmed.is_empty() || trimmed == "." {
        return String::new();
    }
    if !trimmed.contains('\\')
        && !trimmed.contains("//")
        && !trimmed
            .split('/')
            .any(|part| part.is_empty() || part == ".")
    {
        return trimmed.to_string();
    }
    trimmed
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_folder_from_normalized(artist: &str, file_dir: &str) -> String {
    if file_dir == artist {
        return String::new();
    }
    if let Some(rest) = file_dir
        .strip_prefix(artist)
        .and_then(|r| r.strip_prefix('/'))
    {
        return normalize_folder(rest);
    }
    normalize_folder(file_dir.rsplit('/').next().unwrap_or(file_dir))
}
