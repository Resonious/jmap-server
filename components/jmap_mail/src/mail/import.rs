use std::collections::{HashMap, HashSet};

use crate::mail::Keyword;
use crate::mail::{parse::get_message_blob, MESSAGE_RAW};

use jmap::error::method::MethodError;
use jmap::id::blob::BlobId;
use jmap::id::JMAPIdSerialize;
use jmap::jmap_store::blob::JMAPBlobStore;
use jmap::jmap_store::changes::JMAPChanges;
use jmap::protocol::json::JSONValue;
use jmap::request::import::ImportRequest;
use store::batch::Document;
use store::field::{DefaultOptions, Options};
use store::query::{JMAPIdMapFnc, JMAPStoreQuery};
use store::tracing::debug;
use store::tracing::log::error;
use store::{
    batch::WriteBatch, field::Text, roaring::RoaringBitmap, AccountId, Comparator, FieldValue,
    Filter, JMAPId, JMAPStore, Store, Tag, ThreadId,
};
use store::{Collection, DocumentId, JMAPIdPrefix};

use crate::mail::{parse::build_message_document, MessageField};

pub struct MailImportRequest {
    pub emails: Vec<MailImportItem>,
}
pub struct MailImportItem {
    pub id: String,
    pub blob_id: BlobId,
    pub mailbox_ids: Vec<DocumentId>,
    pub keywords: Vec<Keyword>,
    pub received_at: Option<i64>,
}

impl MailImportRequest {
    fn parse_arguments(mut arguments: HashMap<String, JSONValue>) -> jmap::Result<Self> {
        let arguments = arguments
            .remove("emails")
            .ok_or_else(|| MethodError::InvalidArguments("Missing emails property.".to_string()))?
            .unwrap_object()
            .ok_or_else(|| MethodError::InvalidArguments("Expected email object.".to_string()))?;
        let mut emails = Vec::with_capacity(arguments.len());
        for (id, item_value) in arguments {
            let mut item_value = item_value.unwrap_object().ok_or_else(|| {
                MethodError::InvalidArguments(format!("Expected mailImport object for {}.", id))
            })?;
            emails.push(MailImportItem {
                blob_id: item_value
                    .remove("blobId")
                    .ok_or_else(|| {
                        MethodError::InvalidArguments(format!("Missing blobId for {}.", id))
                    })?
                    .parse_blob_id(false)?
                    .unwrap(),
                mailbox_ids: item_value
                    .remove("mailboxIds")
                    .ok_or_else(|| {
                        MethodError::InvalidArguments(format!("Missing mailboxIds for {}.", id))
                    })?
                    .unwrap_object()
                    .ok_or_else(|| {
                        MethodError::InvalidArguments(format!(
                            "Expected mailboxIds object for {}.",
                            id
                        ))
                    })?
                    .into_iter()
                    .filter_map(|(k, v)| {
                        if v.to_bool()? {
                            JMAPId::from_jmap_string(&k).map(|id| id as DocumentId)
                        } else {
                            None
                        }
                    })
                    .collect(),
                keywords: if let Some(keywords) = item_value.remove("keywords") {
                    keywords
                        .parse_array_items::<Keyword>(true)?
                        .unwrap_or_default()
                } else {
                    vec![]
                },
                received_at: if let Some(received_at) = item_value.remove("receivedAt") {
                    received_at.parse_utc_date(true)?
                } else {
                    None
                },
                id,
            });
        }

        Ok(MailImportRequest { emails })
    }
}

pub trait JMAPMailImport {
    fn mail_import(&self, request: ImportRequest) -> jmap::Result<JSONValue>;

    fn mail_import_blob(
        &self,
        account_id: AccountId,
        blob: Vec<u8>,
        mailbox_ids: Vec<DocumentId>,
        keywords: Vec<Tag>,
        received_at: Option<i64>,
    ) -> jmap::Result<JSONValue>;

    fn mail_merge_threads(
        &self,
        account_id: AccountId,
        documents: &mut WriteBatch,
        thread_ids: Vec<ThreadId>,
    ) -> store::Result<ThreadId>;

    #[allow(clippy::too_many_arguments)]
    fn raft_update_mail(
        &self,
        batch: &mut WriteBatch,
        account_id: AccountId,
        document_id: DocumentId,
        thread_id: DocumentId,
        mailbox_ids: HashSet<Tag>,
        keywords: HashSet<Tag>,
        insert: Option<(Vec<u8>, i64)>,
    ) -> store::Result<()>;
}

