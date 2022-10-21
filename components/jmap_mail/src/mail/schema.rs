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

use std::fmt::Display;

use jmap::{
    request::{MaybeIdReference, ResultReference},
    types::{blob::JMAPBlob, date::JMAPDate, jmap::JMAPId},
};
use mail_parser::RfcHeader;

use store::{
    blob::BlobId,
    core::{tag::Tag, vec_map::VecMap},
    FieldId,
};

use super::{HeaderName, MessageField};

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct Email {
    pub properties: VecMap<Property, Value>,
}

impl Email {
    pub fn insert(&mut self, property: Property, value: impl Into<Value>) {
        self.properties.append(property, value.into());
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EmailBodyPart {
    pub properties: VecMap<BodyProperty, Value>,
}

impl EmailBodyPart {
    pub fn get_text(&self, property: BodyProperty) -> Option<&str> {
        self.properties.get(&property).and_then(|v| match v {
            Value::Text { value } => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn get_blob(&self, property: BodyProperty) -> Option<&JMAPBlob> {
        self.properties.get(&property).and_then(|v| match v {
            Value::Blob { value } => Some(value),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
pub struct EmailBodyValue {
    #[serde(rename = "value")]
    pub value: String,

    #[serde(rename = "isEncodingProblem")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_encoding_problem: Option<bool>,

    #[serde(rename = "isTruncated")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_truncated: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
pub struct EmailAddressGroup {
    pub name: Option<String>,
    pub addresses: Vec<EmailAddress>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
pub struct EmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Keyword {
    pub tag: Tag,
}

const MAX_KEYWORD_LENGTH: usize = 100;

impl Keyword {
    pub const SEEN: u8 = 0;
    pub const DRAFT: u8 = 1;
    pub const FLAGGED: u8 = 2;
    pub const ANSWERED: u8 = 3;
    pub const RECENT: u8 = 4;
    pub const IMPORTANT: u8 = 5;
    pub const PHISHING: u8 = 6;
    pub const JUNK: u8 = 7;
    pub const NOTJUNK: u8 = 8;
    pub const DELETED: u8 = 9;
    pub const FORWARDED: u8 = 10;
    pub const MDN_SENT: u8 = 11;

    pub fn new(tag: Tag) -> Self {
        Self { tag }
    }

    pub fn parse(value: &str) -> Self {
        if let Some(k) = value.strip_prefix('$') {
            if k.eq_ignore_ascii_case("seen") {
                return Keyword::new(Tag::Static(Self::SEEN));
            } else if k.eq_ignore_ascii_case("draft") {
                return Keyword::new(Tag::Static(Self::DRAFT));
            } else if k.eq_ignore_ascii_case("flagged") {
                return Keyword::new(Tag::Static(Self::FLAGGED));
            } else if k.eq_ignore_ascii_case("answered") {
                return Keyword::new(Tag::Static(Self::ANSWERED));
            } else if k.eq_ignore_ascii_case("recent") {
                return Keyword::new(Tag::Static(Self::RECENT));
            } else if k.eq_ignore_ascii_case("important") {
                return Keyword::new(Tag::Static(Self::IMPORTANT));
            } else if k.eq_ignore_ascii_case("phishing") {
                return Keyword::new(Tag::Static(Self::PHISHING));
            } else if k.eq_ignore_ascii_case("junk") {
                return Keyword::new(Tag::Static(Self::JUNK));
            } else if k.eq_ignore_ascii_case("notjunk") {
                return Keyword::new(Tag::Static(Self::NOTJUNK));
            } else if k.eq_ignore_ascii_case("deleted") {
                return Keyword::new(Tag::Static(Self::DELETED));
            } else if k.eq_ignore_ascii_case("forwarded") {
                return Keyword::new(Tag::Static(Self::FORWARDED));
            } else if k.eq_ignore_ascii_case("mdnsent") {
                return Keyword::new(Tag::Static(Self::MDN_SENT));
            }
        }

        Keyword::new(if value.len() < MAX_KEYWORD_LENGTH {
            Tag::Text(value.to_lowercase())
        } else {
            Tag::Text(
                value
                    .to_lowercase()
                    .chars()
                    .take(MAX_KEYWORD_LENGTH)
                    .collect(),
            )
        })
    }
}

impl From<&Tag> for Keyword {
    fn from(tag: &Tag) -> Self {
        Keyword { tag: tag.clone() }
    }
}

impl From<Tag> for Keyword {
    fn from(tag: Tag) -> Self {
        Keyword { tag }
    }
}

impl Display for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.tag {
            Tag::Static(keyword) => match *keyword {
                Self::SEEN => write!(f, "$seen"),
                Self::DRAFT => write!(f, "$draft"),
                Self::FLAGGED => write!(f, "$flagged"),
                Self::ANSWERED => write!(f, "$answered"),
                Self::RECENT => write!(f, "$recent"),
                Self::IMPORTANT => write!(f, "$important"),
                Self::PHISHING => write!(f, "$phishing"),
                Self::JUNK => write!(f, "$junk"),
                Self::NOTJUNK => write!(f, "$notjunk"),
                Self::DELETED => write!(f, "$deleted"),
                Self::FORWARDED => write!(f, "$forwarded"),
                Self::MDN_SENT => write!(f, "$mdnsent"),
                12..=u8::MAX => Err(std::fmt::Error),
            },
            Tag::Text(value) => write!(f, "{}", value),
            _ => Err(std::fmt::Error),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Property {
    Id,
    BlobId,
    ThreadId,
    MailboxIds,
    Keywords,
    Size,
    ReceivedAt,
    MessageId,
    InReplyTo,
    References,
    Sender,
    From,
    To,
    Cc,
    Bcc,
    ReplyTo,
    Subject,
    SentAt,
    HasAttachment,
    Preview,
    BodyValues,
    TextBody,
    HtmlBody,
    Attachments,
    BodyStructure,
    Headers,
    Header(HeaderProperty),
    Invalid(String),
}

impl Property {
    pub fn parse(value: &str) -> Self {
        match value {
            "id" => Property::Id,
            "blobId" => Property::BlobId,
            "threadId" => Property::ThreadId,
            "mailboxIds" => Property::MailboxIds,
            "keywords" => Property::Keywords,
            "size" => Property::Size,
            "receivedAt" => Property::ReceivedAt,
            "messageId" => Property::MessageId,
            "inReplyTo" => Property::InReplyTo,
            "references" => Property::References,
            "sender" => Property::Sender,
            "from" => Property::From,
            "to" => Property::To,
            "cc" => Property::Cc,
            "bcc" => Property::Bcc,
            "replyTo" => Property::ReplyTo,
            "subject" => Property::Subject,
            "sentAt" => Property::SentAt,
            "hasAttachment" => Property::HasAttachment,
            "preview" => Property::Preview,
            "bodyValues" => Property::BodyValues,
            "textBody" => Property::TextBody,
            "htmlBody" => Property::HtmlBody,
            "attachments" => Property::Attachments,
            "bodyStructure" => Property::BodyStructure,
            "headers" => Property::Headers,
            _ if value.starts_with("header:") => {
                if let Some(header) = HeaderProperty::parse(value) {
                    Property::Header(header)
                } else {
                    Property::Invalid(value.to_string())
                }
            }
            _ => Property::Invalid(value.to_string()),
        }
    }

    pub fn as_rfc_header(&self) -> RfcHeader {
        match self {
            Property::MessageId => RfcHeader::MessageId,
            Property::InReplyTo => RfcHeader::InReplyTo,
            Property::References => RfcHeader::References,
            Property::Sender => RfcHeader::Sender,
            Property::From => RfcHeader::From,
            Property::To => RfcHeader::To,
            Property::Cc => RfcHeader::Cc,
            Property::Bcc => RfcHeader::Bcc,
            Property::ReplyTo => RfcHeader::ReplyTo,
            Property::Subject => RfcHeader::Subject,
            Property::SentAt => RfcHeader::Date,
            Property::Header(HeaderProperty {
                header: HeaderName::Rfc(rfc),
                ..
            }) => *rfc,
            _ => unreachable!(),
        }
    }
}

impl Display for Property {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Property::Id => write!(f, "id"),
            Property::BlobId => write!(f, "blobId"),
            Property::ThreadId => write!(f, "threadId"),
            Property::MailboxIds => write!(f, "mailboxIds"),
            Property::Keywords => write!(f, "keywords"),
            Property::Size => write!(f, "size"),
            Property::ReceivedAt => write!(f, "receivedAt"),
            Property::MessageId => write!(f, "messageId"),
            Property::InReplyTo => write!(f, "inReplyTo"),
            Property::References => write!(f, "references"),
            Property::Sender => write!(f, "sender"),
            Property::From => write!(f, "from"),
            Property::To => write!(f, "to"),
            Property::Cc => write!(f, "cc"),
            Property::Bcc => write!(f, "bcc"),
            Property::ReplyTo => write!(f, "replyTo"),
            Property::Subject => write!(f, "subject"),
            Property::SentAt => write!(f, "sentAt"),
            Property::HasAttachment => write!(f, "hasAttachment"),
            Property::Preview => write!(f, "preview"),
            Property::BodyValues => write!(f, "bodyValues"),
            Property::TextBody => write!(f, "textBody"),
            Property::HtmlBody => write!(f, "htmlBody"),
            Property::Attachments => write!(f, "attachments"),
            Property::BodyStructure => write!(f, "bodyStructure"),
            Property::Headers => write!(f, "headers"),
            Property::Header(header) => header.fmt(f),
            Property::Invalid(value) => write!(f, "{}", value),
        }
    }
}

impl Default for Property {
    fn default() -> Self {
        Property::Id
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum BodyProperty {
    PartId,
    BlobId,
    Size,
    Name,
    Type,
    Charset,
    Header(HeaderProperty),
    Headers,
    Disposition,
    Cid,
    Language,
    Location,
    Subparts,
}

impl BodyProperty {
    pub fn parse(value: &str) -> Option<BodyProperty> {
        match value {
            "partId" => Some(BodyProperty::PartId),
            "blobId" => Some(BodyProperty::BlobId),
            "size" => Some(BodyProperty::Size),
            "name" => Some(BodyProperty::Name),
            "type" => Some(BodyProperty::Type),
            "charset" => Some(BodyProperty::Charset),
            "headers" => Some(BodyProperty::Headers),
            "disposition" => Some(BodyProperty::Disposition),
            "cid" => Some(BodyProperty::Cid),
            "language" => Some(BodyProperty::Language),
            "location" => Some(BodyProperty::Location),
            "subParts" => Some(BodyProperty::Subparts),
            _ if value.starts_with("header:") => {
                Some(BodyProperty::Header(HeaderProperty::parse(value)?))
            }
            _ => None,
        }
    }
}

impl Display for BodyProperty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyProperty::PartId => write!(f, "partId"),
            BodyProperty::BlobId => write!(f, "blobId"),
            BodyProperty::Size => write!(f, "size"),
            BodyProperty::Name => write!(f, "name"),
            BodyProperty::Type => write!(f, "type"),
            BodyProperty::Charset => write!(f, "charset"),
            BodyProperty::Header(header) => header.fmt(f),
            BodyProperty::Headers => write!(f, "headers"),
            BodyProperty::Disposition => write!(f, "disposition"),
            BodyProperty::Cid => write!(f, "cid"),
            BodyProperty::Language => write!(f, "language"),
            BodyProperty::Location => write!(f, "location"),
            BodyProperty::Subparts => write!(f, "subParts"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct HeaderProperty {
    pub form: HeaderForm,
    pub header: HeaderName,
    pub all: bool,
}

impl HeaderProperty {
    pub fn new_rfc(header: RfcHeader, form: HeaderForm, all: bool) -> Self {
        HeaderProperty {
            form,
            header: HeaderName::Rfc(header),
            all,
        }
    }
    pub fn new_other(header: String, form: HeaderForm, all: bool) -> Self {
        HeaderProperty {
            form,
            header: HeaderName::Other(header),
            all,
        }
    }

    pub fn parse(value: &str) -> Option<HeaderProperty> {
        let mut all = false;
        let mut form = HeaderForm::Raw;
        let mut header = None;
        for (pos, part) in value.split(':').enumerate() {
            match pos {
                0 if part == "header" => (),
                1 => {
                    header = mail_parser::HeaderName::parse(part).map(|h| match h {
                        mail_parser::HeaderName::Rfc(header_name) => HeaderName::Rfc(header_name),
                        mail_parser::HeaderName::Other(header_name) => {
                            HeaderName::Other(header_name.into_owned())
                        }
                    });
                }
                2 | 3 if part == "all" => all = true,
                2 => {
                    form = HeaderForm::parse(part)?;
                }
                _ => return None,
            }
        }
        Some(HeaderProperty {
            form,
            header: header?,
            all,
        })
    }
}

impl Display for HeaderProperty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "header:")?;
        match &self.header {
            HeaderName::Rfc(rfc) => rfc.fmt(f)?,
            HeaderName::Other(name) => name.fmt(f)?,
        }
        self.form.fmt(f)?;
        if self.all {
            write!(f, ":all")
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum HeaderForm {
    Raw,
    Text,
    Addresses,
    GroupedAddresses,
    MessageIds,
    Date,
    URLs,
}

impl HeaderForm {
    pub fn parse(value: &str) -> Option<HeaderForm> {
        match value {
            "asText" => Some(HeaderForm::Text),
            "asAddresses" => Some(HeaderForm::Addresses),
            "asGroupedAddresses" => Some(HeaderForm::GroupedAddresses),
            "asMessageIds" => Some(HeaderForm::MessageIds),
            "asDate" => Some(HeaderForm::Date),
            "asURLs" => Some(HeaderForm::URLs),
            "asRaw" => Some(HeaderForm::Raw),
            _ => None,
        }
    }
}

impl Display for HeaderForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderForm::Raw => Ok(()),
            HeaderForm::Text => write!(f, ":asText"),
            HeaderForm::Addresses => write!(f, ":asAddresses"),
            HeaderForm::GroupedAddresses => write!(f, ":asGroupedAddresses"),
            HeaderForm::MessageIds => write!(f, ":asMessageIds"),
            HeaderForm::Date => write!(f, ":asDate"),
            HeaderForm::URLs => write!(f, ":asURLs"),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Value {
    Id {
        value: JMAPId,
    },
    Blob {
        value: JMAPBlob,
    },
    Size {
        value: usize,
    },
    Bool {
        value: bool,
    },
    Keywords {
        value: VecMap<Keyword, bool>,
        set: bool,
    },
    MailboxIds {
        value: VecMap<MaybeIdReference, bool>,
        set: bool,
    },
    ResultReference {
        value: ResultReference,
    },
    BodyPart {
        value: EmailBodyPart,
    },
    BodyPartList {
        value: Vec<EmailBodyPart>,
    },
    BodyValues {
        value: VecMap<String, EmailBodyValue>,
    },
    Text {
        value: String,
    },
    TextList {
        value: Vec<String>,
    },
    TextListMany {
        value: Vec<Vec<String>>,
    },
    Date {
        value: JMAPDate,
    },
    DateList {
        value: Vec<JMAPDate>,
    },
    Addresses {
        value: Vec<EmailAddress>,
    },
    AddressesList {
        value: Vec<Vec<EmailAddress>>,
    },
    GroupedAddresses {
        value: Vec<EmailAddressGroup>,
    },
    GroupedAddressesList {
        value: Vec<Vec<EmailAddressGroup>>,
    },
    Headers {
        value: Vec<EmailHeader>,
    },
    Null,
}

impl Default for Value {
    fn default() -> Self {
        Value::Null
    }
}

impl From<JMAPId> for Value {
    fn from(value: JMAPId) -> Self {
        Value::Id { value }
    }
}

impl From<JMAPBlob> for Value {
    fn from(value: JMAPBlob) -> Self {
        Value::Blob { value }
    }
}

impl From<&JMAPBlob> for Value {
    fn from(value: &JMAPBlob) -> Self {
        Value::Blob {
            value: value.clone(),
        }
    }
}

impl From<&BlobId> for Value {
    fn from(value: &BlobId) -> Self {
        Value::Blob {
            value: JMAPBlob::new(value.clone()),
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool { value }
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Text { value }
    }
}

impl From<Vec<String>> for Value {
    fn from(value: Vec<String>) -> Self {
        Value::TextList { value }
    }
}

impl From<Vec<EmailBodyPart>> for Value {
    fn from(value: Vec<EmailBodyPart>) -> Self {
        Value::BodyPartList { value }
    }
}

impl From<EmailBodyPart> for Value {
    fn from(value: EmailBodyPart) -> Self {
        Value::BodyPart { value }
    }
}

impl From<usize> for Value {
    fn from(value: usize) -> Self {
        Value::Size { value }
    }
}

impl Value {
    pub fn get_mailbox_ids(&mut self) -> Option<&mut VecMap<MaybeIdReference, bool>> {
        match self {
            Value::MailboxIds { value, .. } => Some(value),
            _ => None,
        }
    }

    pub fn get_keywords(&mut self) -> Option<&mut VecMap<Keyword, bool>> {
        match self {
            Value::Keywords { value, .. } => Some(value),
            _ => None,
        }
    }
}

impl From<Property> for FieldId {
    fn from(value: Property) -> Self {
        match value {
            Property::ThreadId => MessageField::ThreadId.into(),
            Property::MailboxIds => MessageField::Mailbox.into(),
            Property::Keywords => MessageField::Keyword.into(),
            Property::Id => 0,
            Property::BlobId => 1,
            Property::Size => 2,
            Property::ReceivedAt => 3,
            Property::MessageId => 4,
            Property::InReplyTo => 5,
            Property::References => 6,
            Property::Sender => 7,
            Property::From => 8,
            Property::To => 9,
            Property::Cc => 10,
            Property::Bcc => 11,
            Property::ReplyTo => 12,
            Property::Subject => 13,
            Property::SentAt => 14,
            Property::HasAttachment => 15,
            Property::Preview => 16,
            Property::BodyValues => 17,
            Property::TextBody => 18,
            Property::HtmlBody => 19,
            Property::Attachments => 20,
            Property::BodyStructure => 21,
            Property::Headers => 22,
            Property::Header(_) => 23,
            Property::Invalid(_) => 24,
        }
    }
}

impl From<FieldId> for Property {
    fn from(field: FieldId) -> Self {
        match field {
            0 => Property::Id,
            1 => Property::BlobId,
            2 => Property::Size,
            3 => Property::ReceivedAt,
            4 => Property::MessageId,
            5 => Property::InReplyTo,
            6 => Property::References,
            7 => Property::Sender,
            8 => Property::From,
            9 => Property::To,
            10 => Property::Cc,
            11 => Property::Bcc,
            12 => Property::ReplyTo,
            13 => Property::Subject,
            14 => Property::SentAt,
            15 => Property::HasAttachment,
            16 => Property::Preview,
            17 => Property::BodyValues,
            18 => Property::TextBody,
            19 => Property::HtmlBody,
            20 => Property::Attachments,
            21 => Property::BodyStructure,
            22 => Property::Headers,
            136 => Property::ThreadId,
            137 => Property::MailboxIds,
            132 => Property::Keywords,
            _ => Property::Invalid("".into()),
        }
    }
}

impl TryFrom<&str> for Property {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match Property::parse(value) {
            Property::Invalid(_) => Err(()),
            property => Ok(property),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Filter {
    InMailbox { value: JMAPId },
    InMailboxOtherThan { value: Vec<JMAPId> },
    Before { value: JMAPDate },
    After { value: JMAPDate },
    MinSize { value: u32 },
    MaxSize { value: u32 },
    AllInThreadHaveKeyword { value: Keyword },
    SomeInThreadHaveKeyword { value: Keyword },
    NoneInThreadHaveKeyword { value: Keyword },
    HasKeyword { value: Keyword },
    NotKeyword { value: Keyword },
    HasAttachment { value: bool },
    Text { value: String },
    From { value: String },
    To { value: String },
    Cc { value: String },
    Bcc { value: String },
    Subject { value: String },
    Body { value: String },
    Header { value: Vec<String> },
    Unsupported { value: String },

    // Non-standard
    Id { value: Vec<JMAPId> },
    SentBefore { value: JMAPDate },
    SentAfter { value: JMAPDate },
    InThread { value: JMAPId },
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(tag = "property")]
pub enum Comparator {
    #[serde(rename = "receivedAt")]
    ReceivedAt,
    #[serde(rename = "size")]
    Size,
    #[serde(rename = "from")]
    From,
    #[serde(rename = "to")]
    To,
    #[serde(rename = "subject")]
    Subject,
    #[serde(rename = "sentAt")]
    SentAt,
    #[serde(rename = "hasKeyword")]
    HasKeyword { keyword: Keyword },
    #[serde(rename = "allInThreadHaveKeyword")]
    AllInThreadHaveKeyword { keyword: Keyword },
    #[serde(rename = "someInThreadHaveKeyword")]
    SomeInThreadHaveKeyword { keyword: Keyword },

    // Non-standard
    #[serde(rename = "cc")]
    Cc,
}
