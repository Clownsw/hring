use eyre::Context;
use http::header;
use tracing::debug;

use crate::{types::Request, util::read_and_parse, Body, HeadersExt, Response};
use hring_buffet::{PieceList, RollMut};
use maybe_uring::io::{ReadOwned, WriteOwned};

use super::{
    body::{write_h1_body, BodyWriteMode, H1Body, H1BodyKind},
    encode::encode_request,
};

pub struct ClientConf {}

pub trait ClientDriver {
    type Return;

    async fn on_informational_response(&mut self, res: Response) -> eyre::Result<()>;
    async fn on_final_response(
        self,
        res: Response,
        body: &mut impl Body,
    ) -> eyre::Result<Self::Return>;
}

/// Perform an HTTP/1.1 request against an HTTP/1.1 server
///
/// The transport halves will be returned unless the server requested connection
/// close or the request body wasn't fully drained
pub async fn request<R, W, D>(
    (mut transport_r, mut transport_w): (R, W),
    mut req: Request,
    body: &mut impl Body,
    driver: D,
) -> eyre::Result<(Option<(R, W)>, D::Return)>
where
    R: ReadOwned,
    W: WriteOwned,
    D: ClientDriver,
{
    let mode = match body.content_len() {
        Some(0) => BodyWriteMode::Empty,
        Some(len) => {
            // TODO: we can probably save a heap allocation here - we could format
            // directly to a `RollMut`, without going through `format!` machinery
            req.headers
                .insert(header::CONTENT_LENGTH, len.to_string().into_bytes().into());
            BodyWriteMode::ContentLength
        }
        None => BodyWriteMode::Chunked,
    };

    let mut buf = RollMut::alloc()?;

    let mut list = PieceList::default();
    encode_request(req, &mut list, &mut buf)?;
    transport_w
        .writev_all(list)
        .await
        .wrap_err("writing request headers")?;

    // TODO: handle `expect: 100-continue` (don't start sending body until we get a 100 response)

    let send_body_fut = {
        async move {
            match write_h1_body(&mut transport_w, body, mode).await {
                Err(err) => {
                    // TODO: find way to report this error to the driver without
                    // spawning, without ref-counting the driver, etc.
                    panic!("error writing request body: {err:?}");
                }
                Ok(_) => {
                    debug!("done writing request body");
                    Ok::<_, eyre::Report>(transport_w)
                }
            }
        }
    };

    let recv_res_fut = {
        async move {
            let (buf, res) = read_and_parse(
                super::parse::response,
                &mut transport_r,
                buf,
                // TODO: make this configurable
                64 * 1024,
            )
            .await
            .map_err(|e| eyre::eyre!("error reading response headers from server: {e:?}"))?
            .ok_or_else(|| eyre::eyre!("server went away before sending response headers"))?;
            debug!("client received response");
            res.debug_print();

            if res.status.is_informational() {
                todo!("handle informational responses");
            }

            let chunked = res.headers.is_chunked_transfer_encoding();

            // TODO: handle 204/304 separately
            let content_len = res.headers.content_length().unwrap_or_default();

            let mut res_body = H1Body::new(
                transport_r,
                buf,
                if chunked {
                    // TODO: even with chunked transfer-encoding, we can announce
                    // a content length - we should probably detect errors there?
                    H1BodyKind::Chunked
                } else {
                    H1BodyKind::ContentLength(content_len)
                },
            );

            let conn_close = res.headers.is_connection_close();

            let ret = driver.on_final_response(res, &mut res_body).await?;

            let transport_r = match (conn_close, res_body.into_inner()) {
                // can only re-use the body if conn_close is false and the body was fully draided
                (false, Some((_buf, transport_r))) => Some(transport_r),
                _ => None,
            };

            Ok((transport_r, ret))
        }
    };

    // TODO: cancel sending the body if we get a response early?
    let (send_res, recv_res) = tokio::try_join!(send_body_fut, recv_res_fut)?;
    let transport_w = send_res;
    let (transport_r, ret) = recv_res;

    let transport = transport_r.map(|transport_r| (transport_r, transport_w));
    Ok((transport, ret))
}
