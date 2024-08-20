// Modifications Copyright Andeya Lee 2024
// Based on original source code from Google LLC licensed under MIT
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use futures::prelude::*;
use futures::{Sink, SinkExt, Stream, StreamExt, TryStreamExt};
use lrcall::serde_transport::tcp;
use lrcall::server::{BaseChannel, Channel};
use lrcall::tokio_serde::formats::Bincode;
use lrcall::{client, context};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::io;
use std::io::{Read, Write};

/// Type of compression that should be enabled on the request. The transport is free to ignore this.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Deserialize, Serialize)]
pub enum CompressionAlgorithm {
    Deflate,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum CompressedMessage<T> {
    Uncompressed(T),
    Compressed { algorithm: CompressionAlgorithm, payload: ByteBuf },
}

#[derive(Deserialize, Serialize)]
enum CompressionType {
    Uncompressed,
    Compressed,
}

async fn compress<T>(message: T) -> io::Result<CompressedMessage<T>>
where
    T: Serialize,
{
    let message = serialize(message)?;
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&message).unwrap();
    let compressed = encoder.finish()?;
    Ok(CompressedMessage::Compressed {
        algorithm: CompressionAlgorithm::Deflate,
        payload: ByteBuf::from(compressed),
    })
}

async fn decompress<T>(message: CompressedMessage<T>) -> io::Result<T>
where
    for<'a> T: Deserialize<'a>,
{
    match message {
        CompressedMessage::Compressed { algorithm, payload } => {
            if algorithm != CompressionAlgorithm::Deflate {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Compression algorithm {algorithm:?} not supported")));
            }
            let mut deflater = DeflateDecoder::new(payload.as_slice());
            let mut payload = ByteBuf::new();
            deflater.read_to_end(&mut payload)?;
            let message = deserialize(payload)?;
            Ok(message)
        },
        CompressedMessage::Uncompressed(message) => Ok(message),
    }
}

fn serialize<T: Serialize>(t: T) -> io::Result<ByteBuf> {
    bincode::serialize(&t).map(ByteBuf::from).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

fn deserialize<D>(message: ByteBuf) -> io::Result<D>
where
    for<'a> D: Deserialize<'a>,
{
    bincode::deserialize(message.as_ref()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

fn add_compression<In, Out>(
    transport: impl Stream<Item = io::Result<CompressedMessage<In>>> + Sink<CompressedMessage<Out>, Error = io::Error>,
) -> impl Stream<Item = io::Result<In>> + Sink<Out, Error = io::Error>
where
    Out: Serialize,
    for<'a> In: Deserialize<'a>,
{
    transport.with(compress).and_then(decompress)
}

#[lrcall::service]
pub trait World {
    async fn hello(name: String) -> String;
}

#[derive(Clone, Debug)]
struct HelloService;

impl World for HelloService {
    async fn hello(self, _: context::Context, name: String) -> String {
        format!("Hey, {name}!")
    }
}

async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut incoming = tcp::listen("localhost:0", Bincode::default).await?;
    let addr = incoming.local_addr();
    tokio::spawn(async move {
        let transport = incoming.next().await.unwrap().unwrap();
        BaseChannel::with_defaults(add_compression(transport)).execute(HelloService.serve()).for_each(spawn).await;
    });

    let transport = tcp::connect(addr, Bincode::default).await?;
    let client = WorldClient::<UnimplWorld>::rpc_client(WorldChannel::spawn(client::Config::default(), add_compression(transport)));

    println!("{}", client.hello(context::rpc_current(), "friend".into()).await?);
    Ok(())
}