impl<T> JMAPMailImport for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_import(&self, request: ImportRequest) -> jmap::Result<JSONValue> {
        let old_state = self.get_state(request.account_id, Collection::Mail)?;
        if let Some(if_in_state) = request.if_in_state {
            if old_state != if_in_state {
                return Err(MethodError::StateMismatch);
            }
        }

        let emails = MailImportRequest::parse_arguments(request.arguments)?.emails;
        if emails.len() > self.config.mail_import_max_items {
            return Err(MethodError::RequestTooLarge);
        }

        let mailbox_ids = self
            .get_document_ids(request.account_id, Collection::Mailbox)?
            .unwrap_or_else(RoaringBitmap::new);

        let mut created = HashMap::with_capacity(emails.len());
        let mut not_created = HashMap::with_capacity(emails.len());

        'main: for item in emails {
            if item.mailbox_ids.is_empty() {
                not_created.insert(
                    item.id,
                    JSONValue::new_invalid_property(
                        "mailboxIds",
                        "Message must belong to at least one mailbox.",
                    ),
                );
                continue 'main;
            }
            for &mailbox_id in &item.mailbox_ids {
                if !mailbox_ids.contains(mailbox_id) {
                    not_created.insert(
                        item.id,
                        JSONValue::new_invalid_property(
                            "mailboxIds",
                            format!(
                                "Mailbox {} does not exist.",
                                (mailbox_id as JMAPId).to_jmap_string()
                            ),
                        ),
                    );
                    continue 'main;
                }
            }

