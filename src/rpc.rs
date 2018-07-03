extern crate bytes;
extern crate combine;

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::marker::PhantomData;
use std::str;
use std::sync::{Arc, Mutex};

use failure;

use self::combine::combinator::{any_send_partial_state, AnySendPartialState};
use self::combine::error::{ParseError, StreamError};
use self::combine::parser::byte::digit;
use self::combine::parser::range::{range, recognize, take};
use self::combine::stream::easy;
use self::combine::stream::{PartialStream, RangeStream, StreamErrorFor};
use self::combine::{skip_many, skip_many1, Parser};

use self::bytes::{BufMut, BytesMut};

use tokio_io::codec::{Decoder, Encoder};

use futures::sync::mpsc;
use futures::{self, Async, Future, IntoFuture, Poll, Sink, StartSend, Stream};

use jsonrpc_core::{Error, ErrorCode, Params, RpcMethodSimple, RpcNotificationSimple, Value};

use serde;
use serde_json::{from_value, to_string, to_value};

use BoxFuture;

#[derive(Debug, PartialEq)]
pub struct ServerError<E> {
    pub message: String,
    pub data: Option<E>,
}

impl<E, D> From<E> for ServerError<D>
where
    E: fmt::Display,
{
    fn from(err: E) -> ServerError<D> {
        ServerError {
            message: err.to_string(),
            data: None,
        }
    }
}

pub trait LanguageServerCommand<P>: Send + Sync + 'static
where
    Self::Future: Send + 'static,
{
    type Future: IntoFuture<Item = Self::Output, Error = ServerError<Self::Error>> + Send + 'static;
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: P) -> Self::Future;

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

impl<'de, F, R, P, O, E> LanguageServerCommand<P> for F
where
    F: Fn(P) -> R + Send + Sync + 'static,
    R: IntoFuture<Item = O, Error = ServerError<E>> + Send + 'static,
    R::Future: Send + 'static,
    P: serde::Deserialize<'de>,
    O: serde::Serialize,
    E: serde::Serialize,
{
    type Future = F::Output;
    type Output = O;
    type Error = E;

    fn execute(&self, param: P) -> Self::Future {
        self(param)
    }
}

pub trait LanguageServerNotification<P>: Send + Sync + 'static {
    fn execute(&self, param: P);
}

impl<'de, F, P> LanguageServerNotification<P> for F
where
    F: Fn(P) + Send + Sync + 'static,
    P: serde::Deserialize<'de> + 'static,
{
    fn execute(&self, param: P) {
        self(param)
    }
}
pub struct ServerCommand<T, P>(pub T, PhantomData<fn(P)>);

impl<T, P> ServerCommand<T, P> {
    pub fn method(command: T) -> ServerCommand<T, P>
    where
        T: LanguageServerCommand<P>,
        <T::Future as IntoFuture>::Future: Send + 'static,
        P: for<'de> serde::Deserialize<'de> + 'static,
    {
        ServerCommand(command, PhantomData)
    }

    pub fn notification(command: T) -> ServerCommand<T, P>
    where
        T: LanguageServerNotification<P>,
        P: for<'de> serde::Deserialize<'de> + 'static,
    {
        ServerCommand(command, PhantomData)
    }
}

impl<P, T> RpcMethodSimple for ServerCommand<T, P>
where
    T: LanguageServerCommand<P>,
    <T::Future as IntoFuture>::Future: Send + 'static,
    P: for<'de> serde::Deserialize<'de> + 'static,
{
    type Out = BoxFuture<Value, Error>;
    fn call(&self, param: Params) -> BoxFuture<Value, Error> {
        let value = match param {
            Params::Map(map) => Value::Object(map),
            Params::Array(arr) => Value::Array(arr),
            Params::None => Value::Null,
        };
        let err = match from_value(value.clone()) {
            Ok(value) => {
                return Box::new(self.0.execute(value).into_future().then(|result| {
                    match result {
                        Ok(value) => Ok(
                            to_value(&value).expect("result data could not be serialized")
                        ).into_future(),
                        Err(error) => Err(Error {
                            code: ErrorCode::InternalError,
                            message: error.message,
                            data: error
                                .data
                                .as_ref()
                                .map(|v| to_value(v).expect("error data could not be serialized")),
                        }).into_future(),
                    }
                }))
            }
            Err(err) => err,
        };
        let data = self.0.invalid_params();
        Box::new(futures::failed(Error {
            code: ErrorCode::InvalidParams,
            message: format!("Invalid params: {}", err),
            data: data
                .as_ref()
                .map(|v| to_value(v).expect("error data could not be serialized")),
        }))
    }
}

