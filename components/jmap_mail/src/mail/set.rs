/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use super::get::{BlobResult, JMAPGetMail};
use super::schema::{
    BodyProperty, Email, EmailBodyPart, EmailBodyValue, HeaderForm, Keyword, Property, Value,
};
use super::sharing::JMAPShareMail;
use super::{HeaderName, MessageData, MessageField};
use crate::mail::import::JMAPMailImport;
use jmap::error::set::{SetError, SetErrorType};
use jmap::jmap_store::set::{SetHelper, SetObject};
use jmap::orm::{serialize::JMAPOrm, TinyORM};
use jmap::request::set::{SetRequest, SetResponse};
use jmap::request::{ACLEnforce, MaybeIdReference, ResultReference};
use jmap::types::blob::JMAPBlob;
use jmap::types::jmap::JMAPId;
use mail_builder::headers::address::Address;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use mail_builder::headers::url::URL;
use mail_builder::mime::{BodyPart, MimePart};
use mail_builder::MessageBuilder;
use mail_parser::{Message, RfcHeader};
use std::sync::Arc;
use store::ahash::AHashSet;
use store::blob::BlobId;
use store::core::acl::{ACLToken, ACL};
use store::core::collection::Collection;
use store::core::document::Document;
use store::core::error::StoreError;
use store::core::tag::Tag;
use store::core::vec_map::VecMap;
use store::serialize::StoreDeserialize;
use store::tracing::error;
use store::write::batch::WriteBatch;
use store::write::options::{IndexOptions, Options};
use store::{AccountId, DocumentId, JMAPStore, SharedBitmap, Store};

impl SetObject for Email {
    type SetArguments = ();

    type NextCall = SetRequest<Email>;

    fn eval_id_references(&mut self, mut fnc: impl FnMut(&str) -> Option<JMAPId>) {
        if let Some(Value::MailboxIds { value, .. }) =
            self.properties.get_mut(&Property::MailboxIds)
        {
            if value
                .keys()
                .any(|k| matches!(k, MaybeIdReference::Reference(_)))
            {
                let mut new_values = VecMap::with_capacity(value.len());

                for (id, value) in std::mem::take(value).into_iter() {
                    if let MaybeIdReference::Reference(id) = &id {
                        if let Some(id) = fnc(id) {
                            new_values.append(MaybeIdReference::Value(id), value);
                            continue;
                        }
                    }
                    new_values.append(id, value);
                }

                *value = new_values;
            }
        }
    }

    fn eval_result_references(
        &mut self,
        mut fnc: impl FnMut(&ResultReference) -> Option<Vec<u64>>,
    ) {
        for (property, entry) in self.properties.iter_mut() {
            if let (Property::MailboxIds, Value::ResultReference { value }) = (property, &entry) {
                if let Some(value) = fnc(value) {
                    *entry = Value::MailboxIds {
                        value: value
                            .into_iter()
                            .map(|v| (MaybeIdReference::Value(v.into()), true))
                            .collect(),
                        set: true,
                    };
                }
            }
        }
    }

    fn set_property(&mut self, _property: Self::Property, _value: Self::Value) {}
}

