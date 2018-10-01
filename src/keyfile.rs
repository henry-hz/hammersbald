//
// Copyright 2018 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # The key file
//! Specific implementation details to key file
//!

use logfile::LogFile;
use datafile::{DataFile, Content};
use page::{Page, KeyPage, PageFile, PAGE_SIZE};
use error::BCDBError;
use types::Offset;
use cache::Cache;

use rand::{thread_rng, RngCore};
use siphasher::sip::SipHasher;

use std::sync::{Mutex, Arc, Condvar};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::hash::Hasher;
use std::collections::HashMap;

const FIRST_PAGE_HEAD:u64 = 28;
const INIT_BUCKETS: u64 = 512;
const INIT_LOGMOD :u64 = 8;
const FIRST_BUCKETS_PER_PAGE:u64 = 677;
const BUCKETS_PER_PAGE:u64 = 681;
const BUCKET_SIZE: u64 = 6;

/// The key file
pub struct KeyFile {
    async_file: KeyPageFile,
    step: u32,
    buckets: u32,
    log_mod: u32,
    sip0: u64,
    sip1: u64
}

impl KeyFile {
    pub fn new(rw: Box<PageFile>, log_file: Arc<Mutex<LogFile>>) -> Result<KeyFile, BCDBError> {
        let mut rng = thread_rng();
        Ok(KeyFile{async_file: KeyPageFile::new(rw, log_file)?, step: 0,
            buckets: INIT_BUCKETS as u32, log_mod: INIT_LOGMOD as u32,
        sip0: rng.next_u64(), sip1: rng.next_u64() })
    }

    pub fn init (&mut self) -> Result<(), BCDBError> {
        if let Some(first_page) = self.read_page(Offset::from(0))? {
            let buckets = first_page.read_offset(0).unwrap().as_u64() as u32;
            if buckets > 0 {
                self.buckets = buckets;
                self.step = first_page.read_offset(6).unwrap().as_u64() as u32;
                self.log_mod = (32 - buckets.leading_zeros()) as u32 - 1;
                self.sip0 = first_page.read_u64(12).unwrap();
                self.sip1 = first_page.read_u64(20).unwrap();
            }
            info!("open BCDB. buckets {}, step {}, log_mod {}", buckets, self.step, self.log_mod);
        }
        else {
            let mut fp = KeyPage::new(Offset::from(0));
            fp.write_offset(0, Offset::from(self.buckets as u64))?;
            fp.write_offset(6, Offset::from(self.step as u64))?;
            fp.write_u64(12, self.sip0)?;
            fp.write_u64(20, self.sip1)?;
            self.write_page(fp)?;
            info!("open empty BCDB");
        };

        Ok(())
    }

    pub fn put (&mut self, keys: Vec<Vec<u8>>, offset: Offset, bucket_file: &mut DataFile) -> Result<(), BCDBError>{
        for key in keys {
            let hash = self.hash(key.as_slice());
            let mut bucket = hash & (!0u32 >> (32 - self.log_mod)); // hash % 2^(log_mod)
            if bucket < self.step {
                bucket = hash & (!0u32 >> (32 - self.log_mod - 1)); // hash % 2^(log_mod + 1)
            }

            self.store_to_bucket(bucket, vec![(hash, vec![offset])], bucket_file)?;
        }

        if self.step < <u32>::max_value() {

            if self.step < (1 << self.log_mod) {
                let step = self.step;
                self.rehash_bucket(step, bucket_file)?;
            }

            self.step += 1;
            if self.step == (1 << (self.log_mod + 1)) {
                self.log_mod += 1;
                self.step = 0;
            }

            self.buckets += 1;
            if self.buckets as u64 >= FIRST_BUCKETS_PER_PAGE && (self.buckets as u64 - FIRST_BUCKETS_PER_PAGE) % BUCKETS_PER_PAGE == 0 {
                let page = KeyPage::new(Offset::from(((self.buckets as u64 - FIRST_BUCKETS_PER_PAGE)/ BUCKETS_PER_PAGE + 1) * PAGE_SIZE as u64));
                self.write_page(page)?;
            }

            if let Some(mut first_page) = self.read_page(Offset::from(0))? {
                first_page.write_offset(0, Offset::from(self.buckets as u64))?;
                first_page.write_offset(6, Offset::from(self.step as u64))?;
                self.write_page(first_page)?;
            }
        }

        Ok(())
    }

