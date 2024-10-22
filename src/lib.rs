varnish::boilerplate!();

use anyhow::{anyhow, Context, Error, Result};
use bytes::Bytes;
use cloudbucket::aws::S3;
use futures::channel::mpsc::{self, Receiver as FutReceiver, Sender as FutSender};
use http_body_util::StreamBody;
use reqwest::Body;
use std::boxed::Box;
use std::io::Write;
use std::os::raw::{c_char, c_uint, c_void};
use std::time::Duration;
use varnish_sys::VSL_tag_e_SLT_VCL_Log;

use tokio::sync::mpsc::{Receiver, Sender, UnboundedSender};
use varnish::vcl::backend::{Backend, Serve, Transfer, VCLBackendPtr};
use varnish::vcl::ctx::{Ctx, Event, LogTag};
use varnish::vcl::vpriv::VPriv;

mod cloudbucket;

macro_rules! send {
    ($tx:ident, $payload:expr) => {
        if $tx.send($payload).await.is_err() {
            return;
        }
    };
}

struct VCLBackend {
    name: String,
    bgt: *const BgThread,
    client: reqwest::Client,
    aws_s3_client: S3,
}

impl<'a> Serve<BackendResp> for VCLBackend {
    fn get_type(&self) -> &str {
        "cloudbucket"
    }

    fn get_headers(
        &self,
        ctx: &mut Ctx<'_>,
    ) -> Result<Option<BackendResp>, Box<dyn std::error::Error>> {
        if !self.healthy(ctx).0 {
            return Err("unhealthy".into());
        }
        let bereq = ctx.http_bereq.as_ref().unwrap();
        let bereq_method = bereq.method().unwrap().to_string();
        let bereq_url = bereq.url().unwrap().to_string();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let aws_s3_client = self.aws_s3_client.clone();
        let url_res = futures::executor::block_on(async {
            rt.spawn(async move { aws_s3_client.get_presigned_request(&bereq_url).await })
                .await
                .unwrap()
        });

        if url_res.is_err() {
            return Err(url_res.err().unwrap().to_string().into());
        }
        let url = url_res.unwrap().uri().to_string();

        ctx.log(
            LogTag::Any(VSL_tag_e_SLT_VCL_Log),
            format!("Requesting url: {}", url).as_str(),
        );

        let (req_body_tx, body) =
            mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(0);
        let req = Request {
            method: bereq_method,
            url,
            client: self.client.clone(),
            body: ReqBody::Stream(StreamBody::new(body)),
            vcl: false,
            headers: vec![],
        };

        let mut resp_rx = unsafe { (*self.bgt).spawn_req(req) };

        unsafe {
            struct BodyChan<'a> {
                chan: FutSender<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>,
                rt: &'a tokio::runtime::Runtime,
            }

            unsafe extern "C" fn body_send_iterate(
                priv_: *mut c_void,
                _flush: c_uint,
                ptr: *const c_void,
                l: isize,
            ) -> i32 {
                // nothing to do
                if ptr.is_null() || l == 0 {
                    return 0;
                }
                let body_chan = (priv_ as *mut BodyChan).as_mut().unwrap();
                let buf = std::slice::from_raw_parts(ptr as *const u8, l as usize);
                let bytes = hyper::body::Bytes::copy_from_slice(buf);

                body_chan
                    .rt
                    .block_on(async { body_chan.chan.start_send(Ok(bytes)) })
                    .is_err()
                    .into()
            }

            // manually dropped a few lines below
            let bcp = Box::into_raw(Box::new(BodyChan {
                chan: req_body_tx,
                rt: &(*self.bgt).rt,
            }));
            let p = bcp as *mut c_void;
            // mimicking V1F_SendReq in varnish-cache
            let bo = (*ctx.raw).bo.as_mut().unwrap();

            if !(*bo).bereq_body.is_null() {
                varnish_sys::ObjIterate(bo.wrk, bo.bereq_body, p, Some(body_send_iterate), 0);
            } else if !bo.req.is_null()
                && (*bo.req).req_body_status != varnish_sys::BS_NONE.as_ptr()
            {
                let i = varnish_sys::VRB_Iterate(
                    bo.wrk,
                    bo.vsl.as_mut_ptr(),
                    bo.req,
                    Some(body_send_iterate),
                    p,
                );

                if (*bo.req).req_body_status != varnish_sys::BS_CACHED.as_ptr() {
                    bo.no_retry = "req.body not cached\0".as_ptr() as *const c_char;
                }

                if (*bo.req).req_body_status == varnish_sys::BS_ERROR.as_ptr() {
                    assert!(i < 0);
                    (*bo.req).doclose = &varnish_sys::SC_RX_BODY[0];
                }

                if i < 0 {
                    return Err("req.body read error".into());
                }
            }
            // manually drop so reqwest knows there's no more body to push
            drop(Box::from_raw(bcp));
        }
        let resp = match resp_rx.blocking_recv().unwrap() {
            RespMsg::Hdrs(resp) => resp,
            RespMsg::Err(e) => return Err(e.to_string().into()),
            _ => unreachable!(),
        };
        let beresp = ctx.http_beresp.as_mut().unwrap();
        beresp.set_status(resp.status as u16);
        beresp.set_proto("HTTP/1.1")?;
        for (k, v) in &resp.headers {
            beresp.set_header(k.as_str(), v.to_str()?)?;
        }
        Ok(Some(BackendResp {
            bytes: None,
            cursor: 0,
            chan: Some(resp_rx),
            content_length: resp.content_length.map(|s| s as usize),
        }))
    }
}

