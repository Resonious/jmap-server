use std::collections::HashMap;

use jmap_mail::import::JMAPMailImport;
use jmap_mail::mailbox::JMAPMailMailbox;

use store::batch::WriteBatch;
use store::log::{Entry, LogIndex, RaftId, TermId};
use store::roaring::RoaringBitmap;
use store::serialize::{
    DeserializeBigEndian, LogKey, StoreDeserialize, StoreSerialize, LAST_APPLIED_INDEX_KEY,
};
use store::tracing::{debug, error};
use store::{
    bincode, lz4_flex, AccountId, Collection, ColumnFamily, Direction, DocumentId, JMAPStore,
    Store, StoreError,
};
use store::{Collections, WriteOperation};
use tokio::sync::{mpsc, oneshot};

use crate::cluster::log::{AppendEntriesResponse, DocumentUpdate};
use crate::JMAPServer;

use super::log::{AppendEntriesRequest, Event, MergedChanges, RaftStore, Update};

use super::{
    rpc::{self, Response},
    Cluster,
};
use super::{PeerId, IPC_CHANNEL_BUFFER};

#[derive(Debug)]
enum State {
    Synchronize,
    AppendEntries {
        first_id: RaftId,
        last_id: RaftId,
        changed_accounts: HashMap<AccountId, Collections>,
    },
    AppendChanges {
        first_id: RaftId,
        last_id: RaftId,
        changed_accounts: Vec<(AccountId, Collections)>,
    },
    Rollback {
        account_id: AccountId,
        collection: Collection,
        changes: MergedChanges,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Synchronize
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum PendingUpdate {
    UpdateDocument {
        account_id: AccountId,
        document_id: DocumentId,
        update: DocumentUpdate,
    },
    DeleteDocuments {
        account_id: AccountId,
        collection: Collection,
        document_ids: Vec<DocumentId>,
    },
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PendingUpdates {
    updates: Vec<PendingUpdate>,
}

impl PendingUpdates {
    pub fn new(updates: Vec<PendingUpdate>) -> Self {
        Self { updates }
    }
}

impl StoreSerialize for PendingUpdates {
    fn serialize(&self) -> Option<Vec<u8>> {
        bincode::serialize(self).ok()
    }
}

impl StoreDeserialize for PendingUpdates {
    fn deserialize(bytes: &[u8]) -> Option<Self> {
        bincode::deserialize(bytes).ok()
    }
}

impl<T> Cluster<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn spawn_raft_follower(&self) -> mpsc::Sender<Event> {
        let (tx, mut rx) = mpsc::channel::<Event>(IPC_CHANNEL_BUFFER);
        let core = self.core.clone();
        let local_name = self.addr.to_string();

        debug!("[{}] Starting raft follower process.", local_name);

        tokio::spawn(async move {
            let mut change_seq = 0;

            if let Err(err) = core.reset_uncommitted_changes().await {
                error!("Failed to reset uncommitted changes: {:?}", err);
                return;
            }

            if let Err(err) = core.init_last_applied_index().await {
                error!("Failed to set last update key: {:?}", err);
                return;
            }

            let mut state = match core.next_rollback_change().await {
                Ok(Some((account_id, collection, changes))) => State::Rollback {
                    account_id,
                    collection,
                    changes,
                },
                Ok(None) => State::default(),
                Err(err) => {
                    error!("Failed to obtain rollback changes: {:?}", err);
                    return;
                }
            };

            while let Some(event) = rx.recv().await {
                //println!("Follower: {:?}", state);

                let response = match (event.request, state) {
                    (AppendEntriesRequest::Match { last_log }, State::Synchronize) => {
                        if let Some(response) = core.handle_match_log(last_log).await {
                            state = State::Synchronize;
                            response
                        } else {
                            break;
                        }
                    }

                    (AppendEntriesRequest::Synchronize { match_terms }, State::Synchronize) => {
                        if let Some(response) = core.handle_synchronize_log(match_terms).await {
                            state = State::Synchronize;
                            response
                        } else {
                            break;
                        }
                    }

                    (AppendEntriesRequest::Merge { matched_log }, State::Synchronize) => {
                        if let Some((next_state, response)) =
                            core.handle_merge_log(matched_log).await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }
                    (
                        AppendEntriesRequest::Update {
                            commit_index,
                            updates,
                        },
                        State::Synchronize,
                    ) => {
                        core.set_up_to_date(false);

                        debug!(
                            "[{}] Received {} log entries with commit index {} (sync state).",
                            local_name,
                            updates.len(),
                            commit_index,
                        );

                        if let Some((next_state, response)) = core
                            .handle_update_log(
                                RaftId::none(),
                                RaftId::none(),
                                commit_index,
                                HashMap::new(),
                                &mut change_seq,
                                updates,
                            )
                            .await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }

                    (
                        AppendEntriesRequest::Update {
                            commit_index,
                            updates,
                        },
                        State::AppendEntries {
                            first_id,
                            last_id,
                            changed_accounts,
                        },
                    ) => {
                        debug!(
                            concat!(
                                "[{}] Received {} log entries with commit index {}: ",
                                "first id {:?}, last id: {:?}, {} pending accounts."
                            ),
                            local_name,
                            updates.len(),
                            commit_index,
                            first_id,
                            last_id,
                            changed_accounts.len()
                        );

                        core.set_up_to_date(false);

                        if let Some((next_state, response)) = core
                            .handle_update_log(
                                first_id,
                                last_id,
                                commit_index,
                                changed_accounts,
                                &mut change_seq,
                                updates,
                            )
                            .await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }

                    (
                        AppendEntriesRequest::Update {
                            commit_index,
                            updates,
                        },
                        State::AppendChanges {
                            first_id,
                            last_id,
                            changed_accounts,
                        },
                    ) => {
                        debug!(
                            concat!(
                                "[{}] Received {} changes with commit index {}: ",
                                "first id {:?}, last id: {:?}, {} pending accounts."
                            ),
                            local_name,
                            updates.len(),
                            commit_index,
                            first_id,
                            last_id,
                            changed_accounts.len()
                        );

                        if let Some((next_state, response)) = core
                            .handle_pending_updates(
                                first_id,
                                last_id,
                                commit_index,
                                &mut change_seq,
                                changed_accounts,
                                updates,
                            )
                            .await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }

                    (
                        AppendEntriesRequest::Update { updates, .. },
                        State::Rollback {
                            account_id,
                            collection,
                            changes,
                        },
                    ) => {
                        debug!(
                            concat!(
                                "[{}] Received {} rollback entries for account {}, ",
                                "collection {:?}."
                            ),
                            local_name,
                            updates.len(),
                            account_id,
                            collection
                        );

                        if let Some((next_state, response)) = core
                            .handle_rollback_updates(account_id, collection, changes, updates)
                            .await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }

                    (
                        _,
                        State::Rollback {
                            account_id,
                            collection,
                            changes,
                        },
                    ) => {
                        debug!(
                            concat!(
                                "[{}] Resuming rollback for account {}, ",
                                "collection {:?}."
                            ),
                            local_name, account_id, collection
                        );

                        // Resume rollback process when a new leader is elected.
                        if let Some((next_state, response)) = core
                            .handle_rollback_updates(account_id, collection, changes, vec![])
                            .await
                        {
                            state = next_state;
                            response
                        } else {
                            break;
                        }
                    }
                    (_, _) => {
                        unreachable!("Invalid state.");
                    }
                };

                event
                    .response_tx
                    .send(response)
                    .unwrap_or_else(|_| error!("Oneshot response channel closed."));
            }

            debug!("[{}] Raft follower process ended.", local_name);
        });
        tx
    }

    pub async fn handle_become_follower(
        &mut self,
        peer_id: PeerId,
        response_tx: oneshot::Sender<rpc::Response>,
        term: TermId,
        last_log: RaftId,
    ) -> store::Result<()> {
        if self.is_known_peer(peer_id) {
            if self.term < term {
                self.term = term;
            }

            if self.term == term && self.log_is_behind_or_eq(last_log.term, last_log.index) {
                self.follow_leader(peer_id)
                    .await?
                    .send(Event {
                        response_tx,
                        request: AppendEntriesRequest::Match { last_log },
                    })
                    .await
                    .unwrap_or_else(|err| error!("Failed to send event: {}", err));
            } else {
                response_tx
                    .send(Response::StepDown { term: self.term })
                    .unwrap_or_else(|_| error!("Oneshot response channel closed."));
            }
        } else {
            response_tx
                .send(rpc::Response::UnregisteredPeer)
                .unwrap_or_else(|_| error!("Oneshot response channel closed."));
        }
        Ok(())
    }

    pub async fn handle_append_entries(
        &mut self,
        peer_id: PeerId,
        response_tx: oneshot::Sender<rpc::Response>,
        term: TermId,
        request: AppendEntriesRequest,
    ) {
        if term > self.term {
            self.term = term;
        }

        match self.is_following_peer(peer_id) {
            Some(tx) => {
                tx.send(Event {
                    response_tx,
                    request,
                })
                .await
                .unwrap_or_else(|err| error!("Failed to send event: {}", err));
            }
            _ => response_tx
                .send(rpc::Response::StepDown { term: self.term })
                .unwrap_or_else(|_| error!("Oneshot response channel closed.")),
        }
    }
}

impl<T> JMAPServer<T>
where
    T: for<'x> Store<'x> + 'static,
{
    async fn handle_update_log(
        &self,
        mut first_id: RaftId,
        mut last_id: RaftId,
        leader_commit_index: LogIndex,
        mut changed_accounts: HashMap<AccountId, Collections>,
        change_seq: &mut u64,
        updates: Vec<Update>,
    ) -> Option<(State, Response)> {
        let store = self.store.clone();
        match self
            .spawn_worker(move || {
                let mut log_batch = Vec::with_capacity(updates.len());
                let mut is_done = updates.is_empty();

                for update in updates {
                    match update {
                        Update::Change {
                            account_id,
                            collection,
                            change,
                        } => {
                            #[cfg(test)]
                            {
                                assert!(!last_id.is_none());
                                assert!(
                                    store
                                        .db
                                        .get::<Vec<u8>>(
                                            ColumnFamily::Logs,
                                            &LogKey::serialize_change(
                                                account_id,
                                                collection,
                                                last_id.index
                                            )
                                        )
                                        .unwrap()
                                        .is_none(),
                                    "{:?}",
                                    store
                                        .db
                                        .get::<Vec<u8>>(
                                            ColumnFamily::Logs,
                                            &LogKey::serialize_change(
                                                account_id,
                                                collection,
                                                last_id.index
                                            )
                                        )
                                        .unwrap()
                                );
                            }

                            log_batch.push(WriteOperation::set(
                                ColumnFamily::Logs,
                                LogKey::serialize_change(account_id, collection, last_id.index),
                                change,
                            ));
                            changed_accounts
                                .entry(account_id)
                                .or_insert_with(Collections::default)
                                .insert(collection);
                        }
                        Update::Log { raft_id, log } => {
                            if first_id.is_none() {
                                first_id = raft_id;
                            }

                            #[cfg(test)]
                            {
                                assert!(
                                    store
                                        .db
                                        .get::<log::Entry>(
                                            ColumnFamily::Logs,
                                            &LogKey::serialize_raft(&raft_id)
                                        )
                                        .unwrap()
                                        .is_none(),
                                    "{:?}",
                                    store
                                        .db
                                        .get::<log::Entry>(
                                            ColumnFamily::Logs,
                                            &LogKey::serialize_raft(&raft_id)
                                        )
                                        .unwrap()
                                );
                            }

                            last_id = raft_id;
                            log_batch.push(WriteOperation::set(
                                ColumnFamily::Logs,
                                LogKey::serialize_raft(&raft_id),
                                log,
                            ));
                        }
                        Update::Eof => {
                            is_done = true;
                        }
                        _ => {
                            debug_assert!(false, "Invalid update: {:?}", update);
                        }
                    }
                }

                if !log_batch.is_empty() {
                    store.db.write(log_batch)?;
                }

                Ok((first_id, last_id, changed_accounts, is_done))
            })
            .await
        {
            Ok((first_id, last_id, changed_accounts, is_done)) => {
                if is_done {
                    //println!("Changed accounts: {:?}", changed_accounts);
                    self.request_updates(
                        first_id,
                        last_id,
                        leader_commit_index,
                        change_seq,
                        changed_accounts.into_iter().collect::<Vec<_>>(),
                    )
                    .await
                } else {
                    (
                        State::AppendEntries {
                            first_id,
                            last_id,
                            changed_accounts,
                        },
                        Response::AppendEntries(AppendEntriesResponse::Continue),
                    )
                        .into()
                }
            }
            Err(err) => {
                debug!("handle_update_log failed: {:?}", err);
                None
            }
        }
    }

    async fn request_updates(
        &self,
        first_id: RaftId,
        last_id: RaftId,
        leader_commit_index: LogIndex,
        change_seq: &mut u64,
        mut changed_accounts: Vec<(AccountId, Collections)>,
    ) -> Option<(State, Response)> {
        loop {
            let (account_id, collection) =
                if let Some((account_id, collections)) = changed_accounts.last_mut() {
                    if let Some(collection) = collections.pop() {
                        if matches!(collection, Collection::Thread) {
                            continue;
                        }
                        (*account_id, collection)
                    } else {
                        changed_accounts.pop();
                        continue;
                    }
                } else {
                    // Apply changes
                    if last_id.index <= leader_commit_index {
                        let store = self.store.clone();
                        if let Err(err) = self
                            .spawn_worker(move || store.apply_pending_updates(last_id.index, false))
                            .await
                        {
                            error!("Failed to apply changes: {:?}", err);
                            return None;
                        }

                        // Set up to date
                        if last_id.index == leader_commit_index {
                            debug!(
                                "This node is now up to date with the leader's commit index {}.",
                                leader_commit_index
                            );
                            self.set_up_to_date(true);
                            self.update_raft_index(last_id.index);
                            self.store_changed(last_id).await;
                        } else {
                            debug!(
                                concat!(
                                    "This node is still behind the leader's commit index {}, ",
                                    "local commit index is {}."
                                ),
                                leader_commit_index, last_id.index
                            );
                        }
                    }

                    return (
                        State::Synchronize,
                        Response::AppendEntries(AppendEntriesResponse::Commit {
                            commit_index: last_id.index,
                        }),
                    )
                        .into();
                };

            debug!(
                "Merging changes for account {}, collection {:?} from index {} to {}.",
                account_id, collection, first_id.index, last_id.index
            );

            let store = self.store.clone();
            match self
                .spawn_worker(move || {
                    store.merge_changes(account_id, collection, first_id.index, last_id.index)
                })
                .await
            {
                Ok(mut changes) => {
                    if !changes.deletes.is_empty() {
                        let pending_updates_key =
                            LogKey::serialize_pending_update(last_id.index, *change_seq);
                        let pending_updates =
                            match PendingUpdates::new(vec![PendingUpdate::DeleteDocuments {
                                account_id,
                                collection,
                                document_ids: changes.deletes.into_iter().collect(),
                            }])
                            .serialize()
                            {
                                Some(pending_updates) => pending_updates,
                                None => {
                                    error!("Failed to serialize pending updates.");
                                    return None;
                                }
                            };

                        let store = self.store.clone();
                        if let Err(err) = self
                            .spawn_worker(move || {
                                store.db.set(
                                    ColumnFamily::Logs,
                                    &pending_updates_key,
                                    &pending_updates,
                                )
                            })
                            .await
                        {
                            error!("Failed to write pending update: {:?}", err);
                            return None;
                        }

                        *change_seq += 1;
                        changes.deletes = RoaringBitmap::new();
                    }

                    if !changes.inserts.is_empty() || !changes.updates.is_empty() {
                        return (
                            State::AppendChanges {
                                first_id,
                                last_id,
                                changed_accounts,
                            },
                            Response::AppendEntries(AppendEntriesResponse::Update {
                                account_id,
                                collection,
                                changes: match changes.serialize() {
                                    Some(changes) => changes,
                                    None => {
                                        error!("Failed to serialize bitmap.");
                                        return None;
                                    }
                                },
                            }),
                        )
                            .into();
                    } else {
                        continue;
                    }
                }
                Err(err) => {
                    error!("Error getting raft changes: {:?}", err);
                    return None;
                }
            }
        }
    }

    async fn handle_pending_updates(
        &self,
        first_id: RaftId,
        last_id: RaftId,
        leader_commit_index: LogIndex,
        change_seq: &mut u64,
        changed_accounts: Vec<(AccountId, Collections)>,
        updates: Vec<Update>,
    ) -> Option<(State, Response)> {
        //println!("{:#?}", updates);
        let mut pending_updates = Vec::with_capacity(updates.len());
        let mut is_done = false;

        for update in updates {
            match update {
                Update::Document {
                    account_id,
                    document_id,
                    update,
                } => {
                    pending_updates.push(PendingUpdate::UpdateDocument {
                        account_id,
                        document_id,
                        update,
                    });
                }
                Update::Eof => {
                    is_done = true;
                }
                _ => {
                    debug_assert!(false, "Invalid update: {:?}", update);
                }
            }
        }

        if !pending_updates.is_empty() {
            //println!("Storing update: {:?}", pending_updates);
            let pending_updates_key = LogKey::serialize_pending_update(last_id.index, *change_seq);
            let pending_updates = match PendingUpdates::new(pending_updates).serialize() {
                Some(pending_updates) => pending_updates,
                None => {
                    error!("Failed to serialize pending updates.");
                    return None;
                }
            };
            *change_seq += 1;

            let store = self.store.clone();
            if let Err(err) = self
                .spawn_worker(move || {
                    store
                        .db
                        .set(ColumnFamily::Logs, &pending_updates_key, &pending_updates)
                })
                .await
            {
                error!("Failed to write pending update: {:?}", err);
                return None;
            }
        }

        if !is_done {
            (
                State::AppendChanges {
                    first_id,
                    last_id,
                    changed_accounts,
                },
                Response::AppendEntries(AppendEntriesResponse::Continue),
            )
                .into()
        } else {
            self.request_updates(
                first_id,
                last_id,
                leader_commit_index,
                change_seq,
                changed_accounts,
            )
            .await
        }
    }

    async fn handle_match_log(&self, last_log: RaftId) -> Option<Response>
    where
        T: for<'x> Store<'x> + 'static,
    {
        Response::AppendEntries(AppendEntriesResponse::Match {
            match_log: match self.get_prev_raft_id(last_log).await {
                Ok(Some(matched)) => {
                    self.set_up_to_date(matched == last_log);
                    matched
                }
                Ok(None) => {
                    if last_log.is_none() {
                        self.set_up_to_date(true);
                    }
                    RaftId::none()
                }
                Err(err) => {
                    debug!("Failed to get prev raft id: {:?}", err);
                    return None;
                }
            },
        })
        .into()
    }

    async fn handle_synchronize_log(&self, match_terms: Vec<RaftId>) -> Option<Response> {
        if match_terms.is_empty() {
            error!("Log sync failed: match terms list is empty.");
            return None;
        }

        let local_match_terms = match self.get_raft_match_terms().await {
            Ok(local_match_terms) => {
                debug_assert!(!local_match_terms.is_empty());
                local_match_terms
            }
            Err(err) => {
                error!("Error getting raft match list: {:?}", err);
                return None;
            }
        };
        //println!("Match terms: {:?}\n{:?}", match_terms, local_match_terms);

        let mut matched_id = RaftId::none();
        for (local_id, remote_id) in local_match_terms.into_iter().zip(match_terms) {
            if local_id == remote_id {
                matched_id = local_id;
            } else {
                break;
            }
        }

        //debug!("[{}] Found matching terms at {:?}.", local_name, matched_id,);

        Response::AppendEntries(AppendEntriesResponse::Synchronize {
            match_indexes: if !matched_id.is_none() {
                match self.get_raft_match_indexes(matched_id.index).await {
                    Ok((_, match_indexes)) => {
                        if !match_indexes.is_empty() {
                            let mut bytes = Vec::with_capacity(match_indexes.serialized_size());
                            if let Err(err) = match_indexes.serialize_into(&mut bytes) {
                                error!("Failed to serialize match indexes: {}", err);
                                return None;
                            }
                            bytes
                        } else {
                            debug_assert!(false);
                            debug!("No match indexes found for match indexes {:?}", matched_id);
                            return None;
                        }
                    }
                    Err(err) => {
                        error!("Error getting raft match indexes: {:?}", err);
                        return None;
                    }
                }
            } else {
                vec![]
            },
        })
        .into()
    }

    async fn handle_merge_log(&self, matched_log: RaftId) -> Option<(State, Response)> {
        if let Err(err) = self.prepare_rollback_changes(matched_log.index).await {
            error!("Failed to prepare rollback changes: {:?}", err);
            return None;
        }

        let (account_id, collection, changes) = match self.next_rollback_change().await {
            Ok(Some(rollback_change)) => rollback_change,
            Ok(None) => {
                error!("Failed to prepare rollback changes: No changes found.");
                return None;
            }
            Err(err) => {
                error!("Failed to obtain rollback changes: {:?}", err);
                return None;
            }
        };

        self.handle_rollback_updates(account_id, collection, changes, vec![])
            .await
    }

    async fn handle_rollback_updates(
        &self,
        mut account_id: AccountId,
        mut collection: Collection,
        mut changes: MergedChanges,
        mut updates: Vec<Update>,
    ) -> Option<(State, Response)> {
        loop {
            // Thread collection does not contain any actual records,
            // it exists solely for change tracking.
            if let Collection::Thread = collection {
                //println!("Skipping thread changes...");
                changes.inserts.clear();
                changes.updates.clear();
                changes.deletes.clear();
            }

            if !changes.inserts.is_empty() {
                /*println!(
                    "Deleting from collection {:?} items {:?}",
                    collection, changes.inserts
                );*/
                let mut batch = WriteBatch::new(account_id);
                for delete_id in &changes.inserts {
                    batch.delete_document(collection, delete_id);
                }
                let store = self.store.clone();
                if let Err(err) = self.spawn_worker(move || store.write(batch)).await {
                    error!("Failed to delete documents: {:?}", err);
                    return None;
                }
                changes.inserts.clear();
            }

            if !updates.is_empty() {
                let store = self.store.clone();
                match self
                    .spawn_worker(move || store.apply_rollback_updates(updates))
                    .await
                {
                    Ok(is_done) => {
                        if is_done {
                            changes.updates.clear();
                            changes.deletes.clear();
                        } else {
                            return (
                                State::Rollback {
                                    account_id,
                                    collection,
                                    changes,
                                },
                                Response::AppendEntries(AppendEntriesResponse::Continue),
                            )
                                .into();
                        }
                    }
                    Err(err) => {
                        debug!("Failed to update store: {:?}", err);
                        return None;
                    }
                }
                updates = vec![];
            }

            if !changes.deletes.is_empty() || !changes.updates.is_empty() {
                let serialized_changes = match changes.serialize() {
                    Some(changes) => changes,
                    None => {
                        error!("Failed to serialize bitmap.");
                        return None;
                    }
                };

                return (
                    State::Rollback {
                        account_id,
                        collection,
                        changes,
                    },
                    Response::AppendEntries(AppendEntriesResponse::Update {
                        account_id,
                        collection,
                        changes: serialized_changes,
                    }),
                )
                    .into();
            } else {
                if let Err(err) = self.remove_rollback_change(account_id, collection).await {
                    error!("Failed to remove rollback change key: {:?}", err);
                    return None;
                }

                match self.next_rollback_change().await {
                    Ok(Some((next_account_id, next_collection, next_changes))) => {
                        account_id = next_account_id;
                        collection = next_collection;
                        changes = next_changes;
                        continue;
                    }
                    Ok(None) => {
                        return (
                            State::default(),
                            Response::AppendEntries(AppendEntriesResponse::Match {
                                match_log: match self.get_last_log().await {
                                    Ok(Some(last_log)) => last_log,
                                    Ok(None) => {
                                        error!("Unexpected error: Last log not found.");
                                        return None;
                                    }
                                    Err(err) => {
                                        debug!("Failed to get prev raft id: {:?}", err);
                                        return None;
                                    }
                                },
                            }),
                        )
                            .into();
                    }
                    Err(err) => {
                        error!("Failed to obtain rollback changes: {:?}", err);
                        return None;
                    }
                }
            }
        }
    }

    pub async fn init_last_applied_index(&self) -> store::Result<()> {
        let store = self.store.clone();
        self.spawn_worker(move || {
            store.db.set(
                ColumnFamily::Values,
                LAST_APPLIED_INDEX_KEY,
                &store
                    .get_prev_raft_id(RaftId::new(TermId::MAX, LogIndex::MAX))?
                    .map(|v| v.index)
                    .unwrap_or(LogIndex::MAX)
                    .serialize()
                    .unwrap(),
            )
        })
        .await
    }

    pub async fn apply_committed_updates(&self) -> store::Result<bool> {
        let store = self.store.clone();
        self.spawn_worker(move || store.apply_pending_updates(LogIndex::MAX, true))
            .await
    }
}

pub trait JMAPStoreRaftUpdates {
    fn apply_pending_updates(&self, apply_up_to: LogIndex, do_reset: bool) -> store::Result<bool>;
    fn apply_rollback_updates(&self, changes: Vec<Update>) -> store::Result<bool>;
    fn apply_document_update(
        &self,
        account_id: AccountId,
        document_id: DocumentId,
        update: DocumentUpdate,
        document_batch: &mut WriteBatch,
    ) -> store::Result<()>;
}

impl<T> JMAPStoreRaftUpdates for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn apply_pending_updates(&self, apply_up_to: LogIndex, do_reset: bool) -> store::Result<bool> {
        let apply_up_to: LogIndex = if apply_up_to != LogIndex::MAX {
            self.db.set(
                ColumnFamily::Values,
                LAST_APPLIED_INDEX_KEY,
                &apply_up_to.serialize().unwrap(),
            )?;
            apply_up_to
        } else if let Some(apply_up_to) =
            self.db.get(ColumnFamily::Values, LAST_APPLIED_INDEX_KEY)?
        {
            apply_up_to
        } else {
            return Ok(false);
        };

        debug!("Applying pending updates up to index {}.", apply_up_to);

        let mut log_batch = Vec::new();
        for (key, value) in self.db.iterator(
            ColumnFamily::Logs,
            &[LogKey::PENDING_UPDATES_KEY_PREFIX],
            Direction::Forward,
        )? {
            if !key.starts_with(&[LogKey::PENDING_UPDATES_KEY_PREFIX]) {
                break;
            }
            let index = (&key[..]).deserialize_be_u64(1).ok_or_else(|| {
                StoreError::InternalError(format!(
                    "Failed to deserialize account id from changelog key: [{:?}]",
                    key
                ))
            })?;

            if apply_up_to != LogIndex::MAX && index <= apply_up_to {
                let mut document_batch = WriteBatch::new(AccountId::MAX);

                for update in PendingUpdates::deserialize(&value)
                    .ok_or_else(|| {
                        StoreError::InternalError(format!(
                            "Failed to deserialize pending updates for key [{:?}]",
                            key
                        ))
                    })?
                    .updates
                {
                    println!("Applying {:?}", update);
                    match update {
                        PendingUpdate::UpdateDocument {
                            account_id,
                            document_id,
                            update,
                        } => {
                            if account_id != document_batch.account_id {
                                if !document_batch.is_empty() {
                                    self.write(document_batch)?;
                                    document_batch = WriteBatch::new(account_id);
                                } else {
                                    document_batch.account_id = account_id;
                                }
                            }
                            self.apply_document_update(
                                account_id,
                                document_id,
                                update,
                                &mut document_batch,
                            )?;
                        }
                        PendingUpdate::DeleteDocuments {
                            account_id,
                            collection,
                            document_ids,
                        } => {
                            if account_id != document_batch.account_id {
                                if !document_batch.is_empty() {
                                    self.write(document_batch)?;
                                    document_batch = WriteBatch::new(account_id);
                                } else {
                                    document_batch.account_id = account_id;
                                }
                            }

                            for document_id in document_ids {
                                document_batch.delete_document(collection, document_id);
                            }
                        }
                    }
                }

                if !document_batch.is_empty() {
                    self.write(document_batch)?;
                }

                self.db.delete(ColumnFamily::Logs, &key)?;
            } else if do_reset {
                log_batch.push(WriteOperation::Delete {
                    cf: ColumnFamily::Logs,
                    key: key.to_vec(),
                });
            } else {
                return Ok(true);
            }
        }

        if do_reset {
            let key = LogKey::serialize_raft(&RaftId::new(
                0,
                if apply_up_to != LogIndex::MAX {
                    apply_up_to
                } else {
                    0
                },
            ));
            log_batch.push(WriteOperation::Delete {
                cf: ColumnFamily::Values,
                key: LAST_APPLIED_INDEX_KEY.to_vec(),
            });

            for (key, value) in self
                .db
                .iterator(ColumnFamily::Logs, &key, Direction::Forward)?
            {
                if !key.starts_with(&[LogKey::RAFT_KEY_PREFIX]) {
                    break;
                }
                let raft_id = LogKey::deserialize_raft(&key).ok_or_else(|| {
                    StoreError::InternalError(format!("Corrupted raft key for [{:?}]", key))
                })?;
                if apply_up_to == LogIndex::MAX || raft_id.index > apply_up_to {
                    match Entry::deserialize(&value).ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted raft entry for [{:?}]", key))
                    })? {
                        Entry::Item {
                            account_id,
                            changed_collections,
                        } => {
                            for changed_collection in changed_collections {
                                log_batch.push(WriteOperation::Delete {
                                    cf: ColumnFamily::Logs,
                                    key: LogKey::serialize_change(
                                        account_id,
                                        changed_collection,
                                        raft_id.index,
                                    ),
                                });
                            }
                        }
                        Entry::Snapshot { changed_accounts } => {
                            for (changed_collections, changed_accounts_ids) in changed_accounts {
                                for changed_collection in changed_collections {
                                    for changed_account_id in &changed_accounts_ids {
                                        log_batch.push(WriteOperation::Delete {
                                            cf: ColumnFamily::Logs,
                                            key: LogKey::serialize_change(
                                                *changed_account_id,
                                                changed_collection,
                                                raft_id.index,
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                    };

                    log_batch.push(WriteOperation::Delete {
                        cf: ColumnFamily::Logs,
                        key: key.to_vec(),
                    });
                }
            }

            if !log_batch.is_empty() {
                self.db.write(log_batch)?;
            }
        }

        Ok(true)
    }

    fn apply_rollback_updates(&self, updates: Vec<Update>) -> store::Result<bool> {
        let mut document_batch = WriteBatch::new(AccountId::MAX);

        debug!("Inserting {} rollback changes...", updates.len(),);
        let mut is_done = false;

        for update in updates {
            match update {
                Update::Document {
                    account_id,
                    document_id,
                    update,
                } => {
                    if account_id != document_batch.account_id {
                        if !document_batch.is_empty() {
                            self.write(document_batch)?;
                            document_batch = WriteBatch::new(account_id);
                        } else {
                            document_batch.account_id = account_id;
                        }
                    }

                    self.apply_document_update(
                        account_id,
                        document_id,
                        update,
                        &mut document_batch,
                    )?;
                }
                Update::Eof => {
                    is_done = true;
                }
                _ => debug_assert!(false, "Invalid update type: {:?}", update),
            }
        }
        if !document_batch.is_empty() {
            self.write(document_batch)?;
        }

        Ok(is_done)
    }

    fn apply_document_update(
        &self,
        account_id: AccountId,
        document_id: DocumentId,
        update: DocumentUpdate,
        document_batch: &mut WriteBatch,
    ) -> store::Result<()> {
        match update {
            DocumentUpdate::InsertMail {
                thread_id,
                keywords,
                mailboxes,
                received_at,
                body,
            } => {
                self.raft_update_mail(
                    document_batch,
                    account_id,
                    document_id,
                    thread_id,
                    mailboxes,
                    keywords,
                    Some((
                        lz4_flex::decompress_size_prepended(&body).map_err(|err| {
                            StoreError::InternalError(format!(
                                "Failed to decompress raft update: {}",
                                err
                            ))
                        })?,
                        received_at,
                    )),
                )?;
            }
            DocumentUpdate::UpdateMail {
                thread_id,
                keywords,
                mailboxes,
            } => {
                self.raft_update_mail(
                    document_batch,
                    account_id,
                    document_id,
                    thread_id,
                    mailboxes,
                    keywords,
                    None,
                )?;
            }
            DocumentUpdate::UpdateMailbox { mailbox } => {
                self.raft_update_mailbox(document_batch, account_id, document_id, mailbox)?
            }
        }
        Ok(())
    }
}
