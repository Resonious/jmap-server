use crate::mail::import::JMAPMailImport;
use jmap::error::set::{SetError, SetErrorType};
use jmap::id::blob::JMAPBlob;
use jmap::id::jmap::JMAPId;
use jmap::jmap_store::blob::JMAPBlobStore;
use jmap::jmap_store::orm::{JMAPOrm, TinyORM};
use jmap::jmap_store::set::{SetHelper, SetObject};

use jmap::request::set::{SetRequest, SetResponse};
use mail_builder::headers::address::Address;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use mail_builder::headers::url::URL;
use mail_builder::mime::{BodyPart, MimePart};
use mail_builder::MessageBuilder;
use std::collections::{BTreeMap, HashMap, HashSet};
use store::core::collection::Collection;
use store::core::document::Document;
use store::core::error::StoreError;
use store::core::tag::Tag;
use store::write::options::{IndexOptions, Options};

use store::blob::BlobId;
use store::{AccountId, DocumentId, JMAPStore, Store};

use super::parse::get_message_part;
use super::schema::{
    BodyProperty, Email, EmailBodyPart, EmailBodyValue, EmailValue, HeaderForm, Keyword, Property,
};
use super::{MessageData, MessageField};

impl SetObject for Email {
    type SetArguments = ();

    type NextInvocation = ();

    fn map_references(&mut self, fnc: impl FnMut(&str) -> Option<jmap::id::jmap::JMAPId>) {
        todo!()
    }
}

pub trait JMAPSetMail<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_set(&self, request: SetRequest<Email>) -> jmap::Result<SetResponse<Email>>;
    fn mail_delete(
        &self,
        account_id: AccountId,
        document: &mut Document,
    ) -> store::Result<Option<DocumentId>>;
}

impl<T> JMAPSetMail<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_set(&self, request: SetRequest<Email>) -> jmap::Result<SetResponse<Email>> {
        let mut helper = SetHelper::new(self, request)?;
        let mailbox_ids = self
            .get_document_ids(helper.account_id, Collection::Mailbox)?
            .unwrap_or_default();
        let account_id = helper.account_id;

