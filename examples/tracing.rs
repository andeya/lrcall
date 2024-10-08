// Modifications Copyright Andeya Lee 2024
// Based on original source code from Google LLC licensed under MIT
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use crate::add::{Add as AddService, AddRpcStub};
use crate::double::Double as DoubleService;
use futures::future;
use futures::prelude::*;
use lrcall::client::stub::{load_balance, retry};
use lrcall::client::{self, RpcError};
use lrcall::server::incoming::{spawn_incoming, Incoming};
use lrcall::server::request_hook::{self, BeforeRequestList};
use lrcall::server::BaseChannel;
use lrcall::tokio_serde::formats::Json;
use lrcall::{context, serde_transport, ClientMessage, RequestName, Response, ServerError, Transport};
use opentelemetry::trace::TracerProvider as _;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing_subscriber::prelude::*;

pub mod add {
    #[lrcall::service]
    pub trait Add {
        /// Add two ints together.
        async fn add(x: i32, y: i32) -> i32;
    }
}

pub mod double {
    #[lrcall::service]
    pub trait Double {
        /// 2 * x
        async fn double(x: i32) -> Result<i32, String>;
    }
}

#[derive(Clone)]
struct AddServer;

impl AddService for AddServer {
    async fn add(self, _: context::Context, x: i32, y: i32) -> i32 {
        x + y
    }
}

#[derive(Clone)]
struct DoubleServer<L, R> {
    add_client: add::AddClient<L, R>,
}

impl<L, R> DoubleService for DoubleServer<L, R>
where
    L: add::Add + Clone + Send + Sync + 'static,
    R: AddRpcStub + Clone + Send + Sync + 'static,
{
    async fn double(self, _: context::Context, x: i32) -> Result<i32, String> {
        self.add_client.add(context::rpc_current(), x, x).await.map_err(|e| e.to_string())
    }
}

/// Initializes an OpenTelemetry tracing subscriber with a OTLP backend.
pub fn init_tracing(service_name: &'static str) -> anyhow::Result<()> {
    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_batch_config(opentelemetry_sdk::trace::BatchConfig::default())
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default().with_resource(opentelemetry_sdk::Resource::new([opentelemetry::KeyValue::new(
                opentelemetry_semantic_conventions::resource::SERVICE_NAME,
                service_name,
            )])),
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    let tracer = tracer_provider.tracer(service_name);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()?;

    Ok(())
}

async fn listen_on_random_port<Item, SinkItem>() -> anyhow::Result<(impl Stream<Item = serde_transport::Transport<TcpStream, Item, SinkItem, Json<Item, SinkItem>>>, std::net::SocketAddr)>
where
    Item: for<'de> serde::Deserialize<'de>,
    SinkItem: serde::Serialize,
{
    let listener = lrcall::serde_transport::tcp::listen("localhost:0", Json::default).await?.filter_map(|r| future::ready(r.ok())).take(1);
    let addr = listener.get_ref().get_ref().local_addr();
    Ok((listener, addr))
}

fn make_stub<Req, Resp, const N: usize>(
    backends: [impl Transport<ClientMessage<Arc<Req>>, Response<Resp>> + Send + Sync + 'static; N],
) -> retry::Retry<impl Fn(&Result<Resp, RpcError>, u32) -> bool + Clone, load_balance::RoundRobin<client::Channel<Arc<Req>, Resp>>>
where
    Req: RequestName + Send + Sync + 'static,
    Resp: Send + Sync + 'static,
{
    let stub = load_balance::RoundRobin::new(backends.into_iter().map(|transport| lrcall::client::new(client::Config::default(), transport).spawn()).collect());
    retry::Retry::new(stub, |resp, attempts| {
        if let Err(e) = resp {
            tracing::warn!("Got an error: {e:?}");
            attempts < 3
        } else {
            false
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing("lrcall_tracing_example")?;

    let (add_listener1, addr1) = listen_on_random_port().await?;
    let (add_listener2, addr2) = listen_on_random_port().await?;
    let something_bad_happened = Arc::new(AtomicBool::new(false));
    let server = request_hook::before()
        .then_fn(move |_: &mut _, _: &_| {
            let something_bad_happened = something_bad_happened.clone();
            async move {
                if something_bad_happened.fetch_xor(true, Ordering::Relaxed) {
                    Err(ServerError::new(io::ErrorKind::NotFound, "Gamma Ray!".into()))
                } else {
                    Ok(())
                }
            }
        })
        .serving(AddServer.serve());
    let add_server = add_listener1.chain(add_listener2).map(BaseChannel::with_defaults);
    tokio::spawn(spawn_incoming(add_server.execute(server)));

    let add_client = add::AddClient::<add::UnimplAdd, _>::rpc_client(make_stub([
        lrcall::serde_transport::tcp::connect(addr1, Json::default).await?,
        lrcall::serde_transport::tcp::connect(addr2, Json::default).await?,
    ]));

    let double_listener = lrcall::serde_transport::tcp::listen("localhost:0", Json::default).await?.filter_map(|r| future::ready(r.ok()));
    let addr = double_listener.get_ref().local_addr();
    let double_server = double_listener.map(BaseChannel::with_defaults).take(1);
    let server = DoubleServer { add_client };

    tokio::spawn(spawn_incoming(double_server.execute(server.serve())));

    let to_double_server = lrcall::serde_transport::tcp::connect(addr, Json::default).await?;
    let double_client = double::DoubleClient::<double::UnimplDouble>::rpc_client(double::DoubleChannel::spawn(client::Config::default(), to_double_server));

    let ctx = context::rpc_current();
    for _ in 1..=5 {
        tracing::info!("{:?}", double_client.double(ctx, 1).await?);
    }

    opentelemetry::global::shutdown_tracer_provider();

    Ok(())
}
