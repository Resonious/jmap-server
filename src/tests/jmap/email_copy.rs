use actix_web::web;
use jmap::types::jmap::JMAPId;
use jmap_client::{client::Client, mailbox::Role};
use store::Store;

use crate::{tests::store::utils::StoreCompareWith, JMAPServer};

pub async fn test<T>(server: web::Data<JMAPServer<T>>, client: &mut Client)
where
    T: for<'x> Store<'x> + 'static,
{
    println!("Running Email Copy tests...");

    // Create a mailbox on account 1
    let ac1_mailbox_id = client
        .set_default_account_id(JMAPId::new(1).to_string())
        .mailbox_create("Copy Test Ac# 1", None::<String>, Role::None)
        .await
        .unwrap()
        .unwrap_id();

    // Insert a message on account 1
    let ac1_email_id = client
        .email_import(
            concat!(
                "From: bill@example.com\r\n",
                "To: jdoe@example.com\r\n",
                "Subject: TPS Report\r\n",
                "\r\n",
                "I'm going to need those TPS reports ASAP. ",
                "So, if you could do that, that'd be great."
            )
            .as_bytes()
            .to_vec(),
            [&ac1_mailbox_id],
            None::<Vec<&str>>,
            None,
        )
        .await
        .unwrap()
        .unwrap_id();

    // Create a mailbox on account 2
    let ac2_mailbox_id = client
        .set_default_account_id(JMAPId::new(2).to_string())
        .mailbox_create("Copy Test Ac# 2", None::<String>, Role::None)
        .await
        .unwrap()
        .unwrap_id();

    // Copy the email and delete it from the first account
    let mut request = client.build();
    request
        .copy_email(JMAPId::new(1).to_string())
        .on_success_destroy_original(true)
        .create(&ac1_email_id)
        .mailbox_id(&ac2_mailbox_id, true)
        .keyword("$draft", true)
        .received_at(311923920);
    let ac2_email_id = request
        .send()
        .await
        .unwrap()
        .method_response_by_pos(0)
        .unwrap_copy_email()
        .unwrap()
        .created(&ac1_email_id)
        .unwrap()
        .unwrap_id();

    // Check that the email was copied
    let email = client
        .email_get(&ac2_email_id, None::<Vec<_>>)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        email.preview().unwrap(),
        "I'm going to need those TPS reports ASAP. So, if you could do that, that'd be great."
    );
    assert_eq!(email.subject().unwrap(), "TPS Report");
    assert_eq!(email.mailbox_ids(), &[&ac2_mailbox_id]);
    assert_eq!(email.keywords(), &["$draft"]);
    assert_eq!(email.received_at().unwrap(), 311923920);

    // Check that the email was deleted
    assert!(client
        .set_default_account_id(JMAPId::new(1).to_string())
        .email_get(&ac1_email_id, None::<Vec<_>>)
        .await
        .unwrap()
        .is_none());

    // Empty store
    client.mailbox_destroy(&ac1_mailbox_id, true).await.unwrap();
    client
        .set_default_account_id(JMAPId::new(2).to_string())
        .mailbox_destroy(&ac2_mailbox_id, true)
        .await
        .unwrap();
    server.store.assert_is_empty();
}