pub trait JMAPSetMail<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_set(&self, request: SetRequest<Email>) -> jmap::Result<SetResponse<Email>>;
    fn mail_delete(
        &self,
        account_id: AccountId,
        batch: Option<&mut WriteBatch>,
        document: &mut Document,
    ) -> store::Result<Option<JMAPId>>;
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

        helper.disable_write_batch();

        helper.create(|_create_id, item, helper, document| {
            let mut builder = MessageBuilder::new();
            let mut fields = TinyORM::<Email>::new();

            let mut received_at = None;
            let body_values = item
                .properties
                .get(&Property::BodyValues)
                .and_then(|b| match b {
                    Value::BodyValues { value } => Some(value),
                    _ => None,
                });
            let max_size_attachments = helper.store.config.mail_attachments_max_size;
            let mut size_attachments = 0;

            for (property, value) in &item.properties {
                match (property, value) {
                    (Property::MailboxIds, Value::MailboxIds { value, set }) => {
                        if *set {
                            fields.untag_all(&Property::MailboxIds);

                            for (mailbox_id, set) in value {
                                let mailbox_id =
                                    helper.unwrap_id_reference(Property::MailboxIds, mailbox_id)?;

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if *set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::MailboxIds)
                                        .with_description(format!(
                                            "mailboxId {} does not exist.",
                                            mailbox_id
                                        )));
                                }
                            }
                        } else {
                            for (mailbox_id, set) in value {
                                let mailbox_id =
                                    helper.unwrap_id_reference(Property::MailboxIds, mailbox_id)?;

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if *set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::MailboxIds)
                                        .with_description(format!(
                                            "mailboxId {} does not exist.",
                                            mailbox_id
                                        )));
                                }
                            }
                        }
                    }
                    (Property::Keywords, Value::Keywords { value, set }) => {
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
                    (Property::ReceivedAt, Value::Date { value }) => {
                        received_at = value.timestamp().into();
                    }
                    (
                        Property::MessageId | Property::InReplyTo | Property::References,
                        Value::TextList { value },
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
                        Value::Addresses { value },
                    ) => {
                        builder = builder.header(
                            property.as_rfc_header(),
                            Address::new_list(value.iter().map(|x| x.into()).collect()),
                        );
                    }
                    (Property::Subject, Value::Text { value }) => {
                        builder = builder.subject(value);
                    }
                    (Property::SentAt, Value::Date { value }) => {
                        builder = builder.date(Date::new(value.timestamp()));
                    }
                    (Property::TextBody, Value::BodyPartList { value }) => {
                        if item.properties.contains_key(&Property::BodyStructure) {
                            return Err(SetError::invalid_properties()
                                .with_properties([Property::TextBody, Property::BodyStructure])
                                .with_description(
                                    "Cannot set both \"textBody\" and \"bodyStructure\".",
                                ));
                        } else if value.len() > 1 {
                            return Err(SetError::invalid_properties()
                                .with_property(Property::TextBody)
                                .with_description("Only one \"textBody\" part is allowed."));
                        }

                        if let Some(body_part) = value.first() {
                            let text_body = body_part
                                .parse(
                                    self,
                                    &helper.acl,
                                    account_id,
                                    body_values,
                                    "text/plain".into(),
                                )?
                                .0;
                            if max_size_attachments > 0 {
                                size_attachments += text_body.size();
                                if size_attachments > max_size_attachments {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::TextBody)
                                        .with_description(format!(
                                            "Message exceeds maximum size of {} bytes.",
                                            max_size_attachments
                                        )));
                                }
                            }
                            builder.text_body = text_body.into();
                        }
                    }
                    (Property::HtmlBody, Value::BodyPartList { value }) => {
                        if item.properties.contains_key(&Property::BodyStructure) {
                            return Err(SetError::invalid_properties()
                                .with_properties([Property::HtmlBody, Property::BodyStructure])
                                .with_description(
                                    "Cannot set both \"htmlBody\" and \"bodyStructure\".",
                                ));
                        } else if value.len() > 1 {
                            return Err(SetError::invalid_properties()
                                .with_property(Property::HtmlBody)
                                .with_description("Only one \"htmlBody\" part is allowed."));
                        }

                        if let Some(body_part) = value.first() {
                            let html_body = body_part
                                .parse(
                                    self,
                                    &helper.acl,
                                    account_id,
                                    body_values,
                                    "text/html".into(),
                                )?
                                .0;
                            if max_size_attachments > 0 {
                                size_attachments += html_body.size();
                                if size_attachments > max_size_attachments {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::HtmlBody)
                                        .with_description(format!(
                                            "Message exceeds maximum size of {} bytes.",
                                            max_size_attachments
                                        )));
                                }
                            }
                            builder.html_body = html_body.into();
                        }
                    }
                    (Property::Attachments, Value::BodyPartList { value }) => {
                        if item.properties.contains_key(&Property::BodyStructure) {
                            return Err(SetError::invalid_properties()
                                .with_properties([Property::Attachments, Property::BodyStructure])
                                .with_description(
                                    "Cannot set both \"attachments\" and \"bodyStructure\".",
                                ));
                        }

                        let mut attachments = Vec::with_capacity(value.len());
                        for attachment in value {
                            let attachment = attachment
                                .parse(self, &helper.acl, account_id, body_values, None)?
                                .0;
                            if max_size_attachments > 0 {
                                size_attachments += attachment.size();
                                if size_attachments > max_size_attachments {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::Attachments)
                                        .with_description(format!(
                                            "Message exceeds maximum size of {} bytes.",
                                            max_size_attachments
                                        )));
                                }
                            }
                            attachments.push(attachment);
                        }
                        builder.attachments = attachments.into();
                    }
                    (Property::BodyStructure, Value::BodyPart { value }) => {
                        let (mut mime_part, sub_parts) =
                            value.parse(self, &helper.acl, account_id, body_values, None)?;

                        if let Some(sub_parts) = sub_parts {
                            let mut stack = Vec::new();
                            let mut it = sub_parts.iter();

                            loop {
                                while let Some(part) = it.next() {
                                    let (sub_mime_part, sub_parts) = part.parse(
                                        self,
                                        &helper.acl,
                                        account_id,
                                        body_values,
                                        None,
                                    )?;

                                    if max_size_attachments > 0 {
                                        size_attachments += sub_mime_part.size();
                                        if size_attachments > max_size_attachments {
                                            return Err(SetError::invalid_properties()
                                                .with_property(Property::BodyStructure)
                                                .with_description(format!(
                                                    "Message exceeds maximum size of {} bytes.",
                                                    max_size_attachments
                                                )));
                                        }
                                    }

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
                        (HeaderForm::Raw, Value::Text { value }) => {
                            builder = builder.header(header.header.as_str(), Raw::from(value));
                        }
                        (HeaderForm::Raw, Value::TextList { value }) => {
                            builder = builder
                                .headers(header.header.as_str(), value.iter().map(Raw::from));
                        }
                        (HeaderForm::Date, Value::Date { value }) => {
                            builder = builder
                                .header(header.header.as_str(), Date::new(value.timestamp()));
                        }
                        (HeaderForm::Date, Value::DateList { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|v| Date::new(v.timestamp())),
                            );
                        }
                        (HeaderForm::Text, Value::Text { value }) => {
                            builder = builder.header(header.header.as_str(), Text::from(value));
                        }
                        (HeaderForm::Text, Value::TextList { value }) => {
                            builder = builder
                                .headers(header.header.as_str(), value.iter().map(Text::from));
                        }
                        (HeaderForm::URLs, Value::TextList { value }) => {
                            builder =
                                builder.header(header.header.as_str(), URL::from(value.as_slice()));
                        }
                        (HeaderForm::URLs, Value::TextListMany { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|u| URL::from(u.as_slice())),
                            );
                        }
                        (HeaderForm::MessageIds, Value::TextList { value }) => {
                            builder = builder
                                .header(header.header.as_str(), MessageId::from(value.as_slice()));
                        }
                        (HeaderForm::MessageIds, Value::TextListMany { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|m| MessageId::from(m.as_slice())),
                            );
                        }
                        (HeaderForm::Addresses, Value::Addresses { value }) => {
                            builder = builder.header(
                                header.header.as_str(),
                                Address::new_list(value.iter().map(|x| x.into()).collect()),
                            );
                        }
                        (HeaderForm::Addresses, Value::AddressesList { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|v| {
                                    Address::new_list(v.iter().map(|x| x.into()).collect())
                                }),
                            );
                        }
                        (HeaderForm::GroupedAddresses, Value::GroupedAddresses { value }) => {
                            builder = builder.header(
                                header.header.as_str(),
                                Address::new_list(value.iter().map(|x| x.into()).collect()),
                            );
                        }
                        (HeaderForm::GroupedAddresses, Value::GroupedAddressesList { value }) => {
                            builder = builder.headers(
                                header.header.as_str(),
                                value.iter().map(|v| {
                                    Address::new_list(v.iter().map(|x| x.into()).collect())
                                }),
                            );
                        }
                        _ => (),
                    },
                    _ => (),
                }
            }

            // Make sure the message is at least in one mailbox
            if !fields.has_tags(&Property::MailboxIds) {
                return Err(SetError::invalid_properties()
                    .with_property(Property::MailboxIds)
                    .with_description("Message has to belong to at least one mailbox."));
            }

            // Check ACLs
            if helper.acl.is_shared(helper.account_id) {
                let allowed_folders = helper.store.mail_shared_folders(
                    helper.account_id,
                    &helper.acl.member_of,
                    ACL::AddItems,
                )?;

                for mailbox in fields.get_tags(&Property::MailboxIds).unwrap() {
                    let mailbox_id = mailbox.as_id();
                    if !allowed_folders.has_access(mailbox_id) {
                        return Err(SetError::forbidden().with_description(format!(
                            "You are not allowed to add messages to folder {}.",
                            JMAPId::from(mailbox_id)
                        )));
                    }
                }
            }

            // Make sure the message is not empty
            if builder.headers.is_empty()
                && builder.body.is_none()
                && builder.html_body.is_none()
                && builder.text_body.is_none()
                && builder.attachments.is_none()
            {
                return Err(SetError::invalid_properties()
                    .with_description("Message has to have at least one header or body part."));
            }

            // In test, sort headers to avoid randomness
            #[cfg(feature = "debug")]
            {
                builder
                    .headers
                    .sort_unstable_by(|a, b| match a.0.cmp(&b.0) {
                        std::cmp::Ordering::Equal => a.1.cmp(&b.1),
                        ord => ord,
                    });
            }

            // Write blob
            let mut blob = Vec::with_capacity(1024);
            builder.write_to(&mut blob).map_err(|_| {
                StoreError::SerializeError("Failed to write to memory.".to_string())
            })?;
            let blob_id = BlobId::new_external(&blob);
            let raw_blob: JMAPBlob = (&blob_id).into();

            // Add mailbox tags
            for mailbox_tag in fields.get_tags(&Property::MailboxIds).unwrap() {
                helper
                    .changes
                    .log_child_update(Collection::Mailbox, mailbox_tag.as_id() as store::JMAPId);
            }

            // Parse message
            let size = blob.len();
            self.mail_parse_item(
                document,
                blob_id.clone(),
                Message::parse(&blob).ok_or_else(|| {
                    SetError::invalid_properties().with_description("Failed to parse e-mail.")
                })?,
                received_at,
            )?;
            fields.insert(document)?;

            // Store blob
            self.blob_store(&blob_id, blob)?;

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

            Ok(email)
        })?;

        helper.update(|id, item, helper, document| {
            let current_fields = self
                .get_orm::<Email>(account_id, id.get_document_id())?
                .ok_or_else(|| SetError::new(SetErrorType::NotFound))?;
            let mut fields = TinyORM::track_changes(&current_fields);

            for (property, value) in item.properties {
                match (property, value) {
                    (Property::MailboxIds, Value::MailboxIds { value, set }) => {
                        if set {
                            fields.untag_all(&Property::MailboxIds);

                            for (mailbox_id, set) in value {
                                let mailbox_id = helper
                                    .unwrap_id_reference(Property::MailboxIds, &mailbox_id)?;

                                if mailbox_ids.contains(mailbox_id.into()) {
                                    if set {
                                        fields
                                            .tag(Property::MailboxIds, Tag::Id(mailbox_id.into()));
                                    }
                                } else {
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::MailboxIds)
                                        .with_description(format!(
                                            "mailboxId {} does not exist.",
                                            mailbox_id
                                        )));
                                }
                            }
                        } else {
                            for (mailbox_id, set) in value {
                                let mailbox_id = helper
                                    .unwrap_id_reference(Property::MailboxIds, &mailbox_id)?;

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
                                    return Err(SetError::invalid_properties()
                                        .with_property(Property::MailboxIds)
                                        .with_description(format!(
                                            "mailboxId {} does not exist.",
                                            mailbox_id
                                        )));
                                }
                            }
                        }
                    }
                    (Property::Keywords, Value::Keywords { value, set }) => {
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
                return Err(SetError::invalid_properties()
                    .with_property(Property::MailboxIds)
                    .with_description("Message has to belong to at least one mailbox."));
            }
            let changed_tags = current_fields.get_changed_tags(&fields, &Property::Keywords);

            // Check ACLs
            if helper.acl.is_shared(helper.account_id) {
                // All folders have to allow insertions
                let added_mailboxes = current_fields.get_added_tags(&fields, &Property::MailboxIds);
                if !added_mailboxes.is_empty() {
                    let allowed_folders = helper.store.mail_shared_folders(
                        helper.account_id,
                        &helper.acl.member_of,
                        ACL::AddItems,
                    )?;
                    for mailbox in added_mailboxes {
                        let mailbox_id = mailbox.as_id();
                        if !allowed_folders.has_access(mailbox_id) {
                            return Err(SetError::forbidden().with_description(format!(
                                "You are not allowed to add messages to folder {}.",
                                JMAPId::from(mailbox_id)
                            )));
                        }
                    }
                }

                // All folders have to allow deletions
                let added_mailboxes =
                    current_fields.get_removed_tags(&fields, &Property::MailboxIds);
                if !added_mailboxes.is_empty() {
                    let allowed_folders = helper.store.mail_shared_folders(
                        helper.account_id,
                        &helper.acl.member_of,
                        ACL::AddItems,
                    )?;
                    for mailbox in added_mailboxes {
                        let mailbox_id = mailbox.as_id();
                        if !allowed_folders.has_access(mailbox_id) {
                            return Err(SetError::forbidden().with_description(format!(
                                "You are not allowed to delete messages from folder {}.",
                                JMAPId::from(mailbox_id)
                            )));
                        }
                    }
                }

                // Enforce setSeen and setKeywords
                if !changed_tags.is_empty()
                    && !helper
                        .store
                        .mail_shared_messages(
                            helper.account_id,
                            &helper.acl.member_of,
                            ACL::ModifyItems,
                        )?
                        .has_access(document.document_id)
                {
                    return Err(SetError::forbidden()
                        .with_description("You are not allowed to change keywords."));
                }
            }

            // Set all current mailboxes as changed if the Seen tag changed
            let mut changed_mailboxes = AHashSet::default();
            if changed_tags
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

        helper.destroy(|_id, helper, document| {
            // Check ACLs
            if helper.acl.is_shared(helper.account_id)
                && !helper
                    .store
                    .mail_shared_messages(
                        helper.account_id,
                        &helper.acl.member_of,
                        ACL::RemoveItems,
                    )?
                    .has_access(document.document_id)
            {
                return Err(SetError::forbidden()
                    .with_description("You are not allowed to delete this message."));
            }

            self.mail_delete(account_id, Some(&mut helper.changes), document)?;
            Ok(())
        })?;

        helper.into_response()
    }

    fn mail_delete(
        &self,
        account_id: AccountId,
        batch: Option<&mut WriteBatch>,
        document: &mut Document,
    ) -> store::Result<Option<JMAPId>> {
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
        MessageData::deserialize(&self.blob_get(&metadata_blob_id)?.ok_or_else(|| {
            StoreError::NotFound(format!(
                "Message data blob for {}:{} not found.",
                account_id, document_id
            ))
        })?)
        .ok_or_else(|| {
            StoreError::DataCorruption(format!(
                "Failed to deserialize message data for {}:{}.",
                account_id, document_id
            ))
        })?
        .build_index(document, false)?;

        // Remove thread related data
        let thread_id = self
            .get_document_value::<DocumentId>(
                account_id,
                Collection::Mail,
                document_id,
                MessageField::ThreadId.into(),
            )?
            .ok_or_else(|| {
                StoreError::NotFound(format!(
                    "Failed to fetch threadId for {}:{}.",
                    account_id, document_id
                ))
            })?;
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

        // Fetch ORM
        let fields = self
            .get_orm::<Email>(account_id, document_id)?
            .ok_or_else(|| {
                StoreError::DataCorruption(format!(
                    "Failed to fetch Email ORM for {}:{}.",
                    account_id, document_id
                ))
            })?;

        // Log thread and mailbox changes
        if let Some(batch) = batch {
            if let Some(message_doc_ids) = self.get_tag(
                account_id,
                Collection::Mail,
                MessageField::ThreadId.into(),
                Tag::Id(thread_id),
            )? {
                if message_doc_ids.len() > 1 {
                    batch.log_child_update(Collection::Thread, thread_id);
                } else {
                    batch.log_delete(Collection::Thread, thread_id);
                }
            } else {
                batch.log_child_update(Collection::Thread, thread_id);
            }
            if let Some(mailbox_ids) = fields.get_tags(&Property::MailboxIds) {
                for mailbox_id in mailbox_ids {
                    batch.log_child_update(Collection::Mailbox, mailbox_id.as_id());
                }
            }
        }

        // Delete ORM
        fields.delete(document);

        Ok(JMAPId::from_parts(thread_id, document_id).into())
    }
}