            if let Some(blob) =
                self.download_blob(request.account_id, &item.blob_id, get_message_blob)?
            {
                created.insert(
                    item.id,
                    self.mail_import_blob(
                        request.account_id,
                        blob,
                        item.mailbox_ids,
                        item.keywords.into_iter().map(|k| k.tag).collect(),
                        item.received_at,
                    )?,
                );
            } else {
                not_created.insert(
                    item.id,
                    JSONValue::new_invalid_property(
                        "blobId",
                        format!("BlobId {} not found.", item.blob_id.to_jmap_string()),
                    ),
                );
            }
        }

        let mut obj = HashMap::new();
        obj.insert(
            "newState".to_string(),
            if !created.is_empty() {
                self.get_state(request.account_id, Collection::Mail)?
            } else {
                old_state.clone()
            }
            .into(),
        );
        obj.insert("oldState".to_string(), old_state.into());
        obj.insert(
            "created".to_string(),
            if !created.is_empty() {
                created.into()
            } else {
                JSONValue::Null
            },
        );
        obj.insert(
            "notCreated".to_string(),
            if !not_created.is_empty() {
                not_created.into()
            } else {
                JSONValue::Null
            },
        );
        Ok(obj.into())
    }

    fn mail_import_blob(
        &self,
        account_id: AccountId,
        blob: Vec<u8>,
        mailbox_ids: Vec<DocumentId>,
        keywords: Vec<Tag>,
        received_at: Option<i64>,
    ) -> jmap::Result<JSONValue> {
        // Build message document
        let document_id = self.assign_document_id(account_id, Collection::Mail)?;
        let mut batch = WriteBatch::new(account_id, self.config.is_in_cluster);
        let mut document = Document::new(Collection::Mail, document_id);
        let blob_len = blob.len();
        let (reference_ids, thread_name) =
            build_message_document(&mut document, blob, received_at)?;

        // Add mailbox tags
        //TODO validate mailbox ids
        for mailbox_id in mailbox_ids {
            document.tag(
                MessageField::Mailbox,
                Tag::Id(mailbox_id),
                DefaultOptions::new(),
            );
            batch.log_child_update(Collection::Mailbox, mailbox_id);
        }

        // Add keyword tags
        for keyword in keywords {
            document.tag(MessageField::Keyword, keyword, DefaultOptions::new());
        }

        // Lock account while threads are merged
        let _lock = self.lock_account(account_id, Collection::Mail);

        // Obtain thread id
        let thread_id = if !reference_ids.is_empty() {
            // Obtain thread ids for all matching document ids
            let thread_ids = self
                .get_multi_document_tag_id(
                    account_id,
                    Collection::Mail,
                    self.query::<JMAPIdMapFnc>(JMAPStoreQuery::new(
                        account_id,
                        Collection::Mail,
                        Filter::and(vec![
                            Filter::eq(
                                MessageField::ThreadName.into(),
                                FieldValue::Keyword(thread_name.to_string()),
                            ),
                            Filter::or(
                                reference_ids
                                    .iter()
                                    .map(|id| {
                                        Filter::eq(
                                            MessageField::MessageIdRef.into(),
                                            FieldValue::Keyword(id.to_string()),
                                        )
                                    })
                                    .collect(),
                            ),
                        ]),
                        Comparator::None,
                    ))?
                    .into_iter()
                    .map(|id| id.get_document_id())
                    .collect::<Vec<u32>>()
                    .into_iter(),
                    MessageField::ThreadId.into(),
                )?
                .into_iter()
                .filter_map(|id| Some(*id?))
                .collect::<HashSet<ThreadId>>();

            match thread_ids.len() {
                1 => {
                    // There was just one match, use it as the thread id
                    thread_ids.into_iter().next()
                }
                0 => None,
                _ => {
                    // Merge all matching threads
                    Some(self.mail_merge_threads(
                        account_id,
                        &mut batch,
                        thread_ids.into_iter().collect(),
                    )?)
                }
            }
        } else {
            None
        };

        let thread_id = if let Some(thread_id) = thread_id {
            batch.log_child_update(Collection::Thread, thread_id);
            thread_id
        } else {
            let thread_id = self.assign_document_id(account_id, Collection::Thread)?;
            batch.log_insert(Collection::Thread, thread_id);
            thread_id
        };

        for reference_id in reference_ids {
            document.text(
                MessageField::MessageIdRef,
                Text::keyword(reference_id),
                DefaultOptions::new(),
            );
        }

        document.tag(
            MessageField::ThreadId,
            Tag::Id(thread_id),
            DefaultOptions::new(),
        );

        document.text(
            MessageField::ThreadName,
            Text::keyword(thread_name),
            DefaultOptions::new().sort(),
        );

        let jmap_mail_id = JMAPId::from_parts(thread_id, document_id);
        batch.log_insert(Collection::Mail, jmap_mail_id);
        batch.insert_document(document);

        // Write documents to store
        self.write(batch)?;

        // Generate JSON object
        let mut values = HashMap::with_capacity(4);
        values.insert("id".to_string(), jmap_mail_id.to_jmap_string().into());
        values.insert(
            "blobId".to_string(),
            BlobId::new_owned(account_id, Collection::Mail, document_id, MESSAGE_RAW)
                .to_jmap_string()
                .into(),
        );
        values.insert(
            "threadId".to_string(),
            (thread_id as JMAPId).to_jmap_string().into(),
        );
        values.insert("size".to_string(), blob_len.into());

        Ok(values.into())
    }

    fn mail_merge_threads(
        &self,
        account_id: AccountId,
        batch: &mut WriteBatch,
        thread_ids: Vec<ThreadId>,
    ) -> store::Result<ThreadId> {
        // Query tags for all thread ids
        let mut document_sets = Vec::with_capacity(thread_ids.len());

        for (pos, document_set) in self
            .get_tags(
                account_id,
                Collection::Mail,
                MessageField::ThreadId.into(),
                &thread_ids
                    .iter()
                    .map(|id| Tag::Id(*id))
                    .collect::<Vec<Tag>>(),
            )?
            .into_iter()
            .enumerate()
        {
            if let Some(document_set) = document_set {
                debug_assert!(!document_set.is_empty());
                document_sets.push((document_set, thread_ids[pos]));
            } else {
                error!(
                    "No tags found for thread id {}, account: {}.",
                    thread_ids[pos], account_id
                );
            }
        }

        document_sets.sort_unstable_by_key(|i| i.0.len());

        let mut document_sets = document_sets.into_iter().rev();
        let thread_id = document_sets.next().unwrap().1;

        for (document_set, delete_thread_id) in document_sets {
            for document_id in document_set {
                let mut document = Document::new(Collection::Mail, document_id);
                document.tag(
                    MessageField::ThreadId,
                    Tag::Id(thread_id),
                    DefaultOptions::new(),
                );
                document.tag(
                    MessageField::ThreadId,
                    Tag::Id(delete_thread_id),
                    DefaultOptions::new().clear(),
                );
                batch.log_move(
                    Collection::Mail,
                    JMAPId::from_parts(delete_thread_id, document_id),
                    JMAPId::from_parts(thread_id, document_id),
                );
                batch.update_document(document);
            }

            batch.log_delete(Collection::Thread, delete_thread_id);
        }

        Ok(thread_id)
    }

    fn raft_update_mail(
        &self,
        batch: &mut WriteBatch,
        account_id: AccountId,
        document_id: DocumentId,
        thread_id: DocumentId,
        mailboxes: HashSet<Tag>,
        keywords: HashSet<Tag>,
        insert: Option<(Vec<u8>, i64)>,
    ) -> store::Result<()> {
        if let Some((raw_message, received_at)) = insert {
            let mut document = Document::new(Collection::Mail, document_id);

            // Parse and build message document
            let (reference_ids, thread_name) =
                build_message_document(&mut document, raw_message, received_at.into())?;

            // Add mailbox tags
            for mailbox in mailboxes {
                document.tag(MessageField::Mailbox, mailbox, DefaultOptions::new());
            }

            // Add keyword tags
            for keyword in keywords {
                document.tag(MessageField::Keyword, keyword, DefaultOptions::new());
            }

            for reference_id in reference_ids {
                document.text(
                    MessageField::MessageIdRef,
                    Text::keyword(reference_id),
                    DefaultOptions::new(),
                );
            }

            // Add thread id and name
            document.tag(
                MessageField::ThreadId,
                Tag::Id(thread_id),
                DefaultOptions::new(),
            );
            document.text(
                MessageField::ThreadName,
                Text::keyword(thread_name),
                DefaultOptions::new().sort(),
            );

            batch.insert_document(document);
        } else {
            let mut document = Document::new(Collection::Mail, document_id);

            // Process mailbox changes
            if let Some(current_mailboxes) = self.get_document_tags(
                account_id,
                Collection::Mail,
                document_id,
                MessageField::Mailbox.into(),
            )? {
                if current_mailboxes.items != mailboxes {
                    for current_mailbox in &current_mailboxes.items {
                        if !mailboxes.contains(current_mailbox) {
                            document.tag(
                                MessageField::Mailbox,
                                current_mailbox.clone(),
                                DefaultOptions::new().clear(),
                            );
                        }
                    }

                    for mailbox in mailboxes {
                        if !current_mailboxes.contains(&mailbox) {
                            document.tag(MessageField::Mailbox, mailbox, DefaultOptions::new());
                        }
                    }
                }
            } else {
                debug!(
                    "Raft update failed: No mailbox tags found for message {}.",
                    document_id
                );
                return Ok(());
            };

            // Process keyword changes
            let current_keywords = if let Some(current_keywords) = self.get_document_tags(
                account_id,
                Collection::Mail,
                document_id,
                MessageField::Keyword.into(),
            )? {
                current_keywords.items
            } else {
                HashSet::new()
            };
            if current_keywords != keywords {
                for current_keyword in &current_keywords {
                    if !keywords.contains(current_keyword) {
                        document.tag(
                            MessageField::Keyword,
                            current_keyword.clone(),
                            DefaultOptions::new().clear(),
                        );
                    }
                }

                for keyword in keywords {
                    if !current_keywords.contains(&keyword) {
                        document.tag(MessageField::Keyword, keyword, DefaultOptions::new());
                    }
                }
            }

            // Handle thread id changes
            if let Some(current_thread_id) = self.get_document_tag_id(
                account_id,
                Collection::Mail,
                document_id,
                MessageField::ThreadId.into(),
            )? {
                if thread_id != current_thread_id {
                    document.tag(
                        MessageField::ThreadId,
                        Tag::Id(thread_id),
                        DefaultOptions::new(),
                    );
                    document.tag(
                        MessageField::ThreadId,
                        Tag::Id(current_thread_id),
                        DefaultOptions::new().clear(),
                    );
                }
            } else {
                debug!(
                    "Raft update failed: No thread id found for message {}.",
                    document_id
                );
                return Ok(());
            };

            if !document.is_empty() {
                batch.update_document(document);
            }
        }
        Ok(())
    }
}