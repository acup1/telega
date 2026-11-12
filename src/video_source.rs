//! Seekable HTTP range bridge between ffmpeg and Telegram. Chunks stay in RAM.
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use grammers_client::{Client, media::Media};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, Notify, OnceCell};

const CHUNK: u64 = 512 * 1024;
const CACHE_CHUNKS: usize = 24;
const MAX_CONNECTIONS: usize = 8;
type Chunk = Arc<OnceCell<Arc<Vec<u8>>>>;

#[derive(Default)]
struct Cache {
    chunks: HashMap<u64, Chunk>,
    order: VecDeque<u64>,
}

struct Source {
    client: Client,
    media: Media,
    size: u64,
    cache: Mutex<Cache>,
    stopped: AtomicBool,
    cancel: Notify,
    fetched: AtomicU64,
    cache_chunks: usize,
}

impl Source {
    async fn chunk(&self, index: u64) -> Result<Arc<Vec<u8>>> {
        let cell = {
            let mut cache = self.cache.lock().await;
            cache.order.retain(|existing| *existing != index);
            cache.order.push_back(index);
            let cell = cache.chunks.entry(index).or_default().clone();
            while cache.order.len() > self.cache_chunks {
                if let Some(oldest) = cache.order.pop_front() {
                    cache.chunks.remove(&oldest);
                }
            }
            cell
        };
        cell.get_or_try_init(|| async {
            let index =
                i32::try_from(index).context("video offset exceeds Telegram chunk range")?;
            let mut download = self.client.iter_download(&self.media).skip_chunks(index);
            let bytes = download
                .next()
                .await?
                .context("Telegram returned an empty video chunk")?;
            self.fetched
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            Ok(Arc::new(bytes))
        })
        .await
        .cloned()
    }
}

pub struct VideoSource {
    source: Arc<Source>,
    listener: Option<thread::JoinHandle<()>>,
    pub url: String,
}

impl VideoSource {
    pub fn new(client: Client, media: Media) -> Result<Self> {
        Self::with_cache(client, media, CACHE_CHUNKS)
    }

    pub fn with_cache(client: Client, media: Media, cache_chunks: usize) -> Result<Self> {
        let size = media.size().context("video size is unavailable")? as u64;
        if size == 0 {
            bail!("video is empty");
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let source = Arc::new(Source {
            client,
            media,
            size,
            cache: Mutex::new(Cache::default()),
            stopped: AtomicBool::new(false),
            cancel: Notify::new(),
            fetched: AtomicU64::new(0),
            cache_chunks: cache_chunks.max(1),
        });
        let worker = source.clone();
        let runtime = Handle::current();
        let join = thread::spawn(move || {
            let mut clients: Vec<(TcpStream, thread::JoinHandle<()>)> = Vec::new();
            while !worker.stopped.load(Ordering::Acquire) {
                clients.retain(|(_, client)| !client.is_finished());
                match listener.accept() {
                    Ok((stream, _)) => {
                        if clients.len() >= MAX_CONNECTIONS {
                            continue;
                        }
                        let Ok(socket) = stream.try_clone() else {
                            continue;
                        };
                        let source = worker.clone();
                        let runtime = runtime.clone();
                        let client = thread::spawn(move || {
                            let _ = serve(stream, source, runtime);
                        });
                        clients.push((socket, client));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            worker.cancel.notify_waiters();
            close_connections(clients);
        });
        Ok(Self {
            source,
            listener: Some(join),
            url: format!("http://{address}/video"),
        })
    }

    pub fn fetched_bytes(&self) -> u64 {
        self.source.fetched.load(Ordering::Relaxed)
    }
    pub fn size(&self) -> u64 {
        self.source.size
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        self.source.stopped.store(true, Ordering::Release);
        self.source.cancel.notify_waiters();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

fn close_connections(clients: Vec<(TcpStream, thread::JoinHandle<()>)>) {
    // Close every socket before joining any worker: reads, writes, and slow
    // request headers must not hold the UI until the socket timeout expires.
    for (socket, _) in &clients {
        let _ = socket.shutdown(Shutdown::Both);
    }
    for (_, client) in clients {
        let _ = client.join();
    }
}

async fn wait_for_stop(stopped: &AtomicBool, cancel: &Notify) {
    let notified = cancel.notified();
    tokio::pin!(notified);
    // Subscribe before checking the flag. notify_waiters does not retain a
    // permit for a future created after the shutdown notification.
    notified.as_mut().enable();
    if !stopped.load(Ordering::Acquire) {
        notified.await;
    }
}

fn serve(mut stream: TcpStream, source: Arc<Source>, runtime: Handle) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let length = stream.read(&mut buffer)?;
        if length == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..length]);
        if request.len() > 8192 {
            bail!("video HTTP request is too large");
        }
    }
    let request = std::str::from_utf8(&request)?;
    let range = request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("range").then_some(value.trim())
    });
    let (start, end) = match parse_range(range, source.size) {
        Ok(range) => range,
        Err(_) => {
            write!(
                stream,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                source.size
            )?;
            return Ok(());
        }
    };
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
        end - start + 1,
        source.size
    )?;
    if request.starts_with("HEAD ") {
        return Ok(());
    }
    let mut position = start;
    while position <= end && !source.stopped.load(Ordering::Acquire) {
        let chunk = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = wait_for_stop(&source.stopped, &source.cancel) => Err(anyhow::anyhow!("video viewer closed")),
                result = source.chunk(position / CHUNK) => result,
            }
        })?;
        let offset = (position % CHUNK) as usize;
        let count = (end - position + 1).min(chunk.len().saturating_sub(offset) as u64) as usize;
        if count == 0 {
            bail!("truncated video chunk");
        }
        stream.write_all(&chunk[offset..offset + count])?;
        position += count as u64;
    }
    Ok(())
}

