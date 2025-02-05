use rustls::pki_types::pem::PemObject;
use std::{
    any::TypeId,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection,
};

use crate::{
    core::{
        config::{command::Command, config_context::ConfigContext},
        processor::{HttpProcessor, Processor},
    },
    events::thread_pool::THREAD_POOL,
    http::{http_manager::HttpContext, http_ssl::HttpSSL},
    register_commands,
};

use super::{http_location::HttpLocationContext, http_type::HttpVersion};

register_commands!(
    Command::new(
        "server",
        vec![TypeId::of::<HttpContext>()],
        handle_create_server,
    ),
    Command::new(
        "listen",
        vec![TypeId::of::<HttpServerContext>()],
        handle_set_listen
    ),
    Command::new(
        "server_name",
        vec![TypeId::of::<HttpServerContext>()],
        handle_set_server_name
    )
);

/// 建立 Server 區塊時建立 HttpServerContext，並順便初始化 processor
pub fn handle_create_server(ctx: &mut ConfigContext) {
    println!("Creating server");
    let server_ctx = Arc::new(HttpServerContext::new());
    let server_raw = Arc::into_raw(server_ctx.clone()) as *mut u8;
    ctx.current_ctx = Some(atomic_ptr_new(server_raw));
    ctx.current_block_type_id = Some(TypeId::of::<HttpServerContext>());
}

/// 處理 listen 指令，設定伺服器監聽的位址
pub fn handle_set_listen(ctx: &mut ConfigContext) {
    let listen = ctx.current_cmd_args.first().unwrap();
    if let Some(srv_ctx_ptr) = &ctx.current_ctx {
        let srv_ptr = srv_ctx_ptr.load(Ordering::SeqCst);
        if !srv_ptr.is_null() {
            let srv_ctx = unsafe { &mut *(srv_ptr as *mut HttpServerContext) };
            srv_ctx.set_listen(listen);
        }
    }
}

/// 處理 server_name 指令，將伺服器名稱登錄到配置中
pub fn handle_set_server_name(ctx: &mut ConfigContext) {
    let server_name = ctx.current_cmd_args.first().unwrap();
    if let Some(srv_ctx_ptr) = &ctx.current_ctx {
        let srv_ptr = srv_ctx_ptr.load(Ordering::SeqCst);
        if !srv_ptr.is_null() {
            let srv_ctx = unsafe { &mut *(srv_ptr as *mut HttpServerContext) };
            srv_ctx.add_server_name(server_name);
        }
    }
}

fn atomic_ptr_new<T>(ptr: *mut T) -> std::sync::atomic::AtomicPtr<u8> {
    std::sync::atomic::AtomicPtr::new(ptr as *mut u8)
}

/// HttpServerContext 保存伺服器配置，包括監聽位址、伺服器名稱與 processor
#[derive(Default)]
pub struct HttpServerContext {
    listen: Mutex<String>,
    server_names: Mutex<Vec<String>>,
    http_version: Mutex<HttpVersion>,
    processor: Mutex<HttpProcessor>,
}

impl HttpServerContext {
    pub fn new() -> Self {
        Self {
            listen: Mutex::new("127.0.0.1:8080".to_string()),
            server_names: Mutex::new(Vec::new()),
            http_version: Mutex::new(HttpVersion::default()),
            processor: Mutex::new(HttpProcessor::new()),
        }
    }

    pub fn set_listen(&self, addr: &str) {
        if let Ok(mut listen) = self.listen.lock() {
            *listen = addr.to_string();
        }
    }

    pub fn listen(&self) -> String {
        self.listen.lock().unwrap().clone()
    }

    pub fn add_server_name(&self, name: &str) {
        if let Ok(mut names) = self.server_names.lock() {
            names.push(name.to_string());
        }
    }

    pub fn get_http_version(&self) -> HttpVersion {
        self.http_version.lock().unwrap().clone()
    }
}

/// 代表最終運行的 HTTP 伺服器，持有 Processor 處理請求
pub struct HttpServer {
    listener: TcpListener,
    http_version: Arc<HttpVersion>,
    processor: Arc<HttpProcessor>,
    ssl: Option<Arc<ServerConfig>>,
    running: Arc<AtomicBool>,
}

