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

use store_rocksdb::RocksDB;

use super::{jmap::init_jmap_tests, store::utils::destroy_temp_dir};

pub mod email_changes;
pub mod email_copy;
pub mod email_get;
pub mod email_parse;
pub mod email_query;
pub mod email_query_changes;
pub mod email_set;
pub mod email_submission;
pub mod email_thread;
pub mod email_thread_merge;
pub mod lmtp;
pub mod mailbox;
pub mod search_snippet;
pub mod sieve;
pub mod vacation_response;

#[actix_web::test]
#[ignore]
async fn jmap_mail_tests() {
    let (server, mut client, temp_dir) = init_jmap_tests::<RocksDB>("jmap_mail_tests").await;

    // Run tests
    email_changes::test(server.clone(), &mut client).await;
    email_query_changes::test(server.clone(), &mut client).await;
    email_thread::test(server.clone(), &mut client).await;
    email_thread_merge::test(server.clone(), &mut client).await;
    email_get::test(server.clone(), &mut client).await;
    email_parse::test(server.clone(), &mut client).await;
    email_set::test(server.clone(), &mut client).await;
    email_query::test(server.clone(), &mut client).await;
    email_copy::test(server.clone(), &mut client).await;
    email_submission::test(server.clone(), &mut client).await;
    lmtp::test(server.clone(), &mut client).await;
    vacation_response::test(server.clone(), &mut client).await;
    mailbox::test(server.clone(), &mut client).await;
    search_snippet::test(server.clone(), &mut client).await;
    sieve::test(server.clone(), &mut client).await;

    destroy_temp_dir(&temp_dir);
}

pub fn find_values(string: &str, name: &str) -> Vec<String> {
    let mut last_pos = 0;
    let mut values = Vec::new();

    while let Some(pos) = string[last_pos..].find(name) {
        let mut value = string[last_pos + pos + name.len()..]
            .split('"')
            .nth(1)
            .unwrap();
        if value.ends_with('\\') {
            value = &value[..value.len() - 1];
        }
        values.push(value.to_string());
        last_pos += pos + name.len();
    }

    values
}

pub fn replace_values(mut string: String, find: &[String], replace: &[String]) -> String {
    for (find, replace) in find.iter().zip(replace.iter()) {
        string = string.replace(find, replace);
    }
    string
}

pub fn replace_boundaries(string: String) -> String {
    let values = find_values(&string, "boundary=");
    if !values.is_empty() {
        replace_values(
            string,
            &values,
            &(0..values.len())
                .map(|i| format!("boundary_{}", i))
                .collect::<Vec<_>>(),
        )
    } else {
        string
    }
}

pub fn replace_blob_ids(string: String) -> String {
    let values = find_values(&string, "blobId\":");
    if !values.is_empty() {
        replace_values(
            string,
            &values,
            &(0..values.len())
                .map(|i| format!("blob_{}", i))
                .collect::<Vec<_>>(),
        )
    } else {
        string
    }
}
