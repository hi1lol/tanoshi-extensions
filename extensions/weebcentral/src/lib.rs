use anyhow::Result;
use bytes::Bytes;
use chrono::prelude::*;
use lazy_static::lazy_static;
use networking::{RateLimitedAgent, build_rate_limited_ureq_agent};
use scraper::{ElementRef, Html, Selector};
use std::env;
use tanoshi_lib::extensions::PluginRegistrar;
use tanoshi_lib::prelude::{ChapterInfo, Extension, Input, Lang, MangaInfo, SourceInfo};
use urlencoding::encode;

tanoshi_lib::export_plugin!(register);

fn register(registrar: &mut dyn PluginRegistrar) {
    registrar.register_function(Box::new(Weebcentral::default()));
}

lazy_static! {
    static ref PREFERENCES: Vec<Input> = vec![];
}

const ID: i64 = 28;
const NAME: &str = "WeebCentral";
const URL: &str = "https://weebcentral.com";
const REQUESTS_PER_SECOND: f64 = 10.0;
// Get Pages seems to have its own rate limit
const PAGES_REQUESTS_PER_SECOND: f64 = 1.0;

fn parse_upload_timestamp(upload: &str) -> i64 {
    upload
        .parse::<DateTime<Utc>>()
        .map(|date| date.timestamp())
        .unwrap_or(0)
}

fn find_sidebar_section<'a>(
    document: &'a Html,
    sidebar_selector: &Selector,
    label_selector: &Selector,
    label: &str,
) -> Option<ElementRef<'a>> {
    document.select(sidebar_selector).find(|section| {
        section
            .select(label_selector)
            .next()
            .map(|label_element| label_element.text().collect::<String>())
            .is_some_and(|section_label| section_label.trim().trim_end_matches(':') == label)
    })
}

pub struct Weebcentral {
    preferences: Vec<Input>,
    client: RateLimitedAgent,
    client_pages: RateLimitedAgent,
}

impl Default for Weebcentral {
    fn default() -> Self {
        Self {
            preferences: PREFERENCES.clone(),
            client: build_rate_limited_ureq_agent(None, Some(REQUESTS_PER_SECOND)),
            client_pages: build_rate_limited_ureq_agent(None, Some(PAGES_REQUESTS_PER_SECOND)),
        }
    }
}

fn get_manga_list(
    mut page: i64,
    suburl: &str,
    client: &RateLimitedAgent,
) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
    if page < 1 {
        page = 1;
    }
    let offset = (page - 1) * 32;

    let mut manga_list = Vec::new();
    let url = format!("{}{}{}", URL, suburl, offset);
    let mut resp = client.get(&url).call()?;
    let body = resp.body_mut().read_to_string()?;
    let document = Html::parse_document(&body);

    let manga_selector = Selector::parse("article.bg-base-300").unwrap();
    let title_selector = Selector::parse("div.text-ellipsis.truncate").unwrap();
    let author_selector = Selector::parse("div > span > a.link.link-info.link-hover").unwrap();
    let genre_selector = Selector::parse("div.opacity-70 > span").unwrap();
    let status_selector = Selector::parse("strong + span").unwrap();
    let cover_selector = Selector::parse("picture img").unwrap();
    let url_selector = Selector::parse("a").unwrap();

    for manga in document.select(&manga_selector) {
        let title = manga.select(&title_selector).next().map_or_else(
            || "Unknown Title".to_string(),
            |el| el.inner_html().trim().to_string(),
        );

        let mut authors: Vec<String> = Vec::new();
        for author in manga.select(&author_selector) {
            authors.push(author.inner_html().trim().to_string());
        }

        let mut genres: Vec<String> = Vec::new();
        let mut i = 0;
        for genre in manga.select(&genre_selector) {
            if i < 3 {
                i += 1;
                continue;
            }
            genres.push(genre.inner_html().trim().to_string());
        }

        let manga_url = manga.select(&url_selector).next().map_or_else(
            || "".to_string(),
            |el| el.value().attr("href").unwrap_or("").to_string(),
        );

        let manga_id = manga_url.split('/').nth(4).unwrap_or("").to_string();

        let status = manga
            .select(&status_selector)
            .nth(1)
            .map_or_else(|| "".to_string(), |el| el.inner_html().trim().to_string());
        let cover_url = manga.select(&cover_selector).next().map_or_else(
            || "".to_string(),
            |el| el.value().attr("src").unwrap_or("").to_string(),
        );

        manga_list.push(MangaInfo {
            source_id: ID,
            title: title,
            author: authors,
            genre: genres,
            status: Some(status),
            description: None,
            path: format!("/series/{}", manga_id),
            cover_url,
        });
    }
    Ok(manga_list)
}

impl Extension for Weebcentral {
    fn set_preferences(&mut self, preferences: Vec<Input>) -> Result<()> {
        for input in preferences {
            for pref in self.preferences.iter_mut() {
                if input.eq(pref) {
                    *pref = input.clone();
                }
            }
        }

        Ok(())
    }

