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

use super::schema::{Address, EmailSubmission, Envelope, Property, Value};
use crate::identity;
use crate::identity::schema::Identity;
use crate::mail::schema::Email;
use crate::mail::{MessageData, MessageField};
use jmap::error::set::{SetError, SetErrorType};
use jmap::jmap_store::set::SetHelper;
use jmap::jmap_store::Object;
use jmap::orm::{serialize::JMAPOrm, TinyORM};
use jmap::request::set::SetResponse;
use jmap::request::{MaybeIdReference, MaybeResultReference, ResultReference};
use jmap::types::date::JMAPDate;
use jmap::types::jmap::JMAPId;
use jmap::{jmap_store::set::SetObject, request::set::SetRequest};
use mail_parser::RfcHeader;
use std::time::SystemTime;
use store::ahash::{AHashMap, AHashSet};
use store::blob::BlobId;
use store::core::collection::Collection;
use store::core::document::Document;
use store::core::error::StoreError;
use store::core::vec_map::VecMap;
use store::serialize::{StoreDeserialize, StoreSerialize};
use store::write::options::{IndexOptions, Options};
use store::{AccountId, JMAPStore, Store};

#[derive(Debug, Clone, Default)]
pub struct SetArguments {
    pub on_success_update_email: Option<VecMap<MaybeIdReference, Email>>,
    pub on_success_destroy_email: Option<Vec<MaybeIdReference>>,
}

impl SetObject for EmailSubmission {
    type SetArguments = SetArguments;

    type NextCall = SetRequest<Email>;

    fn eval_id_references(&mut self, mut fnc: impl FnMut(&str) -> Option<JMAPId>) {
        for (_, entry) in self.properties.iter_mut() {
            if let Value::IdReference { value } = entry {
                if let Some(value) = fnc(value) {
                    *entry = Value::Id { value };
                }
            }
        }
    }

    fn eval_result_references(
        &mut self,
        mut fnc: impl FnMut(&ResultReference) -> Option<Vec<u64>>,
    ) {
        for (_, entry) in self.properties.iter_mut() {
            if let Value::ResultReference { value } = entry {
                if let Some(value) = fnc(value).and_then(|mut v| v.pop()) {
                    *entry = Value::Id {
                        value: value.into(),
                    };
                }
            }
        }
    }

    fn set_property(&mut self, property: Self::Property, value: Self::Value) {
        self.properties.set(property, value);
    }
}

pub trait JMAPSetEmailSubmission<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn email_submission_set(
        &self,
        request: SetRequest<EmailSubmission>,
    ) -> jmap::Result<SetResponse<EmailSubmission>>;

    fn email_submission_delete(
        &self,
        account_id: AccountId,
        document: &mut Document,
    ) -> store::Result<()>;
}