impl EmailBodyPart {
    fn parse<'y, T>(
        &'y self,
        store: &JMAPStore<T>,
        acl: &Arc<ACLToken>,
        account_id: AccountId,
        body_values: Option<&'y VecMap<String, EmailBodyValue>>,
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
            return Err(SetError::invalid_properties().with_description(format!(
                "Expected one body part of type \"{}\"",
                strict_type.unwrap()
            )));
        }

        let is_multipart = content_type.starts_with("multipart/");
        let mut mime_part = MimePart {
            headers: Vec::new(),
            contents: if is_multipart {
                BodyPart::Multipart(vec![])
            } else if let Some(part_id) = self.get_text(BodyProperty::PartId) {
                if self.properties.contains_key(&BodyProperty::BlobId) {
                    return Err(SetError::invalid_properties().with_description(
                        "Cannot specify both \"partId\" and \"bodyValues\".".to_string(),
                    ));
                } else if self.properties.contains_key(&BodyProperty::Charset) {
                    return Err(SetError::invalid_properties().with_description(
                        "Cannot specify a character set when providing a \"partId\".".to_string(),
                    ));
                }
                BodyPart::Text(
                    body_values
                        .as_ref()
                        .ok_or_else(|| {
                            SetError::invalid_properties().with_description(
                                "Missing \"bodyValues\" object containing partId.".to_string(),
                            )
                        })?
                        .get(part_id)
                        .ok_or_else(|| {
                            SetError::invalid_properties().with_description(format!(
                                "Missing body value for partId \"{}\"",
                                part_id
                            ))
                        })?
                        .value
                        .as_str()
                        .into(),
                )
            } else if let Some(blob_id) = self.get_blob(BodyProperty::BlobId) {
                BodyPart::Binary(match store.mail_blob_get(account_id, acl, blob_id) {
                    Ok(BlobResult::Blob(bytes)) => bytes.into(),
                    Ok(BlobResult::NotFound) => {
                        return Err(SetError::new(SetErrorType::BlobNotFound).with_description(
                            format!("blob {} does not exist on this server.", blob_id),
                        ));
                    }
                    Ok(BlobResult::Unauthorized) => {
                        return Err(SetError::forbidden().with_description(format!(
                            "You do not have access to blob {}.",
                            blob_id
                        )));
                    }
                    Err(err) => {
                        error!("Failed to retrieve blob: {}", err);
                        return Err(SetError::new(SetErrorType::BlobNotFound)
                            .with_description(format!("Failed to retrieve blob {}.", blob_id)));
                    }
                })
            } else {
                return Err(SetError::invalid_properties().with_description(
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
                        .push(("charset".into(), "utf-8".into()));
                } else if let Some(charset) = self.get_text(BodyProperty::Charset) {
                    content_type
                        .attributes
                        .push(("charset".into(), charset.into()));
                };
            }

            match (
                self.get_text(BodyProperty::Disposition),
                self.get_text(BodyProperty::Name),
            ) {
                (Some(disposition), Some(filename)) => {
                    mime_part.headers.push((
                        "Content-Disposition".into(),
                        ContentType::new(disposition)
                            .attribute("filename", filename)
                            .into(),
                    ));
                }
                (Some(disposition), None) => {
                    mime_part.headers.push((
                        "Content-Disposition".into(),
                        ContentType::new(disposition).into(),
                    ));
                }
                (None, Some(filename)) => {
                    content_type
                        .attributes
                        .push(("name".into(), filename.into()));
                }
                (None, None) => (),
            };
        }

        mime_part
            .headers
            .push(("Content-Type".into(), content_type.into()));

        let mut sub_parts = None;

        for (property, value) in self.properties.iter() {
            match (property, value) {
                (BodyProperty::Language, Value::TextList { value }) if !is_multipart => {
                    mime_part.headers.push((
                        "Content-Language".into(),
                        Text::new(value.join(", ")).into(),
                    ));
                }
                (BodyProperty::Cid, Value::Text { value }) if !is_multipart => {
                    mime_part
                        .headers
                        .push(("Content-ID".into(), MessageId::new(value).into()));
                }
                (BodyProperty::Location, Value::Text { value }) if !is_multipart => {
                    mime_part
                        .headers
                        .push(("Content-Location".into(), Text::new(value).into()));
                }
                (BodyProperty::Headers, Value::Headers { .. }) => {
                    return Err(SetError::invalid_properties()
                        .with_description("Headers have to be set individually."));
                }
                (BodyProperty::Header(header), value) => {
                    if header.header != HeaderName::Rfc(RfcHeader::ContentTransferEncoding) {
                        match value {
                            Value::Text { value } => {
                                mime_part
                                    .headers
                                    .push((header.header.as_str().into(), Raw::from(value).into()));
                            }
                            Value::TextList { value } => {
                                for value in value {
                                    mime_part.headers.push((
                                        header.header.as_str().into(),
                                        Raw::from(value).into(),
                                    ));
                                }
                            }
                            _ => (),
                        }
                    } else {
                        return Err(SetError::invalid_properties()
                            .with_description("Cannot specify Content-Transfer-Encoding header."));
                    }
                }
                (BodyProperty::Size, _) => {
                    if self.properties.contains_key(&BodyProperty::PartId) {
                        return Err(SetError::invalid_properties().with_description(
                            "Cannot specify \"size\" when providing a \"partId\".",
                        ));
                    }
                }
                (BodyProperty::Subparts, Value::BodyPartList { value }) => {
                    sub_parts = Some(value);
                }
                _ => (),
            }
        }

        // In test, sort headers to avoid randomness
        #[cfg(feature = "debug")]
        {
            mime_part
                .headers
                .sort_unstable_by(|a, b| match a.0.cmp(&b.0) {
                    std::cmp::Ordering::Equal => a.1.cmp(&b.1),
                    ord => ord,
                });
        }

        Ok((mime_part, if is_multipart { sub_parts } else { None }))
    }
}
