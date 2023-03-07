//! http server implementation on top of `MAY`

use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;

use crate::request::{self, Request};
use crate::response::{self, Response};
use bytes::{Buf, BufMut, BytesMut};
#[cfg(unix)]
use may::io::WaitIo;
use may::net::{TcpListener, TcpStream};
use may::{coroutine, go};

macro_rules! t_c {
    ($e: expr) => {
        match $e {
            Ok(val) => val,
            Err(err) => {
                error!("call = {:?}\nerr = {:?}", stringify!($e), err);
                continue;
            }
        }
    };
}

/// the http service trait
/// user code should supply a type that impl the `call` method for the http server
///
pub trait HttpService {
    fn call(&mut self, req: Request, rsp: &mut Response) -> io::Result<()>;
}

pub trait HttpServiceFactory: Send + Sized + 'static {
    type Service: HttpService + Send;
    // create a new http service for each connection
    fn new_service(&self, id: usize) -> Self::Service;

    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        go!(
            coroutine::Builder::new().name("TcpServerFac".to_owned()),
            move || {
                #[cfg(unix)]
                use std::os::fd::AsRawFd;
                #[cfg(windows)]
                use std::os::windows::io::AsRawSocket;
                for stream in listener.incoming() {
                    let mut stream = t_c!(stream);
                    #[cfg(unix)]
                    let id = stream.as_raw_fd() as usize;
                    #[cfg(windows)]
                    let id = stream.as_raw_socket() as usize;
                    // t_c!(stream.set_nodelay(true));
                    let service = self.new_service(id);
                    let builder = may::coroutine::Builder::new().id(id);
                    go!(
                        builder,
                        move || if let Err(e) = each_connection_loop(&mut stream, service) {
                            error!("service err = {:?}", e);
                            stream.shutdown(std::net::Shutdown::Both).ok();
                        }
                    )
                    .unwrap();
                }
            }
        )
    }
}

fn internal_error_rsp(e: io::Error, buf: &mut BytesMut) -> Response {
    error!("error in service: err = {:?}", e);
    buf.clear();
    let mut err_rsp = Response::new(buf);
    err_rsp.status_code("500", "Internal Server Error");
    err_rsp
        .body_mut()
        .extend_from_slice(e.to_string().as_bytes());
    err_rsp
}

#[cfg(unix)]
#[inline]
fn nonblock_read(stream: &mut impl Read, req_buf: &mut BytesMut) -> io::Result<usize> {
    let read_buf: &mut [u8] = unsafe { std::mem::transmute(&mut *req_buf.chunk_mut()) };
    let len = read_buf.len();
    let mut read_cnt = 0;
    while read_cnt < len {
        match stream.read(unsafe { read_buf.get_unchecked_mut(read_cnt..) }) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
            Ok(n) => read_cnt += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }

    unsafe { req_buf.advance_mut(read_cnt) };
    Ok(read_cnt)
}

#[cfg(unix)]
#[inline]
fn nonblock_write(stream: &mut impl Write, write_buf: &mut BytesMut) -> io::Result<usize> {
    let len = write_buf.len();
    if len == 0 {
        return Ok(0);
    }

    let mut written = 0;
    while written < len {
        match stream.write(unsafe { write_buf.get_unchecked(written..) }) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
            Ok(n) => written += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }
    write_buf.advance(written);
    Ok(written)
}

const BUF_LEN: usize = 4096 * 8;
#[inline]
fn reserve_buf(buf: &mut BytesMut) {
    let capacity = buf.capacity();
    if capacity < 1024 {
        buf.reserve(BUF_LEN - capacity);
    }
}

/// this is the generic type http server
/// with a type parameter that impl `HttpService` trait
///
pub struct HttpServer<T>(pub T);

#[cfg(unix)]
fn each_connection_loop<T: HttpService>(stream: &mut TcpStream, mut service: T) -> io::Result<()> {
    let mut req_buf = BytesMut::with_capacity(BUF_LEN);
    let mut rsp_buf = BytesMut::with_capacity(BUF_LEN);
    let mut body_buf = BytesMut::with_capacity(BUF_LEN);

    loop {
        stream.reset_io();

        let inner_stream = stream.inner_mut();

        // read the socket for requests
        reserve_buf(&mut req_buf);
        let read_cnt = nonblock_read(inner_stream, &mut req_buf)?;

        // prepare the requests
        if read_cnt > 0 {
            while let Some(req) = request::decode(&req_buf)? {
                let len = req.len();
                let mut rsp = Response::new(&mut body_buf);
                match service.call(req, &mut rsp) {
                    Ok(()) => response::encode(rsp, &mut rsp_buf),
                    Err(e) => {
                        let err_rsp = internal_error_rsp(e, &mut body_buf);
                        response::encode(err_rsp, &mut rsp_buf);
                    }
                }
                req_buf.advance(len);
            }
        }

        // write out the responses
        nonblock_write(inner_stream, &mut rsp_buf)?;

        stream.wait_io();
    }
}

#[cfg(not(unix))]
fn each_connection_loop<T: HttpService>(stream: &mut TcpStream, mut service: T) -> io::Result<()> {
    let mut req_buf = BytesMut::with_capacity(BUF_LEN);
    let mut rsp_buf = BytesMut::with_capacity(BUF_LEN);
    let mut body_buf = BytesMut::with_capacity(BUF_LEN);
    loop {
        // read the socket for requests
        reserve_buf(&mut req_buf);
        let read_buf: &mut [u8] = unsafe { std::mem::transmute(&mut *req_buf.chunk_mut()) };
        let read_cnt = stream.read(read_buf)?;
        if read_cnt == 0 {
            //connection was closed
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        unsafe { req_buf.advance_mut(read_cnt) };

        // prepare the requests
        if read_cnt > 0 {
            while let Some(req) = request::decode(&req_buf)? {
                let len = req.len();
                let mut rsp = Response::new(&mut body_buf);
                if let Err(e) = service.call(req, &mut rsp) {
                    let err_rsp = internal_error_rsp(e, &mut body_buf);
                    response::encode(err_rsp, &mut rsp_buf);
                } else {
                    response::encode(rsp, &mut rsp_buf);
                }
                req_buf.advance(len);
            }
        }

        // send the result back to client
        stream.write_all(rsp_buf.as_ref())?;
    }
}

impl<T: HttpService + Clone + Send + Sync + 'static> HttpServer<T> {
    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    pub fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        let service = self.0;
        go!(
            coroutine::Builder::new().name("TcpServer".to_owned()),
            move || {
                for stream in listener.incoming() {
                    let mut stream = t_c!(stream);
                    // t_c!(stream.set_nodelay(true));
                    let service = service.clone();
                    go!(
                        move || if let Err(e) = each_connection_loop(&mut stream, service) {
                            error!("service err = {:?}", e);
                            stream.shutdown(std::net::Shutdown::Both).ok();
                        }
                    );
                }
            }
        )
    }
}
