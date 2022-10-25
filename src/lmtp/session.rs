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

use std::{net::SocketAddr, sync::Arc};

use actix_web::web;
use serde::{Deserialize, Serialize};
use store::{ahash::AHashSet, chrono::Local, tracing::debug, AccountId, RecipientType, Store};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};

use crate::JMAPServer;

use super::{
    ingest::DeliveryStatus,
    request::{Event, Param, Request, RequestParser},
    response::{Extension, Response},
};

const MAX_COMMAND_LENGTH: usize = 1024;

pub struct Session<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub core: web::Data<JMAPServer<T>>,
    pub tls_acceptor: Option<Arc<TlsAcceptor>>,
    pub hostname: Arc<String>,
    pub parser: RequestParser,
    pub peer_addr: SocketAddr,
    pub stream: Stream,

    // State
    pub remote_hostname: Option<String>,
    pub mail_from: Option<String>,
    pub mail_size: Option<usize>,
    pub rcpt_to: Vec<RcptType>,
    pub rcpt_to_dup: AHashSet<AccountId>,
    pub message: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RcptType {
    Mailbox {
        id: AccountId,
        name: String,
        status: DeliveryStatus,
    },
    List {
        ids: Vec<AccountId>,
        name: String,
        status: DeliveryStatus,
    },
}

#[allow(clippy::large_enum_variant)]
pub enum Stream {
    Clear(TcpStream),
    Tls(TlsStream<TcpStream>),
    None,
}

