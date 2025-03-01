use anyhow::Result;
use tanoshi_lib::extensions::PluginRegistrar;
use tanoshi_lib::prelude::{Extension, Input, Lang, SourceInfo};
use lazy_static::lazy_static;
use networking::{Agent, build_ureq_agent};
use std::env;

tanoshi_lib::export_plugin!(register);

fn register(registrar: &mut dyn PluginRegistrar) {
    registrar.register_function(Box::new(Mangalife::default()));
}

lazy_static! {
    static ref PREFERENCES: Vec<Input> = vec![];
}

const ID: i64 = 4;
const NAME: &str = "MangaLife";
const URL: &str = "https://weebcentral.com";

pub struct Mangalife {
    preferences: Vec<Input>,
    client: Agent,
}

impl Default for Mangalife {
    fn default() -> Self {
        Self {
            preferences: PREFERENCES.clone(),
            client: build_ureq_agent(None, None),
        }
    }
}

impl Extension for Mangalife {
    fn set_preferences(
        &mut self,
        preferences: Vec<Input>,
    ) -> Result<()> {
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
            icon: "https://manga4life.com/media/favicon.png",
            languages: Lang::Single("en".to_string()),
            nsfw: false,
        }
    }

    fn filter_list(&self) -> Vec<Input> {
        nepnep::get_filter_list()
    }

    fn get_popular_manga(&self, page: i64) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        nepnep::get_popular_manga(ID, URL, page, &self.client)
    }

    fn get_latest_manga(&self, page: i64) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        nepnep::get_latest_manga(ID, URL, page, &self.client)
    }

    fn search_manga(
        &self,
        page: i64,
        query: Option<String>,
        filters: Option<Vec<Input>>,
    ) -> Result<Vec<tanoshi_lib::prelude::MangaInfo>> {
        nepnep::search_manga(ID, URL, page, query, filters, &self.client)
    }

    fn get_manga_detail(&self, path: String) -> Result<tanoshi_lib::prelude::MangaInfo> {
        nepnep::get_manga_detail(ID, URL, path, &self.client)
    }

    fn get_chapters(&self, path: String) -> Result<Vec<tanoshi_lib::prelude::ChapterInfo>> {
        nepnep::get_chapters(ID, URL, path, &self.client)
    }

    fn get_pages(&self, path: String) -> Result<Vec<String>> {
        nepnep::get_pages(URL, path, &self.client)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_latest_manga() {
        let mangalife = Mangalife::default();

        let res = mangalife.get_latest_manga(1).unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_get_popular_manga() {
        let mangalife = Mangalife::default();

        let res = mangalife.get_popular_manga(1).unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_search_manga() {
        let mangalife = Mangalife::default();

        let res = mangalife
            .search_manga(1, Some("komi".to_string()), None)
            .unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_get_manga_detail() {
        let mangalife = Mangalife::default();

        let res = mangalife
            .get_manga_detail("/manga/a96676e5-8ae2-425e-b549-7f15dd34a6d8".to_string())
            .unwrap();
        assert_eq!(res.title, "Komi-san wa Komyushou Desu.");
    }

    #[test]
    fn test_get_chapters() {
        let mangalife = Mangalife::default();

        let res = mangalife
            .get_chapters("/manga/a96676e5-8ae2-425e-b549-7f15dd34a6d8".to_string())
            .unwrap();
        assert!(!res.is_empty());
    }

    #[test]
    fn test_get_pages() {
        let mangalife = Mangalife::default();

        let res = mangalife
            .get_pages("/chapter/03d3e4b9-db8d-4fb5-88fc-b6a087bd6410".to_string())
            .unwrap();

        assert!(!res.is_empty());
    }
}
