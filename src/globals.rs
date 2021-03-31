use crate::{
    bloom::{self, Filter},
    save::{self, Saver},
};
use lazy_static::lazy_static;
use log::trace;
use regex::Regex;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, ClientBuilder,
};
use serde::{self, Deserialize, Serialize};
use serde_json;
use sled::{Db, Tree};
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
};

#[derive(Deserialize, Debug)]
pub struct PathExcludeSettings {
    re: String,
    invert: bool,
}

#[derive(Deserialize, Debug)]
pub struct LabelMaps {
    pub domain: String,
    pub path_exclude: Option<PathExcludeSettings>,
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
    save_path: String,
    pub filter_path: String,
    chunk_size: usize,

    pub filter_bytes: usize,
    pub filter_expected_entries: usize,
    pub filter_checkpoint_secs: u64,

    pub workers: usize,
    pub worker_check_ms: u64,
    pub saver_check_secs: u64,

    pub label_map: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum SelectorValue {
    Str(String),
    Arr(Vec<String>),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Save {
    pub url: String,
    pub raw: String,
    pub input: String,
    pub labels: BTreeMap<String, SelectorValue>,
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
            .path_exclude
            .as_ref()
            .and_then(|s| Some(Regex::new(&s.re).unwrap()))
    };
    pub static ref INVERT_EXCLUDE: bool = {
        if LABEL_MAP.path_exclude.is_none() {
            return false;
        }
        LABEL_MAP.path_exclude.as_ref().unwrap().invert
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
                    .gzip(true)
                    .brotli(true)
                    .build()
                    .unwrap()
            }
            None => Client::new(),
        };
        client
    };
    pub static ref SAVER: Saver<Save> = {
        fn save(idx: usize, v: &Vec<Save>) {
            trace!("Saving chunk: {}...", idx);
            let path = format!("{}/{}.json", CONFIG.save_path, idx);
            if Path::new(&path).exists() {
                panic!("Path: {} already exists", path)
            }
            let mut f = File::create(path).unwrap();
            let bytes = serde_json::to_vec(v).unwrap();
            f.write_all(&bytes).unwrap();
        }
        fs::create_dir_all(&CONFIG.save_path).unwrap();
        let paths = fs::read_dir(&CONFIG.save_path).unwrap();

        Saver::new(
            save,
            save::Config {
                check_interval_secs: CONFIG.saver_check_secs,
                chunk_size: CONFIG.chunk_size,
                start_chunk: paths.count() + 1,
            },
        )
    };
}
