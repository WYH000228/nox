/*
 *   MIT License
 *
 *   Copyright (c) 2020 Fluence Labs Limited
 *
 *   Permission is hereby granted, free of charge, to any person obtaining a copy
 *   of this software and associated documentation files (the "Software"), to deal
 *   in the Software without restriction, including without limitation the rights
 *   to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 *   copies of the Software, and to permit persons to whom the Software is
 *   furnished to do so, subject to the following conditions:
 *
 *   The above copyright notice and this permission notice shall be included in all
 *   copies or substantial portions of the Software.
 *
 *   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 *   IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 *   FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 *   AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 *   LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 *   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 *   SOFTWARE.
 */

// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use super::record::*;
use libp2p::kad::kbucket;
use libp2p::kad::record::store::*;
use libp2p::kad::record::{Key, ProviderRecord, Record};
use libp2p::kad::K_VALUE;
use libp2p::PeerId;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{hash_map, hash_set, HashMap, HashSet};
use std::iter;

/// In-memory implementation of a `RecordStore`.
pub struct MemoryStore {
    /// The identity of the peer owning the store.
    local_key: kbucket::Key<PeerId>,
    /// The configuration of the store.
    config: MemoryStoreConfig,
    /// The stored (regular) records.
    records: HashMap<Key, HashSet<Record>>,
    /// The stored provider records.
    providers: HashMap<Key, SmallVec<[ProviderRecord; K_VALUE.get()]>>,
    /// The set of all provider records for the node identified by `local_key`.
    ///
    /// Must be kept in sync with `providers`.
    provided: HashSet<ProviderRecord>,
}

/// Configuration for a `MemoryStore`.
pub struct MemoryStoreConfig {
    /// The maximum number of records.
    pub max_records: usize,
    /// The maximum size of record values, in bytes.
    pub max_value_bytes: usize,
    /// The maximum number of providers stored for a key.
    ///
    /// This should match up with the chosen replication factor.
    pub max_providers_per_key: usize,
    /// The maximum number of provider records for which the
    /// local node is the provider.
    pub max_provided_keys: usize,
}

impl Default for MemoryStoreConfig {
    fn default() -> Self {
        Self {
            max_records: 1024,
            max_value_bytes: 65 * 1024,
            max_provided_keys: 1024,
            max_providers_per_key: K_VALUE.get(),
        }
    }
}

impl MemoryStore {
    /// Creates a new `MemoryRecordStore` with a default configuration.
    pub fn new(local_id: PeerId) -> Self {
        Self::with_config(local_id, Default::default())
    }

    /// Creates a new `MemoryRecordStore` with the given configuration.
    pub fn with_config(local_id: PeerId, config: MemoryStoreConfig) -> Self {
        MemoryStore {
            local_key: kbucket::Key::new(local_id),
            config,
            records: HashMap::default(),
            provided: HashSet::default(),
            providers: HashMap::default(),
        }
    }

    /// Retains the records satisfying a predicate.
    #[allow(dead_code)]
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&Key, &Record) -> bool,
    {
        self.records
            .iter_mut()
            .map(|(k, set)| set.retain(|v| f(k, v)))
            .for_each(drop); // exhaust iterator
    }

    fn reduce_record_set(&self, key: Key, set: HashSet<Record>) -> Record {
        reduce_record_set(key, set)
    }

    fn insert(set: &mut HashSet<Record>, record: Record) -> Result<()> {
        match expand_record_set(record) {
            Ok(r) => match r {
                Ok(records) => set.extend(records),
                Err(e) => {
                    log::error!("Can't parse multiple records from DHT record: {}", e);
                    return Err(Error::MaxRecords); // TODO custom error?
                }
            },
            Err(record) => {
                set.insert(record);
            }
        }

        Ok(())
    }
}

impl<'a> RecordStore<'a> for MemoryStore {
    type RecordsIter = std::vec::IntoIter<Cow<'a, Record>>;

