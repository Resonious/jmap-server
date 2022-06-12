use roaring::RoaringBitmap;

use crate::serialize::leb128::Leb128;
use crate::serialize::StoreDeserialize;
use crate::DocumentId;
use crate::{
    core::{acl::ACL, bitmap::Bitmap, collection::Collection, error::StoreError},
    serialize::key::ValueKey,
    AccountId, ColumnFamily, Direction, JMAPStore, Store,
};

impl<T> JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn get_shared_accounts(
        &self,
        member_of: &[AccountId],
    ) -> crate::Result<Vec<(AccountId, Bitmap<Collection>)>> {
        let mut shared_accounts: Vec<(AccountId, Bitmap<Collection>)> = Vec::new();
        for account_id in member_of {
            let prefix =
                ValueKey::serialize_acl_prefix(*account_id, AccountId::MAX, Collection::None);
            for (key, value) in
                self.db
                    .iterator(ColumnFamily::Values, &prefix, Direction::Forward)?
            {
                if key.starts_with(&prefix)
                    && key.len() > prefix.len() + 2
                    && key[prefix.len()] != u8::MAX
                {
                    let (to_account_id, to_collection, _) = ValueKey::deserialize_acl_target(
                        &key[prefix.len() + 1..],
                    )
                    .ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted ACL key for [{:?}]", key))
                    })?;
                    let acl = Bitmap::from(u64::deserialize(&value).ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted ACL value for [{:?}]", key))
                    })?);

                    if !member_of.contains(&to_account_id) {
                        let mut collections: Bitmap<Collection> = Bitmap::new();
                        if acl.contains(ACL::Read) {
                            collections.insert(to_collection);
                        }
                        if (acl.contains(ACL::ReadItems)) && to_collection == Collection::Mailbox {
                            collections.insert(Collection::Mail);
                        }

                        if !collections.is_empty() {
                            if let Some(sharing) = shared_accounts
                                .iter_mut()
                                .find(|(account_id, _)| *account_id == to_account_id)
                            {
                                sharing.1.union(&collections);
                            } else {
                                shared_accounts.push((to_account_id, collections));
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        }
        Ok(shared_accounts)
    }

    pub fn get_shared_documents(
        &self,
        member_of: &[AccountId],
        to_account_id: AccountId,
        to_collection: Collection,
        acls: Bitmap<ACL>,
    ) -> crate::Result<Option<RoaringBitmap>> {
        let mut shared_documents = RoaringBitmap::new();
        for account_id in member_of {
            let prefix = ValueKey::serialize_acl_prefix(*account_id, to_account_id, to_collection);
            for (key, value) in
                self.db
                    .iterator(ColumnFamily::Values, &prefix, Direction::Forward)?
            {
                if key.starts_with(&prefix) && key.len() > prefix.len() {
                    let (document_id, _) = DocumentId::from_leb128_bytes(&key[prefix.len()..])
                        .ok_or_else(|| {
                            StoreError::InternalError(format!(
                                "Corrupted ACL members key for [{:?}]",
                                key
                            ))
                        })?;

                    let mut acl = Bitmap::from(u64::deserialize(&value).ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted ACL value for [{:?}]", key))
                    })?);
                    acl.intersection(&acls);
                    if !acl.is_empty() {
                        shared_documents.insert(document_id);
                    }
                } else {
                    break;
                }
            }
        }
        Ok(if !shared_documents.is_empty() {
            shared_documents.into()
        } else {
            None
        })
    }

    pub fn get_acl(
        &self,
        member_of: &[AccountId],
        to_account_id: AccountId,
        to_collection: Collection,
        to_document_id: DocumentId,
    ) -> crate::Result<Bitmap<ACL>> {
        let mut acl = Bitmap::new();
        for account_id in member_of {
            if let Some(item_acl) = self
                .db
                .get::<u64>(
                    ColumnFamily::Values,
                    &ValueKey::serialize_acl(
                        *account_id,
                        to_account_id,
                        to_collection,
                        to_document_id,
                    ),
                )?
                .map(Bitmap::from)
            {
                acl.union(&item_acl);
            }
        }
        Ok(acl)
    }
}