impl<T, P> RpcNotificationSimple for ServerCommand<T, P>
where
    T: LanguageServerNotification<P>,
    P: for<'de> serde::Deserialize<'de> + 'static,
{
    fn execute(&self, param: Params) {
        match param {
            Params::Map(map) => match from_value(Value::Object(map)) {
                Ok(value) => {
                    self.0.execute(value);
                }
                Err(err) => error!("{}", err), // FIXME log_message!("Invalid parameters. Reason: {}", err),
            },
            _ => (), // FIXME log_message!("Invalid parameters: {:?}", param),
        }
    }
}

pub fn read_message<R>(mut reader: R) -> Result<Option<String>, failure::Error>
where
    R: BufRead + Read,
{
    let mut header = String::new();
    let n = try!(reader.read_line(&mut header));
    if n == 0 {
        return Ok(None);
    }

    if header.starts_with("Content-Length: ") {
        let content_length = {
            let len = header["Content-Length:".len()..].trim();
            debug!("{}", len);
            try!(len.parse::<usize>())
        };
        while header != "\r\n" {
            header.clear();
            try!(reader.read_line(&mut header));
        }
        let mut content = vec![0; content_length];
        try!(reader.read_exact(&mut content));
        Ok(Some(try!(String::from_utf8(content))))
    } else {
        Err(failure::err_msg(format!("Invalid message: `{}`", header)))
    }
}

pub fn write_message<W, T>(output: W, value: &T) -> io::Result<()>
where
    W: Write,
    T: serde::Serialize,
{
    let response = to_string(&value).unwrap();
    write_message_str(output, &response)
}

pub fn write_message_str<W>(mut output: W, response: &str) -> io::Result<()>
where
    W: Write,
{
    debug!("Respond: {}", response);
    try!(write!(
        output,
        "Content-Length: {}\r\n\r\n{}",
        response.len(),
        response
    ));
    try!(output.flush());
    Ok(())
}

pub struct LanguageServerDecoder {
    state: AnySendPartialState,
}

impl LanguageServerDecoder {
    pub fn new() -> LanguageServerDecoder {
        LanguageServerDecoder {
            state: Default::default(),
        }
    }
}

/// Parses blocks of data with length headers
///
/// ```ignore
/// Content-Length: 18
///
/// { "some": "data" }
/// ```
fn decode_parser<'a, I>(
) -> impl Parser<Input = I, Output = Vec<u8>, PartialState = AnySendPartialState> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]> + 'a,
    // Necessary due to rust-lang/rust#24159
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    let content_length = range(&b"Content-Length: "[..]).with(
        recognize(skip_many1(digit())).and_then(|digits: &[u8]| {
            str::from_utf8(digits).unwrap().parse::<usize>()
                                // Convert the error from `.parse` into an error combine understands
                                .map_err(StreamErrorFor::<I>::other)
        }),
    );

    any_send_partial_state(
        (
            skip_many(range(&b"\r\n"[..])),
            content_length,
            range(&b"\r\n\r\n"[..]).map(|_| ()),
        ).then_partial(|&mut (_, message_length, _)| {
            take(message_length).map(|bytes: &[u8]| bytes.to_owned())
        }),
    )
}

impl Decoder for LanguageServerDecoder {
    type Item = String;
    type Error = failure::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let (opt, removed_len) = combine::stream::decode(
            decode_parser(),
            easy::Stream(PartialStream(&src[..])),
            &mut self.state,
        ).map_err(|err| {
            let err =
                err.map_range(|r| {
                    str::from_utf8(r)
                        .ok()
                        .map_or_else(|| format!("{:?}", r), |s| s.to_string())
                }).map_position(|p| p.translate_position(&src[..]));
            failure::err_msg(format!(
                "{}\nIn input: `{}`",
                err,
                str::from_utf8(src).unwrap()
            ))
        })?;

