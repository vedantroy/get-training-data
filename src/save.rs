use bincode;
use serde::{de::DeserializeOwned, ser::Serialize};
use sled::{self, Db, Tree};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{self, time::Duration};

struct Config {
    check_interval_secs: u64,
    chunk_size: usize,
}

struct Saver<T: Serialize + DeserializeOwned> {
    // This is eventually consistent with the actual queue length
    queue_len: AtomicUsize,
    db: Db,
    queue: Tree,
    save: fn(&Vec<T>),
    config: Config,
}

impl<T: Serialize + DeserializeOwned> Saver<T> {
    fn new(db: Db, save: fn(&Vec<T>), config: Config) -> Saver<T> {
        let queue = db.open_tree("saved_data").unwrap();
        let queue_len = queue.len();
        Saver {
            queue_len: AtomicUsize::new(queue_len),
            queue,
            save,
            db,
            config,
        }
    }

    fn add(&self, xs: Vec<T>) {
        for x in &xs {
            let id = self.db.generate_id().unwrap();
            let bytes = bincode::serialize(x).unwrap();
            self.queue.insert(id.to_be_bytes(), bytes).unwrap();
        }
        self.queue_len.fetch_add(xs.len(), Ordering::Relaxed);
    }

    async fn run(&self) {
        let chunk_size = self.config.chunk_size;
        let mut chunks = 0;
        loop {
            let count = self.queue_len.load(Ordering::Relaxed);
            if count >= chunk_size {
                self.queue_len.fetch_sub(chunk_size, Ordering::Relaxed);
                let mut xs: Vec<T> = vec![];
                for i in 0..chunk_size {
                    match self.queue.pop_min().unwrap() {
                        Some((_, v)) => xs.push(bincode::deserialize(&v).unwrap()),
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

                println!("Saving chunk: {}", chunks);
                chunks += 1;
                (self.save)(&xs);
            } else {
                tokio::time::sleep(Duration::from_secs(self.config.check_interval_secs)).await;
                continue;
            }
        }
    }
}
