use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_DISPOSITION};
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use soup::prelude::*;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufReader, Write};

pub mod structs;
use crate::api::structs::*;
use crate::util::slice_string;

pub struct BandcampPage {
    pub download_urls: DownloadsMap,
    pub page_name: String,
}

/// Body used to paginate through Bandcamp's collection API.
#[derive(Serialize, Debug)]
struct PostCollectionBody<'a> {
    fan_id: &'a str,
    older_than_token: &'a str,
}

pub struct Api {
    client: Client,
    // cookies: String,
}

impl Api {
    pub fn new(cookies: String) -> Api {
        // Cookie jar doesn't work properly for some reason, I'm probably doing
        // something wrong there.
        let mut headers = HeaderMap::new();
        headers.insert("Cookie", HeaderValue::from_str(&cookies).unwrap());

        let client = Client::builder().default_headers(headers).build().unwrap();

        Api { client }
    }

    fn bc_path(path: &str) -> String {
        format!("https://bandcamp.com/{path}")
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.client.get(path)
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.client.post(path)
    }

    /// Scrape a user's Bandcamp page to find download urls
    pub async fn get_download_urls(&self, name: &str) -> Result<BandcampPage, Box<dyn Error>> {
        let body = self.get(&Api::bc_path(name)).send().await?.text().await?;
        let soup = Soup::new(&body);

        let data_el = soup
            .attr("id", "pagedata")
            .find()
            .expect("Failed to extract data from collection page.");
        let data_blob = data_el
            .get("data-blob")
            .expect("Failed to extract data from element on collection page.");
        let fanpage_data: ParsedFanpageData = serde_json::from_str(&data_blob)
            .expect("Failed to deserialise collection page data blob.");

        match fanpage_data.fan_data.is_own_page {
            Some(true) => (),
            _ => bail!(format!(
                r#"Failed to scrape collection data for "{name}" (`is_own_page` is false). Perhaps check your cookies, or your spelling."#
            )),
        }

        // TODO: make sure this exists
        let mut collection = fanpage_data
            .collection_data
            .redownload_urls
            .clone()
            .unwrap();

        let skip_hidden_items = true;
        if skip_hidden_items {
            // TODO: filter `collection` to remove items that have their value containing a `sale_item_id` from `fanpage_data.item_cache.hidden`
            // collection.iter().filter(|&(k, v)| !fanpage_data.item_cache.hidden.contains_key(k))
        }

        if fanpage_data.collection_data.item_count > fanpage_data.collection_data.batch_size {
            let rest = self
                .get_rest_downloads_in_collection(&fanpage_data, "collection_items")
                .await?;
            collection.extend(rest);
        }

        if !skip_hidden_items
            && (fanpage_data.hidden_data.item_count > fanpage_data.hidden_data.batch_size)
        {
            let rest = self
                .get_rest_downloads_in_collection(&fanpage_data, "hidden_items")
                .await?;
            collection.extend(rest);
        }

        let title = soup.tag("title").find().unwrap().text();

        Ok(BandcampPage {
            page_name: title,
            download_urls: collection,
        })
    }

