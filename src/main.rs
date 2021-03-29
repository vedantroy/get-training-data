use anyhow::{anyhow, bail, Result};
use env_logger;
use fasthash::metro::hash64;
use futures;
use get_training_data::{
    bloom::{self, Filter},
    globals::{
        LabelMap, Selector, BLOOM, CLIENT, CONFIG, DB, EXCLUDE_RE, LABEL_MAP, MATCH_RE, URL_QUEUE,
    },
};
use kuchiki::{self, traits::*, NodeRef};
use log::{debug, info, trace, warn};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{self, json};
use std::{collections::BTreeMap, fs, path::PathBuf};
use tokio::{
    self,
    time::{sleep, Duration},
};
use url::Url;

async fn fetch(url: Url) -> Result<Vec<u8>> {
    let resp = CLIENT.get(url).send().await?;
    if resp.status() != StatusCode::OK {
        bail!("Received status code: {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    Ok(bytes.to_vec())
}

fn get_training_input(root: &NodeRef) -> Option<String> {
    let mut has_text_children = false;
    let mut out = String::new();
    for child in root.children() {
        if let Some(el) = child.as_element() {
            let tag_name = &el.name.local;
            if tag_name == "script" || tag_name == "style" || tag_name == "noscript" {
                continue;
            }
            if let Some(t) = get_training_input(&child) {
                out.push_str(&t);
            }
        } else if let Some(text_node) = child.as_text() {
            let text = text_node.borrow();
            let trimmed = text.trim();
            if trimmed.len() > 0 {
                // TODO: The tokenizer might already handle this
                let trimmed = trimmed.replace("\u{a0}", " ");
                out.push_str(&trimmed);
                has_text_children = true;
            }
        }
    }
    if !out.is_empty() {
        let out = if has_text_children {
            if let Some(el) = root.as_element() {
                let tag_name = &el.name.local;
                format!("<{}>{}</{}>", tag_name, out, tag_name)
            } else {
                warn!(
                    "NodeRef: {:?} was not element even though it had direct text children",
                    root
                );
                out
            }
        } else {
            out
        };
        Some(out)
    } else {
        None
    }
}

#[derive(Serialize)]
enum SelectorValue {
    Str(String),
    Arr(Vec<String>),
}

fn get_training_output(page: &NodeRef, url: &Url) -> BTreeMap<String, SelectorValue> {
    let mut path = url.path();
    if path.starts_with("/") {
        path = &path[1..];
    }

    let matches: Vec<_> = LABEL_MAP
        .maps
        .iter()
        .filter(|m| {
            let re = MATCH_RE.get(&m.path_match_re).unwrap();
            re.is_match(path)
        })
        .collect();

    if matches.len() > 1 {
        warn!("Multiple ({}) label maps for url: {:?}", matches.len(), url);
    }

    let mut out = BTreeMap::new();
    for map in matches {
        let map = apply_label_map(page, map);
        out.extend(map);
    }
    out
}

fn apply_label_map(page: &NodeRef, map: &LabelMap) -> BTreeMap<String, SelectorValue> {
    macro_rules! try_selector {
        ($sel:expr, $label:ident) => {
            match $sel {
                Ok(x) => x,
                Err(e) => {
                    warn!(
                        "Failed to apply selector: {} with error: {:?}",
                        $label.selector, e
                    );
                    continue;
                }
            }
        };
    }

    let mut out = BTreeMap::new();
    for label in &map.labels {
        if label.list.is_some() {
            let els: Vec<_> = try_selector!(page.select(&label.selector), label).collect();
            let texts: Vec<_> = els.iter().map(|e| e.text_contents()).collect();
            out.insert(label.name.clone(), SelectorValue::Arr(texts));
        } else {
            let el = try_selector!(page.select_first(&label.selector), label);
            out.insert(label.name.clone(), SelectorValue::Str(el.text_contents()));
        }
    }
    out
}

fn get_links(page: &NodeRef, cur: Url) -> Vec<Url> {
    // TODO: Using a regex to get URLs would (probably) be faster.
    // It doesn't matter though b/c real bottlenecks are disk/applying the label map
    let links: Vec<_> = match page.select("a[href]") {
        Ok(els) => els.collect(),
        Err(_) => vec![],
    };
    let old_len = links.len();
    let links: Vec<_> = links
        .iter()
        .filter_map(|l| {
            let attrs = l.attributes.borrow();
            // Theoretically this should never return None since we already filter by a[href]
            attrs.get("href").and_then(|l| Some(String::from(l)))
        })
        .collect();
    if links.len() != old_len {
        warn!("Links dropped: {} -> {}", old_len, links.len());
    }

    let target_domain = &LABEL_MAP.domain;
    let scheme = cur.scheme();
    // Slower than using a single `filter_map` above but the "Links dropped" check is probably good
    let links: Vec<_> = links
        .iter()
        .filter_map(|l| {
            // remove links to the same page
            if l.starts_with("#") {
                return None;
            };

            let u = if l.starts_with("/") {
                Url::parse(&format!("{}://{}{}", scheme, target_domain, l)).ok()
            } else {
                Url::parse(&l).ok().and_then(|u| {
                    let domain = match u.domain() {
                        Some(d) => d,
                        None => return None,
                    };
                    if domain == target_domain {
                        Some(u)
                    } else {
                        None
                    }
                })
            };
            // EXCLUDE_RE is not a real option (even though we can call option methods on it)
            // so we can't do `u.and(EXCLUDE_RE)`
            let u = match EXCLUDE_RE.as_ref().and(u) {
                Some(u) => {
                    let mut path = u.path();
                    if !path.starts_with("/") {
                        warn!("Path: {} did not start with \"/\"", path);
                    }
                    // chop off the "/"
                    path = &path[1..];
                    let exclude = EXCLUDE_RE.as_ref().unwrap();
                    if exclude.is_match(path) {
                        None
                    } else {
                        Some(u)
                    }
                }
                None => None,
            };
            u
        })
        .collect();
    links
}

// all non-fatal errors bubble up to this function
async fn process(url: Url) -> Result<()> {
    let bytes = fetch(url.clone()).await?;
    // TODO: Is there a way to do this w/o clone?
    let page = String::from_utf8(bytes.clone())?;
    let page = kuchiki::parse_html().one(page);
    let input = get_training_input(&page).ok_or(anyhow!("No training input for: {:?}", url))?;
    let output = get_training_output(&page, &url);

    let json = json!({
        "raw": bytes,
        "input": input,
        "labels": output,
    });

    //Add JSON to the saver

    let links = get_links(&page, url);
    for url in &links {
        add_url(url).await;
    }
    Ok(())
}

async fn worker() -> Result<()> {
    trace!("Running worker...");
    loop {
        // ? operator for fatal errors
        let url = match URL_QUEUE.pop_min()? {
            Some((_, v)) => {
                let bytes = v.to_vec();
                let s = String::from_utf8(bytes)?;
                Url::parse(&s)?
            }
            None => {
                trace!("No work, sleeping...");
                sleep(Duration::from_millis(CONFIG.ms_worker_check)).await;
                continue;
            }
        };

        process(url).await?;
    }
}

async fn add_url(s: &Url) -> Result<()> {
    let bytes = s.as_str().as_bytes();
    let hash = hash64(bytes);
    if !BLOOM.check(hash).await {
        let id = DB.generate_id()?;
        BLOOM.set(hash).await;
        URL_QUEUE.insert(id.to_be_bytes(), bytes)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    if URL_QUEUE.is_empty() {
        let root_urls = &LABEL_MAP.maps.len();
        for map in &LABEL_MAP.maps {
            let url = Url::parse(&map.abs_root_url)?;
            add_url(url).await?;
        }
        if URL_QUEUE.is_empty() {
            bail!("URL queue is empty after adding {} root url(s) from label maps. We are either completely out of URLs or there's a bug.", root_urls);
        }
    }
    trace!("Starting with: {} urls", URL_QUEUE.len());
    // If we don't wait on  the join handles then
    // we can't use async inside the workers b/c the
    // runtime terminates?!
    let mut handles = vec![];
    for _ in 0..CONFIG.workers {
        let handle = tokio::spawn(async move {
            worker().await.unwrap();
        });
        handles.push(handle);
    }

    futures::future::join_all(handles).await;

    Ok(())
}
