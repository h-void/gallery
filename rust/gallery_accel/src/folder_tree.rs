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
    counts.insert(String::new(), 0);
    child_paths.insert(String::new(), HashSet::new());

    for file_path in file_paths {
        *counts.entry(String::new()).or_insert(0) += 1;
        let folder = relative_folder_path(artist_path, &file_path);
        if folder.is_empty() {
            continue;
        }
        let mut current = String::new();
        for part in folder.split('/').filter(|part| !part.is_empty()) {
            let next_path = if current.is_empty() {
                part.to_string()
            } else {
                format!("{current}/{part}")
            };
            names
                .entry(next_path.clone())
                .or_insert_with(|| part.to_string());
            counts.entry(next_path.clone()).or_insert(0);
            child_paths.entry(next_path.clone()).or_default();
            child_paths
                .entry(current.clone())
                .or_default()
                .insert(next_path.clone());
            *counts.entry(next_path.clone()).or_insert(0) += 1;
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
    folder
        .replace('\\', "/")
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_folder_path(artist_path: &str, file_path: &str) -> String {
    let artist = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let normalized_file = file_path.replace('\\', "/");
    let file_dir = match normalized_file.rfind('/') {
        Some(index) => normalized_file[..index].trim_end_matches('/').to_string(),
        None => String::new(),
    };
    if file_dir == artist {
        return String::new();
    }
    let prefix = format!("{artist}/");
    if file_dir.starts_with(&prefix) {
        return normalize_folder(&file_dir[prefix.len()..]);
    }
    normalize_folder(file_dir.rsplit('/').next().unwrap_or(file_dir.as_str()))
}