        helper.create(|_create_id, item, helper, document| {
            let mut builder = MessageBuilder::new();
            let mut fields = TinyORM::<Email>::new();

            let mut received_at = None;
            let body_values = item
                .properties
                .get(&Property::BodyValues)
                .and_then(|b| match b {
                    EmailValue::BodyValues { value } => Some(value),
                    _ => None,
                });

            for (property, value) in &item.properties {
                match (property, value) {
                    (Property::MailboxIds, EmailValue::MailboxIds { value, set }) => {
                        if *set {
                            fields.untag_all(&Property::MailboxIds);

                            for (mailbox_id, set) in value {
                                let mailbox_id = mailbox_id.as_id();

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if *set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_property(
                                        Property::MailboxIds,
                                        format!("mailboxId {} does not exist.", mailbox_id),
                                    ));
                                }
                            }
                        } else {
                            for (mailbox_id, set) in value {
                                let mailbox_id = mailbox_id.as_id();

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if *set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_property(
                                        Property::MailboxIds,
                                        format!("mailboxId {} does not exist.", mailbox_id),
                                    ));
                                }
                            }
                        }
                    }
                    (Property::Keywords, EmailValue::Keywords { value, set }) => {
                        if *set {
                            fields.untag_all(&Property::Keywords);

                            for (keyword, set) in value {
                                if *set {
                                    fields.tag(Property::Keywords, keyword.tag.clone());
                                }
                            }
                        } else {
                            for (keyword, set) in value {
                                if *set {
                                    fields.tag(Property::Keywords, keyword.tag.clone());
                                }
                            }
                        }
                    }
                    (Property::ReceivedAt, EmailValue::Date { value }) => {
                        received_at = value.timestamp().into();
                    }
                    (
                        Property::MessageId | Property::InReplyTo | Property::References,
                        EmailValue::TextList { value },
                    ) => {
                        builder = builder
                            .header(property.as_rfc_header(), MessageId::from(value.as_slice()));
                    }
                    (
                        Property::Sender
                        | Property::From
                        | Property::To
                        | Property::Cc
                        | Property::Bcc
                        | Property::ReplyTo,
                        EmailValue::Addresses { value },
                    ) => {
                        builder = builder
                            .header(property.as_rfc_header(), Address::from(value.as_slice()));
                    }
                    (Property::Subject, EmailValue::Text { value }) => {
                        builder = builder.subject(value);
                    }
                    (Property::SentAt, EmailValue::Date { value }) => {
                        builder = builder.date(value);
                    }
                    (Property::TextBody, EmailValue::BodyPartList { value }) => {
                        if let Some(body_part) = value.first() {
                            builder.html_body = body_part
                                .parse(self, account_id, body_values, "text/plain".into())?
                                .0
                                .into();
                        }
                    }
                    (Property::HtmlBody, EmailValue::BodyPartList { value }) => {
                        if let Some(body_part) = value.first() {
                            builder.html_body = body_part
                                .parse(self, account_id, body_values, "text/html".into())?
                                .0
                                .into();
                        }
                    }
                    (Property::Attachments, EmailValue::BodyPartList { value }) => {
                        let mut attachments = Vec::with_capacity(value.len());
                        for attachment in value {
                            attachments
                                .push(attachment.parse(self, account_id, body_values, None)?.0);
                        }
                        builder.attachments = attachments.into();
                    }
                    (Property::BodyStructure, EmailValue::BodyPart { value }) => {
                        let (mut mime_part, sub_parts) =
                            value.parse(self, account_id, body_values, None)?;

                        if let Some(sub_parts) = sub_parts {
                            let mut stack = Vec::new();
                            let mut it = sub_parts.iter();

                            loop {
                                while let Some(part) = it.next() {
                                    let (sub_mime_part, sub_parts) =
                                        part.parse(self, account_id, body_values, None)?;
                                    if let Some(sub_parts) = sub_parts {
                                        stack.push((mime_part, it));
                                        mime_part = sub_mime_part;
                                        it = sub_parts.iter();
                                    } else {
                                        mime_part.add_part(sub_mime_part);
                                    }
                                }
                                if let Some((mut prev_mime_part, prev_it)) = stack.pop() {
                                    prev_mime_part.add_part(mime_part);
                                    mime_part = prev_mime_part;
                                    it = prev_it;
                                } else {
                                    break;
                                }
                            }
                        }

                        builder.body = mime_part.into();
                    }
                    (Property::Header(header), value) => match (header.form, value) {
                        (HeaderForm::Raw, EmailValue::Text { value }) => {
                            builder = builder.header(header.header.as_str(), Raw::from(value));
                        }
                        (HeaderForm::Raw, EmailValue::TextList { value }) => {
                            builder = builder
                                .headers(header.header.as_str(), value.iter().map(Raw::from));
                        }
                        (HeaderForm::Date, EmailValue::Date { value }) => {
                            builder = builder.header(header.header.as_str(), Date::from(value));
                        }
                        (HeaderForm::Date, EmailValue::DateList { value }) => {
                            builder = builder
                                .headers(header.header.as_str(), value.iter().map(Date::from));
                        }
                        (HeaderForm::Text, EmailValue::Text { value }) => {
                            builder = builder.header(header.header.as_str(), Text::from(value));
                        }
                        (HeaderForm::Text, EmailValue::TextList { value }) => {
                            builder = builder
                                .headers(header.header.as_str(), value.iter().map(Text::from));
                        }
                        (HeaderForm::URLs, EmailValue::TextList { value }) => {
                            builder =
                                builder.header(header.header.as_str(), URL::from(value.as_slice()));
                        }
                        (HeaderForm::URLs, EmailValue::TextListMany { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|u| URL::from(u.as_slice())),
                            );
                        }
                        (HeaderForm::MessageIds, EmailValue::TextList { value }) => {
                            builder = builder
                                .header(header.header.as_str(), MessageId::from(value.as_slice()));
                        }
                        (HeaderForm::MessageIds, EmailValue::TextListMany { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|m| MessageId::from(m.as_slice())),
                            );
                        }
                        (HeaderForm::Addresses, EmailValue::Addresses { value }) => {
                            builder = builder
                                .header(header.header.as_str(), Address::from(value.as_slice()));
                        }
                        (HeaderForm::Addresses, EmailValue::AddressesList { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|v| Address::from(v.as_slice())),
                            );
                        }
                        (HeaderForm::GroupedAddresses, EmailValue::GroupedAddresses { value }) => {
                            builder = builder
                                .header(header.header.as_str(), Address::from(value.as_slice()));
                        }
                        (
                            HeaderForm::GroupedAddresses,
                            EmailValue::GroupedAddressesList { value },
                        ) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|v| Address::from(v.as_slice())),
                            );
                        }
                        _ => (),
                    },
                    _ => (),
                }
            }

            // Make sure the message is at least in one mailbox
            if !fields.has_tags(&Property::MailboxIds) {
                return Err(SetError::new(
                    SetErrorType::InvalidProperties,
                    "Message has to belong to at least one mailbox.",
                ));
            }

            // Make sure the message is not empty
            if builder.headers.is_empty()
                && builder.body.is_none()
                && builder.html_body.is_none()
                && builder.text_body.is_none()
                && builder.attachments.is_none()
            {
                return Err(SetError::new(
                    SetErrorType::InvalidProperties,
                    "Message has to have at least one header or body part.",
                ));
            }

            // Store blob
            let mut blob = Vec::with_capacity(1024);
            builder.write_to(&mut blob).map_err(|_| {
                StoreError::SerializeError("Failed to write to memory.".to_string())
            })?;
            let blob_id = self.blob_store(&blob)?;
            let raw_blob: JMAPBlob = (&blob_id).into();

            // Add mailbox tags
            for mailbox_tag in fields.get_tags(&Property::MailboxIds).unwrap() {
                helper
                    .changes
                    .log_child_update(Collection::Mailbox, mailbox_tag.as_id() as store::JMAPId);
            }

            // Parse message
            // TODO: write parsed message directly to store, avoid parsing it again.
            let size = blob.len();
            self.mail_parse(document, blob_id, &blob, received_at)?;
            fields.insert(document)?;

            // Lock collection
            let lock = self.lock_account(account_id, Collection::Mail);

            // Obtain thread Id
            let thread_id = self.mail_set_thread(&mut helper.changes, document)?;

            // Build email result
            let mut email = Email::default();
            email.insert(
                Property::Id,
                JMAPId::from_parts(thread_id, document.document_id),
            );
            email.insert(Property::BlobId, raw_blob);
            email.insert(Property::ThreadId, JMAPId::from(thread_id));
            email.insert(Property::Size, size);

            Ok((email, lock.into()))
        })?;

        helper.update(|id, item, helper, document| {
            let current_fields = self
                .get_orm::<Email>(account_id, id.get_document_id())?
                .ok_or_else(|| SetError::new_err(SetErrorType::NotFound))?;
            let mut fields = TinyORM::track_changes(&current_fields);

            for (property, value) in item.properties {
                match (property, value) {
                    (Property::MailboxIds, EmailValue::MailboxIds { value, set }) => {
                        if set {
                            fields.untag_all(&Property::MailboxIds);

                            for (mailbox_id, set) in value {
                                let mailbox_id = mailbox_id.as_id();

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_property(
                                        Property::MailboxIds,
                                        format!("mailboxId {} does not exist.", mailbox_id),
                                    ));
                                }
                            }
                        } else {
                            for (mailbox_id, set) in value {
                                let mailbox_id = mailbox_id.as_id();

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    } else {
                                        fields.untag(
                                            &Property::MailboxIds,
                                            &Tag::Id(mailbox_id.into()),
                                        );
                                    }
                                } else {
                                    return Err(SetError::invalid_property(
                                        Property::MailboxIds,
                                        format!("mailboxId {} does not exist.", mailbox_id),
                                    ));
                                }
                            }
                        }
                    }
                    (Property::Keywords, EmailValue::Keywords { value, set }) => {
                        if set {
                            fields.untag_all(&Property::Keywords);

                            for (keyword, set) in value {
                                if set {
                                    fields.tag(Property::Keywords, keyword.tag);
                                }
                            }
                        } else {
                            for (keyword, set) in value {
                                if set {
                                    fields.tag(Property::Keywords, keyword.tag);
                                } else {
                                    fields.untag(&Property::Keywords, &keyword.tag);
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }

            // Make sure the message is at least in one mailbox
            if !fields.has_tags(&Property::MailboxIds) {
                return Err(SetError::new(
                    SetErrorType::InvalidProperties,
                    "Message has to belong to at least one mailbox.",
                ));
            }

            // Set all current mailboxes as changed if the Seen tag changed
            let mut changed_mailboxes = HashSet::new();
            if current_fields
                .get_changed_tags(&fields, &Property::Keywords)
                .iter()
                .any(|keyword| matches!(keyword, Tag::Static(k_id) if k_id == &Keyword::SEEN))
            {
                for mailbox_tag in fields.get_tags(&Property::MailboxIds).unwrap() {
                    changed_mailboxes.insert(mailbox_tag.as_id());
                }
            }

            // Add all new or removed mailboxes
            for changed_mailbox_tag in
                current_fields.get_changed_tags(&fields, &Property::MailboxIds)
            {
                changed_mailboxes.insert(changed_mailbox_tag.as_id());
            }

            // Log mailbox changes
            if !changed_mailboxes.is_empty() {
                for changed_mailbox_id in changed_mailboxes {
                    helper
                        .changes
                        .log_child_update(Collection::Mailbox, changed_mailbox_id);
                }
            }

            // Merge changes
            current_fields.merge_validate(document, fields)?;

            Ok(None)
        })?;

        helper.destroy(|id, _helper, document| {
            self.mail_delete(account_id, document)?;
            Ok(())
        })?;

        helper.into_response()
    }

    fn mail_delete(
        &self,
        account_id: AccountId,
        document: &mut Document,
    ) -> store::Result<Option<DocumentId>> {
        let document_id = document.document_id;
        let metadata_blob_id = if let Some(metadata_blob_id) = self.get_document_value::<BlobId>(
            account_id,
            Collection::Mail,
            document_id,
            MessageField::Metadata.into(),
        )? {
            metadata_blob_id
        } else {
            return Ok(None);
        };

        // Remove index entries
        MessageData::from_metadata(
            &self
                .blob_get(&metadata_blob_id)?
                .ok_or(StoreError::DataCorruption)?,
        )
        .ok_or(StoreError::DataCorruption)?
        .build_index(document, false)?;

        // Remove thread related data
        let thread_id = self
            .get_document_value::<DocumentId>(
                account_id,
                Collection::Mail,
                document_id,
                MessageField::ThreadId.into(),
            )?
            .ok_or(StoreError::DataCorruption)?;
        document.tag(
            MessageField::ThreadId,
            Tag::Id(thread_id),
            IndexOptions::new().clear(),
        );
        document.number(
            MessageField::ThreadId,
            thread_id,
            IndexOptions::new().store().clear(),
        );

        // Unlink metadata
        document.blob(metadata_blob_id, IndexOptions::new().clear());
        document.binary(
            MessageField::Metadata,
            Vec::with_capacity(0),
            IndexOptions::new().clear(),
        );

        // Delete ORM
        let fields = self
            .get_orm::<Email>(account_id, document_id)?
            .ok_or(StoreError::DataCorruption)?;
        fields.delete(document);

        Ok(thread_id.into())
    }
}

