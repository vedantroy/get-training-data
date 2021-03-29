// Concurrent bloom fitler w/ WAL
use bincode;
use bloomfilter::Bloom;
use log::{trace, warn};
use std::cell::RefCell;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

pub struct Filter {
    bloom: RwLock<Bloom<u64>>,
    wal: RwLock<File>,
    prev_checkpoint: Mutex<Box<Instant>>,

    checkpoint_secs: u64,
    checkpoint_path: PathBuf,
}

pub struct Config {
    pub dir: PathBuf,
    pub checkpoint_secs: u64,

    pub bytes: usize,
    pub expected_entries: usize,
}

fn checkpoint(checkpoint: &PathBuf, bloom: &Bloom<u64>) {
    trace!("Checkpointing...");
    let temp_path = checkpoint.with_extension("temp");
    let mut temp = File::create(&temp_path).unwrap();
    // Serializing directly into a file is (maybe) slow
    let bytes = bincode::serialize(&bloom).unwrap();
    temp.write(&bytes).unwrap();
    std::fs::rename(temp_path, checkpoint).unwrap();
}

impl Filter {
    pub fn new(c: Config) -> Filter {
        let Config {
            dir,
            checkpoint_secs,
            bytes,
            expected_entries,
        } = c;
        let checkpoint_path = dir.join("checkpoint.bincode");
        let wal_path = dir.join("wal.log");

        let mut bloom: Bloom<u64> = if checkpoint_path.exists() {
            trace!("Loading bloom filter from checkpoint");
            // Deserializing from file is (maybe) slow
            let mut checkpoint = File::open(&checkpoint_path).unwrap();
            let mut bytes = vec![];
            checkpoint.read_to_end(&mut bytes).unwrap();
            bincode::deserialize(&bytes).unwrap()
        } else {
            trace!("Creating new filter");
            std::fs::create_dir_all(dir).unwrap();
            Bloom::new(bytes, expected_entries)
        };
        trace!("Filter loaded");

        if wal_path.exists() {
            let wal = File::open(&wal_path).unwrap();
            let lines: Vec<_> = BufReader::new(wal).lines().collect();
            if lines.len() > 0 {
                trace!("Saving WAL...");
                for url in lines {
                    // https://stackoverflow.com/questions/63358858/rust-chaining-results-combinators
                    let url: Result<u64, _> = url.map_err(|e| e.to_string()).and_then(|u| {
                        if u.len() < 19 || u.len() > 21 {
                            return Err(format!("Unexpected hash length: {}", u.len()));
                        }
                        u.parse()
                            .map_err(|e: std::num::ParseIntError| e.to_string())
                    });
                    let url = match url {
                        Ok(u) => u,
                        Err(e) => {
                            warn!("Failed to deserialize URL in WAL with error: {:?}", e);
                            continue;
                        }
                    };
                    bloom.set(&url);
                }
                checkpoint(&checkpoint_path, &bloom);
            }
        }

        Filter {
            bloom: RwLock::new(bloom),
            wal: RwLock::new(File::create(&wal_path).unwrap()),
            prev_checkpoint: Mutex::new(Box::new(Instant::now())),
            checkpoint_path,
            checkpoint_secs,
        }
    }

    pub async fn set(&self, url_hash: u64) {
        let mut wal = self.wal.write().await;
        let mut bloom = self.bloom.write().await;
        let mut prev_checkpoint = self.prev_checkpoint.lock().await;
        writeln!(wal, "{}", url_hash).unwrap();
        bloom.set(&url_hash);
        if Instant::now() > *prev_checkpoint.as_ref() + Duration::from_secs(self.checkpoint_secs) {
            checkpoint(&self.checkpoint_path, &bloom);
            wal.set_len(0).unwrap();
            *prev_checkpoint = Box::new(Instant::now());
        }
    }

    pub async fn check(&self, url: u64) -> bool {
        let bloom = self.bloom.read().await;
        bloom.check(&url)
    }
}
