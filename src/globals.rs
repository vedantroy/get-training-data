use crate::bloom::{self, Filter};
use lazy_static::lazy_static;
use log::trace;
use regex::Regex;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, ClientBuilder,
};
use serde::{self, Deserialize};
use sled::{Db, Tree};
use std::path::PathBuf;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
};

#[derive(Deserialize, Debug)]
pub struct LabelMaps {
    pub domain: String,
    pub path_exclude_re: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    pub maps: Vec<LabelMap>,
}

#[derive(Deserialize, Debug)]
pub struct LabelMap {
    pub path_match_re: String,
    pub abs_root_url: String,
    pub labels: Vec<Selector>,
}

#[derive(Deserialize, Debug)]
pub struct Selector {
    pub list: Option<bool>,
    pub name: String,
    pub selector: String,
}

#[derive(Deserialize, Debug)]
pub struct Config {
    db_path: String,
    pub filter_path: String,

    pub filter_bytes: usize,
    pub filter_expected_entries: usize,
    pub filter_checkpoint_secs: u64,

    pub workers: usize,
    #[serde(default = "default_ms_worker_check")]
    pub ms_worker_check: u64,

    pub label_map: String,
}

fn default_ms_worker_check() -> u64 {
    500
}

lazy_static! {
    pub static ref CONFIG: Config = {
        let args: Vec<_> = std::env::args().collect();
        let config_name = match args.get(1) {
            Some(v) => v.clone(),
            None => {
                let default_config = "config.toml";
                trace!("Using default config: {}", default_config);
                default_config.to_string()
            }
        };
        let s = fs::read_to_string(config_name).unwrap();
        toml::from_str(&s).unwrap()
    };
    pub static ref DB: Db = sled::open(&CONFIG.db_path).unwrap();
    pub static ref URL_QUEUE: Tree = DB.open_tree("url_queue").unwrap();
    pub static ref BLOOM: Filter = {
        let filter_path = PathBuf::from(&CONFIG.filter_path);
        Filter::new(bloom::Config {
            dir: filter_path,
            bytes: CONFIG.filter_bytes,
            expected_entries: CONFIG.filter_expected_entries,
            checkpoint_secs: CONFIG.filter_checkpoint_secs,
        })
    };
    pub static ref LABEL_MAP: LabelMaps = {
        serde_yaml::from_str::<LabelMaps>(&fs::read_to_string(&CONFIG.label_map).unwrap()).unwrap()
    };
    pub static ref EXCLUDE_RE: Option<Regex> = {
        LABEL_MAP
            .path_exclude_re
            .as_ref()
            .and_then(|re| Some(Regex::new(&re).unwrap()))
    };
    pub static ref MATCH_RE: HashMap<String, Regex> = {
        let mut re_map = HashMap::new();
        for map in LABEL_MAP.maps.iter() {
            let re = &map.path_match_re;
            re_map.insert(re.clone(), Regex::new(&re).unwrap());
        }
        re_map
    };
    pub static ref CLIENT: Client = {
        let client = match &LABEL_MAP.headers {
            Some(h) => {
                let mut headers = HeaderMap::new();
                for (k, v) in h {
                    headers.insert(k.as_str(), HeaderValue::from_str(&v).unwrap());
                }
                ClientBuilder::new()
                    .default_headers(headers)
                    .build()
                    .unwrap()
            }
            None => Client::new(),
        };
        client
    };
}