    fn rehash_bucket(&mut self, bucket: u32, bucket_file: &mut DataFile) -> Result<(), BCDBError> {
        let bucket_offset = Self::bucket_offset(bucket);

        if let Some(mut bucket_page) = self.read_page(bucket_offset.this_page())? {
            let mut spill_offset = bucket_page.read_offset(bucket_offset.in_page_pos())?;

            let mut current_spillovers = Vec::new();
            let mut rewrite = false;
            loop {
                if !spill_offset.is_valid() {
                    break;
                }
                match bucket_file.get_content(spill_offset)? {
                    Some(Content::Spillover(spills, next)) => {
                        current_spillovers.push(spills);
                        spill_offset = next;
                        if spill_offset.is_valid() {
                            rewrite = true;
                        }
                    },
                    _ => return Err(BCDBError::Corrupted(format!("expected spillover at rehash {}", spill_offset).to_string()))
                }
            }
            // process in reverse order to ensure latest put takes precedence in the new bucket
            current_spillovers.reverse();

            let mut remaining_spillovers = HashMap::new();
            for spill in current_spillovers {
                for s in spill.iter() {
                    let hash = s.0;
                    let mut same_hash_spill = s.1.clone();
                    let new_bucket = hash & (!0u32 >> (32 - self.log_mod - 1)); // hash % 2^(log_mod + 1)
                    if new_bucket != bucket {
                        // store to new bucket
                        self.store_to_bucket(new_bucket, vec![(hash, same_hash_spill)], bucket_file)?;
                        if let Some(bp) = self.read_page(bucket_offset.this_page())? {
                            bucket_page = bp;
                        }
                        rewrite = true;
                    } else {
                        // merge spillovers of same hash
                        remaining_spillovers.entry(hash).or_insert(Vec::new()).extend(
                            same_hash_spill.iter());
                    }
                }
            }

            if rewrite {
                let mut next = Offset::invalid();
                let spills: Vec<(u32, Vec<Offset>)> =
                    remaining_spillovers.iter().map(|(h, s)| {
                        let mut offsets = s.clone();
                        offsets.sort_unstable();
                        offsets.reverse();
                        (*h, offsets)
                    }).collect();
                for spill in spills.chunks(255).rev() {
                    let so = bucket_file.append_content(Content::Spillover(spill.to_vec(), next))?;
                    next = so;
                }
                bucket_page.write_offset(bucket_offset.in_page_pos(), next)?;
                self.write_page(bucket_page)?;
            }
        }
        else {
            return Err(BCDBError::Corrupted(format!("missing link page {}", bucket_offset)));
        }
        Ok(())
    }

    pub fn dedup(&mut self, key: &[u8], bucket_file: &mut DataFile, data_file: &DataFile) -> Result<(), BCDBError> {
        let hash = self.hash(key);
        let mut bucket = hash & (!0u32 >> (32 - self.log_mod)); // hash % 2^(log_mod)
        if bucket < self.step {
            bucket = hash & (!0u32 >> (32 - self.log_mod - 1)); // hash % 2^(log_mod + 1)
        }

        let bucket_offset = Self::bucket_offset(bucket);
        if let Some(mut bucket_page) = self.read_page(bucket_offset.this_page())? {
            let mut spill_offset = bucket_page.read_offset(bucket_offset.in_page_pos())?;

            let mut remaining_spillovers = HashMap::new();
            let mut original_spillovers = Vec::new();
            loop {
                if !spill_offset.is_valid() {
                    break;
                }
                match bucket_file.get_content(spill_offset)? {
                    Some(Content::Spillover(spills, next)) => {
                        for s in &spills {
                            let h = s.0;
                            let mut spills = s.1.clone();
                            // merge spillovers of same hash
                            remaining_spillovers.entry(h).or_insert(Vec::new()).extend(spills.iter());
                        }
                        original_spillovers.push(spills);
                        spill_offset = next;
                    },
                    _ => return Err(BCDBError::Corrupted("unknown content at dedup".to_string()))
                };
            }

            {
                let ds = remaining_spillovers.entry(hash).or_insert(Vec::new());
                ds.dedup_by_key(|offset| {
                    if let Ok(Some(Content::Data(k, _))) = data_file.get_content(*offset) {
                        return k;
                    }
                    Vec::new()
                });
            }
            let mut rewrite = false;
            for v in original_spillovers {
                for (hash, s) in v {
                    if let Some(sl) = remaining_spillovers.get(&hash) {
                        if *sl != s {
                            rewrite = true;
                        }
                    } else {
                        rewrite = true;
                    }
                }
            }

            if rewrite {
                let mut next = Offset::invalid();
                let spills: Vec<(u32, Vec<Offset>)> = remaining_spillovers.iter().map(|(h, s)| (*h, s.clone())).collect();
                for spill in spills.chunks(255).rev() {
                    if !spill.is_empty() {
                        let so = bucket_file.append_content(Content::Spillover(spill.to_vec(), next))?;
                        next = so;
                    }
                }
                bucket_page.write_offset(bucket_offset.in_page_pos(), next)?;
            }
            self.write_page(bucket_page)?;
        }
        else {
            return Err(BCDBError::Corrupted(format!("missing link page {}", bucket_offset)));
        }
        Ok(())
    }

