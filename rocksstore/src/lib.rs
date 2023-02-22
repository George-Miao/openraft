#[cfg(test)]
mod test;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use async_std::sync::RwLock;
use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use openraft::async_trait::async_trait;
use openraft::storage::LogState;
use openraft::storage::Snapshot;
use openraft::AnyError;
use openraft::AppData;
use openraft::AppDataResponse;
use openraft::EffectiveMembership;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::ErrorSubject;
use openraft::ErrorVerb;
use openraft::HardState;
use openraft::LogId;
use openraft::RaftStorage;
use openraft::SnapshotMeta;
use openraft::StateMachineChanges;
use openraft::StorageError;
use openraft::StorageIOError;
use rocksdb::ColumnFamily;
use rocksdb::ColumnFamilyDescriptor;
use rocksdb::Direction;
use rocksdb::Options;
use rocksdb::DB;
use serde::Deserialize;
use serde::Serialize;

pub type RocksNodeId = u64;

/**
 * Here you will set the types of request that will interact with the raft nodes.
 * For example the `Set` will be used to write data (key and value) to the raft database.
 * The `AddNode` will append a new node to the current existing shared list of nodes.
 * You will want to add any request that can write data in all nodes here.
 */
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RocksRequest {
    Set { key: String, value: String },
}

impl AppData for RocksRequest {}

/**
 * Here you will defined what type of answer you expect from reading the data of a node.
 * In this example it will return a optional value from a given key in
 * the `RocksRequest.Set`.
 *
 * TODO: SHould we explain how to create multiple `AppDataResponse`?
 *
 */
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RocksResponse {
    pub value: Option<String>,
}

impl AppDataResponse for RocksResponse {}

#[derive(Serialize, Deserialize, Debug)]
pub struct RocksSnapshot {
    pub meta: SnapshotMeta,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/**
 * Here defines a state machine of the raft, this state represents a copy of the data
 * between each node. Note that we are using `serde` to serialize the `data`, which has
 * a implementation to be serialized. Note that for this test we set both the key and
 * value as String, but you could set any type of value that has the serialization impl.
 */
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct SerializableRocksStateMachine {
    pub last_applied_log: Option<LogId>,

    pub last_membership: Option<EffectiveMembership>,

    /// Application data.
    pub data: BTreeMap<String, String>,
}

impl From<&RocksStateMachine> for SerializableRocksStateMachine {
    fn from(state: &RocksStateMachine) -> Self {
        let mut data = BTreeMap::new();

        let it = state.db.iterator_cf(state.cf_sm_data(), rocksdb::IteratorMode::Start);

        for item in it {
            let (key, value) = item.expect("invalid kv record");

            let key: &[u8] = &key;
            let value: &[u8] = &value;
            data.insert(
                String::from_utf8(key.to_vec()).expect("invalid key"),
                String::from_utf8(value.to_vec()).expect("invalid data"),
            );
        }
        Self {
            last_applied_log: state.get_last_applied_log().expect("last_applied_log"),
            last_membership: state.get_last_membership().expect("last_membership"),
            data,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RocksStateMachine {
    /// Application data.
    pub db: Arc<rocksdb::DB>,
}

fn sm_r_err<E: Error + 'static>(e: E) -> StorageError {
    StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)).into()
}
fn sm_w_err<E: Error + 'static>(e: E) -> StorageError {
    StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Write, AnyError::new(&e)).into()
}

impl RocksStateMachine {
    fn cf_sm_meta(&self) -> &ColumnFamily {
        self.db.cf_handle("sm_meta").unwrap()
    }

    fn cf_sm_data(&self) -> &ColumnFamily {
        self.db.cf_handle("sm_data").unwrap()
    }

    fn get_last_membership(&self) -> StorageResult<Option<EffectiveMembership>> {
        let x = self.db.get_cf(self.cf_sm_meta(), "last_membership".as_bytes()).map_err(sm_r_err)?;
        if let Some(v) = x {
            let d = serde_json::from_slice(&v).map_err(sm_r_err)?;
            Ok(Some(d))
        } else {
            Ok(None)
        }
    }

    fn set_last_membership(&self, membership: EffectiveMembership) -> StorageResult<()> {
        self.db
            .put_cf(
                self.cf_sm_meta(),
                "last_membership".as_bytes(),
                serde_json::to_vec(&membership).map_err(sm_w_err)?,
            )
            .map_err(sm_w_err)
    }

    fn get_last_applied_log(&self) -> StorageResult<Option<LogId>> {
        self.db
            .get_cf(self.cf_sm_meta(), "last_applied_log".as_bytes())
            .map_err(sm_r_err)
            .and_then(|value| value.map(|v| serde_json::from_slice(&v).map_err(sm_r_err)).transpose())
    }