impl<T> Session<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn new(
        core: web::Data<JMAPServer<T>>,
        peer_addr: SocketAddr,
        stream: Stream,
        tls_acceptor: Option<Arc<TlsAcceptor>>,
        hostname: Arc<String>,
    ) -> Self {
        Self {
            parser: RequestParser::new(MAX_COMMAND_LENGTH, core.store.config.mail_max_size),
            tls_acceptor,
            peer_addr,
            stream,
            core,
            remote_hostname: None,
            mail_from: None,
            mail_size: None,
            rcpt_to: Vec::new(),
            rcpt_to_dup: AHashSet::new(),
            message: Vec::new(),
            hostname,
        }
    }

    pub async fn ingest(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let mut bytes = bytes.iter();

        loop {
            match self.parser.parse(&mut bytes) {
                Ok(request) => match request {
                    Request::Lhlo { domain } => {
                        let mut extensions = vec![
                            Extension::EnhancedStatusCodes,
                            Extension::Pipelining,
                            Extension::Chunking,
                            Extension::EightBitMime,
                            Extension::BinaryMime,
                            Extension::SmtpUtf8,
                            Extension::Vrfy,
                            Extension::Help,
                            Extension::Size(self.core.store.config.mail_max_size as u32),
                        ];
                        if !self.stream.is_tls() {
                            extensions.push(Extension::StartTls);
                        }
                        self.write_bytes(
                            &Response::Lhlo {
                                local_host: self.hostname.as_ref().into(),
                                remote_host: domain.as_str().into(),
                                extensions,
                            }
                            .into_bytes(),
                        )
                        .await?;
                        self.remote_hostname = domain.into();
                    }
                    Request::Mail { sender, params } => {
                        self.write_bytes(
                            format!("250 2.1.0 Sender <{}> accepted.\r\n", sender).as_bytes(),
                        )
                        .await?;
                        self.mail_from = sender.into();
                        self.mail_size = params.iter().find_map(|p| {
                            if let Param::Size(size) = p {
                                Some(*size as usize)
                            } else {
                                None
                            }
                        });
                    }
                    Request::Rcpt { recipient, .. } => match self.expand_rcpt(&recipient).await {
                        Some(recipient_) => match recipient_.as_ref() {
                            RecipientType::Individual(account_id) => {
                                self.write_bytes(
                                    format!("250 2.1.5 Recipient <{}> accepted.\r\n", recipient)
                                        .as_bytes(),
                                )
                                .await?;

                                self.rcpt_to.push(RcptType::Mailbox {
                                    id: *account_id,
                                    name: recipient,
                                    status: if self.rcpt_to_dup.insert(*account_id) {
                                        DeliveryStatus::Success
                                    } else {
                                        DeliveryStatus::Duplicated
                                    },
                                });
                            }
                            RecipientType::List(account_ids) => {
                                self.write_bytes(
                                    format!("250 2.1.5 Recipient <{}> accepted.\r\n", recipient)
                                        .as_bytes(),
                                )
                                .await?;

                                let mut ids = Vec::with_capacity(account_ids.len());
                                for (account_id, _) in account_ids {
                                    if self.rcpt_to_dup.insert(*account_id) {
                                        ids.push(*account_id);
                                    }
                                }
                                self.rcpt_to.push(RcptType::List {
                                    status: if !ids.is_empty() {
                                        DeliveryStatus::Success
                                    } else {
                                        DeliveryStatus::Duplicated
                                    },
                                    ids,
                                    name: recipient,
                                });
                            }
                            RecipientType::NotFound => {
                                self.write_bytes(b"550 5.1.1 Mailbox not found.\r\n")
                                    .await?;
                            }
                        },
                        None => {
                            self.write_bytes(b"450 4.3.2 Temporary server failure.\r\n")
                                .await?;
                        }
                    },
                    Request::Data { data } => {
                        self.message = data;
                        self.ingest_message().await?;
                    }
                    Request::Bdat { data, is_last } => {
                        if self.message.len() + data.len() < self.core.store.config.mail_max_size {
                            if self.message.is_empty() {
                                let rp = self.build_return_path();
                                self.message = Vec::with_capacity(
                                    self.mail_size
                                        .unwrap_or_else(|| std::cmp::max(1024, data.len()))
                                        + rp.len(),
                                );
                                self.message.extend_from_slice(rp.as_bytes());
                            }
                            self.message.extend_from_slice(&data);
                            if is_last {
                                self.ingest_message().await?;
                            } else {
                                self.write_bytes(b"250 2.1.0 Message chunk accepted.\r\n")
                                    .await?;
                            }
                        } else {
                            self.write_bytes(
                                format!(
                                    "500 5.3.4 Message exceeds maximum size of {} bytes.\r\n",
                                    self.core.store.config.mail_max_size
                                )
                                .as_bytes(),
                            )
                            .await?;
                        }
                    }
                    Request::Vrfy { mailbox } => match self.expand_rcpt(&mailbox).await {
                        Some(recipient_) => match recipient_.as_ref() {
                            RecipientType::Individual(_) | RecipientType::List(_) => {
                                self.write_bytes(
                                    format!("250 2.1.5 Mailbox <{}> exists.\r\n", mailbox)
                                        .as_bytes(),
                                )
                                .await?;
                            }
                            RecipientType::NotFound => {
                                self.write_bytes(b"550 5.1.1 Mailbox not found.\r\n")
                                    .await?;
                            }
                        },
                        None => {
                            self.write_bytes(b"450 4.3.2 Temporary server failure.\r\n")
                                .await?;
                        }
                    },
                    Request::Expn { list } => match self.expand_rcpt(&list).await {
                        Some(recipient_) => match recipient_.as_ref() {
                            RecipientType::List(list) => {
                                let mut buf = Vec::with_capacity(list.len() * 50);
                                for (pos, (_, addr)) in list.iter().enumerate() {
                                    if pos < list.len() - 1 {
                                        buf.extend_from_slice(b"250- <");
                                    } else {
                                        buf.extend_from_slice(b"250  <");
                                    }
                                    buf.extend_from_slice(addr.as_bytes());
                                    buf.extend_from_slice(b">\r\n");
                                }
                                self.write_bytes(&buf).await?;
                            }
                            RecipientType::Individual(_) => {
                                self.write_bytes(
                                    format!("550 5.1.0 Address <{}> exists but is not a mailing list.\r\n", list)
                                        .as_bytes(),
                                )
                                .await?;
                            }
                            RecipientType::NotFound => {
                                self.write_bytes(b"550 5.1.1 List not found.\r\n").await?;
                            }
                        },
                        None => {
                            self.write_bytes(b"450 4.3.2 Temporary server failure.\r\n")
                                .await?;
                        }
                    },
                    Request::Help { .. } => {
                        self.write_bytes(
                            b"250 2.0.0 Help can be found at https://stalw.art/jmap/\r\n",
                        )
                        .await?;
                    }
                    Request::StartTls => match (&self.stream, &self.tls_acceptor) {
                        (Stream::Clear(_), Some(_)) => {
                            self.write_bytes(b"220 2.0.0 Ready to start TLS\r\n")
                                .await?;
                            match self
                                .tls_acceptor
                                .as_ref()
                                .unwrap()
                                .accept(std::mem::take(&mut self.stream).unwrap_clear())
                                .await
                            {
                                Ok(stream) => self.stream = stream.into(),
                                Err(e) => {
                                    debug!("Failed to accept TLS connection: {}", e);
                                    return Err(());
                                }
                            };
                        }
                        (Stream::Clear(_), None) => {
                            self.write_bytes(b"501 5.7.4 TLS not configured on this server.\r\n")
                                .await?;
                        }
                        (Stream::Tls(_), _) => {
                            self.write_bytes(b"501 5.7.0 Already in TLS mode.\r\n")
                                .await?;
                        }
                        (_, _) => {
                            unreachable!()
                        }
                    },
                    Request::Rset => {
                        self.mail_from = None;
                        self.mail_size = None;
                        self.rcpt_to.clear();
                        self.rcpt_to_dup.clear();
                        self.message = Vec::new();
                        self.write_bytes(b"250 2.0.0 OK\r\n").await?;
                    }
                    Request::Noop => {
                        self.write_bytes(b"250 2.0.0 OK\r\n").await?;
                    }
                    Request::Quit => {
                        self.write_bytes(b"221 2.0.0 Bye\r\n").await?;
                        return Err(());
                    }
                },
                Err(Event::NeedsMoreBytes) => {
                    break;
                }
                Err(Event::Data) => {
                    if !self.rcpt_to_dup.is_empty() {
                        let rp = self.build_return_path();
                        self.parser.buf =
                            Vec::with_capacity(self.mail_size.unwrap_or(1024) + rp.len());
                        self.parser.buf.extend_from_slice(rp.as_bytes());
                        self.write_bytes(
                            b"354 3.0.0 Start mail input; end with <CRLF>.<CRLF>.\r\n",
                        )
                        .await?;
                    } else {
                        self.write_bytes(b"503 5.5.1 Missing RCPT TO.\r\n").await?;
                    }
                }
                Err(Event::Message { response }) => {
                    self.write_bytes(&response.into_bytes()).await?;
                }
            }
        }

        Ok(())
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ()> {
        match &mut self.stream {
            Stream::Clear(stream) => stream.write_all(bytes).await.map_err(|err| {
                debug!("Failed to write to stream: {}", err);
            }),
            Stream::Tls(stream) => stream.write_all(bytes).await.map_err(|err| {
                debug!("Failed to write to TLS stream: {}", err);
            }),
            _ => unreachable!(),
        }
    }

    pub async fn read_bytes(&mut self, bytes: &mut [u8]) -> Result<usize, ()> {
        match &mut self.stream {
            Stream::Clear(stream) => stream.read(bytes).await.map_err(|err| {
                debug!("Failed to read from stream: {}", err);
            }),
            Stream::Tls(stream) => stream.read(bytes).await.map_err(|err| {
                debug!("Failed to read from TLS stream: {}", err);
            }),
            _ => unreachable!(),
        }
    }

    fn build_return_path(&self) -> String {
        format!(
            concat!(
                "Received: from {} ([{}])\r\n",
                "\tby {} (Stalwart JMAP) with LMTP;\r\n",
                "\t{}\r\n"
            ),
            self.remote_hostname.as_deref().unwrap_or("unknown"),
            self.hostname.as_ref(),
            self.peer_addr.ip(),
            Local::now().to_rfc2822()
        )
    }
}

impl From<TcpStream> for Stream {
    fn from(stream: TcpStream) -> Self {
        Stream::Clear(stream)
    }
}

impl From<TlsStream<TcpStream>> for Stream {
    fn from(stream: TlsStream<TcpStream>) -> Self {
        Stream::Tls(stream)
    }
}

impl Stream {
    pub fn unwrap_clear(self) -> TcpStream {
        match self {
            Stream::Clear(stream) => stream,
            _ => unreachable!(),
        }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }
}

impl Default for Stream {
    fn default() -> Self {
        Stream::None
    }
}