struct BackendResp {
    chan: Option<Receiver<RespMsg>>,
    bytes: Option<Bytes>,
    cursor: usize,
    content_length: Option<usize>,
}

impl Transfer for BackendResp {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, Box<dyn std::error::Error>> {
        let mut n = 0;
        loop {
            if self.bytes.is_none() && self.chan.is_some() {
                match self.chan.as_mut().unwrap().blocking_recv() {
                    Some(RespMsg::Hdrs(_)) => panic!("invalid message type: RespMsg::Hdrs"),
                    Some(RespMsg::Chunk(bytes)) => {
                        self.bytes = Some(bytes);
                        self.cursor = 0
                    }
                    Some(RespMsg::Err(e)) => return Err(e.to_string().into()),
                    None => return Ok(n),
                };
            }

            let pull_buf = self.bytes.as_ref().unwrap();
            let to_write = &pull_buf[self.cursor..];
            let used = buf.write(to_write).unwrap();
            self.cursor += used;
            n += used;
            assert!(self.cursor <= pull_buf.len());
            if self.cursor == pull_buf.len() {
                self.bytes = None;
            }
        }
    }

    fn len(&self) -> Option<usize> {
        self.content_length
    }
}

#[allow(non_camel_case_types)]
struct client {
    name: String,
    be: Backend<VCLBackend, BackendResp>,
}

impl client {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: &mut Ctx,
        vcl_name: &str,
        vp_vcl: &mut VPriv<BgThread>,
        s3_bucket: &str,
        region: &str,
        follow: i64,
        timeout: Option<Duration>,
        connect_timeout: Option<Duration>,
        auto_gzip: bool,
        auto_deflate: bool,
        auto_brotli: bool,
        accept_invalid_certs: bool,
        accept_invalid_hostnames: bool,
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let s3_bucket_str = s3_bucket.to_string();
        let region_str = region.to_string();
        let aws_s3_client = futures::executor::block_on(async {
            rt.spawn(async move { S3::new(s3_bucket_str, region_str).await })
                .await
                .unwrap()
        });

        let mut rcb = reqwest::ClientBuilder::new()
            .brotli(auto_brotli)
            .deflate(auto_deflate)
            .gzip(auto_gzip)
            .danger_accept_invalid_certs(accept_invalid_certs)
            .danger_accept_invalid_hostnames(accept_invalid_hostnames);
        if let Some(t) = timeout {
            rcb = rcb.timeout(t);
        }
        if let Some(t) = connect_timeout {
            rcb = rcb.connect_timeout(t);
        }
        if let Some(proxy) = http_proxy {
            rcb = rcb.proxy(reqwest::Proxy::http(proxy).with_context(|| {
                format!("reqwest: couldn't initialize {}'s HTTP proxy", vcl_name)
            })?);
        }
        if let Some(proxy) = https_proxy {
            rcb = rcb.proxy(reqwest::Proxy::https(proxy).with_context(|| {
                format!("reqwest: couldn't initialize {}'s HTTPS proxy", vcl_name)
            })?);
        }
        if follow <= 0 {
            rcb = rcb.redirect(reqwest::redirect::Policy::none());
        } else {
            rcb = rcb.redirect(reqwest::redirect::Policy::limited(follow as usize));
        }
        let reqwest_client = rcb
            .build()
            .with_context(|| format!("reqwest: couldn't initialize {}", vcl_name))?;