impl<T> JMAPSetEmailSubmission<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn email_submission_set(
        &self,
        request: SetRequest<EmailSubmission>,
    ) -> jmap::Result<SetResponse<EmailSubmission>> {
        let mut helper = SetHelper::new(self, request)?;

        let has_on_success = helper
            .request
            .arguments
            .on_success_destroy_email
            .as_ref()
            .map_or(false, |p| !p.is_empty())
            || helper
                .request
                .arguments
                .on_success_update_email
                .as_ref()
                .map_or(false, |p| !p.is_empty());
        let mut update_emails: VecMap<JMAPId, Email> = VecMap::new();
        let mut destroy_emails: Vec<JMAPId> = Vec::new();

        helper.create(|create_id, item, helper, document| {
            let mut fields = TinyORM::<EmailSubmission>::new();
            let mut email_id = JMAPId::from(u32::MAX);
            let mut identity_id = u32::MAX;
            let mut envelope = None;

            for (property, mut value) in item.properties {
                if let Value::IdReference { value: id } = &value {
                    value = Value::Id {
                        value: helper.get_id_reference(property, id)?,
                    };
                }
                let value = match (property, value) {
                    (Property::EmailId, Value::Id { value }) => {
                        fields.set(
                            Property::ThreadId,
                            Value::Id {
                                value: value.get_prefix_id().into(),
                            },
                        );
                        email_id = value;
                        Value::Id { value }
                    }
                    (Property::IdentityId, Value::Id { value }) => {
                        identity_id = value.get_document_id();
                        Value::Id { value }
                    }
                    (Property::Envelope, Value::Envelope { value }) => {
                        envelope = Some(value);
                        continue;
                    }
                    (Property::Envelope, Value::Null) => {
                        continue;
                    }
                    (Property::UndoStatus, value @ Value::UndoStatus { .. }) => value,
                    (property, _) => {
                        return Err(SetError::invalid_properties()
                            .with_property(property)
                            .with_description("Field could not be set."));
                    }
                };
                fields.set(property, value);
            }

            // Fetch mailFrom
            let mail_from = helper
                .store
                .get_orm::<Identity>(helper.account_id, identity_id)?
                .ok_or_else(|| {
                    SetError::invalid_properties()
                        .with_property(Property::IdentityId)
                        .with_description("Identity not found.")
                })?
                .remove(&identity::schema::Property::Email)
                .and_then(|v| {
                    if let identity::schema::Value::Text { value } = v {
                        Some(value)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    SetError::invalid_properties()
                        .with_property(Property::IdentityId)
                        .with_description(
                            "The speficied identity does not have a valid e-mail address.",
                        )
                })?;

            // Make sure the envelope address matches the identity email address
            let mut send_at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0) as i64;
            let mut envelope = if let Some(envelope) = envelope {
                if !envelope.mail_from.email.eq_ignore_ascii_case(&mail_from) {
                    return Err(SetError::invalid_properties()
                        .with_property(Property::IdentityId)
                        .with_description(format!(
                            "The envelope mailFrom ({}) does not match the identity email ({})",
                            envelope.mail_from.email, mail_from
                        )));
                }

                // Parse future release
                if let Some(parameters) = &envelope.mail_from.parameters {
                    if let Some(hold_for) = parameters
                        .get("HOLDFOR")
                        .and_then(|s| s.as_ref().and_then(|s| s.parse::<u64>().ok()))
                    {
                        send_at += hold_for as i64;
                    } else if let Some(Some(hold_until)) = parameters.get("HOLDUNTIL") {
                        if let Some(hold_until) = JMAPDate::parse(hold_until) {
                            send_at = hold_until.timestamp();
                        }
                    }
                }

                envelope
            } else {
                Envelope::new(mail_from)
            };

            // Make sure we have all required fields.
            if email_id.get_document_id() == u32::MAX || identity_id == u32::MAX {
                return Err(SetError::invalid_properties()
                    .with_property(Property::EmailId)
                    .with_description("emailId and identityId properties are required."));
            }

            // Set the sentAt property
            fields.set(
                Property::SendAt,
                Value::DateTime {
                    value: JMAPDate::from_timestamp(send_at),
                },
            );

            // Fetch message data
            let mut message_data = MessageData::deserialize(
                &helper
                    .store
                    .blob_get(
                        &helper
                            .store
                            .get_document_value::<BlobId>(
                                helper.account_id,
                                Collection::Mail,
                                email_id.get_document_id(),
                                MessageField::Metadata.into(),
                            )?
                            .ok_or_else(|| {
                                SetError::invalid_properties()
                                    .with_property(Property::EmailId)
                                    .with_description("Email not found.")
                            })?,
                    )?
                    .ok_or_else(|| {
                        StoreError::NotFound(format!(
                            "Message data for {}:{} not found.",
                            helper.account_id,
                            email_id.get_document_id()
                        ))
                    })?,
            )
            .ok_or_else(|| {
                StoreError::DataCorruption(format!(
                    "Failed to deserialize Message data for {:}:{}",
                    helper.account_id,
                    email_id.get_document_id()
                ))
            })?;

            // Obtain recipients from e-mail if missing
            if envelope.rcpt_to.is_empty() {
                let mut rcpt_to = AHashSet::default();
                for header in [RfcHeader::To, RfcHeader::Cc] {
                    if let Some(values) = message_data.headers.remove(&header) {
                        for value in values {
                            if let Some(recipients) = value.into_addresses() {
                                for recipient in recipients {
                                    rcpt_to.insert(recipient.email.trim().to_string());
                                }
                            }
                        }
                    }
                }

                if !rcpt_to.is_empty() {
                    for addr in rcpt_to {
                        envelope.rcpt_to.push(Address {
                            email: addr,
                            parameters: None,
                        });
                    }
                } else {
                    return Err(SetError::invalid_properties()
                        .with_property(Property::Envelope)
                        .with_description("No recipients found in the e-mail."));
                }
            } else {
                // De-duplicate and sanitize recipients
                envelope.rcpt_to = envelope
                    .rcpt_to
                    .into_iter()
                    .map(|a| (a.email.trim().to_string(), a.parameters))
                    .collect::<AHashMap<_, _>>()
                    .into_iter()
                    .map(|(email, parameters)| Address { email, parameters })
                    .collect::<Vec<_>>();
            }

            // Add and link blob
            document.binary(
                Property::EmailId,
                message_data.raw_message.serialize().unwrap(),
                IndexOptions::new(),
            );
            document.blob(message_data.raw_message, IndexOptions::new());

            // Insert envelope
            fields.set(Property::Envelope, Value::Envelope { value: envelope });

            // Validate fields
            fields.insert_validate(document)?;

            // Update onSuccess actions
            if has_on_success {
                let id_ref = MaybeIdReference::Reference(create_id.to_string());
                if let Some(update) = helper
                    .request
                    .arguments
                    .on_success_update_email
                    .as_mut()
                    .and_then(|p| p.remove(&id_ref))
                {
                    update_emails.append(email_id, update);
                }

                if helper
                    .request
                    .arguments
                    .on_success_destroy_email
                    .as_ref()
                    .map_or(false, |p| p.contains(&id_ref))
                {
                    destroy_emails.push(email_id);
                }
            }

            Ok(EmailSubmission::new(document.document_id.into()))
        })?;

        helper.update(|id, mut item, helper, document| {
            // Only undoStatus can be changed
            if let Some(Value::UndoStatus { value }) = item.properties.remove(&Property::UndoStatus)
            {
                let current_fields = self
                    .get_orm::<EmailSubmission>(helper.account_id, id.get_document_id())?
                    .ok_or_else(|| SetError::new(SetErrorType::NotFound))?;
                let mut fields = TinyORM::track_changes(&current_fields);

                fields.set(Property::UndoStatus, Value::UndoStatus { value });

                // Merge changes
                current_fields.merge_validate(document, fields)?;
            }
            Ok(None)
        })?;

        helper.destroy(|_id, helper, document| {
            self.email_submission_delete(helper.account_id, document)
                .map_err(|err| err.into())
        })?;

        let account_id = JMAPId::from(helper.account_id);
        let acl = helper.acl.clone();
        helper.into_response().map(|mut r| {
            if has_on_success && (!update_emails.is_empty() || !destroy_emails.is_empty()) {
                r.next_call = SetRequest {
                    acl: acl.into(),
                    account_id,
                    if_in_state: None,
                    create: None,
                    update: if !update_emails.is_empty() {
                        update_emails.into()
                    } else {
                        None
                    },
                    destroy: if !destroy_emails.is_empty() {
                        MaybeResultReference::Value(destroy_emails).into()
                    } else {
                        None
                    },
                    arguments: (),
                }
                .into();
            }
            r
        })
    }

    fn email_submission_delete(
        &self,
        account_id: AccountId,
        document: &mut Document,
    ) -> store::Result<()> {
        let document_id = document.document_id;

        // Fetch ORM
        let email_submission = self
            .get_orm::<EmailSubmission>(account_id, document_id)?
            .ok_or_else(|| {
                StoreError::NotFound(format!(
                    "EmailSubmission ORM data for {}:{} not found.",
                    account_id, document_id
                ))
            })?;

        // Delete ORM
        email_submission.delete(document);

        // Unlink e-mail
        if let Some(raw_message_id) = self.get_document_value::<BlobId>(
            account_id,
            Collection::EmailSubmission,
            document_id,
            Property::EmailId.into(),
        )? {
            document.blob(raw_message_id, IndexOptions::new().clear());
            document.binary(
                Property::EmailId,
                Vec::with_capacity(0),
                IndexOptions::new().clear(),
            );
            Ok(())
        } else {
            Err(StoreError::NotFound(format!(
                "EmailSubmission Blob for {}:{} not found.",
                account_id, document_id
            )))
        }
    }
}
