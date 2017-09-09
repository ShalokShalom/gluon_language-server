use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use jsonrpc_core::{Error, ErrorCode, Params, RpcMethodSimple, RpcNotificationSimple, Value};
use futures::{self, Async, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend};

use serde;
use serde_json::{from_value, to_string, to_value};

use BoxFuture;

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

pub trait LanguageServerCommand<P>: Send + Sync + 'static {
    type Output: serde::Serialize;
    type Error: serde::Serialize;
    fn execute(&self, param: P) -> BoxFuture<Self::Output, ServerError<Self::Error>>;

    fn invalid_params(&self) -> Option<Self::Error> {
        None
    }
}

impl<'de, F, P, O, E> LanguageServerCommand<P> for F
where
    F: Fn(P) -> BoxFuture<O, ServerError<E>> + Send + Sync + 'static,
    P: serde::Deserialize<'de>,
    O: serde::Serialize,
    E: serde::Serialize,
{
    type Output = O;
    type Error = E;

    fn execute(&self, param: P) -> BoxFuture<Self::Output, ServerError<Self::Error>> {
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
    P: for<'de> serde::Deserialize<'de> + 'static,
{
    fn call(&self, param: Params) -> BoxFuture<Value, Error> {
        let value = match param {
            Params::Map(map) => Value::Object(map),
            Params::Array(arr) => Value::Array(arr),
            Params::None => Value::Null,
        };
        let err = match from_value(value.clone()) {
            Ok(value) => {
                return Box::new(self.0.execute(value).then(|result| {
                    match result {
                        Ok(value) => Ok(
                            to_value(&value).expect("result data could not be serialized"),
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
            data: data.as_ref()
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
                Err(err) => log_message!("Invalid parameters. Reason: {}", err),
            },
            _ => log_message!("Invalid parameters: {:?}", param),
        }
    }
}


pub fn read_message<R>(mut reader: R) -> Result<Option<String>, Box<StdError>>
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
        Err(format!("Invalid message: `{}`", header).into())
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


extern crate bytes;

use std::str;

use tokio_io::codec::{Decoder, Encoder};
use self::bytes::{BufMut, BytesMut};

#[derive(Debug)]
pub struct LanguageServerDecoder {
    message_length: Option<usize>,
}

impl LanguageServerDecoder {
    pub fn new() -> LanguageServerDecoder {
        LanguageServerDecoder {
            message_length: None,
        }
    }
}

impl Decoder for LanguageServerDecoder {
    type Item = String;
    type Error = Box<::std::error::Error>;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match self.message_length {
            Some(message_length) if message_length <= src.len() => {
                let newlines_offset = src.iter()
                    .position(|&b| b != b'\n' && b != b'\r')
                    .unwrap_or(src.len());
                if newlines_offset != 0 {
                    src.split_to(newlines_offset);
                    return self.decode(src);
                }
                // Message is at least
                let result = String::from_utf8(src[..message_length].to_owned());
                src.split_to(message_length);
                // Start reading the next message
                self.message_length = None;
                Ok(Some(result?))
            }
            Some(_) => Ok(None),
            None => {
                const PREFIX: &str = "Content-Length: ";
                if src.starts_with(PREFIX.as_bytes()) {
                    let removed_len;
                    let content_length = {
                        removed_len = match src.iter().position(|&b| b == b'\r') {
                            Some(x) => x + 1,
                            None => return Ok(None),
                        };
                        let len = &src[PREFIX.len()..removed_len];
                        debug!("Parsing content length: {:?}", str::from_utf8(len));
                        str::from_utf8(len)?.trim().parse::<usize>()?
                    };
                    src.split_to(removed_len);
                    self.message_length = Some(content_length);
                    self.decode(src)
                } else {
                    let newlines_offset = src.iter()
                        .position(|&b| b != b'\n' && b != b'\r')
                        .unwrap_or(src.len());
                    if newlines_offset != 0 {
                        src.split_to(newlines_offset);
                        self.decode(src)
                    } else {
                        Ok(None)
                    }
                }
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

pub struct Entry<K, V> {
    pub key: K,
    pub value: V,
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
pub struct UniqueQueue<S, K, V> {
    sink: S,
    queue: VecDeque<Entry<K, V>>,
}

impl<S, K, V> UniqueQueue<S, K, V> {
    pub fn new(sink: S) -> Self {
        UniqueQueue {
            sink,
            queue: VecDeque::new(),
        }
    }
}

impl<S, K, V> Sink for UniqueQueue<S, K, V>
where
    S: Sink<SinkItem = Entry<K, V>>,
    K: PartialEq,
{
    type SinkItem = Entry<K, V>;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.sink.start_send(item)? {
            AsyncSink::Ready => Ok(AsyncSink::Ready),
            AsyncSink::NotReady(item) => {
                if let Some(entry) = self.queue.iter_mut().find(|entry| entry.key == item.key) {
                    entry.value = item.value;
                }
                Ok(AsyncSink::Ready)
            }
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        while let Some(item) = self.queue.pop_front() {
            match self.sink.start_send(item)? {
                AsyncSink::Ready => (),
                AsyncSink::NotReady(item) => {
                    self.queue.push_front(item);
                    break;
                }
            }
        }
        if self.queue.is_empty() {
            self.sink.poll_complete()
        } else {
            Ok(Async::NotReady)
        }
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        try_ready!(self.poll_complete());
        self.sink.close()
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
