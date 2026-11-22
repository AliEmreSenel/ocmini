//! One serialized writer per channel; no userspace frame queue.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const CONTROL_MAX: usize = 256 * 1024;
pub const MTU: usize = 1500;
pub const SOCKET_BUFFER: i32 = 256 * 1024;
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Request {
        id: u64,
        method: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
    },
    Cancel {
        id: u64,
    },
    Result {
        id: u64,
        value: Value,
    },
    Error {
        id: u64,
        message: Value,
    },
}
pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    tag: u8,
    payload: &[u8],
) -> io::Result<()> {
    validate(tag, payload.len())?;
    let len = (payload.len() as u32).to_be_bytes();
    let header = [len[0], len[1], len[2], len[3], tag];
    let mut h_off = 0;
    let mut p_off = 0;
    while h_off < header.len() || p_off < payload.len() {
        let written = if h_off < header.len() {
            let bufs = [
                io::IoSlice::new(&header[h_off..]),
                io::IoSlice::new(&payload[p_off..]),
            ];
            writer.write_vectored(&bufs).await?
        } else {
            writer.write(&payload[p_off..]).await?
        };
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write whole frame",
            ));
        }
        let h_rem = header.len() - h_off;
        if written <= h_rem {
            h_off += written;
        } else {
            h_off = header.len();
            p_off += written - h_rem;
        }
    }
    Ok(())
}
pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    expected: u8,
) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let tag = header[4];
    validate(tag, length)?;
    if tag != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame on wrong channel",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}
fn validate(tag: u8, length: usize) -> io::Result<()> {
    let max = match tag {
        b'C' => CONTROL_MAX,
        b'P' => MTU,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown frame type",
            ))
        }
    };
    if length == 0 || length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ));
    }
    Ok(())
}
pub async fn write_control(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Control,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    write_frame(writer, b'C', &bytes).await
}
pub async fn read_control(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Control> {
    serde_json::from_slice(&read_frame(reader, b'C').await?).map_err(io::Error::other)
}
#[cfg(unix)]
pub fn socketpair() -> io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    use std::os::fd::AsRawFd;
    let pair = std::os::unix::net::UnixStream::pair()?;
    for socket in [&pair.0, &pair.1] {
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&SOCKET_BUFFER as *const i32).cast(),
                std::mem::size_of::<i32>() as _,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}

/// Large JSON values spill through the session's existing output storage. Only
/// single path components are accepted, and symlinks are never followed.
#[derive(Clone)]
pub struct Spill {
    #[cfg(not(unix))]
    root: std::path::PathBuf,
    #[cfg(unix)]
    directory: std::sync::Arc<std::fs::File>,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl Spill {
    pub fn new(root: std::path::PathBuf) -> io::Result<Self> {
        #[cfg(unix)]
        let directory = {
            use std::os::unix::fs::OpenOptionsExt;
            std::sync::Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&root)?,
            )
        };
        Ok(Self {
            #[cfg(not(unix))]
            root,
            #[cfg(unix)]
            directory,
            next: Default::default(),
        })
    }
    fn open(&self, name: &str, create: bool) -> io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK
                        | if create {
                            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                        } else {
                            libc::O_RDONLY
                        },
                    0o600,
                )
            };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(unsafe { std::fs::File::from_raw_fd(fd) })
            }
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new()
                .read(!create)
                .write(create)
                .create_new(create)
                .open(self.root.join(name))
        }
    }
    fn remove(&self, name: &str) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if let Ok(name) = std::ffi::CString::new(name) {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(self.root.join(name));
        }
    }
    pub fn pack(&self, value: Value) -> io::Result<Value> {
        if inline_value(&value) {
            return Ok(value);
        }
        let mut sink = SpillWriter {
            spill: self,
            prefix: Vec::new(),
            file: None,
        };
        let result = serde_json::to_writer(&mut sink, &value)
            .map_err(io::Error::other)
            .and_then(|_| std::io::Write::flush(&mut sink));
        if let Err(error) = result {
            if let Some((name, file)) = sink.file.take() {
                drop(file);
                self.remove(&name);
            }
            return Err(error);
        }
        match sink.file.take() {
            Some((name, file)) => {
                drop(file);
                Ok(serde_json::json!({"$spill":name}))
            }
            None => Ok(value),
        }
    }
    pub async fn pack_async(&self, value: Value) -> io::Result<Value> {
        if inline_value(&value) {
            return Ok(value);
        }
        let spill = self.clone();
        tokio::task::spawn_blocking(move || spill.pack(value))
            .await
            .map_err(io::Error::other)?
    }
    pub async fn unpack_async(&self, value: Value) -> io::Result<Value> {
        if value.get("$spill").is_none() {
            return Ok(value);
        }
        let spill = self.clone();
        tokio::task::spawn_blocking(move || spill.unpack(value))
            .await
            .map_err(io::Error::other)?
    }
    pub fn unpack(&self, value: serde_json::Value) -> io::Result<serde_json::Value> {
        let Some(name) = value.get("$spill").and_then(serde_json::Value::as_str) else {
            return Ok(value);
        };
        if value.as_object().is_none_or(|o| o.len() != 1)
            || !name.starts_with("wire-")
            || !name.ends_with(".json")
            || std::path::Path::new(name).components().count() != 1
            || name.contains(['/', '\\', '\0'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid spill reference",
            ));
        }
        let file = self.open(name, false)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("spill must be a regular file"));
        }
        let result =
            serde_json::from_reader(std::io::BufReader::new(file)).map_err(io::Error::other);
        self.remove(name);
        result
    }
}