    fn set_last_applied_log(&self, log_id: LogId) -> StorageResult<()> {
        self.db
            .put_cf(
                self.cf_sm_meta(),
                "last_applied_log".as_bytes(),
                serde_json::to_vec(&log_id).map_err(sm_w_err)?,
            )
            .map_err(sm_w_err)
    }

    fn from_serializable(sm: SerializableRocksStateMachine, db: Arc<rocksdb::DB>) -> StorageResult<Self> {
        let r = Self { db };

        for (key, value) in sm.data {
            r.db.put_cf(r.cf_sm_data(), key.as_bytes(), value.as_bytes()).map_err(sm_w_err)?;
        }

        if let Some(log_id) = sm.last_applied_log {
            r.set_last_applied_log(log_id)?;
        }

        if let Some(m) = sm.last_membership {
            r.set_last_membership(m)?;
        }

        Ok(r)
    }

    fn new(db: Arc<rocksdb::DB>) -> RocksStateMachine {
        Self { db }
    }

    fn insert(&self, key: String, value: String) -> StorageResult<()> {
        self.db
            .put_cf(self.cf_sm_data(), key.as_bytes(), value.as_bytes())
            .map_err(|e| StorageIOError::new(ErrorSubject::Store, ErrorVerb::Write, AnyError::new(&e)).into())
    }

    pub fn get(&self, key: &str) -> StorageResult<Option<String>> {
        let key = key.as_bytes();
        self.db
            .get_cf(self.cf_sm_data(), key)
            .map(|value| value.map(|v| String::from_utf8(v).expect("invalid data")))
            .map_err(|e| StorageIOError::new(ErrorSubject::Store, ErrorVerb::Read, AnyError::new(&e)).into())
    }
}

#[derive(Debug)]
pub struct RocksStore {
    db: Arc<rocksdb::DB>,

    /// The Raft state machine.
    pub state_machine: RwLock<RocksStateMachine>,
}
type StorageResult<T> = Result<T, StorageError>;

/// converts an id to a byte vector for storing in the database.
/// Note that we're using big endian encoding to ensure correct sorting of keys
fn id_to_bin(id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.write_u64::<BigEndian>(id).unwrap();
    buf
}

fn bin_to_id(buf: &[u8]) -> u64 {
    (&buf[0..8]).read_u64::<BigEndian>().unwrap()
}

/// Meta data of a raft-store.
///
/// In raft, except logs and state machine, the store also has to store several piece of metadata.
/// This sub mod defines the key-value pairs of these metadata.
mod meta {
    use openraft::ErrorSubject;
    use openraft::LogId;

    use crate::RocksSnapshot;

    /// Defines metadata key and value
    pub(crate) trait StoreMeta {
        /// The key used to store in rocksdb
        const KEY: &'static str;

        /// The type of the value to store
        type Value: serde::Serialize + serde::de::DeserializeOwned;

        /// The subject this meta belongs to, and will be embedded into the returned storage error.
        fn subject(v: Option<&Self::Value>) -> ErrorSubject;
    }

    pub(crate) struct LastPurged {}
    pub(crate) struct SnapshotIndex {}
    pub(crate) struct HardState {}
    pub(crate) struct Snapshot {}

    impl StoreMeta for LastPurged {
        const KEY: &'static str = "last_purged_log_id";
        type Value = LogId;

        fn subject(_v: Option<&Self::Value>) -> ErrorSubject {
            ErrorSubject::Store
        }
    }
    impl StoreMeta for SnapshotIndex {
        const KEY: &'static str = "snapshot_index";
        type Value = u64;

        fn subject(_v: Option<&Self::Value>) -> ErrorSubject {
            ErrorSubject::Store
        }
    }
    impl StoreMeta for HardState {
        const KEY: &'static str = "hard_state";
        type Value = openraft::HardState;

        fn subject(_v: Option<&Self::Value>) -> ErrorSubject {
            ErrorSubject::HardState
        }
    }
    impl StoreMeta for Snapshot {
        const KEY: &'static str = "snapshot";
        type Value = RocksSnapshot;

        fn subject(v: Option<&Self::Value>) -> ErrorSubject {
            ErrorSubject::Snapshot(v.unwrap().meta.clone())
        }
    }
}

impl RocksStore {
    fn cf_meta(&self) -> &ColumnFamily {
        self.db.cf_handle("meta").unwrap()
    }
    fn cf_logs(&self) -> &ColumnFamily {
        self.db.cf_handle("logs").unwrap()
    }