        let be = Backend::new(
            ctx,
            vcl_name,
            VCLBackend {
                name: vcl_name.to_string(),
                bgt: vp_vcl.as_ref().unwrap(),
                client: reqwest_client,
                aws_s3_client,
            },
            false,
        )?;
        let client = client {
            name: vcl_name.to_owned(),
            be,
        };
        Ok(client)
    }

    pub fn backend(&self, _ctx: &Ctx) -> VCLBackendPtr {
        self.be.vcl_ptr()
    }
}

#[derive(Debug)]
enum RespMsg {
    Hdrs(Response),
    Chunk(Bytes),
    Err(Error),
}

#[derive(Debug)]
pub struct Entry {
    client_name: String,
    req_name: String,
    transaction: VclTransaction,
}

// try to keep the object on stack as small as possible, we'll flesh it out into a reqwest::Request
// once in the Background thread
#[derive(Debug)]
struct Request {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: ReqBody,
    client: reqwest::Client,
    vcl: bool,
}

// calling reqwest::Response::body() consumes the object, so we keep a copy of the interesting bits
// in this struct
#[derive(Debug)]
pub struct Response {
    headers: reqwest::header::HeaderMap,
    content_length: Option<u64>,
    body: Option<Bytes>,
    status: i64,
}

#[derive(Debug)]
enum ReqBody {
    None,
    Full(Vec<u8>),
    Stream(StreamBody<FutReceiver<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>>),
}

#[derive(Debug)]
enum VclTransaction {
    Transition,
    Req(Request),
    Sent(Receiver<RespMsg>),
    Resp(Result<Response>),
}

impl VclTransaction {
    fn unwrap_resp(&self) -> Result<&Response> {
        match self {
            VclTransaction::Resp(Ok(rsp)) => Ok(rsp),
            VclTransaction::Resp(Err(e)) => Err(anyhow!(e.to_string())),
            _ => panic!("wrong VclTransaction type"),
        }
    }
    fn into_req(self) -> Request {
        match self {
            VclTransaction::Req(rq) => rq,
            _ => panic!("wrong VclTransaction type"),
        }
    }
}

pub struct BgThread {
    rt: tokio::runtime::Runtime,
    sender: UnboundedSender<(Request, Sender<RespMsg>)>,
}

impl BgThread {
    fn spawn_req(&self, req: Request) -> Receiver<RespMsg> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        self.sender.send((req, tx)).unwrap();
        rx
    }
}

async fn process_req(req: Request, tx: Sender<RespMsg>) {
    let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            send!(tx, RespMsg::Err(e.into()));
            return;
        }
    };
    let mut rreq = req.client.request(method, req.url);
    for (k, v) in req.headers {
        rreq = rreq.header(k, v);
    }
    match req.body {
        ReqBody::None => (),
        ReqBody::Stream(b) => rreq = rreq.body(Body::wrap_stream(b)),
        ReqBody::Full(v) => rreq = rreq.body(v),
    }
    let mut resp = match rreq.send().await {
        Err(e) => {
            send!(tx, RespMsg::Err(e.into()));
            return;
        }
        Ok(resp) => resp,
    };
    let mut beresp = Response {
        status: resp.status().as_u16() as i64,
        headers: resp.headers().clone(),
        content_length: resp.content_length(),
        body: None,
    };

    if req.vcl {
        beresp.body = match resp.bytes().await {
            Err(e) => {
                send!(tx, RespMsg::Err(e.into()));
                return;
            }
            Ok(b) => Some(b),
        };
        send!(tx, RespMsg::Hdrs(beresp));
    } else {
        send!(tx, RespMsg::Hdrs(beresp));

        loop {
            match resp.chunk().await {
                Ok(None) => return,
                Ok(Some(bytes)) => {
                    if tx.send(RespMsg::Chunk(bytes)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    send!(tx, RespMsg::Err(e.into()));
                    return;
                }
            };
        }
    }
}

pub(crate) unsafe fn event(_ctx: &Ctx, vp: &mut VPriv<BgThread>, event: Event) -> Result<()> {
    // we only need to worry about Load, BgThread will be destroyed with the VPriv when the VCL is
    // discarded
    if let Event::Load = event {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (sender, mut receiver) =
            tokio::sync::mpsc::unbounded_channel::<(Request, Sender<RespMsg>)>();
        rt.spawn(async move {
            loop {
                let (req, tx) = receiver.recv().await.unwrap();
                tokio::spawn(async move {
                    process_req(req, tx).await;
                });
            }
        });
        vp.store(BgThread { rt, sender });
    }
    Ok(())
}
