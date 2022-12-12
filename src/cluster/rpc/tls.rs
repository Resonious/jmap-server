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

use std::{fs::File, io::BufReader, sync::Arc};

use rustls::{
    client::WebPkiVerifier, Certificate, ClientConfig, OwnedTrustAnchor, PrivateKey, RootCertStore,
    ServerConfig,
};
use rustls_pemfile::{certs, pkcs8_private_keys};
use store::tracing::error;

use crate::server::UnwrapFailure;

pub fn load_tls_client_config(allow_invalid_certs: bool) -> ClientConfig {
    let config = ClientConfig::builder().with_safe_defaults();

    if !allow_invalid_certs {
        let mut root_cert_store = RootCertStore::empty();

        root_cert_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(
            |ta| {
                OwnedTrustAnchor::from_subject_spki_name_constraints(
                    ta.subject,
                    ta.spki,
                    ta.name_constraints,
                )
            },
        ));

        config
            .with_custom_certificate_verifier(Arc::new(WebPkiVerifier::new(root_cert_store, None)))
    } else {
        config.with_custom_certificate_verifier(Arc::new(DummyVerifier {}))
    }
    .with_no_client_auth()
}

pub fn load_tls_server_config(cert_path: &str, key_path: &str) -> ServerConfig {
    // Init server config builder with safe defaults
    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth();

    // load TLS key/cert files
    let cert_file = &mut BufReader::new(File::open(cert_path).failed_to("open certificate file"));
    let key_file = &mut BufReader::new(File::open(key_path).failed_to("open key file"));

    // convert files to key/cert objects
    let cert_chain = certs(cert_file)
        .failed_to("parse certificate file")
        .into_iter()
        .map(Certificate)
        .collect();
    let mut keys: Vec<PrivateKey> = pkcs8_private_keys(key_file)
        .failed_to("parse key file")
        .into_iter()
        .map(PrivateKey)
        .collect();

    // exit if no keys could be parsed
    if keys.is_empty() {
        error!("Could not locate PKCS 8 private keys.");
        std::process::exit(1);
    }

    config.with_single_cert(cert_chain, keys.remove(0)).unwrap()
}

struct DummyVerifier;

impl rustls::client::ServerCertVerifier for DummyVerifier {
    fn verify_server_cert(
        &self,
        _e: &tokio_rustls::rustls::Certificate,
        _i: &[tokio_rustls::rustls::Certificate],
        _sn: &tokio_rustls::rustls::ServerName,
        _sc: &mut dyn Iterator<Item = &[u8]>,
        _o: &[u8],
        _n: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}
