use actix_web::{http::StatusCode, web, HttpResponse};
use jmap::jmap_store::blob::JMAPBlobStore;
use jmap::types::jmap::JMAPId;

use jmap::{error::problem_details::ProblemDetails, types::blob::JMAPBlob};
use jmap_mail::mail::parse::get_message_part;
use store::{tracing::error, Store};

use crate::JMAPServer;

#[derive(serde::Deserialize)]
pub struct Params {
    accept: String,
}

pub async fn handle_jmap_download<T>(
    path: web::Path<(JMAPId, JMAPBlob, String)>,
    params: web::Query<Params>,
    core: web::Data<JMAPServer<T>>,
) -> HttpResponse
where
    T: for<'x> Store<'x> + 'static,
{
    let (account_id, blob_id, filename) = path.into_inner();

    let store = core.store.clone();
    let error = match core
        .spawn_worker(move || {
            store.blob_jmap_get(account_id.get_document_id(), &blob_id, get_message_part)
        })
        .await
    {
        Ok(Some(bytes)) => {
            return HttpResponse::build(StatusCode::OK)
                .insert_header(("Content-Type", params.into_inner().accept))
                .insert_header((
                    "Content-Disposition",
                    format!("attachment; filename=\"{}\"", filename), //TODO escape filename
                ))
                .insert_header(("Cache-Control", "private, immutable, max-age=31536000"))
                .body(bytes);
        }
        Ok(None) => ProblemDetails::not_found(),
        Err(err) => {
            error!("Blob download failed: {:?}", err);
            ProblemDetails::internal_server_error()
        }
    };

    HttpResponse::build(StatusCode::from_u16(error.status).unwrap())
        .insert_header(("Content-Type", "application/problem+json"))
        .body(error.to_json())
}