fn parse_range(header: Option<&str>, size: u64) -> Result<(u64, u64)> {
    let Some(header) = header else {
        return Ok((0, size - 1));
    };
    let (start, end) = header
        .strip_prefix("bytes=")
        .context("invalid range unit")?
        .split_once('-')
        .context("invalid byte range")?;
    if start.is_empty() {
        let suffix = end.parse::<u64>()?.min(size);
        if suffix == 0 {
            bail!("empty suffix");
        }
        return Ok((size - suffix, size - 1));
    }
    let start = start.parse::<u64>()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>()?.min(size - 1)
    };
    if start > end || start >= size {
        bail!("range outside video");
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_ffmpeg_ranges_and_rejects_outside_file() {
        assert_eq!(parse_range(Some("bytes=512-"), 1024).unwrap(), (512, 1023));
        assert_eq!(parse_range(Some("bytes=-128"), 1024).unwrap(), (896, 1023));
        assert!(parse_range(Some("bytes=2048-"), 1024).is_err());
    }

    #[tokio::test]
    async fn cancellation_survives_notification_before_subscription() {
        let stopped = AtomicBool::new(true);
        let cancel = Notify::new();
        cancel.notify_waiters();
        tokio::time::timeout(Duration::from_millis(100), wait_for_stop(&stopped, &cancel))
            .await
            .expect("a late subscriber must observe the stopped flag");

        stopped.store(false, Ordering::Release);
        let waiting = wait_for_stop(&stopped, &cancel);
        tokio::pin!(waiting);
        tokio::select! {
            _ = &mut waiting => panic!("source is still running"),
            _ = tokio::task::yield_now() => {}
        }
        stopped.store(true, Ordering::Release);
        cancel.notify_waiters();
        tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("registered subscriber must wake at shutdown");
    }

    #[test]
    fn closing_source_interrupts_a_worker_waiting_for_request_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let socket = stream.try_clone().unwrap();
        let client = thread::spawn(move || {
            let _ = stream.read(&mut [0]);
        });
        let started = std::time::Instant::now();
        close_connections(vec![(socket, client)]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
