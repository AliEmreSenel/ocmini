//! Upstream smoltcp's buffer capacities are fixed. Anonymous mappings reserve
//! the configured windows without committing them per idle connection. Pages
//! become resident when traffic touches them; no hardware-dependent behavior.
use smoltcp::socket::tcp;
use std::io;
struct Mapping {
    pointer: *mut u8,
    len: usize,
}
unsafe impl Send for Mapping {}
impl Mapping {
    fn new(len: usize) -> io::Result<Self> {
        // Rust slices cannot span more than isize::MAX bytes, including on
        // 32-bit ARM where mmap can otherwise accept a larger reservation.
        if len == 0 || len > isize::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TCP buffer size cannot be represented as a Rust slice",
            ));
        }
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pointer: pointer.cast(),
            len,
        })
    }
    // Private: a mapping yields its one mutable slice exactly once; the broker
    // drops its socket before dropping the allocation, including on unwinding.
    unsafe fn slice(&mut self) -> &'static mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer.cast(), self.len);
        }
    }
}
pub struct TcpBuffers {
    _recv: Mapping,
    _send: Mapping,
}
impl TcpBuffers {
    pub fn socket(recv: usize, send: usize) -> io::Result<(tcp::Socket<'static>, Self)> {
        // TCP window scaling has a protocol maximum of 2^30 bytes (RFC 7323).
        let mut recv = Mapping::new(recv.min(1 << 30))?;
        let mut send = Mapping::new(send)?;
        let socket = unsafe {
            tcp::Socket::new(
                tcp::SocketBuffer::new(recv.slice()),
                tcp::SocketBuffer::new(send.slice()),
            )
        };
        Ok((
            socket,
            Self {
                _recv: recv,
                _send: send,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unrepresentable_slice_lengths() {
        for len in [0, isize::MAX as usize + 1] {
            assert!(matches!(Mapping::new(len), Err(e) if e.kind() == io::ErrorKind::InvalidInput));
        }
    }
    #[test]
    fn fixed_windows_are_demand_paged() {
        let (mut sock, mut backing) = TcpBuffers::socket(4 * 1024 * 1024, 4 * 1024 * 1024).unwrap();
        assert_eq!(sock.recv_capacity(), 4 * 1024 * 1024);
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut resident = vec![0; backing._recv.len / page];
        assert_eq!(
            unsafe {
                libc::mincore(
                    backing._recv.pointer.cast(),
                    backing._recv.len,
                    resident.as_mut_ptr(),
                )
            },
            0
        );
        assert!(resident.iter().all(|bit| bit & 1 == 0));
        // End the socket's borrow before directly testing its backing allocation.
        sock.abort();
        drop(sock);
        unsafe {
            *backing._recv.slice().first_mut().unwrap() = 1;
        }
        assert_eq!(
            unsafe {
                libc::mincore(
                    backing._recv.pointer.cast(),
                    backing._recv.len,
                    resident.as_mut_ptr(),
                )
            },
            0
        );
        assert!(resident.iter().any(|bit| *bit & 1 != 0));
    }
}