impl EmailBodyPart {
    fn parse<'y, T>(
        &'y self,
        store: &JMAPStore<T>,
        account_id: AccountId,
        body_values: Option<&'y HashMap<String, EmailBodyValue>>,
        strict_type: Option<&'static str>,
    ) -> jmap::error::set::Result<(MimePart<'y>, Option<&'y Vec<EmailBodyPart>>), Property>
    where
        T: for<'x> Store<'x> + 'static,
    {
        let content_type = self
            .get_text(BodyProperty::Type)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "text/plain".to_string());

        if matches!(strict_type, Some(strict_type) if strict_type != content_type) {
            return Err(SetError::new(
                SetErrorType::InvalidProperties,
                format!(
                    "Expected one body part of type \"{}\"",
                    strict_type.unwrap()
                ),
            ));
        }

        let is_multipart = content_type.starts_with("multipart/");
        let mut mime_part = MimePart {
            headers: BTreeMap::new(),
            contents: if is_multipart {
                BodyPart::Multipart(vec![])
            } else if let Some(part_id) = self.get_text(BodyProperty::PartId) {
                BodyPart::Text(
                    body_values
                        .as_ref()
                        .ok_or_else(|| {
                            SetError::new(
                                SetErrorType::InvalidProperties,
                                "Missing \"bodyValues\" object containing partId.".to_string(),
                            )
                        })?
                        .get(part_id)
                        .ok_or_else(|| {
                            SetError::new(
                                SetErrorType::InvalidProperties,
                                format!("Missing body value for partId \"{}\"", part_id),
                            )
                        })?
                        .value
                        .as_str()
                        .into(),
                )
            } else if let Some(blob_id) = self.get_blob(BodyProperty::BlobId) {
                BodyPart::Binary(
                    store
                        .blob_jmap_get(account_id, blob_id, get_message_part)
                        .map_err(|_| {
                            SetError::new(SetErrorType::BlobNotFound, "Failed to fetch blob.")
                        })?
                        .ok_or_else(|| {
                            SetError::new(
                                SetErrorType::BlobNotFound,
                                "blobId does not exist on this server.",
                            )
                        })?
                        .into(),
                )
            } else {
                return Err(SetError::new(
                    SetErrorType::InvalidProperties,
                    "Expected a \"partId\" or \"blobId\" field in body part.".to_string(),
                ));
            },
        };

        let mut content_type = ContentType::new(content_type);
        if !is_multipart {
            if content_type.c_type.starts_with("text/") {
                if matches!(mime_part.contents, BodyPart::Text(_)) {
                    content_type
                        .attributes
                        .insert("charset".into(), "utf-8".into());
                } else if let Some(charset) = self.get_text(BodyProperty::Charset) {
                    content_type
                        .attributes
                        .insert("charset".into(), charset.into());
                };
            }

            match (
                self.get_text(BodyProperty::Disposition),
                self.get_text(BodyProperty::Name),
            ) {
                (Some(disposition), Some(filename)) => {
                    mime_part.headers.insert(
                        "Content-Disposition".into(),
                        ContentType::new(disposition)
                            .attribute("filename", filename)
                            .into(),
                    );
                }
                (Some(disposition), None) => {
                    mime_part.headers.insert(
                        "Content-Disposition".into(),
                        ContentType::new(disposition).into(),
                    );
                }
                (None, Some(filename)) => {
                    content_type
                        .attributes
                        .insert("name".into(), filename.into());
                }
                (None, None) => (),
            };
        }

        mime_part
            .headers
            .insert("Content-Type".into(), content_type.into());

        let mut sub_parts = None;

        for (property, value) in self.properties.iter() {
            match (property, value) {
                (BodyProperty::Language, EmailValue::TextList { value }) if !is_multipart => {
                    mime_part.headers.insert(
                        "Content-Language".into(),
                        Text::new(value.join(", ")).into(),
                    );
                }
                (BodyProperty::Cid, EmailValue::Text { value }) if !is_multipart => {
                    mime_part
                        .headers
                        .insert("Content-ID".into(), MessageId::new(value).into());
                }
                (BodyProperty::Location, EmailValue::Text { value }) if !is_multipart => {
                    mime_part
                        .headers
                        .insert("Content-Location".into(), Text::new(value).into());
                }
                (BodyProperty::Headers, EmailValue::Headers { value }) => {
                    for header in value {
                        mime_part
                            .headers
                            .insert(header.name.as_str().into(), Raw::from(&header.value).into());
                    }
                }
                (BodyProperty::Header(header), value) => match value {
                    EmailValue::Text { value } => {
                        mime_part
                            .headers
                            .insert(header.header.as_str().into(), Raw::from(value).into());
                    }
                    EmailValue::TextList { value } => {
                        for value in value {
                            mime_part
                                .headers
                                .insert(header.header.as_str().into(), Raw::from(value).into());
                        }
                    }
                    _ => (),
                },
                (BodyProperty::Subparts, EmailValue::BodyPartList { value }) => {
                    sub_parts = Some(value);
                }
                _ => (),
            }
        }

        Ok((mime_part, if is_multipart { sub_parts } else { None }))
    }
}