        eprintln!(
            "Accept: {:?}",
            ::std::str::from_utf8(&src[..removed_len]).unwrap()
        );
        eprintln!("{:?}", ::std::str::from_utf8(&src[removed_len..]).unwrap());
        src.split_to(removed_len);

        match opt {
            None => Ok(None),

            Some(output) => {
                let value = String::from_utf8(output)?;
                Ok(Some(value))
            }
        }
    }
}

#[derive(Debug)]
pub struct LanguageServerEncoder;

impl Encoder for LanguageServerEncoder {
    type Item = String;
    type Error = Box<::std::error::Error>;
    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        write_message_str(dst.writer(), &item)?;
        Ok(())
    }
}

pub struct Entry<K, V, W> {
    pub key: K,
    pub value: V,
    pub version: W,
}

#[derive(Debug)]
pub struct SharedSink<S>(Arc<Mutex<S>>);

impl<S> Clone for SharedSink<S> {
    fn clone(&self) -> Self {
        SharedSink(self.0.clone())
    }
}

impl<S> SharedSink<S> {
    pub fn new(sink: S) -> SharedSink<S> {
        SharedSink(Arc::new(Mutex::new(sink)))
    }
}

impl<S> Sink for SharedSink<S>
where
    S: Sink,
{
    type SinkItem = S::SinkItem;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.0.lock().unwrap().start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.0.lock().unwrap().poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.0.lock().unwrap().close()
    }
}

/// Queue which only keeps the latest work item for each key
pub struct UniqueSink<K, V, W> {
    sender: mpsc::UnboundedSender<Entry<K, V, W>>,
}

impl<K, V, W> Clone for UniqueSink<K, V, W> {
    fn clone(&self) -> Self {
        UniqueSink {
            sender: self.sender.clone(),
        }
    }
}

pub struct UniqueStream<K, V, W> {
    queue: VecDeque<Entry<K, V, W>>,
    receiver: mpsc::UnboundedReceiver<Entry<K, V, W>>,
    exhausted: bool,
}

pub fn unique_queue<K, V, W>() -> (UniqueSink<K, V, W>, UniqueStream<K, V, W>)
where
    K: PartialEq,
    W: Ord,
{
    let (sender, receiver) = mpsc::unbounded();
    (
        UniqueSink { sender },
        UniqueStream {
            queue: VecDeque::new(),
            receiver,
            exhausted: false,
        },
    )
}

impl<K, V, W> Stream for UniqueStream<K, V, W>
where
    K: PartialEq,
    W: Ord,
{
    type Item = Entry<K, V, W>;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        while !self.exhausted {
            match self.receiver.poll()? {
                Async::Ready(Some(item)) => {
                    if let Some(entry) = self.queue.iter_mut().find(|entry| entry.key == item.key) {
                        if entry.version < item.version {
                            *entry = item;
                        }
                        continue;
                    }
                    self.queue.push_back(item);
                }
                Async::Ready(None) => {
                    self.exhausted = true;
                }
                Async::NotReady => break,
            }
        }
        match self.queue.pop_front() {
            Some(item) => Ok(Async::Ready(Some(item))),
            None => {
                if self.exhausted {
                    Ok(Async::Ready(None))
                } else {
                    Ok(Async::NotReady)
                }
            }
        }
    }
}

impl<K, V, W> Sink for UniqueSink<K, V, W> {
    type SinkItem = Entry<K, V, W>;
    type SinkError = mpsc::SendError<Entry<K, V, W>>;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.sender.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sender.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.sender.close()
    }
}

pub struct SinkFn<F, I> {
    f: F,
    _marker: PhantomData<fn(I) -> I>,
}

pub fn sink_fn<F, I, E>(f: F) -> SinkFn<F, I>
where
    F: FnMut(I) -> StartSend<I, E>,
{
    SinkFn {
        f,
        _marker: PhantomData,
    }
}

impl<F, I, E> Sink for SinkFn<F, I>
where
    F: FnMut(I) -> StartSend<I, E>,
{
    type SinkItem = I;
    type SinkError = E;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        (self.f)(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