    fn store_to_bucket(&mut self, bucket: u32, spill: Vec<(u32, Vec<Offset>)>, bucket_file: &mut DataFile) -> Result<(), BCDBError> {
        let bucket_offset = Self::bucket_offset(bucket);
        if let Some(mut bucket_page) = self.read_page(bucket_offset.this_page())? {
            let spill_offset = bucket_page.read_offset(bucket_offset.in_page_pos())?;
            // prepend spillover chain
            // this logically overwrites previous key association in the spillover chain
            // since search stops at first key match
            let so = bucket_file.append_content(Content::Spillover(spill, spill_offset))?;
            bucket_page.write_offset(bucket_offset.in_page_pos(), so)?;
            self.write_page(bucket_page)?;
        }
        else {
            return Err(BCDBError::Corrupted(format!("missing link page {}", bucket_offset)));
        }
        Ok(())
    }

    pub fn get_unique (&self, key: &[u8], data_file: &DataFile, bucket_file: &DataFile) -> Result<Option<Vec<u8>>, BCDBError> {
        let hash = self.hash(key);
        let mut bucket = hash & (!0u32 >> (32 - self.log_mod)); // hash % 2^(log_mod)
        if bucket < self.step {
            bucket = hash & (!0u32 >> (32 - self.log_mod - 1)); // hash % 2^(log_mod + 1)
        }
        let bucket_offset = Self::bucket_offset(bucket);
        if let Some (bucket_page) = self.read_page(bucket_offset.this_page())? {
            let mut spill_offset = bucket_page.read_offset(bucket_offset.in_page_pos())?;
            loop {
                if !spill_offset.is_valid() {
                    return Ok(None);
                }
                match bucket_file.get_content(spill_offset)? {
                    Some(Content::Spillover(spills, next)) => {
                        for s in spills {
                            let h = s.0;
                            let offsets = s.1;
                            if h == hash {
                                for current in offsets {
                                    match data_file.get_content(current)? {
                                        Some(Content::Data(data_key, data)) => {
                                            for k in data_key {
                                                if k == key {
                                                    return Ok(Some(data));
                                                }
                                            }
                                        },
                                        _ => return Err(BCDBError::Corrupted("spillover should point to data".to_string()))
                                    }
                                }
                            }
                        }
                        spill_offset = next;
                    },
                    _ => return Err(BCDBError::Corrupted("unexpected content".to_string()))
                }
            }
        }
        Ok(None)
    }

    pub fn get (&self, key: &[u8], data_file: &DataFile, bucket_file: &DataFile) -> Result<Vec<Offset>, BCDBError> {
        let hash = self.hash(key);
        let mut bucket = hash & (!0u32 >> (32 - self.log_mod)); // hash % 2^(log_mod)
        if bucket < self.step {
            bucket = hash & (!0u32 >> (32 - self.log_mod - 1)); // hash % 2^(log_mod + 1)
        }
        let bucket_offset = Self::bucket_offset(bucket);
        if let Some(bucket_page) = self.read_page(bucket_offset.this_page())? {
            let mut spill_offset = bucket_page.read_offset(bucket_offset.in_page_pos())?;
            let mut result = Vec::new();
            loop {
                if !spill_offset.is_valid() {
                    return Ok(result);
                }
                match bucket_file.get_content(spill_offset)? {
                    Some(Content::Spillover(spills, next)) => {
                        for s in spills {
                            let h = s.0;
                            let offsets = s.1;
                            if h == hash {
                                for current in offsets {
                                    match data_file.get_content(current)? {
                                        Some(Content::Data(data_key, _)) => {
                                            if data_key.iter().any(|k| k.as_slice() == key) {
                                                result.push(current);
                                            }
                                        },
                                        _ => return Err(BCDBError::Corrupted("spillover should point to data".to_string()))
                                    }
                                }
                            }
                        }
                        spill_offset = next;
                    },
                    _ => return Err(BCDBError::Corrupted("unexpected content".to_string()))
                }
            }
        }
        else {
            return Err(BCDBError::Corrupted(format!("missing link page {}", bucket_offset)));
        }
    }

