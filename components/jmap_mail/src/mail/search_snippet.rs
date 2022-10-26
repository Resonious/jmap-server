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

use std::sync::Arc;

use jmap::{
    error::method::MethodError,
    request::{
        query::{self, Filter},
        ACLEnforce, MaybeResultReference, ResultReference,
    },
    types::jmap::JMAPId,
};
use mail_parser::{decoders::html::html_to_text, Message, RfcHeader};
use store::{
    blob::BlobId,
    core::{
        acl::{ACLToken, ACL},
        collection::Collection,
        document::MAX_TOKEN_LENGTH,
        error::StoreError,
    },
    nlp::{search_snippet::generate_snippet, stemmer::Stemmer, tokenizers::Tokenizer, Language},
    read::filter::{LogicalOperator, Text},
    serialize::StoreDeserialize,
    tracing::error,
    JMAPStore, Store,
};

use super::{sharing::JMAPShareMail, MessageData, MessageField};

#[derive(Debug, Clone)]
pub struct SearchSnippetGetRequest {
    pub acl: Option<Arc<ACLToken>>,
    pub account_id: JMAPId,
    pub filter: Option<Filter<super::schema::Filter>>,
    pub email_ids: MaybeResultReference<Vec<JMAPId>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchSnippetGetResponse {
    #[serde(rename = "accountId")]
    pub account_id: JMAPId,

    #[serde(rename = "list")]
    pub list: Vec<SearchSnippet>,

    #[serde(rename = "notFound")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_found: Option<Vec<JMAPId>>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct SearchSnippet {
    #[serde(rename = "emailId")]
    pub email_id: JMAPId,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl SearchSnippet {
    pub fn empty(email_id: JMAPId) -> Self {
        SearchSnippet {
            email_id,
            subject: None,
            preview: None,
        }
    }
}

impl SearchSnippetGetRequest {
    pub fn eval_result_references(
        &mut self,
        mut fnc: impl FnMut(&ResultReference) -> Option<Vec<u64>>,
    ) -> jmap::Result<()> {
        if let Some(rr) = self.email_ids.result_reference()? {
            if let Some(ids) = fnc(rr) {
                self.email_ids =
                    MaybeResultReference::Value(ids.into_iter().map(Into::into).collect());
            } else {
                return Err(MethodError::InvalidResultReference(
                    "Failed to evaluate #ids result reference.".to_string(),
                ));
            }
        }
        Ok(())
    }
}

struct QueryState {
    op: LogicalOperator,
    it: std::vec::IntoIter<query::Filter<super::schema::Filter>>,
}

pub trait JMAPMailSearchSnippet<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_search_snippet(
        &self,
        request: SearchSnippetGetRequest,
    ) -> jmap::Result<SearchSnippetGetResponse>;
}

impl<T> JMAPMailSearchSnippet<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_search_snippet(
        &self,
        request: SearchSnippetGetRequest,
    ) -> jmap::Result<SearchSnippetGetResponse> {
        let account_id = request.account_id.get_document_id();
        let email_ids = request.email_ids.unwrap_value().unwrap_or_default();
        let acl = request.acl.unwrap();

        let mut terms = Vec::new();

        let mut list = Vec::with_capacity(email_ids.len());
        let mut not_found = Vec::new();

        // Fetch document ids
        let document_ids = if acl.is_member(account_id) {
            Arc::new(self.get_document_ids(account_id, Collection::Mail)?)
        } else {
            self.mail_shared_messages(account_id, &acl.member_of, ACL::ReadItems)?
        };

        // Obtain text terms
        if let Some(filter) = request.filter {
            let mut state = match filter {
                query::Filter::FilterOperator(op) => QueryState {
                    op: op.operator.into(),
                    it: op.conditions.into_iter(),
                },
                condition => QueryState {
                    op: LogicalOperator::And,
                    it: vec![condition].into_iter(),
                },
            };

            let mut state_stack = Vec::new();

            'outer: loop {
                while let Some(term) = state.it.next() {
                    match term {
                        query::Filter::FilterOperator(op) => {
                            state_stack.push(state);
                            state = QueryState {
                                op: op.operator.into(),
                                it: op.conditions.into_iter(),
                            };
                        }
                        query::Filter::FilterCondition(
                            super::schema::Filter::Text { value }
                            | super::schema::Filter::Subject { value }
                            | super::schema::Filter::Body { value },
                        ) => {
                            let mut include_term = true;
                            for state in &state_stack {
                                if state.op == LogicalOperator::Not {
                                    include_term = !include_term;
                                }
                            }
                            if state.op == LogicalOperator::Not {
                                include_term = !include_term;
                            }
                            if include_term {
                                terms.push(Text::new(value, Language::Unknown));
                            }
                        }
                        _ => (),
                    }
                }

                if let Some(prev_state) = state_stack.pop() {
                    state = prev_state;
                } else {
                    break 'outer;
                }
            }
        }