// Inspect only up to a control frame's worth of JSON. Large strings are
// detected by length before scanning, and serialization spills as a stream.
fn inline_value(value: &Value) -> bool {
    fn consume(budget: &mut usize, amount: usize) -> bool {
        if let Some(left) = budget.checked_sub(amount) {
            *budget = left;
            true
        } else {
            false
        }
    }
    fn string(text: &str, budget: &mut usize) -> bool {
        consume(budget, text.len().saturating_add(2))
            && text.bytes().all(|b| {
                consume(
                    budget,
                    match b {
                        b'"' | b'\\' => 1,
                        0..=31 => 5,
                        _ => 0,
                    },
                )
            })
    }
    fn visit(value: &Value, budget: &mut usize) -> bool {
        match value {
            Value::Null => consume(budget, 4),
            Value::Bool(_) => consume(budget, 5),
            Value::Number(_) => consume(budget, 32),
            Value::String(text) => string(text, budget),
            Value::Array(items) => {
                consume(budget, 2 + items.len()) && items.iter().all(|v| visit(v, budget))
            }
            Value::Object(items) => {
                consume(budget, 2 + items.len())
                    && items.iter().all(|(key, value)| {
                        string(key, budget) && consume(budget, 1) && visit(value, budget)
                    })
            }
        }
    }
    visit(value, &mut (CONTROL_MAX - 1024))
}
struct SpillWriter<'a> {
    spill: &'a Spill,
    prefix: Vec<u8>,
    file: Option<(String, std::io::BufWriter<std::fs::File>)>,
}
impl std::io::Write for SpillWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.file.is_none() && self.prefix.len().saturating_add(bytes.len()) < CONTROL_MAX - 1024
        {
            self.prefix.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        if self.file.is_none() {
            loop {
                let id = self
                    .spill
                    .next
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let name = format!("wire-{}-{id}.json", std::process::id());
                match self.spill.open(&name, true) {
                    Ok(file) => {
                        self.file = Some((name, std::io::BufWriter::new(file)));
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            self.file.as_mut().unwrap().1.write_all(&self.prefix)?;
            self.prefix.clear();
        }
        self.file.as_mut().unwrap().1.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some((_, file)) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub async fn serve<F, Fut>(
    socket: tokio::net::UnixStream,
    spill: Spill,
    handler: F,
) -> io::Result<()>
where
    F: Fn(String, serde_json::Value, Option<String>, tokio_util::sync::CancellationToken) -> Fut
        + Clone
        + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, String>> + 'static,
{
    use std::collections::HashMap;
    use tokio_util::sync::CancellationToken;
    let (reader, mut writer) = socket.into_split();
    let mut frames = Frames::new(reader, b'C');
    let mut jobs = tokio::task::JoinSet::new();
    let mut tokens = HashMap::new();
    let mut last = 0;
    loop {
        tokio::select! {
            frame=frames.control()=>{match frame?{
                Control::Request{id,method,args,directory}=>{
                    if id<=last{return Err(io::Error::other("request ids must increase"));}last=id;
                    let args=spill.unpack_async(args).await?;let token=CancellationToken::new();tokens.insert(id,token.clone());let handler=handler.clone();let spill=spill.clone();
                    jobs.spawn_local(async move{let (success,value)=match handler(method,args,directory,token).await{Ok(value)=>(true,value),Err(message)=>(false,Value::String(message))};let result=spill.pack_async(value).await.map(|value|(success,value)).unwrap_or_else(|e|(false,Value::String(e.to_string())));(id,result)});
                },
                Control::Cancel{id}=>{if let Some(token)=tokens.get(&id){token.cancel();}},
                _=>return Err(io::Error::other("unexpected runner frame")),
            }},
            done=jobs.join_next(),if !jobs.is_empty()=>{
                let(id,result)=done.ok_or_else(||io::Error::other("runner completion missing"))?.map_err(io::Error::other)?;tokens.remove(&id);
                let frame=match result{(true,value)=>Control::Result{id,value},(false,message)=>Control::Error{id,message}};
                write_control(&mut writer,&frame).await?;
            },
        }
    }
}

/// A single reader owns partial-frame state across scheduling and cancellation.
/// The one-frame channel applies socket backpressure before the next allocation.
pub struct Frames {
    rx: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    task: tokio::task::AbortHandle,
}
impl Frames {
    pub fn new(mut reader: impl AsyncRead + Unpin + Send + 'static, tag: u8) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                let permit = match tx.reserve().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let frame = read_frame(&mut reader, tag).await;
                let failed = frame.is_err();
                permit.send(frame);
                if failed {
                    break;
                }
            }
        });
        Self {
            rx,
            task: task.abort_handle(),
        }
    }
    pub async fn frame(&mut self) -> io::Result<Vec<u8>> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "frame reader ended"))?
    }
    pub async fn control(&mut self) -> io::Result<Control> {
        serde_json::from_slice(&self.frame().await?).map_err(io::Error::other)
    }
}
impl Drop for Frames {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn partial_frames_survive_wait_cancellation_and_channels_reject_wrong_tags() {
        let (mut tx, rx) = tokio::io::duplex(128);
        let mut frames = Frames::new(rx, b'C');
        tx.write_all(&6u32.to_be_bytes()).await.unwrap();
        tx.write_all(b"Cabc").await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), frames.frame())
                .await
                .is_err()
        );
        tx.write_all(b"def").await.unwrap();
        assert_eq!(frames.frame().await.unwrap(), b"abcdef");
        tx.write_all(&1u32.to_be_bytes()).await.unwrap();
        tx.write_all(b"Px").await.unwrap();
        assert!(frames.frame().await.is_err());
    }
    #[test]
    fn spill_roundtrip_and_symlink_refusal() {
        let root = tempfile::tempdir().unwrap();
        let spill = Spill::new(root.path().to_owned()).unwrap();
        let value = serde_json::json!({"output":"x".repeat(CONTROL_MAX)});
        let packed = spill.pack(value.clone()).unwrap();
        assert!(packed["$spill"].is_string());
        assert_eq!(spill.unpack(packed).unwrap(), value);
        assert!(spill
            .unpack(serde_json::json!({"$spill":"../secret"}))
            .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.path().join("wire-link.json")).unwrap();
            assert!(spill
                .unpack(serde_json::json!({"$spill":"wire-link.json"}))
                .is_err());
        }
    }
    #[tokio::test]
    async fn vectored_write_frame_handles_partial_writes_across_header_and_payload() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        struct ChunkedWriter {
            max_write: usize,
            out: Vec<u8>,
        }
        impl AsyncWrite for ChunkedWriter {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                let n = buf.len().min(self.max_write);
                self.out.extend_from_slice(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            fn poll_write_vectored(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                bufs: &[io::IoSlice<'_>],
            ) -> Poll<io::Result<usize>> {
                let mut left = self.max_write;
                let mut total = 0;
                for slice in bufs {
                    if left == 0 {
                        break;
                    }
                    let n = slice.len().min(left);
                    self.out.extend_from_slice(&slice[..n]);
                    left -= n;
                    total += n;
                }
                Poll::Ready(Ok(total))
            }
            fn is_write_vectored(&self) -> bool {
                true
            }
            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        let payload = b"hello vectored frame payload!";
        for max_write in [1, 2, 3, 5, 7, 64] {
            let mut writer = ChunkedWriter {
                max_write,
                out: Vec::new(),
            };
            write_frame(&mut writer, b'P', payload).await.unwrap();
            let mut cursor = std::io::Cursor::new(writer.out);
            let decoded = read_frame(&mut cursor, b'P').await.unwrap();
            assert_eq!(decoded, payload, "max_write={max_write}");
        }
    }
}
