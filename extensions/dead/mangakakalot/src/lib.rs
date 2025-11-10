use anyhow::bail;
use lazy_static::lazy_static;
use mangakakalot_common::{
    get_chapters, get_manga_detail, get_pages, parse_manga_list, parse_search_manga_list,
};
use networking::{build_ureq_agent, Agent};
use std::env;
use tanoshi_lib::prelude::{Extension, Input, Lang, PluginRegistrar, SourceInfo};

tanoshi_lib::export_plugin!(register);

fn register(registrar: &mut dyn PluginRegistrar) {
    registrar.register_function(Box::new(Mangakakalot::default()));
}

lazy_static! {
    static ref PREFERENCES: Vec<Input> = vec![];
}

const ID: i64 = 10;
const NAME: &str = "Mangakakalot";
const URL: &str = "https://mangakakalot.com";

pub struct Mangakakalot {
    preferences: Vec<Input>,
    client: Agent,
}

impl Default for Mangakakalot {
    fn default() -> Self {
        Self {
            preferences: PREFERENCES.clone(),
            client: build_ureq_agent(None),
        }
    }
}

impl Extension for Mangakakalot {
    fn set_preferences(&mut self, preferences: Vec<Input>) -> anyhow::Result<()> {
        for input in preferences {
            for pref in self.preferences.iter_mut() {
                if input.eq(pref) {
                    *pref = input.clone();
                }
            }
        }
        Ok(())
    }

    fn get_preferences(&self) -> anyhow::Result<Vec<Input>> {
        Ok(self.preferences.clone())
    }

    fn get_source_info(&self) -> SourceInfo {
        SourceInfo {
            id: ID,
            name: NAME.to_string(),
            url: URL.to_string(),
            version: env!("CARGO_PKG_VERSION"),
            icon: "https://mangakakalot.com/favicon.ico",
            languages: Lang::Single("en".to_string()),
            nsfw: false,
        }
    }

    fn get_popular_manga(&self, page: i64) -> anyhow::Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        let mut resp = self
            .client
            .get(&format!(
                "{URL}/manga_list?type=topview&category=all&state=all&page={page}",
            ))
            .call()?;
        let body = resp.body_mut().read_to_string()?;
        parse_manga_list(ID, &body, ".list-truyen-item-wrap")
    }

    fn get_latest_manga(&self, page: i64) -> anyhow::Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        let mut resp = self
            .client
            .get(&format!(
                "{URL}/manga_list?type=latest&category=all&state=all&page={page}",
            ))
            .call()?;
        let body = resp.body_mut().read_to_string()?;
        parse_manga_list(ID, &body, ".list-truyen-item-wrap")
    }

    fn search_manga(
        &self,
        page: i64,
        query: Option<String>,
        _: Option<Vec<Input>>,
    ) -> anyhow::Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        if let Some(query) = query {
            let mut resp = self
                .client
                .get(&format!(
                    "{URL}/search/story/{}?page={page}",
                    query.replace(' ', "_").to_lowercase()
                ))
                .call()?;
            let body = resp.body_mut().read_to_string()?;
            parse_search_manga_list(ID, &body, "div.story_item")
        } else {
            bail!("query can not be empty")
        }
    }

    fn get_manga_detail(&self, path: String) -> anyhow::Result<tanoshi_lib::prelude::MangaInfo> {
        get_manga_detail(&path, ID, &self.client)
    }

    fn get_chapters(&self, path: String) -> anyhow::Result<Vec<tanoshi_lib::prelude::ChapterInfo>> {
        get_chapters(&path, ID, &self.client)
    }

    fn get_pages(&self, path: String) -> anyhow::Result<Vec<String>> {
        let mut resp = self
            .client
            .get(&format!("https://chapmanganato.com{path}"))
            .call()?;
        let body = resp.body_mut().read_to_string()?;
        get_pages(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_latest_manga() {
        let source = Mangakakalot::default();

        let res1 = source.get_latest_manga(1).unwrap();
        assert!(!res1.is_empty());

        let res2 = source.get_latest_manga(2).unwrap();
        assert!(!res2.is_empty());

        assert!(
            res1[0].path != res2[0].path,
            "{} should be different than {}",
            res1[0].path,
            res2[0].path
        );
    }

    #[test]
    fn test_get_popular_manga() {
        let source = Mangakakalot::default();

        let res = source.get_popular_manga(1).unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_search_manga() {
        let source = Mangakakalot::default();

        let res = source
            .search_manga(1, Some("one piece".to_string()), None)
            .unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_get_manga_detail() {
        let source = Mangakakalot::default();

        let res = source
            .get_manga_detail("/manga-hs951953".to_string())
            .unwrap();
        assert_eq!(res.title, "Shokugeki no Soma");
        assert!(!res.cover_url.is_empty());
        assert!(res.description.is_some());
        assert_ne!(res.description, Some("".to_string()));
        assert_eq!(
            res.author,
            vec!["Tsukuda Yuuto".to_string(), "Saeki Shun".to_string()]
        );
        assert_eq!(
            res.genre,
            vec![
                "Comedy".to_string(),
                "Cooking".to_string(),
                "Drama".to_string(),
                "Ecchi".to_string(),
                "School life".to_string(),
                "Shounen".to_string(),
            ]
        );
    }

    #[test]
    fn test_get_chapters() {
        let source = Mangakakalot::default();

        let res = source.get_chapters("/manga-hs951953".to_string()).unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_get_pages() {
        let source = Mangakakalot::default();

        let res = source
            .get_pages("/manga-hs951953/chapter-315.3".to_string())
            .unwrap();

        assert!(!res.is_empty());
    }
}
