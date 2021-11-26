use std::{
    array::TryFromSliceError,
    borrow::Borrow,
    fs,
    io::Error as IoError,
    path::{Path, PathBuf},
    sync::Arc,
};

use bincode::config::{BigEndian, DefaultOptions, Options as _, WithOtherEndian};
use derive_more::{AsRef, Deref};
use lazy_static::lazy_static;
use log::*;
use rlp::{Decodable, Encodable};
use rocksdb::{
    backup::{BackupEngine, BackupEngineOptions, RestoreOptions},
    ColumnFamily, ColumnFamilyDescriptor, Env, Options, DB,
};
use serde::{de::DeserializeOwned, Serialize};
use tempfile::TempDir;

use crate::{
    transactions::{Transaction, TransactionReceipt},
    types::*,
};
use triedb::{empty_trie_hash, rocksdb::RocksMemoryTrieMut, FixedSecureTrieMut};

pub mod inspectors;
pub mod walker;

pub type Result<T> = std::result::Result<T, Error>;
pub use rocksdb; // avoid mess with dependencies for another crates

type BincodeOpts = WithOtherEndian<DefaultOptions, BigEndian>;
lazy_static! {
    static ref CODER: BincodeOpts = DefaultOptions::new().with_big_endian();
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] rocksdb::Error),
    #[error("Type {1} :: {0}")]
    Bincode(bincode::Error, &'static str),
    #[error("Unable to construct key from bytes")]
    Key(#[from] TryFromSliceError),
    #[error("Internal IO error: {0:?}")]
    Internal(#[from] IoError),
}

const BACKUP_SUBDIR: &str = "backup";
const CUSTOM_LOCATION: &str = "tmp_inner_space";

/// Marker-like wrapper for cleaning temporary directory.
/// Temporary directory is only used in tests.
#[derive(Clone, Debug)]
enum Location {
    Temporary(Arc<TempDir>),
    Persisent(PathBuf),
}
impl Eq for Location {}
impl PartialEq for Location {
    fn eq(&self, other: &Location) -> bool {
        match (self, other) {
            (Location::Persisent(p1), Location::Persisent(p2)) => p1 == p2,
            (Location::Temporary(p1), Location::Temporary(p2)) => p1.path() == p2.path(),
            _ => false,
        }
    }
}

impl AsRef<Path> for Location {
    fn as_ref(&self) -> &Path {
        match self {
            Self::Temporary(temp_dir) => temp_dir.as_ref().path(),
            Self::Persisent(path) => path.as_ref(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Storage {
    pub(crate) db: Arc<DbWithClose>,
    // Location should be second field, because of drop order in Rust.
    location: Location,
    gc_enabled: bool,
}

impl Storage {
    pub fn open_persistent<P: AsRef<Path>>(path: P, gc_enabled: bool) -> Result<Self> {
        Self::open(Location::Persisent(path.as_ref().to_owned()), gc_enabled)
    }

    pub fn create_temporary() -> Result<Self> {
        Self::open(Location::Temporary(Arc::new(TempDir::new()?)), false)
    }

    // without gc_enabled
    fn open(location: Location, gc_enabled: bool) -> Result<Self> {
        let db_opts = default_db_opts()?;

        // TODO: if gc_enabled remove deprecated columns, and add gc column
        let descriptors: &[_] = if gc_enabled {
            &[
                Codes::COLUMN_NAME,
                ReferenceCounter::COLUMN_NAME,
                // Make sure to reflect changes in `merge_from_db`
            ]
        } else {
            &[
                Codes::COLUMN_NAME,
                Transactions::COLUMN_NAME,
                Receipts::COLUMN_NAME,
                TransactionHashesPerBlock::COLUMN_NAME,
                // Make sure to reflect changes in `merge_from_db`
            ]
        };

        let descriptors = descriptors
            .iter()
            .map(|column| ColumnFamilyDescriptor::new(*column, db_opts.clone()));

        let db = DB::open_cf_descriptors(&db_opts, &location, descriptors)?;

        Ok(Self {
            db: Arc::new(DbWithClose(db)),
            location,
            gc_enabled,
        })
    }

    pub fn backup(&self, backup_dir: Option<PathBuf>) -> Result<PathBuf> {
        let backup_dir = backup_dir.unwrap_or_else(|| self.location.as_ref().join(BACKUP_SUBDIR));
        info!("EVM Backup storage data into {}", backup_dir.display());

        let mut engine = BackupEngine::open(&BackupEngineOptions::default(), &backup_dir)?;
        if engine.get_backup_info().len() > HARD_BACKUPS_COUNT {
            // TODO: measure
            engine.purge_old_backups(HARD_BACKUPS_COUNT)?;
        }
        engine.create_new_backup_flush(self.db.as_ref(), true)?;
        Ok(backup_dir)
    }

    pub fn restore_from(path: impl AsRef<Path>, target: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let target = target.as_ref();

        // TODO: ensure target dir is empty or doesn't exists at all
        fs::create_dir_all(target).expect("Unable to create target dir");

        assert!(
            path.is_dir() && path.exists(),
            "Storage can be loaded only from existing directory"
        );
        assert!(
            target.is_dir(),
            "Loaded storage data must lays in target dir"
        );

        info!(
            "Loading storage data from {} into {} (restore from backup)",
            path.display(),
            target.display()
        );
        let mut engine = BackupEngine::open(&BackupEngineOptions::default(), path)?;
        engine.restore_from_latest_backup(&target, &target, &RestoreOptions::default())?;

        Ok(())
    }

    /// Temporary solution to check if anything was purged from bd.
    pub fn check_root_exist(&self, root: H256) -> bool {
        if root == empty_trie_hash() {
            true // empty root should exist always
        } else {
            // only return true if root is retrivable
            matches!(self.db.get(root.as_ref()), Ok(Some(_)))
        }
    }

    pub fn typed_for<K: AsRef<[u8]>, V: Encodable + Decodable>(
        &self,
        root: H256,
    ) -> FixedSecureTrieMut<RocksMemoryTrieMut<&DB>, K, V> {
        FixedSecureTrieMut::new(RocksMemoryTrieMut::new(self.db.as_ref(), root))
    }

    // Returns evm state subdirectory that can be used temporary used by extern users.
    pub fn get_inner_location(&self) -> PathBuf {
        self.location.as_ref().join(CUSTOM_LOCATION)
    }

    pub fn db(&self) -> &DB {
        (*self.db).borrow()
    }

    pub fn flush_changes(&self, state_root: H256, state_updates: crate::ChangedState) -> H256 {
        use triedb::{
            gc::TrieCollection,
            rocksdb::{RocksDatabaseHandle, RocksHandle},
        };
        let r = RocksHandle::new(RocksDatabaseHandle::new(self.db()));

        let mut storage_tries = TrieCollection::new(r.clone(), crate::StaticEntries::default());
        let mut account_tries = TrieCollection::new(r, crate::StaticEntries::default());

        let mut accounts =
            FixedSecureTrieMut::<_, H160, Account>::new(account_tries.trie_for(state_root));

        for (address, (state, storages)) in state_updates {
            if let Maybe::Just(AccountState {
                nonce,
                balance,
                code,
            }) = state
            {
                let mut account = accounts.get(&address).unwrap_or_default();

                account.nonce = nonce;
                account.balance = balance;

                if !code.is_empty() {
                    let code_hash = code.hash();
                    self.set::<Codes>(code_hash, code);
                    account.code_hash = code_hash;
                }

                let mut storage = FixedSecureTrieMut::<_, H256, U256>::new(
                    storage_tries.trie_for(account.storage_root),
                );

                for (index, value) in storages {
                    if value != H256::default() {
                        let value = U256::from_big_endian(&value[..]);
                        storage.insert(&index, &value);
                    } else {
                        storage.delete(&index);
                    }
                }

                let storage_patch = storage.to_trie().into_patch();
                let storage_root = storage_tries.apply(storage_patch);
                account.storage_root = storage_root;
                self.gc_register_root_link(storage_root)
                    .expect("Unable to increment reference counter.");

                accounts.insert(&address, &account);
            } else {
                accounts.delete(&address);
            }
        }

        let accounts_patch = accounts.to_trie().into_patch();
        account_tries.apply(accounts_patch)
    }

    pub fn merge_from_db(&self, other_db: &Self) -> Result<()> {
        assert!(!self.gc_enabled, "Cannot merge to db with rc counters");
        assert!(
            !other_db.gc_enabled,
            "Cannot merge from db with rc counters"
        );
        info!("Iterating over storage");
        for (k, v) in other_db.db.full_iterator(rocksdb::IteratorMode::Start) {
            self.db.put(&k, &v)?
        }
        info!("Iterating over codes");
        // copy all codes
        let cf = self.cf::<Codes>();
        for (k, v) in other_db
            .db
            .full_iterator_cf(cf, rocksdb::IteratorMode::Start)
        {
            self.db.put_cf(cf, &k, &v)?
        }
        // TODO: make it possible to merge reference counters?
        Ok(())
    }

    /// Our garbage collection counts only references of child objects.
    /// Because root_hash has no parents it should be handled separately.
    ///
    /// Increment root_link reference counter.
    ///
    /// This operation is used in two cases:
    /// 1. When our root tree is modified, and new root_hash is produced.
    /// 2. When we insert new account_state object - this object contain reference to storage_root, which should be registered too.
    pub(crate) fn gc_register_root_link(&self, root_link: H256) -> Result<()> {
        if !self.gc_enabled {
            return Ok(());
        }
        // merge
        todo!()
    }

    pub fn gc_count(&self, link: H256) -> Result<u64> {
        if !self.gc_enabled {
            return Ok(0);
        }
        todo!()
    }

    /// Start gc cleanup routine.
    /// - Checks if reference has 0 references in GC.
    /// - Find all child nodes, and decrement their reference counter.
    /// - Collect all child nodes with reference counter == 0.
    /// - Remove reference object.
    /// - Return Array of child objectswith reference counter == 0.
    ///
    /// Note: This method could be called in background and uses atomic transaction.
    /// If any of objects it's references is changed (for example any link is incremented) it will return error and revert.
    pub fn gc_remove_references(&self, reference: &[H256]) -> Result<Vec<H256>> {
        if !self.gc_enabled {
            return Ok(vec![]);
        }
        todo!()
    }

    // Because solana handle each bank independently.
    // We also inherit this behaviour.
    /// Mark slot as removed, also find root_hash that correspond to this bank, and decrement its counter.
    /// Return root_hash if it counter == 0 after removing
    pub fn purge_slot(&self, slot: u64) -> Result<Option<H256>> {
        if !self.gc_enabled {
            return Ok(None);
        }
        // let root = self.take_root_by_slot(slot)?;// remove by slot
        // self.gc_remove_root(root)? // decrement reference counter, in workst case some roots would be with invalid counter.
        todo!()
    }

    // Save info. slot -> root_hash
    // Increment root_hash references counter.
    pub fn register_slot(&self, slot: u64, root: H256) -> Result<()> {
        if !self.gc_enabled {
            return Ok(());
        }
        self.gc_register_root_link(root)?; // increment reference counter
                                           // self.insert_root_by_slot(slot,root); // insert by root
        todo!()
    }
}

impl Borrow<DB> for Storage {
    fn borrow(&self) -> &DB {
        self.db()
    }
}

#[derive(Debug, AsRef, Deref)]
// Hack to close rocksdb background threads. And flush database.
pub struct DbWithClose(DB);

impl Drop for DbWithClose {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            error!("Error during rocksdb flush: {:?}", e);
        }
        self.cancel_all_background_work(true);
    }
}

pub trait SubStorage {
    const COLUMN_NAME: &'static str;
    type Key: Encodable + Decodable;
    type Value: Serialize + DeserializeOwned;
}

pub enum ReferenceCounter {}
impl SubStorage for ReferenceCounter {
    const COLUMN_NAME: &'static str = "reference_counter";
    type Key = H256;
    type Value = u64;
}
pub enum Codes {}
impl SubStorage for Codes {
    const COLUMN_NAME: &'static str = "codes";
    type Key = H256;
    type Value = Code;
}

pub enum Transactions {}
impl SubStorage for Transactions {
    const COLUMN_NAME: &'static str = "transactions";
    type Key = H256;
    type Value = Transaction;
}

pub enum Receipts {}
impl SubStorage for Receipts {
    const COLUMN_NAME: &'static str = "receipts";
    type Key = H256;
    type Value = TransactionReceipt;
}

pub enum TransactionHashesPerBlock {}
impl SubStorage for TransactionHashesPerBlock {
    const COLUMN_NAME: &'static str = "transactions_per_block";
    type Key = BlockNum;
    type Value = Vec<H256>;
}

impl Storage {
    pub fn get<S: SubStorage>(&self, key: S::Key) -> Option<S::Value> {
        let cf = self.cf::<S>();
        let key_bytes = rlp::encode(&key);

        self.db
            .get_pinned_cf(cf, key_bytes)
            .expect("Error on reading mapped column")
            .map(|slice| {
                CODER
                    .deserialize(slice.as_ref())
                    .expect("Unable to decode value")
            })
    }

    pub fn set<S: SubStorage>(&self, key: S::Key, value: S::Value) {
        let cf = self.cf::<S>();
        let key_bytes = rlp::encode(&key);
        let value_bytes = CODER.serialize(&value).expect("Unable to serialize value");
        self.db
            .put_cf(cf, key_bytes, value_bytes)
            .expect("Error when put value into database");
    }

    pub fn cf<S: SubStorage>(&self) -> &ColumnFamily {
        self.db
            .cf_handle(S::COLUMN_NAME)
            .unwrap_or_else(|| panic!("Column Family descriptor {} not found", S::COLUMN_NAME))
    }
}

// hard limit of backups count
const HARD_BACKUPS_COUNT: usize = 1; // TODO: tweak it

// #[macro_export]
// macro_rules! persistent_types {
//     ($($Marker:ident in $Column:expr => $Key:ty : $Value:ty,)+) => {
//         const COLUMN_NAMES: &[&'static str] = &[$($Column),+];

//         $(
//             #[derive(Debug)]
//             pub(crate) enum $Marker {}
//             impl PersistentAssoc for $Marker {
//                 const COLUMN_NAME: &'static str = $Column;
//                 type Key = $Key;
//                 type Value = $Value;
//             }
//         )+
//     };
//     ($($Marker:ident in $Column:expr => $Key:ty : $Value:ty),+) => {
//         persistent_types! { $($Marker in $Column => $Key : $Value,)+ }
//     }
// }

pub fn default_db_opts() -> Result<Options> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    let mut env = Env::default()?;
    env.join_all_threads();
    opts.set_env(&env);
    Ok(opts)
}

pub mod cleaner {
    use super::inspectors::memorizer;
    use std::borrow::Borrow;

    use primitive_types::H256;

    use anyhow::{anyhow, Result};
    use log::*;

    use super::{Codes, SubStorage};

    pub struct Cleaner<DB, T> {
        db: DB,
        trie_nodes: T,
        accounts: memorizer::AccountStorageRootsCollector,
    }

    impl<DB, T> Cleaner<DB, T>
    where
        T: AsRef<memorizer::TrieCollector>,
    {
        pub fn new_with(
            db: DB,
            trie_nodes: T,
            accounts: memorizer::AccountStorageRootsCollector,
        ) -> Self {
            Self {
                db,
                trie_nodes,
                accounts,
            }
        }

        pub fn cleanup(self) -> Result<()>
        where
            DB: Borrow<rocksdb::DB>,
        {
            let db = self.db.borrow();

            let trie_nodes = self.trie_nodes.as_ref();
            // Cleanup unused trie keys in default column family
            {
                let mut batch = rocksdb::WriteBatch::default();

                for (key, _data) in db.iterator(rocksdb::IteratorMode::Start) {
                    let key =
                        <H256 as super::inspectors::encoding::TryFromSlice>::try_from_slice(&key)?;
                    if trie_nodes.trie_keys.contains(&key) {
                        continue; // skip this key
                    } else {
                        batch.delete(key);
                    }
                }

                let batch_size = batch.len();
                db.write(batch)?;
                info!("{} keys was removed", batch_size);
            }

            // Cleanup unused Account Code keys
            {
                let column_name = Codes::COLUMN_NAME;
                let codes_cf = db
                    .cf_handle(column_name)
                    .ok_or_else(|| anyhow!("Codes Column Family '{}' not found", column_name))?;
                let mut batch = rocksdb::WriteBatch::default();

                for (key, _data) in db.iterator_cf(codes_cf, rocksdb::IteratorMode::Start) {
                    let code_hash = rlp::decode(&key)?; // NOTE: keep in sync with ::storage mod
                    if self.accounts.code_hashes.contains(&code_hash) {
                        continue; // skip this key
                    } else {
                        batch.delete_cf(codes_cf, key);
                    }
                }

                let batch_size = batch.len();
                db.write(batch)?;
                info!("{} code keys was removed", batch_size);
            }

            Ok(())
        }
    }
}