    /// Get a store metadata.
    ///
    /// It returns `None` if the store does not have such a metadata stored.
    fn get_meta<M: meta::StoreMeta>(&self) -> Result<Option<M::Value>, StorageError> {
        let v = self
            .db
            .get_cf(self.cf_meta(), M::KEY)
            .map_err(|e| StorageIOError::new(M::subject(None), ErrorVerb::Read, AnyError::new(&e)))?;

        let t = match v {
            None => None,
            Some(bytes) => Some(
                serde_json::from_slice(&bytes)
                    .map_err(|e| StorageIOError::new(M::subject(None), ErrorVerb::Read, AnyError::new(&e)))?,
            ),
        };
        Ok(t)
    }

    /// Save a store metadata.
    fn put_meta<M: meta::StoreMeta>(&self, value: &M::Value) -> Result<(), StorageError> {
        let json_value = serde_json::to_vec(value)
            .map_err(|e| StorageIOError::new(M::subject(Some(value)), ErrorVerb::Write, AnyError::new(&e)))?;

        self.db
            .put_cf(self.cf_meta(), M::KEY, json_value)
            .map_err(|e| StorageIOError::new(M::subject(Some(value)), ErrorVerb::Write, AnyError::new(&e)))?;

        Ok(())
    }
}

#[async_trait]
impl RaftStorage<RocksRequest, RocksResponse> for Arc<RocksStore> {
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_hard_state(&self, vote: &HardState) -> Result<(), StorageError> {
        self.put_meta::<meta::HardState>(vote)
    }

