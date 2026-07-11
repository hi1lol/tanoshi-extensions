mod dto;
use std::collections::HashMap;

use anyhow::Result;
use networking::RateLimitedAgent;
use tanoshi_lib::prelude::*;

use crate::dto::{Detail, Series};

fn first_group_key(groups: &HashMap<String, Vec<String>>) -> Option<&str> {
    groups.keys().map(String::as_str).min()
}

fn ensure_non_empty<T>(body: &str, url: &str, items: Vec<T>) -> Result<Vec<T>> {
    if !body.trim().is_empty() && items.is_empty() {
        return Err(anyhow::anyhow!(
            "parsed 0 items from {url} — markup change?"
        ));
    }

    Ok(items)
}

pub fn get_manga_list(
    url: &str,
    source_id: i64,
    client: &RateLimitedAgent,
) -> Result<Vec<MangaInfo>> {
    let request_url = format!("{}/api/get_all_series", url);
    let mut resp = client.get(&request_url).call()?;
    let text = resp.body_mut().read_to_string()?;
    let results: HashMap<String, Detail> = serde_json::from_str(&text)?;

    let mut manga: Vec<MangaInfo> = results
        .into_iter()
        .map(|(title, detail)| MangaInfo {
            source_id,
            title,
            author: vec![detail.author, detail.artist],
            genre: vec![],
            status: Some("Ongoing".to_string()),
            description: Some(detail.description),
            path: format!("/api/series/{}", detail.slug),
            cover_url: format!("{}{}", url, detail.cover),
        })
        .collect();

    manga.sort_by(|a, b| a.title.cmp(&b.title));
    ensure_non_empty(&text, &request_url, manga)
}

pub fn get_manga_detail(
    url: &str,
    path: &str,
    source_id: i64,
    client: &RateLimitedAgent,
) -> Result<MangaInfo> {
    let mut resp = client.get(&format!("{}{}", url, path)).call()?;
    let text = resp.body_mut().read_to_string()?;
    let series: Series = serde_json::from_str(&text)?;

    Ok(MangaInfo {
        source_id,
        title: series.title.clone(),
        author: vec![series.author.clone(), series.artist.clone()],
        genre: vec![],
        status: Some("Ongoing".to_string()),
        description: Some(series.description.clone()),
        path: path.to_string(),
        cover_url: format!("{}{}", url, series.cover),
    })
}

pub fn get_chapters(
    url: &str,
    path: &str,
    source_id: i64,
    client: &RateLimitedAgent,
) -> Result<Vec<ChapterInfo>> {
    let request_url = format!("{}{}", url, path);
    let mut resp = client.get(&request_url).call()?;
    let text = resp.body_mut().read_to_string()?;
    let series: Series = serde_json::from_str(&text)?;

    let mut chapters = vec![];
    for (number, chapter) in series.chapters {
        let group = first_group_key(&chapter.groups);
        chapters.push(ChapterInfo {
            source_id,
            title: chapter.title.clone(),
            path: format!("{}/{}", path.trim_end_matches('/'), number),
            number: number.parse().unwrap_or_default(),
            scanlator: group.and_then(|group| series.groups.get(group).cloned()),
            uploaded: group
                .and_then(|group| chapter.release_date.get(group))
                .copied()
                .unwrap_or_default() as i64,
        })
    }

    ensure_non_empty(&text, &request_url, chapters)
}

pub fn get_pages(url: &str, path: &str, client: &RateLimitedAgent) -> Result<Vec<String>> {
    let path = path.trim_end_matches('/');
    let (series_path, chapter_number) = path
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid Guya chapter path: {path}"))?;

    let mut resp = client.get(&format!("{}{}", url, series_path)).call()?;
    let text = resp.body_mut().read_to_string()?;
    let series: Series = serde_json::from_str(&text)?;

    let chapter = series.chapters.get(chapter_number).ok_or_else(|| {
        anyhow::anyhow!("chapter {chapter_number} not found in series {series_path}")
    })?;
    let group = first_group_key(&chapter.groups)
        .ok_or_else(|| anyhow::anyhow!("chapter {chapter_number} has no scanlation groups"))?;
    let pages = chapter
        .groups
        .get(group)
        .filter(|pages| !pages.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("chapter {chapter_number} has no pages for scanlation group {group}")
        })?;

    Ok(pages
        .iter()
        .map(|page| {
            format!(
                "{}/media/manga/{}/chapters/{}/{}/{}",
                url, series.slug, chapter.folder, group, page
            )
        })
        .collect())
}