    pub fn patch_page(&mut self, page: KeyPage) -> Result<(), BCDBError> {
        self.async_file.patch_page(page)
    }

    pub fn clear_cache(&mut self, len: u64) {
        self.async_file.clear_cache(len);
    }

    pub fn log_file (&self) -> Arc<Mutex<LogFile>> {
        self.async_file.log_file()
    }

    pub fn shutdown (&mut self) {
        self.async_file.shutdown()
    }

    fn bucket_offset (bucket: u32) -> Offset {
        if (bucket as u64) < FIRST_BUCKETS_PER_PAGE {
            Offset::from((bucket as u64 / FIRST_BUCKETS_PER_PAGE) * PAGE_SIZE as u64
                + (bucket as u64 % FIRST_BUCKETS_PER_PAGE) * BUCKET_SIZE + FIRST_PAGE_HEAD)
        }
        else {
            Offset::from((((bucket as u64 - FIRST_BUCKETS_PER_PAGE) / BUCKETS_PER_PAGE) + 1) * PAGE_SIZE as u64
                + (bucket as u64 % BUCKETS_PER_PAGE) * BUCKET_SIZE)
        }
    }

    fn hash (&self, key: &[u8]) -> u32 {
        let mut hasher = SipHasher::new_with_keys(self.sip0, self.sip1);
        hasher.write(key);
        hasher.finish() as u32
    }
}

impl KeyFile {
    pub fn flush(&mut self) -> Result<(), BCDBError> {
        self.async_file.flush()
    }

    pub fn len(&self) -> Result<u64, BCDBError> {
        self.async_file.len()
    }

    pub fn truncate(&mut self, len: u64) -> Result<(), BCDBError> {
        self.async_file.truncate(len)
    }

    pub fn sync(&self) -> Result<(), BCDBError> {
        self.async_file.sync()
    }

    fn read_page(&self, offset: Offset) -> Result<Option<KeyPage>, BCDBError> {
        if let Some(page) = self.async_file.read_page(offset)? {
            let key_page = KeyPage::from(page);
            if key_page.offset.as_u64() != offset.as_u64() {
                return Err(BCDBError::Corrupted(format!("hash table page {} does not have the offset of its position", offset)));
            }
            return Ok(Some(key_page));
        }
        Ok(None)
    }

    fn write_page(&mut self, page: KeyPage) -> Result<(), BCDBError> {
        self.async_file.write_page(page.offset, page.page)
    }
}

struct KeyPageFile {
    inner: Arc<KeyPageFileInner>
}

struct KeyPageFileInner {
    file: Mutex<Box<PageFile>>,
    log: Arc<Mutex<LogFile>>,
    cache: Mutex<Cache>,
    flushed: Condvar,
    work: Condvar,
    run: AtomicBool,
    flushing: AtomicBool
}

impl KeyPageFileInner {
    pub fn new (file: Box<PageFile>, log: Arc<Mutex<LogFile>>) -> Result<KeyPageFileInner, BCDBError> {
        let len = file.len()?;
        Ok(KeyPageFileInner { file: Mutex::new(file), log,
            cache: Mutex::new(Cache::new(len)), flushed: Condvar::new(),
            work: Condvar::new(),
            run: AtomicBool::new(true),
            flushing: AtomicBool::new(false)
        })
    }
}

impl KeyPageFile {
    pub fn new (file: Box<PageFile>, log: Arc<Mutex<LogFile>>) -> Result<KeyPageFile, BCDBError> {
        let inner = Arc::new(KeyPageFileInner::new(file, log)?);
        let inner2 = inner.clone();
        thread::spawn(move || { KeyPageFile::background(inner2) });
        Ok(KeyPageFile { inner })
    }

