use anyhow::{Result, anyhow};
use chrono::NaiveDateTime;
use networking::Agent;
use scraper::{ElementRef, Html, Selector};
use tanoshi_lib::prelude::{ChapterInfo, MangaInfo};

fn get_data_src(el: &ElementRef) -> Option<String> {
    el.value()
        .attr("data-lazy-src")
        .or_else(|| el.value().attr("data-src"))
        .or_else(|| el.value().attr("src"))
        .map(|s| s.to_string())
}

pub fn parse_manga_list(url: &str, source_id: i64, body: &str) -> Result<Vec<MangaInfo>> {
    let doc = Html::parse_document(body);

    let selector = Selector::parse(".utao .uta .imgu, .listupd .bs .bsx, .listo .bs .bsx")
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_name =
        Selector::parse("a").map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_img =
        Selector::parse("img").map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;

    Ok(doc
        .select(&selector)
        .filter_map(|el| {
            let Some(link) = el.select(&selector_name).next() else {
                log::warn!("Skipping malformed WP Manga Reader card from {url}: missing link");
                return None;
            };
            let Some(title) = link
                .value()
                .attr("title")
                .map(str::trim)
                .filter(|title| !title.is_empty())
            else {
                log::warn!("Skipping malformed WP Manga Reader card from {url}: missing title");
                return None;
            };
            let Some(path) = link.value().attr("href").filter(|href| !href.is_empty()) else {
                log::warn!("Skipping malformed WP Manga Reader card from {url}: missing URL");
                return None;
            };
            let Some(cover_url) = el
                .select(&selector_img)
                .next()
                .and_then(|image| get_data_src(&image))
                .filter(|cover_url| !cover_url.trim().is_empty())
            else {
                log::warn!("Skipping malformed WP Manga Reader card from {url}: missing image");
                return None;
            };

            Some(MangaInfo {
                source_id,
                title: title.to_string(),
                author: vec![],
                genre: vec![],
                status: None,
                description: None,
                path: path.replace(url, ""),
                cover_url,
            })
        })
        .collect())
}

pub fn get_latest_manga(
    url: &str,
    source_id: i64,
    page: i64,
    client: &Agent,
) -> Result<Vec<MangaInfo>> {
    let mut resp = client
        .get(&format!("{}/manga/?page={}&order=latest", url, page))
        .header("Referer", url)
        .call()?;
    let body = resp.body_mut().read_to_string()?;
    parse_manga_list(url, source_id, &body)
}

pub fn get_popular_manga(
    url: &str,
    source_id: i64,
    page: i64,
    client: &Agent,
) -> Result<Vec<MangaInfo>> {
    let mut resp = client
        .get(&format!("{}/manga/?page={}&order=popular", url, page))
        .header("Referer", url)
        .call()?;
    let body = resp.body_mut().read_to_string()?;
    parse_manga_list(url, source_id, &body)
}

pub fn search_manga(
    url: &str,
    source_id: i64,
    page: i64,
    query: &str,
    client: &Agent,
) -> Result<Vec<MangaInfo>> {
    let mut resp = client
        .get(&format!("{}/page/{}/?s={}", url, page, query))
        .header("Referer", url)
        .call()?;
    let body = resp.body_mut().read_to_string()?;
    parse_manga_list(url, source_id, &body)
}

pub fn get_manga_detail(
    url: &str,
    path: &str,
    source_id: i64,
    client: &Agent,
) -> Result<MangaInfo> {
    let mut resp = client
        .get(&format!("{}{}", url, path))
        .header("Referer", url)
        .call()?;
    let body = resp.body_mut().read_to_string()?;

    let doc = Html::parse_document(&body);

    let selector_name = Selector::parse(r#"h1.entry-title"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_img = Selector::parse(".infomanga > div[itemprop=image] img, .thumb img")
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_genre = Selector::parse(r#"div.gnr a, .mgen a, .seriestugenre a"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_desc = Selector::parse(".desc, .entry-content[itemprop=description]")
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;

    Ok(MangaInfo {
        source_id,
        title: doc
            .select(&selector_name)
            .next()
            .and_then(|item| item.last_child())
            .and_then(|t| t.value().as_text())
            .unwrap()
            .trim()
            .to_string(),
        author: vec![],
        genre: doc
            .select(&selector_genre)
            .flat_map(|el| el.text())
            .map(|s| s.to_string())
            .collect(),
        status: None,
        description: Option::from(
            doc.select(&selector_desc)
                .flat_map(|el| el.text())
                .collect::<Vec<&str>>()
                .join("")
                .trim()
                .to_string(),
        ),
        path: path.to_string().replace(url, ""),
        cover_url: doc
            .select(&selector_img)
            .find_map(|el| get_data_src(&el))
            .unwrap_or_default(),
    })
}

pub fn get_chapters(
    url: &str,
    path: &str,
    source_id: i64,
    client: &Agent,
) -> Result<Vec<ChapterInfo>> {
    let mut resp = client
        .get(&format!("{}{}", url, path))
        .header("Referer", url)
        .header("X-Requested-With", "XMLHttpRequest")
        .call()?;
    let body = resp.body_mut().read_to_string()?;

    let doc = Html::parse_document(&body);

    let selector = Selector::parse(r#"div.bxcl li, #chapterlist li .eph-num a"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_chapter_name = Selector::parse(r#".lch a, .chapternum"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_chapter_time = Selector::parse(r#".chapterdate"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;
    let selector_chapter_url =
        Selector::parse("a").map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;

    let chapters: Vec<ChapterInfo> = doc
        .select(&selector)
        .map(|el| {
            let chapter_name = el
                .select(&selector_chapter_name)
                .flat_map(|el| el.text())
                .collect::<Vec<&str>>()
                .join("")
                .trim()
                .to_string();
            let chapter_time = el
                .select(&selector_chapter_time)
                .flat_map(|el| el.text())
                .collect::<Vec<&str>>()
                .join("");

            let uploaded = NaiveDateTime::parse_from_str(
                &format!("{} 00:00", chapter_time.trim()),
                "%B %d, %Y %H:%M",
            )
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(0);

            ChapterInfo {
                source_id,
                title: chapter_name.clone(),
                path: el
                    .select(&selector_chapter_url)
                    .filter_map(|el| el.value().attr("href"))
                    .collect::<Vec<&str>>()
                    .join("")
                    .replace(url, ""),
                number: chapter_name
                    .replace("Chapter ", "")
                    .split(' ')
                    .next()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_default(),
                scanlator: None,
                uploaded,
            }
        })
        .collect();

    Ok(chapters)
}

pub fn get_pages(url: &str, path: &str, client: &Agent) -> Result<Vec<String>> {
    let mut resp = client
        .get(&format!("{}{}", url, path))
        .header("Referer", url)
        .call()?;
    let body = resp.body_mut().read_to_string()?;

    let doc = Html::parse_document(&body);

    let selector = Selector::parse(r#"div#readerarea img"#)
        .map_err(|e| anyhow!("failed to parse selector: {:?}", e))?;

    Ok(doc
        .select(&selector)
        .flat_map(|el| get_data_src(&el))
        .map(|p| p.trim().to_string())
        .collect())
}