    fn get_preferences(&self) -> Result<Vec<Input>> {
        Ok(self.preferences.clone())
    }

    fn get_source_info(&self) -> tanoshi_lib::prelude::SourceInfo {
        SourceInfo {
            id: ID,
            name: NAME.to_string(),
            url: URL.to_string(),
            version: env!("CARGO_PKG_VERSION"),
            icon: "https://weebcentral.com/static/images/144.png",
            languages: Lang::Single("en".to_string()),
            nsfw: false,
        }
    }

    fn get_popular_manga(&self, page: i64) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        get_manga_list(
            page,
            "/search/data?limit=32&author=&text=&sort=Popularity&order=Descending&official=Any&anime=Any&adult=Any&display_mode=Full%20Display&offset=",
            &self.client,
        )
    }

    fn get_latest_manga(&self, page: i64) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        get_manga_list(
            page,
            "/search/data?limit=32&sort=Latest+Updates&order=Descending&official=Any&anime=Any&adult=Any&display_mode=Full+Display&offset=",
            &self.client,
        )
    }

    fn search_manga(
        &self,
        page: i64,
        query: Option<String>,
        _: Option<Vec<Input>>,
    ) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        //TODO: Add filters
        get_manga_list(
            page,
            &format!(
                "/search/data?author=&text={}&sort=Latest%20Updates&order=Descending&official=Any&anime=Any&adult=Any&display_mode=Full%20Display&offset=",
                encode(query.unwrap_or_default().as_str()).into_owned()
            ),
            &self.client,
        )
    }

    fn get_manga_detail(&self, path: String) -> Result<tanoshi_lib::prelude::MangaInfo> {
        let mut resp = self.client.get(&format!("{URL}{path}")).call()?;
        let body = resp.body_mut().read_to_string()?;

        let manga = Html::parse_document(&body);

        let title_selector = Selector::parse("h1.hidden.md\\:block.text-2xl.font-bold").unwrap();
        let sidebar_selector: Selector = Selector::parse("ul.flex.flex-col.gap-4 > li").unwrap();
        let label_selector = Selector::parse("strong").unwrap();
        let link_selector = Selector::parse("span > a.link.link-info.link-hover").unwrap();
        let status_selector = Selector::parse("strong + a.link.link-info.link-hover").unwrap();
        let description_selector = Selector::parse(
            "ul.flex.flex-col.gap-4 > li > strong + p.whitespace-pre-wrap.break-words",
        )
        .unwrap();
        let cover_selector = Selector::parse("picture img").unwrap();

        let title = manga.select(&title_selector).next().map_or_else(
            || "Unknown Title".to_string(),
            |el| el.inner_html().trim().to_string(),
        );

        let author_sec =
            find_sidebar_section(&manga, &sidebar_selector, &label_selector, "Author(s)");
        let genre_sec = find_sidebar_section(&manga, &sidebar_selector, &label_selector, "Tags(s)");
        let status_sec = find_sidebar_section(&manga, &sidebar_selector, &label_selector, "Status");

        let mut authors: Vec<String> = Vec::new();
        if let Some(author_sec) = author_sec {
            for author in author_sec.select(&link_selector) {
                authors.push(author.inner_html().trim().to_string());
            }
        }

        let mut genres: Vec<String> = Vec::new();
        if let Some(genre_sec) = genre_sec {
            for genre in genre_sec.select(&link_selector) {
                genres.push(genre.inner_html().trim().to_string());
            }
        }

        let status = status_sec
            .and_then(|section| section.select(&status_selector).next())
            .map_or_else(|| "".to_string(), |el| el.inner_html().trim().to_string());

        let description = manga
            .select(&description_selector)
            .next()
            .map_or_else(|| "".to_string(), |el| el.inner_html().trim().to_string());

        let cover_url = manga.select(&cover_selector).next().map_or_else(
            || "".to_string(),
            |el| el.value().attr("src").unwrap_or("").to_string(),
        );

        Ok(MangaInfo {
            source_id: ID,
            title: title,
            author: authors,
            genre: genres,
            status: Some(status),
            description: Some(description),
            path: path,
            cover_url,
        })
    }

    fn get_chapters(&self, path: String) -> Result<Vec<tanoshi_lib::prelude::ChapterInfo>> {
        let mut resp = self
            .client
            .get(&format!("{URL}{path}/full-chapter-list"))
            .call()?;
        let body = resp.body_mut().read_to_string()?;

        let document = Html::parse_document(&body);

        let chapter_selector = Selector::parse("body > div.flex.items-center").unwrap();
        let time_selector = Selector::parse("a > time.text-datetime.opacity-50").unwrap();
        let link_selector = Selector::parse("a").unwrap();
        let title_selector =
            Selector::parse("a > span.grow.flex.items-center.gap-2 > span").unwrap();

        let chapter_count = document.select(&chapter_selector).count();
        let mut chapters = vec![];
        let mut number = chapter_count.clone();

        for chapter in document.select(&chapter_selector) {
            let title = chapter.select(&title_selector).next().map_or_else(
                || "Unknown Title".to_string(),
                |el| el.inner_html().trim().to_string(),
            );

            let link = chapter.select(&link_selector).next().map_or_else(
                || "".to_string(),
                |el| el.value().attr("href").unwrap_or("").to_string(),
            );

            let upload = chapter
                .select(&time_selector)
                .next()
                .map_or_else(|| "".to_string(), |el| el.inner_html().trim().to_string());

            chapters.push(ChapterInfo {
                source_id: ID,
                title: title.clone(),
                path: format!(
                    "/chapters/{}",
                    link.clone().split('/').nth(4).unwrap_or("").to_string()
                ),
                number: number as f64,
                scanlator: None,
                uploaded: parse_upload_timestamp(&upload),
            });
            number -= 1;
        }

        Ok(chapters)
    }

    fn get_pages(&self, path: String) -> Result<Vec<String>> {
        let mut resp = self
            .client_pages
            .get(&format!(
                "{URL}{path}/images?is_prev=False&current_page=1&reading_style=single_page"
            ))
            .call()?;
        let body = resp.body_mut().read_to_string()?;

        let document = Html::parse_document(&body);

        let mut panels = vec![];

        let panel_selector =
            Selector::parse("section.w-full.pb-4.cursor-pointer > img.mx-auto").unwrap();

        for panel in document.select(&panel_selector) {
            panels.push(panel.value().attr("src").unwrap_or("").to_string());
        }

        Ok(panels)
    }

    fn get_image_bytes(&self, url: String) -> anyhow::Result<Bytes> {
        self.client.fetch_bytes(&url, Some(URL))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Planetes: completed series, so the values asserted below are stable.
    const MANGA_PATH: &str = "/series/01J76XY8K8BPR60XQNGPTEJ767";

    fn create_test_instance() -> Weebcentral {
        let preferences: Vec<Input> = vec![];

        let mut weebcentral: Weebcentral = Weebcentral::default();

        weebcentral.set_preferences(preferences).unwrap();

        weebcentral
    }

    #[test]
    fn test_get_latest_manga() {
        let weebcentral = create_test_instance();

        let res1 = weebcentral.get_latest_manga(1).unwrap();
        assert!(!res1.is_empty());

        let res2 = weebcentral.get_latest_manga(2).unwrap();
        assert!(!res2.is_empty());

        assert_ne!(
            res1[0].path, res2[0].path,
            "{} should be different than {}",
            res1[0].path, res2[0].path
        );
    }

    #[test]
    fn test_get_popular_manga() {
        let weebcentral = create_test_instance();

        let res = weebcentral.get_popular_manga(1).unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_search_manga() {
        let weebcentral = create_test_instance();

        let res = weebcentral
            .search_manga(1, Some("planetes".to_string()), None)
            .unwrap();

        assert!(!res.is_empty());
        assert!(
            res.iter().any(|m| m.path == MANGA_PATH),
            "search results should contain {}, got {:?}",
            MANGA_PATH,
            res.iter().map(|m| m.path.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_get_manga_detail() {
        let weebcentral = create_test_instance();

        let res = weebcentral
            .get_manga_detail(MANGA_PATH.to_string())
            .unwrap();

        assert_eq!(res.title, "Planetes");

        // The fields below come from sidebar sections located by their
        // <strong> label text, so they double as label-parsing tests.
        assert!(
            res.author.iter().any(|a| a.contains("Makoto")),
            "author should be parsed from the Author(s) section, got {:?}",
            res.author
        );
        assert!(
            res.genre.iter().any(|g| g == "Sci-fi"),
            "genre should be parsed from the Tags(s) section, got {:?}",
            res.genre
        );
        assert_eq!(
            res.status.as_deref(),
            Some("Complete"),
            "status should be parsed from the Status section"
        );
    }

    #[test]
    fn test_get_chapters() {
        let weebcentral = create_test_instance();

        let res = weebcentral.get_chapters(MANGA_PATH.to_string()).unwrap();

        assert!(!res.is_empty());
        assert!(
            res.iter().all(|c| c.path.starts_with("/chapters/")),
            "chapter paths should look like /chapters/<id>"
        );
        let uploaded_count = res.iter().filter(|c| c.uploaded > 0).count();
        assert!(
            uploaded_count * 2 >= res.len(),
            "at least half of upload dates should parse instead of falling back to epoch 0; got {uploaded_count}/{}",
            res.len()
        );
    }

    #[test]
    fn test_get_pages() {
        let weebcentral = create_test_instance();

        let chapters = weebcentral.get_chapters(MANGA_PATH.to_string()).unwrap();
        let chapter = chapters.first().expect("chapter list should not be empty");

        let res = weebcentral.get_pages(chapter.path.clone()).unwrap();
        assert!(!res.is_empty());
        assert!(
            res.iter().all(|p| p.starts_with("http")),
            "pages should be absolute image urls, got {:?}",
            res.first()
        );
    }
}