    fn background (inner: Arc<KeyPageFileInner>) {
        while inner.run.load(Ordering::Relaxed) {
            let mut writes = Vec::new();
            {
                let mut cache = inner.cache.lock().expect("cache lock poisoned");
                if cache.is_empty() {
                    inner.flushed.notify_all();
                }
                cache = inner.work.wait(cache).expect("cache lock poisoned while waiting for work");
                if cache.new_writes > 1000 || inner.flushing.swap(false, Ordering::Relaxed) {
                    writes = cache.move_writes_to_wrote();
                }
            }
            if !writes.is_empty() {
                writes.sort_unstable_by(|a, b| u64::cmp(&a.0.as_u64(), &b.0.as_u64()));
                let mut file = inner.file.lock().expect("file lock poisoned");
                let mut log = inner.log.lock().expect("log lock poisoned");
                let mut logged = false;
                for write in &writes {
                    let offset = write.0;
                    if offset.as_u64() < log.tbl_len && !log.is_logged(offset) {
                        if let Some(page) = file.read_page(offset).expect("can not read hash table file") {
                            log.append_page(page).expect("can not write log file");
                            logged = true;
                        }
                        else {
                            panic!("can not find pre-image to log {}", offset);
                        }
                    }
                }
                if logged {
                    log.flush().expect("can not flush log");
                    log.sync().expect("can not sync log");
                }
                for (offset, page) in writes {
                    file.write_page(offset, page.clone()).expect("write hash table failed");
                }
            }
        }
    }

    pub fn patch_page(&mut self, page: KeyPage) -> Result<(), BCDBError> {
        self.inner.file.lock().unwrap().write_page(page.offset, page.page)
    }

    fn read_page_from_store (&self, offset: Offset) -> Result<Option<Page>, BCDBError> {
        if offset != offset.this_page () {
            return Err(BCDBError::Corrupted(format!("data or link read is not page aligned {}", offset)))
        }
        self.inner.file.lock().unwrap().read_page(offset)
    }

    pub fn log_file (&self) -> Arc<Mutex<LogFile>> {
        self.inner.log.clone()
    }

    pub fn shutdown (&mut self) {
        self.inner.run.store(false, Ordering::Relaxed);
        self.inner.work.notify_one();
    }

    pub fn clear_cache(&mut self, len: u64) {
        self.inner.cache.lock().unwrap().clear(len);
    }
}

impl PageFile for KeyPageFile {
    #[allow(unused_assignments)]
    fn flush(&mut self) -> Result<(), BCDBError> {
        let mut cache = self.inner.cache.lock().unwrap();
        if !cache.is_empty() {
            self.inner.flushing.store(true, Ordering::Relaxed);
            self.inner.work.notify_one();
            cache = self.inner.flushed.wait(cache)?;
        }
        self.inner.file.lock().unwrap().flush()
    }

    fn len(&self) -> Result<u64, BCDBError> {
        self.inner.file.lock().unwrap().len()
    }

    fn truncate(&mut self, new_len: u64) -> Result<(), BCDBError> {
        self.inner.file.lock().unwrap().truncate(new_len)
    }

    fn sync(&self) -> Result<(), BCDBError> {
        self.inner.file.lock().unwrap().sync()
    }

    fn read_page(&self, offset: Offset) -> Result<Option<Page>, BCDBError> {
        {
            let cache = self.inner.cache.lock().unwrap();
            if let Some(page) = cache.get(offset) {
                return Ok(Some(page));
            }
        }
        if let Some(page) = self.read_page_from_store(offset)? {
            // write cache takes precedence therefore no problem if there was
            // a write between above read and this lock
            let mut cache = self.inner.cache.lock().unwrap();
            cache.cache(offset, page.clone());
            return Ok(Some(page));
        }
        Ok(None)
    }

    fn append_page(&mut self, _: Page) -> Result<(), BCDBError> {
        unimplemented!()
    }

    fn write_page(&mut self, offset: Offset, page: Page) -> Result<(), BCDBError> {
        self.inner.cache.lock().unwrap().write(offset, page);
        self.inner.work.notify_one();
        Ok(())

    }
}