impl HttpServer {
    /// 根據配置建立 HttpServer，主要步驟：
    /// 1. 從 ConfigContext 中取得 HttpServerContext
    /// 2. 遍歷所有子區塊（例如 location），從中提取各路由的處理器，登錄到 processor 中
    /// 3. 將 processor 從 HttpServerContext 中取出，並建立 Server
    pub fn new(server_config: &ConfigContext) -> Self {
        // 取得 server 區塊的 HttpServerContext
        let server_arc: Arc<HttpServerContext> = if let Some(ptr) = &server_config.current_ctx {
            let srv_raw = ptr.load(Ordering::SeqCst);
            unsafe { Arc::from_raw(srv_raw as *const HttpServerContext) }
        } else {
            panic!("Server block missing HttpServerContext");
        };
        let server_ctx = server_arc.clone();
        std::mem::forget(server_arc);

        let listen = server_ctx.listen();
        println!("Listening on: {}", listen);

        let mut ssl_config: Option<Arc<ServerConfig>> = None;

        // 處理所有子區塊
        for child in &server_config.children {
            match child.block_name.trim() {
                "location" => {
                    // location 區塊第一個參數即為路徑
                    let path = child
                        .block_args
                        .first()
                        .expect("location block must have a path")
                        .clone();
                    if let Some(ptr) = &child.current_ctx {
                        let loc_raw = ptr.load(Ordering::SeqCst);
                        let loc_arc: Arc<HttpLocationContext> =
                            unsafe { Arc::from_raw(loc_raw as *const HttpLocationContext) };
                        let handlers = loc_arc.take_handlers();
                        for (code, handler) in handlers {
                            if let Ok(mut proc_lock) = server_ctx.processor.lock() {
                                proc_lock.add_handler(path.clone(), code, handler);
                            }
                        }
                        std::mem::forget(loc_arc);
                    }
                }
                "ssl" => {
                    if child.current_ctx.is_some() {
                        if let Ok(http_ssl) = HttpSSL::from_config(child) {
                            let pem_key = http_ssl
                                .cert_key
                                .pri_key
                                .private_key_to_pem_pkcs8()
                                .unwrap();
                            let pri_key = PrivateKeyDer::Pkcs8(
                                PrivatePkcs8KeyDer::from_pem_slice(&pem_key).expect("Invalid key"),
                            );
                            let pem_cert = http_ssl.cert.cert.to_pem().unwrap();
                            let cert = CertificateDer::from_pem_slice(&pem_cert).unwrap();

                            ssl_config = Some(Arc::new(
                                ServerConfig::builder()
                                    .with_no_client_auth()
                                    .with_single_cert(vec![cert], pri_key)
                                    .unwrap(),
                            ));
                        } else {
                            eprintln!("Failed to create SSL config");
                        }
                    }
                }
                _ => {}
            }
        }

        let processor = {
            let mut proc_lock = server_ctx.processor.lock().unwrap();
            std::mem::replace(&mut *proc_lock, HttpProcessor::new())
        };

        let listener = TcpListener::bind(&listen).unwrap();
        let http_version = Arc::new(server_ctx.get_http_version());

        println!("SSL enabled: {}", ssl_config.is_some());

        Self {
            listener,
            http_version,
            processor: Arc::new(processor),
            ssl: ssl_config,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn start(self) -> thread::JoinHandle<()> {
        println!("Server started");
        let running_flag = self.running.clone();
        let listener = self.listener;
        let http_version = self.http_version.clone();
        let processor = self.processor.clone();
        let ssl_config = self.ssl.clone();

        thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("Failed to set non-blocking");

            if processor.is_empty() {
                eprintln!("No routes configured for server");
                return;
            }

            while running_flag.load(Ordering::SeqCst) {
                match listener.incoming().next() {
                    Some(Ok(stream)) => {
                        println!("Connection from: {}", stream.peer_addr().unwrap());
                        process_connection(
                            stream,
                            processor.clone(),
                            http_version.clone(),
                            ssl_config.clone(),
                        );
                    }
                    Some(Err(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Some(Err(e)) => {
                        eprintln!("Connection failed: {}", e);
                    }
                    None => break,
                }
            }
            println!("Server stopped accepting connections.");
        })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        println!("Server stop requested");
    }
}

fn process_connection(
    stream: TcpStream,
    processor: Arc<HttpProcessor>,
    http_version: Arc<HttpVersion>,
    ssl_config: Option<Arc<ServerConfig>>,
) {
    if let Ok(pool) = THREAD_POOL.lock() {
        let _ = pool.spawn(move || {
            if let Err(e) = if let Some(ssl_cfg) = ssl_config {
                process_tls_connection(stream, ssl_cfg, &processor, &http_version)
            } else {
                process_plain_connection(stream, &processor, &http_version)
            } {
                eprintln!("Error handling connection: {}", e);
            }
        });
    } else {
        eprintln!("Thread pool error");
    }
}

fn process_plain_connection(
    mut stream: TcpStream,
    processor: &HttpProcessor,
    http_version: &HttpVersion,
) -> std::io::Result<()> {
    handle_connection(&mut stream, processor, http_version)
}

fn process_tls_connection(
    mut stream: TcpStream,
    ssl_cfg: Arc<ServerConfig>,
    processor: &HttpProcessor,
    http_version: &HttpVersion,
) -> std::io::Result<()> {
    let mut conn = ServerConnection::new(ssl_cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let mut tls_stream = rustls::Stream::new(&mut conn, &mut stream);

    tls_stream.flush()?;
    handle_connection(&mut tls_stream, processor, http_version)
}

/// 處理單一連線：讀取請求，透過 processor 產生回應
fn handle_connection<S: Read + Write>(
    stream: &mut S,
    processor: &HttpProcessor,
    http_version: &HttpVersion,
) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = stream.read(&mut buffer)?;
    if n == 0 {
        return Ok(());
    }
    let request_bytes = buffer[..n].to_vec();

    // 呼叫 processor 處理請求
    let response_bytes = match processor.process(request_bytes) {
        Ok(resp) => resp,
        Err(_) => HttpProcessor::create_404_response(http_version).as_bytes(),
    };

    stream.write_all(&response_bytes)?;
    stream.flush()?;
    Ok(())
}