    /// Loop over a user's collection to retrieve all paginated items.
    async fn get_rest_downloads_in_collection(
        &self,
        data: &ParsedFanpageData,
        collection_name: &str,
    ) -> Result<DownloadsMap, Box<dyn Error>> {
        let collection_data = match collection_name {
            "collection_items" => &data.collection_data,
            "hidden_items" => &data.hidden_data,
            x => bail!(format!(r#"unexpected value for `collection_name`: "{x}""#)),
        };

        let mut last_token = collection_data.last_token.clone().unwrap();
        let mut more_available = true;
        let mut collection = DownloadsMap::new();

        while more_available {
            // retries
            let body = PostCollectionBody {
                fan_id: &data.fan_data.fan_id,
                older_than_token: &last_token,
            };
            let body = self
                .post(&Api::bc_path(&format!(
                    "api/fancollection/1/{collection_name}"
                )))
                .json(&body)
                .send()
                .await?
                .json::<ParsedCollectionItems>()
                .await?;

            collection.extend(body.redownload_urls);
            more_available = body.more_available;
            last_token = body.last_token;
        }

        Ok(collection)
    }

    // TODO: cache on API object?
    pub async fn get_digital_item(&self, url: &str) -> Result<DigitalItem, Box<dyn Error>> {
        let res = self.get(&url).send().await?.text().await?;
        let soup = Soup::new(&res);

        let download_page_blob = soup
            .attr("id", "pagedata")
            .find()
            .unwrap()
            .get("data-blob")
            .unwrap();
        let ParsedItemsData { digital_items } = serde_json::from_str(&download_page_blob).unwrap();
        let item = digital_items.first().cloned().unwrap();

        Ok(item)
    }

    // pub async fn retrieve_real_download_url(
    //     &self,
    //     item: &DigitalItem,
    //     audio_format: &str,
    // ) -> Result<String, Box<dyn Error>> {
    //     let downloads = &item.downloads;
    //     let download_url = &downloads.get(audio_format).unwrap().url;

    //     // TODO: do some testing to see if this is all really necessary, and if
    //     // we can just use the url given above (since it works in the browser).
    //     let random = rand::random::<u8>();
    //     let url = download_url
    //         .clone()
    //         .replace("/download/", "/statdownload/")
    //         .replace("http:", "https")
    //         + &format!("&.vrs=1&.rand={random}");
    //     let js_content = self.get(&url).send().await?.text().await?;
    //     let json_text = js_content
    //         .replace("if ( window.Downloads ) { Downloads.statResult ( ", "")
    //         .replace(" ) };", "");
    //     let json = serde_json::from_str::<ParsedStatDownload>(&json_text)?;

    //     Ok(json.download_url)
    // }

    pub async fn download_item(
        &self,
        item: &DigitalItem,
        path: String,
        audio_format: &str,
        pb: &indicatif::ProgressBar,
    ) -> Result<(), Box<dyn Error>> {
        // let download_url = self
        //     .retrieve_real_download_url(item, audio_format)
        //     .await
        //     .expect("Failed to retrieve item download URL");
        let download_url = &item.downloads.get(audio_format).unwrap().url;
        let res = self.get(download_url).send().await?;

        let disposition = res.headers().get(CONTENT_DISPOSITION).unwrap();
        // What should the default be?
        // Should probably use a thing to properly parse the content of content disposition.
        let filename = slice_string(
            disposition
                .to_str()?
                .split("; ")
                .find(|x| x.starts_with("filename="))
                .unwrap(),
            9,
        )
        .trim_matches('"');

        let total_size = res.content_length().unwrap();

        pb.set_length(total_size);
        pb.set_message(format!("{} - {}", item.title, item.artist));

        // TODO: tokio IO for threading?
        let full_path = format!("{path}/{filename}");
        let mut file = File::create(&full_path)?;
        let mut downloaded: u64 = 0;
        let mut stream = res.bytes_stream();

        while let Some(item) = stream.next().await {
            // TODO: Handle better
            let chunk = item?;
            file.write_all(&chunk)?;

            let new = std::cmp::min(downloaded + (chunk.len() as u64), total_size);
            downloaded = new;
            pb.set_position(new)
        }

        // TODO: see if we can have a interim buffer for the downloaded data,
        // and write directly to file if it's a single, or extract to the FS
        // without a intermediate file. (Is this a better idea? could possibly
        // fuck memory on large releases though).
        drop(file);

        if !item.is_single() {
            println!("Extracting zip");
            let file = File::open(&full_path)?;
            let reader = BufReader::new(file);
            let mut archive = zip::ZipArchive::new(reader)?;

            archive.extract(path)?;
            fs::remove_file(&full_path)?;
        }
        // Cover folder downloading

        pb.finish_with_message("Done");

        Ok(())
    }
}