    #[allow(clippy::type_complexity)]
    type ProvidedIter = iter::Map<
        hash_set::Iter<'a, ProviderRecord>,
        fn(&'a ProviderRecord) -> Cow<'a, ProviderRecord>,
    >;

    fn get(&'a self, k: &Key) -> Option<Cow<Record>> {
        // TODO: Performance? Was Cow::Borrowed, now Cow::Owned, and lot's of .clone()'s :(
        self.records
            .get(k)
            .map(|set| self.reduce_record_set(k.clone(), set.clone()))
            .map(Cow::Owned)
    }

    fn put(&'a mut self, r: Record) -> Result<()> {
        if r.value.len() >= self.config.max_value_bytes {
            return Err(Error::ValueTooLarge);
        }

        let num_records = self.records.len();

        match self.records.entry(r.key.clone()) {
            hash_map::Entry::Occupied(mut e) => Self::insert(e.get_mut(), r)?,
            hash_map::Entry::Vacant(e) => {
                if num_records >= self.config.max_records {
                    return Err(Error::MaxRecords);
                }
                let mut set = HashSet::new();
                Self::insert(&mut set, r)?;
                e.insert(set);
            }
        }

        Ok(())
    }

    fn remove(&'a mut self, k: &Key) {
        self.records.remove(k);
    }

    fn records(&'a self) -> Self::RecordsIter {
        let vec = self
            .records
            .iter()
            // .cloned() //::<'a, (Key, HashSet<Record>)> // : (Key, HashSet<Record>)
            .map(|(key, set)| self.reduce_record_set(key.clone(), set.clone()))
            .map(Cow::Owned)
            .collect::<Vec<_>>();

        vec.into_iter()
    }

    fn add_provider(&'a mut self, record: ProviderRecord) -> Result<()> {
        let num_keys = self.providers.len();

        // Obtain the entry
        let providers = match self.providers.entry(record.key.clone()) {
            e @ hash_map::Entry::Occupied(_) => e,
            e @ hash_map::Entry::Vacant(_) => {
                if self.config.max_provided_keys == num_keys {
                    return Err(Error::MaxProvidedKeys);
                }
                e
            }
        }
        .or_insert_with(Default::default);

        if let Some(i) = providers.iter().position(|p| p.provider == record.provider) {
            // In-place update of an existing provider record.
            providers.as_mut()[i] = record;
        } else {
            // It is a new provider record for that key.
            let local_key = self.local_key.clone();
            let key = kbucket::Key::new(record.key.clone());
            let provider = kbucket::Key::new(record.provider.clone());
            if let Some(i) = providers.iter().position(|p| {
                let pk = kbucket::Key::new(p.provider.clone());
                provider.distance(&key) < pk.distance(&key)
            }) {
                // Insert the new provider.
                if local_key.preimage() == &record.provider {
                    self.provided.insert(record.clone());
                }
                providers.insert(i, record);
                // Remove the excess provider, if any.
                if providers.len() > self.config.max_providers_per_key {
                    if let Some(p) = providers.pop() {
                        self.provided.remove(&p);
                    }
                }
            } else if providers.len() < self.config.max_providers_per_key {
                // The distance of the new provider to the key is larger than
                // the distance of any existing provider, but there is still room.
                if local_key.preimage() == &record.provider {
                    self.provided.insert(record.clone());
                }
                providers.push(record);
            }
        }
        Ok(())
    }

    fn providers(&'a self, key: &Key) -> Vec<ProviderRecord> {
        self.providers
            .get(key)
            .map_or_else(Vec::new, |ps| ps.clone().into_vec())
    }

    fn provided(&'a self) -> Self::ProvidedIter {
        self.provided.iter().map(Cow::Borrowed)
    }

    fn remove_provider(&'a mut self, key: &Key, provider: &PeerId) {
        if let hash_map::Entry::Occupied(mut e) = self.providers.entry(key.clone()) {
            let providers = e.get_mut();
            if let Some(i) = providers.iter().position(|p| &p.provider == provider) {
                let p = providers.remove(i);
                self.provided.remove(&p);
            }
            if providers.is_empty() {
                e.remove();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multihash::{wrap, Code, Multihash};
    use quickcheck::*;
    use rand::Rng;
    use std::time::{Duration, Instant};

    #[derive(Clone, Debug)]
    struct NewProviderRecord(ProviderRecord);
    #[derive(Clone, Debug)]
    struct NewRecord(Record);
    #[derive(Clone, Debug)]
    struct NewKey(Key);
    #[derive(Clone, Debug)]
    struct NewKBucketKey(kbucket::Key<PeerId>);

    impl Arbitrary for NewKBucketKey {
        fn arbitrary<G: Gen>(_: &mut G) -> NewKBucketKey {
            NewKBucketKey(kbucket::Key::from(PeerId::random()))
        }
    }

    impl Arbitrary for NewKey {
        fn arbitrary<G: Gen>(_: &mut G) -> NewKey {
            let hash = rand::thread_rng().gen::<[u8; 32]>();
            NewKey(Key::from(wrap(Code::Sha2_256, &hash)))
        }
    }

    impl Arbitrary for NewRecord {
        fn arbitrary<G: Gen>(g: &mut G) -> NewRecord {
            NewRecord(Record {
                key: NewKey::arbitrary(g).0,
                value: Vec::arbitrary(g),
                publisher: if g.gen() {
                    Some(PeerId::random())
                } else {
                    None
                },
                expires: if g.gen() {
                    Some(Instant::now() + Duration::from_secs(g.gen_range(0, 60)))
                } else {
                    None
                },
            })
        }
    }

    impl Arbitrary for NewProviderRecord {
        fn arbitrary<G: Gen>(g: &mut G) -> NewProviderRecord {
            NewProviderRecord(ProviderRecord {
                key: NewKey::arbitrary(g).0,
                provider: PeerId::random(),
                expires: if g.gen() {
                    Some(Instant::now() + Duration::from_secs(g.gen_range(0, 60)))
                } else {
                    None
                },
            })
        }
    }

    fn random_multihash() -> Multihash {
        wrap(Code::Sha2_256, &rand::thread_rng().gen::<[u8; 32]>())
    }

    fn distance(r: &ProviderRecord) -> kbucket::Distance {
        kbucket::Key::new(r.key.clone()).distance(&kbucket::Key::new(r.provider.clone()))
    }

    #[test]
    fn put_get_remove_record() {
        fn prop(r: NewRecord) {
            let r = r.0;
            let mut store = MemoryStore::new(PeerId::random());
            assert!(store.put(r.clone()).is_ok());
            let mut set = HashSet::new();
            set.insert(r.clone());
            let reduced = reduce_record_set(r.key.clone(), set);
            assert_eq!(Some(Cow::Owned(reduced)), store.get(&r.key));
            store.remove(&r.key);
            assert!(store.get(&r.key).is_none());
        }
        quickcheck(prop as fn(_))
    }

    #[test]
    fn add_get_remove_provider() {
        fn prop(r: NewProviderRecord) {
            let r = r.0;
            let mut store = MemoryStore::new(PeerId::random());
            assert!(store.add_provider(r.clone()).is_ok());
            assert!(store.providers(&r.key).contains(&r));
            store.remove_provider(&r.key, &r.provider);
            assert!(!store.providers(&r.key).contains(&r));
        }
        quickcheck(prop as fn(_))
    }

    #[test]
    fn providers_ordered_by_distance_to_key() {
        fn prop(providers: Vec<NewKBucketKey>) -> bool {
            let mut store = MemoryStore::new(PeerId::random());
            let key = Key::from(random_multihash());

            let providers = providers.into_iter().map(|v| v.0).collect::<Vec<_>>();
            let mut records = providers
                .into_iter()
                .map(|p| ProviderRecord::new(key.clone(), p.into_preimage()))
                .collect::<Vec<_>>();

            for r in &records {
                assert!(store.add_provider(r.clone()).is_ok());
            }

            records.sort_by(|r1, r2| distance(r1).cmp(&distance(r2)));
            records.truncate(store.config.max_providers_per_key);

            records == store.providers(&key).to_vec()
        }

        quickcheck(prop as fn(_) -> _)
    }

    #[test]
    fn provided() {
        let id = PeerId::random();
        let mut store = MemoryStore::new(id.clone());
        let key = random_multihash();
        let rec = ProviderRecord::new(key, id.clone());
        assert!(store.add_provider(rec.clone()).is_ok());
        assert_eq!(
            vec![Cow::Borrowed(&rec)],
            store.provided().collect::<Vec<_>>()
        );
        store.remove_provider(&rec.key, &id);
        assert_eq!(store.provided().count(), 0);
    }

    #[test]
    fn update_provider() {
        let mut store = MemoryStore::new(PeerId::random());
        let key = random_multihash();
        let prv = PeerId::random();
        let mut rec = ProviderRecord::new(key, prv);
        assert!(store.add_provider(rec.clone()).is_ok());
        assert_eq!(vec![rec.clone()], store.providers(&rec.key).to_vec());
        rec.expires = Some(Instant::now());
        assert!(store.add_provider(rec.clone()).is_ok());
        assert_eq!(vec![rec.clone()], store.providers(&rec.key).to_vec());
    }

    #[test]
    fn max_provided_keys() {
        let mut store = MemoryStore::new(PeerId::random());
        for _ in 0..store.config.max_provided_keys {
            let key = random_multihash();
            let prv = PeerId::random();
            let rec = ProviderRecord::new(key, prv);
            let _ = store.add_provider(rec);
        }
        let key = random_multihash();
        let prv = PeerId::random();
        let rec = ProviderRecord::new(key, prv);
        match store.add_provider(rec) {
            Err(Error::MaxProvidedKeys) => {}
            _ => panic!("Unexpected result"),
        }
    }

    #[test]
    fn generate_publisher() {
        use unsigned_varint::encode;
        // use libp2p::PeerId;
        // use multihash::{wrap, Code};

        // let mh = b"record_set_stored_here";
        // let wrapped = wrap(Code::Identity, mh.to_vec().as_slice());
        // println!("size {}", mh.len());
        // let _base58 = bs58::encode(wrapped.as_bytes()).into_string();
        let mut buf: [u8; 10] = encode::u64_buffer();
        encode::u64(23, &mut buf);
        println!("{}", bs58::encode(buf).into_string());

        let base58 = "1UMULTPLExxRECRDSxxSTREDxxHERExx".to_string();
        println!("{}", base58);

        let bytes = bs58::decode(base58.clone())
            .into_vec()
            .expect("spanish spanish");
        println!("bytes {:?}", bytes.as_slice());
        let _mhash = Multihash::from_bytes(bytes).expect("inq inq");
        // let mhash_base58 = bs58::encode(mhash.as_bytes()).into_string();
        // println!("{} => {}", base58, mhash_base58);
        //
        // let peer_id: PeerId = base58.as_str().parse().expect("spanish inquisition");
        // println!("peer id {:?} {}", &peer_id, peer_id.to_base58());
    }
}
