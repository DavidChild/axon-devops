use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use rand::{rngs::SmallRng, Rng, SeedableRng};
use rocksdb::{Options, WriteBatch, DB};

use common_apm::metrics::storage::{on_storage_get_state, on_storage_put_state};
use protocol::types::Bytes;
use protocol::{Display, From, ProtocolError, ProtocolErrorKind, ProtocolResult};

// 49999 is the largest prime number within 50000.
const RAND_SEED: u64 = 49999;

pub struct RocksTrieDB {
    db:         Arc<DB>,
    cache:      DashMap<Vec<u8>, Vec<u8>>,
    cache_size: usize,
}

impl RocksTrieDB {
    pub fn new<P: AsRef<Path>>(
        path: P,
        max_open_files: i32,
        cache_size: usize,
    ) -> ProtocolResult<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_max_open_files(max_open_files);

        let db = DB::open(&opts, path).map_err(RocksTrieDBError::from)?;

        // Init HashMap with capacity 2 * cache_size to avoid reallocate memory.
        Ok(RocksTrieDB {
            db: Arc::new(db),
            cache: DashMap::with_capacity(cache_size + cache_size),
            cache_size,
        })
    }

    fn inner_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RocksTrieDBError> {
        let res = self.cache.get(key);

        if res.is_none() {
            let inst = Instant::now();
            let ret = self.db.get(key).map_err(to_store_err)?;
            on_storage_get_state(inst.elapsed(), 1.0);

            if let Some(val) = ret.clone() {
                self.cache.insert(key.to_owned(), val);
            }

            return Ok(ret);
        }

        Ok(Some(res.unwrap().clone()))
    }
}

impl cita_trie::DB for RocksTrieDB {
    type Error = RocksTrieDBError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        self.inner_get(key)
    }

    fn contains(&self, key: &[u8]) -> Result<bool, Self::Error> {
        let res = self.cache.contains_key(key);

        if res {
            Ok(true)
        } else {
            if let Some(val) = self.db.get(key).map_err(to_store_err)? {
                self.cache.insert(key.to_owned(), val);
                return Ok(true);
            }
            Ok(false)
        }
    }

    fn insert(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Self::Error> {
        let inst = Instant::now();
        let size = key.len() + value.len();

        {
            self.cache.insert(key.clone(), value.clone());
        }

        self.db
            .put(Bytes::from(key), Bytes::from(value))
            .map_err(to_store_err)?;

        on_storage_put_state(inst.elapsed(), size as f64);
        Ok(())
    }

    fn insert_batch(&self, keys: Vec<Vec<u8>>, values: Vec<Vec<u8>>) -> Result<(), Self::Error> {
        if keys.len() != values.len() {
            return Err(RocksTrieDBError::BatchLengthMismatch);
        }

        let mut total_size = 0;
        let mut batch = WriteBatch::default();

        {
            for (key, val) in keys.iter().zip(values.iter()) {
                total_size += key.len();
                total_size += val.len();
                batch.put(key, val);
                self.cache.insert(key.clone(), val.clone());
            }
        }

        let inst = Instant::now();
        self.db.write(batch).map_err(to_store_err)?;
        on_storage_put_state(inst.elapsed(), total_size as f64);
        Ok(())
    }

    fn remove(&self, _key: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn remove_batch(&self, _keys: &[Vec<u8>]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn flush(&self) -> Result<(), Self::Error> {
        let len = self.cache.len();

        if len <= self.cache_size {
            return Ok(());
        }

        let keys = self
            .cache
            .iter()
            .map(|kv| kv.key().clone())
            .collect::<Vec<_>>();
        let remove_list = rand_remove_list(keys, len - self.cache_size);

        for item in remove_list.iter() {
            self.cache.remove(item);
        }
        Ok(())
    }
}

fn rand_remove_list<T: Clone>(keys: Vec<T>, num: usize) -> Vec<T> {
    let mut len = keys.len() - 1;
    let mut idx_list = (0..len).collect::<Vec<_>>();
    let mut rng = SmallRng::seed_from_u64(RAND_SEED);
    let mut ret = Vec::new();

    for _ in 0..num {
        let tmp = rng.gen_range(0..len);
        let idx = idx_list.remove(tmp);
        ret.push(keys[idx].to_owned());
        len -= 1;
    }
    ret
}

#[derive(Debug, Display, From)]
pub enum RocksTrieDBError {
    #[display(fmt = "store error")]
    Store,

    #[display(fmt = "rocksdb {}", _0)]
    RocksDB(rocksdb::Error),

    #[display(fmt = "parameters do not match")]
    InsertParameter,

    #[display(fmt = "batch length dont match")]
    BatchLengthMismatch,
}

impl std::error::Error for RocksTrieDBError {}

impl From<RocksTrieDBError> for ProtocolError {
    fn from(err: RocksTrieDBError) -> ProtocolError {
        ProtocolError::new(ProtocolErrorKind::Executor, Box::new(err))
    }
}

fn to_store_err(e: rocksdb::Error) -> RocksTrieDBError {
    log::error!("[executor] trie db {:?}", e);
    RocksTrieDBError::Store
}

#[cfg(test)]
mod tests {
    extern crate test;
    use test::Bencher;

    use super::*;

    #[bench]
    fn bench_rand(b: &mut Bencher) {
        b.iter(|| {
            let mut rng = SmallRng::seed_from_u64(RAND_SEED);
            for _ in 0..10000 {
                rng.gen_range(10..1000000);
            }
        })
    }

    #[test]
    fn test_rand_remove() {
        let list = (0..10).collect::<Vec<_>>();
        let keys = list.iter().collect::<Vec<_>>();

        for num in 1..10 {
            let res = rand_remove_list(keys.clone(), num);
            assert_eq!(res.len(), num);
        }
    }
}