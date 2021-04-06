use crate::globals::DB;
use anyhow::Result;
use bincode;
use log::{info, trace};
use serde::{de::DeserializeOwned, ser::Serialize};
use sled::{self, Tree};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{self, time::Duration};

// The saver uses Bincode for its internal storage
// This prevents untagged types from being used
// Src: https://github.com/serde-rs/serde/issues/1310
// 2 options:
// 1. make the Saver only accept bytes (removes ability to pass structs into Saver
// & change serialization format in the lambda)
// 2. (hack, chosen) pre-serialize to JSON & pass in a string

pub struct Config {
    pub check_interval_secs: u64,
    pub chunk_size: usize,
    pub start_chunk: usize,
}

pub struct Saver<T: Serialize + DeserializeOwned> {
    // This is eventually consistent with the actual queue length
    queue_len: AtomicUsize,
    //db: Db,
    queue: Tree,
    // TODO: This should return a future since everything is async anyway
    // but that is hard
    save: fn(usize, &Vec<T>) -> Result<()>,
    config: Config,
}

impl<T: Serialize + DeserializeOwned> Saver<T> {
    pub fn new(
        /*db: Db,*/ save: fn(usize, &Vec<T>) -> Result<()>,
        config: Config,
    ) -> Saver<T> {
        let queue = DB.open_tree("saved_data").unwrap();
        let queue_len = queue.len();
        Saver {
            queue_len: AtomicUsize::new(queue_len),
            queue,
            save,
            config,
        }
    }

    pub fn add(&self, x: T) {
        let id = DB.generate_id().unwrap();
        let bytes = bincode::serialize(&x).unwrap();
        self.queue.insert(id.to_be_bytes(), bytes).unwrap();
        self.queue_len.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn run(&self) -> Result<()> {
        let chunk_size = self.config.chunk_size;
        let mut chunks = self.config.start_chunk;
        loop {
            let count = self.queue_len.load(Ordering::Relaxed);
            if count >= chunk_size {
                self.queue_len.fetch_sub(chunk_size, Ordering::Relaxed);
                let mut xs: Vec<T> = vec![];
                for i in 0..chunk_size {
                    match self.queue.pop_min()? {
                        Some((_, v)) => xs.push(bincode::deserialize(&v)?),
                        None => {
                            // We could just ignore this & break from the loop, but this indicates
                            // a bug in our code
                            panic!(
                                "Tried to load chunk of size: {}, but only found: {} elements",
                                chunk_size, i
                            );
                        }
                    };
                }

                (self.save)(chunks, &xs)?;
                chunks += 1;
            } else {
                info!("Nothing found to save, sleeping...");
                tokio::time::sleep(Duration::from_secs(self.config.check_interval_secs)).await;
                continue;
            }
        }
    }
}
