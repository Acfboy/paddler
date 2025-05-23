use async_trait::async_trait;
use bytes::Bytes;
use log::error;
use pingora::{
    http::RequestHeader,
    protocols::Digest,
    proxy::{ProxyHttp, Session},
    upstreams::peer::HttpPeer,
    Error, ErrorSource, Result,
};
use std::{sync::Arc, time::Duration};

use crate::{
    balancer::{upstream_peer::UpstreamPeerInfo, upstream_peer_pool::UpstreamPeerPool},
    errors::result::Result as PaddlerResult,
};

pub struct LlamaCppContext {
    slot_taken: bool,
    selected_peer: Option<UpstreamPeerInfo>,
    uses_slots: bool,
}

pub struct ProxyService {
    rewrite_host_header: bool,
    slots_endpoint_enable: bool,
    upstream_peer_pool: Arc<UpstreamPeerPool>,
}

impl ProxyService {
    pub fn new(
        rewrite_host_header: bool,
        slots_endpoint_enable: bool,
        upstream_peer_pool: Arc<UpstreamPeerPool>,
    ) -> Self {
        Self {
            rewrite_host_header,
            slots_endpoint_enable,
            upstream_peer_pool,
        }
    }

    #[inline]
    fn release_slot(&self, ctx: &mut LlamaCppContext) -> PaddlerResult<()> {
        if let Some(peer) = &ctx.selected_peer {
            self.upstream_peer_pool
                .release_slot(&peer.agent_id, peer.last_update)?;
            self.upstream_peer_pool.restore_integrity()?;

            ctx.slot_taken = false;
        }

        Ok(())
    }

    #[inline]
    fn release_permit(&self, ctx: &mut LlamaCppContext) -> PaddlerResult<()> {
        if let Some(peer) = &ctx.selected_peer {
            self.upstream_peer_pool.release_one_permit(&peer.agent_id)?;

            ctx.slot_taken = false;
        }

        Ok(())
    }

    #[inline]
    fn take_slot(&self, ctx: &mut LlamaCppContext) -> PaddlerResult<()> {
        if let Some(peer) = &ctx.selected_peer {
            self.upstream_peer_pool.take_slot(&peer.agent_id)?;
            self.upstream_peer_pool.restore_integrity()?;

            ctx.slot_taken = true;
        }

        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for ProxyService {
    type CTX = LlamaCppContext;

    fn new_ctx(&self) -> Self::CTX {
        LlamaCppContext {
            selected_peer: None,
            slot_taken: false,
            uses_slots: false,
        }
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx.uses_slots && !ctx.slot_taken {
            if let Err(e) = self.take_slot(ctx) {
                error!("Failed to take slot: {}", e);

                return Err(Error::new(pingora::InternalError));
            }
        }

        Ok(())
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        error!("Error while proxying: {}", e);

        let retry = client_reused && !session.as_ref().retry_buffer_truncated();

        if ctx.slot_taken {
            if let Err(err) = self.release_slot(ctx) {
                error!("Failed to release slot: {}", err);

                return Error::new(pingora::InternalError);
            }
            if !retry {
                if let Err(err) = self.release_permit(ctx) {
                    error!("Failed to release permit: {}", err);

                    return Error::new(pingora::InternalError);
                }
            }
        }

        let mut e = e.more_context(format!("Peer: {}", peer));

        // only reused client connections where retry buffer is not truncated
        e.retry.decide_reuse(retry);

        e
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        error!("Failed to connect: {}", e);
        if let Some(peer) = &ctx.selected_peer {
            match self.upstream_peer_pool.quarantine_peer(&peer.agent_id) {
                Ok(true) => {
                    if let Err(err) = self.upstream_peer_pool.restore_integrity() {
                        error!("Failed to restore integrity: {}", err);

                        return Error::new(pingora::InternalError);
                    }

                    // ask server to retry, but try a different best peer
                    ctx.selected_peer = None;
                    e.set_retry(true);
                }
                Ok(false) => {
                    // no need to quarantine for some reason
                }
                Err(err) => {
                    error!("Failed to quarantine peer: {}", err);

                    return Error::new(pingora::InternalError);
                }
            }
        }

        e
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.uses_slots = match session.req_header().uri.path() {
            "/slots" => {
                if !self.slots_endpoint_enable {
                    return Err(Error::create(
                        pingora::Custom("Slots endpoint is disabled"),
                        ErrorSource::Downstream,
                        None,
                        None,
                    ));
                }

                false
            }
            "/chat/completions" => true,
            "/completion" => true,
            "/v1/chat/completions" => true,
            _ => false,
        };

        Ok(false)
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.slot_taken && end_of_stream {
            if let Err(err) = self.release_slot(ctx) {
                error!("Failed to release slot: {}", err);

                return Err(Error::new(pingora::InternalError));
            } else if let Err(err) = self.release_permit(ctx) {
                error!("Failed to release permit: {}", err);
                return Err(Error::new(pingora::InternalError));
            }
        }

        Ok(None)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        if ctx.selected_peer.is_none() {
            let smaphore = self.upstream_peer_pool.upstream_slots_permits.clone();
            let permit = match smaphore.acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    error!("Failed to get slot permit: {}", e);
                    return Err(Error::new(pingora::InternalError));
                }
            };

            ctx.selected_peer = match self.upstream_peer_pool.use_best_peer() {
                Ok(peer) => peer,
                Err(e) => {
                    // ideally unreachable
                    error!("Failed to get peer even under permits: {e}");
                    return Err(Error::new(pingora::InternalError));
                }
            };

            if ctx.selected_peer.is_none() {
                error!("Failed to get peer even under permits!");
                return Err(Error::new(pingora::InternalError));
            }

            let store_res = self
                .upstream_peer_pool
                .store_permit(&ctx.selected_peer.as_ref().unwrap().agent_id, permit);

            match store_res {
                Ok(r) => {
                    if !r {
                        // ideally unreachable
                        error!("Failed to get peer even under permits!");
                        return Err(Error::new(pingora::InternalError));
                    }
                }
                Err(e) => {
                    // ideally unreachable
                    error!("Failed to get peer even under permits: {e}");
                    return Err(Error::new(pingora::InternalError));
                }
            }
        }

        let selected_peer = match ctx.selected_peer.as_ref() {
            Some(peer) => peer,
            None => {
                // ideally unreachable
                return Err(Error::create(
                    pingora::Custom("No peer available"),
                    ErrorSource::Upstream,
                    None,
                    None,
                ));
            }
        };

        Ok(Box::new(HttpPeer::new(
            selected_peer.external_llamacpp_addr,
            false,
            "".to_string(),
        )))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if self.rewrite_host_header {
            if let Some(peer) = &ctx.selected_peer {
                upstream_request
                    .insert_header("Host".to_string(), peer.external_llamacpp_addr.to_string())?;
            }
        }

        Ok(())
    }
}