    async fn read_hard_state(&self) -> Result<Option<HardState>, StorageError> {
        self.get_meta::<meta::HardState>()
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(&self, entries: &[&Entry<RocksRequest>]) -> StorageResult<()> {
        for entry in entries {
            let id = id_to_bin(entry.log_id.index);
            assert_eq!(bin_to_id(&id), entry.log_id.index);
            self.db
                .put_cf(
                    self.cf_logs(),
                    id,
                    serde_json::to_vec(entry)
                        .map_err(|e| StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Write, AnyError::new(&e)))?,
                )
                .map_err(|e| StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Write, AnyError::new(&e)))?;
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn delete_conflict_logs_since(&self, log_id: LogId) -> StorageResult<()> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        let from = id_to_bin(log_id.index);
        let to = id_to_bin(0xff_ff_ff_ff_ff_ff_ff_ff);
        self.db
            .delete_range_cf(self.cf_logs(), &from, &to)
            .map_err(|e| StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Write, AnyError::new(&e)).into())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge_logs_upto(&self, log_id: LogId) -> Result<(), StorageError> {
        tracing::debug!("delete_log: [0, {:?}]", log_id);

        self.put_meta::<meta::LastPurged>(&log_id)?;

        let from = id_to_bin(0);
        let to = id_to_bin(log_id.index + 1);
        self.db
            .delete_range_cf(self.cf_logs(), &from, &to)
            .map_err(|e| StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Write, AnyError::new(&e)).into())
    }

    async fn get_log_state(&self) -> StorageResult<LogState> {
        let last = self.db.iterator_cf(self.cf_logs(), rocksdb::IteratorMode::End).next();

        let last_log_id = match last {
            None => None,
            Some(res) => {
                let (_log_index, entry_bytes) = res.map_err(read_logs_err)?;
                let ent = serde_json::from_slice::<Entry<RocksRequest>>(&entry_bytes).map_err(read_logs_err)?;
                Some(ent.log_id)
            }
        };

        let last_purged_log_id = self.get_meta::<meta::LastPurged>()?;

        let last_log_id = match last_log_id {
            None => last_purged_log_id,
            Some(x) => Some(x),
        };

        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RB,
    ) -> StorageResult<Vec<Entry<RocksRequest>>> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(x) => id_to_bin(*x),
            std::ops::Bound::Excluded(x) => id_to_bin(*x + 1),
            std::ops::Bound::Unbounded => id_to_bin(0),
        };

        let mut res = Vec::new();

        let it = self.db.iterator_cf(self.cf_logs(), rocksdb::IteratorMode::From(&start, Direction::Forward));
        for item_res in it {
            let (id, val) = item_res.map_err(read_logs_err)?;

            let id = bin_to_id(&id);
            if !range.contains(&id) {
                break;
            }

            let entry: Entry<_> = serde_json::from_slice(&val).map_err(read_logs_err)?;

            assert_eq!(id, entry.log_id.index);

            res.push(entry);
        }
        Ok(res)
    }

    async fn last_applied_state(&self) -> Result<(Option<LogId>, Option<EffectiveMembership>), StorageError> {
        let state_machine = self.state_machine.read().await;
        Ok((
            state_machine.get_last_applied_log()?,
            state_machine.get_last_membership()?,
        ))
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply_to_state_machine(
        &self,
        entries: &[&Entry<RocksRequest>],
    ) -> Result<Vec<RocksResponse>, StorageError> {
        let mut res = Vec::with_capacity(entries.len());

        let sm = self.state_machine.write().await;

        for entry in entries {
            tracing::debug!(%entry.log_id, "replicate to sm");

            sm.set_last_applied_log(entry.log_id)?;

            match entry.payload {
                EntryPayload::Blank => res.push(RocksResponse { value: None }),
                EntryPayload::Normal(ref req) => match req {
                    RocksRequest::Set { key, value } => {
                        sm.insert(key.clone(), value.clone())?;
                        res.push(RocksResponse {
                            value: Some(value.clone()),
                        })
                    }
                },
                EntryPayload::Membership(ref mem) => {
                    sm.set_last_membership(EffectiveMembership::new(entry.log_id, mem.clone()))?;
                    res.push(RocksResponse { value: None })
                }
            };
        }
        self.db
            .flush_wal(true)
            .map_err(|e| StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Write, AnyError::new(&e)))?;
        Ok(res)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn build_snapshot(&self) -> Result<Snapshot<Cursor<Vec<u8>>>, StorageError> {
        let data;
        let last_applied_log;

        {
            // Serialize the data of the state machine.
            let state_machine = SerializableRocksStateMachine::from(&*self.state_machine.read().await);
            data = serde_json::to_vec(&state_machine)
                .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)))?;

            last_applied_log = state_machine.last_applied_log;
        }

        // TODO: we probably want this to be atomic.
        let snapshot_idx: u64 = self.get_meta::<meta::SnapshotIndex>()?.unwrap_or_default() + 1;
        self.put_meta::<meta::SnapshotIndex>(&snapshot_idx)?;

        let snapshot_id = if let Some(last) = last_applied_log {
            format!("{}-{}-{}", last.term, last.index, snapshot_idx)
        } else {
            format!("--{}", snapshot_idx)
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            snapshot_id,
        };

        let snapshot = RocksSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };

        self.put_meta::<meta::Snapshot>(&snapshot)?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn begin_receiving_snapshot(&self) -> Result<Box<Self::SnapshotData>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn install_snapshot(
        &self,
        meta: &SnapshotMeta,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<StateMachineChanges, StorageError> {
        tracing::info!(
            { snapshot_size = snapshot.get_ref().len() },
            "decoding snapshot for installation"
        );

        let new_snapshot = RocksSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        // Update the state machine.
        {
            let updated_state_machine: SerializableRocksStateMachine = serde_json::from_slice(&new_snapshot.data)
                .map_err(|e| {
                    StorageIOError::new(
                        ErrorSubject::Snapshot(new_snapshot.meta.clone()),
                        ErrorVerb::Read,
                        AnyError::new(&e),
                    )
                })?;
            let mut state_machine = self.state_machine.write().await;
            *state_machine = RocksStateMachine::from_serializable(updated_state_machine, self.db.clone())?;
        }

        self.put_meta::<meta::Snapshot>(&new_snapshot)?;

        Ok(StateMachineChanges {
            last_applied: meta.last_log_id,
            is_snapshot: true,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_current_snapshot(&self) -> Result<Option<Snapshot<Self::SnapshotData>>, StorageError> {
        let curr_snap = self.get_meta::<meta::Snapshot>()?;

        match curr_snap {
            Some(snapshot) => {
                let data = snapshot.data.clone();
                Ok(Some(Snapshot {
                    meta: snapshot.meta,
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
            None => Ok(None),
        }
    }
}

impl RocksStore {
    pub async fn new<P: AsRef<Path>>(db_path: P) -> Arc<RocksStore> {
        let mut db_opts = Options::default();
        db_opts.create_missing_column_families(true);
        db_opts.create_if_missing(true);

        let meta = ColumnFamilyDescriptor::new("meta", Options::default());
        let sm_meta = ColumnFamilyDescriptor::new("sm_meta", Options::default());
        let sm_data = ColumnFamilyDescriptor::new("sm_data", Options::default());
        let logs = ColumnFamilyDescriptor::new("logs", Options::default());

        let db = DB::open_cf_descriptors(&db_opts, db_path, vec![meta, sm_meta, sm_data, logs]).unwrap();

        let db = Arc::new(db);
        let state_machine = RwLock::new(RocksStateMachine::new(db.clone()));
        Arc::new(RocksStore { db, state_machine })
    }
}

fn read_logs_err(e: impl Error + 'static) -> StorageError {
    StorageError::IO {
        source: StorageIOError::new(ErrorSubject::Logs, ErrorVerb::Read, AnyError::new(&e)),
    }
}