        for email_id in email_ids {
            let document_id = email_id.get_document_id();
            if document_ids
                .as_ref()
                .as_ref()
                .map_or(true, |b| !b.contains(document_id))
            {
                not_found.push(email_id);
                continue;
            }

            if terms.is_empty() {
                list.push(SearchSnippet::empty(email_id));
                continue;
            }

            // Fetch message data
            let message_data = MessageData::deserialize(
                &self
                    .blob_get(
                        &self
                            .get_document_value::<BlobId>(
                                account_id,
                                Collection::Mail,
                                document_id,
                                MessageField::Metadata.into(),
                            )?
                            .ok_or_else(|| {
                                StoreError::NotFound(format!(
                                    "Message data blobId for {}:{} not found.",
                                    account_id, document_id
                                ))
                            })?,
                    )?
                    .ok_or_else(|| {
                        StoreError::NotFound(format!(
                            "Message data blob for {}:{} not found.",
                            account_id, document_id
                        ))
                    })?,
            )
            .ok_or_else(|| {
                StoreError::DataCorruption(format!(
                    "Failed to deserialize message data for {}:{} not found.",
                    account_id, document_id
                ))
            })?;

            // Fetch raw message
            let raw_message = self.blob_get(&message_data.raw_message)?.ok_or_else(|| {
                StoreError::NotFound(format!(
                    "Failed to fetch raw message blobId {:?}.",
                    message_data.raw_message
                ))
            })?;

            // Fetch term index
            let term_index = self
                .get_term_index(account_id, Collection::Mail, document_id)?
                .ok_or_else(|| {
                    StoreError::NotFound(format!(
                        "Term index not found for email {}/{}",
                        account_id, document_id
                    ))
                })?;
            let mut match_terms = Vec::new();
            let mut match_phrase = false;

            // Tokenize and stem terms
            for term in &terms {
                if !term.match_phrase {
                    for token in Stemmer::new(&term.text, term.language, MAX_TOKEN_LENGTH) {
                        match_terms.push(term_index.get_match_term(
                            token.word.as_ref(),
                            token.stemmed_word.as_ref().map(|w| w.as_ref()),
                        ));
                    }
                } else {
                    match_phrase = true;
                    for token in Tokenizer::new(&term.text, term.language, MAX_TOKEN_LENGTH) {
                        match_terms.push(term_index.get_match_term(token.word.as_ref(), None));
                    }
                }
            }

            let mut subject = None;
            let mut preview = None;

            for term_group in term_index
                .match_terms(&match_terms, None, match_phrase, true, true)
                .map_err(|err| match err {
                    store::nlp::term_index::Error::InvalidArgument => {
                        MethodError::UnsupportedFilter("Too many search terms.".to_string())
                    }
                    err => {
                        error!("Failed to generate search snippet: {:?}", err);
                        MethodError::UnsupportedFilter(
                            "Failed to generate search snippet.".to_string(),
                        )
                    }
                })?
                .unwrap_or_default()
            {
                if term_group.part_id == 0 {
                    // Generate subject snippent
                    subject = generate_snippet(
                        &term_group.terms,
                        message_data
                            .headers
                            .get(&RfcHeader::Subject)
                            .and_then(|value| value.last())
                            .and_then(|value| value.as_text())
                            .unwrap_or(""),
                    );
                } else if term_group.part_id <= message_data.mime_parts.len() as u32 {
                    // Generate snippet of a body part
                    let part = &message_data.mime_parts[(term_group.part_id - 1) as usize];

                    if let Some(message_part) = part.mime_type.part() {
                        let mut text = message_part
                            .decode_text(&raw_message, part.charset.as_deref(), false)
                            .unwrap_or_else(|| {
                                error!(
                                    "Failed to decode message part {:?} for blob {:?}.",
                                    message_part, message_data.raw_message
                                );
                                "".to_string()
                            });
                        if part.mime_type.is_html() {
                            text = html_to_text(&text);
                        }
                        preview = generate_snippet(&term_group.terms, &text);
                    } else {
                        error!(
                            "Corrupted term index for email {}/{}: MIME part does not contain a blob.",
                            account_id, document_id
                        );
                    }
                } else {
                    // Generate snippet of an attached email subpart
                    let part_id = term_group.part_id >> 16;
                    let subpart_id = term_group.part_id & (u16::MAX as u32);

                    if part_id < message_data.mime_parts.len() as u32 {
                        if let Some(message_part) =
                            message_data.mime_parts[part_id as usize].mime_type.part()
                        {
                            let nested_raw_message =
                                message_part.decode(&raw_message).unwrap_or_else(|| {
                                    error!(
                                        "Failed to decode message part {:?} for blob {:?}.",
                                        message_part, message_data.raw_message
                                    );
                                    Vec::new()
                                });
                            let message =
                                Message::parse(&nested_raw_message).unwrap_or_else(|| {
                                    error!(
                                        "Failed to parse nested message in blob {:?}.",
                                        message_data.raw_message
                                    );
                                    Message::default()
                                });
                            if subpart_id == 0 {
                                preview = generate_snippet(
                                    &term_group.terms,
                                    message.get_subject().unwrap_or(""),
                                );
                            } else if let Some(sub_part) =
                                message.parts.get((subpart_id - 1) as usize)
                            {
                                let text = sub_part.get_text_contents().unwrap_or_else(|| {
                                    error!(
                                        "Failed to fetch text part for nested message in blob {:?}.",
                                        message_data.raw_message
                                    );
                                    ""
                                });

                                preview = if !sub_part.is_text_html() {
                                    generate_snippet(&term_group.terms, text)
                                } else {
                                    generate_snippet(&term_group.terms, &html_to_text(text))
                                };
                            } else {
                                error!(
                                    "Corrupted term index for email {}/{}: Could not find subpart {}/{}.",
                                    account_id, document_id, part_id, subpart_id
                                );
                            }
                        } else {
                            error!(
                                "Corrupted term index for email {}/{}: Could not find message attachment {}.",
                                account_id, document_id, part_id
                            );
                        }
                    }
                }

                if preview.is_some() {
                    break;
                }
            }

            list.push(SearchSnippet {
                email_id,
                subject,
                preview,
            });
        }

        Ok(SearchSnippetGetResponse {
            account_id: request.account_id,
            list,
            not_found: if !not_found.is_empty() {
                not_found.into()
            } else {
                None
            },
        })
    }
}
