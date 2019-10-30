use std::mem;
use std::io::{self, Read, Write, IoSlice, IoSliceMut};
use std::cmp;
use std::sync::atomic::{AtomicBool, Ordering};

use libc::{c_int, c_void, ssize_t};

use super::commom::AsInner;

#[derive(Debug)]
pub struct FileDesc {
    fd: c_int,
}

pub fn max_len() -> usize {
    <ssize_t>::max_value() as usize
}

impl FileDesc {
    pub fn new(fd: c_int) -> FileDesc {
        FileDesc { fd }
    }

    pub fn raw(&self) -> c_int { self.fd }

    /// Extracts the actual file descriptor without closing it.
    pub fn into_raw(self) -> c_int {
        let fd = self.fd;
        mem::forget(self);
        fd
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = syscall!(read(self.fd,
                       buf.as_mut_ptr() as *mut c_void,
                       cmp::min(buf.len(), max_len()))
        )?;
        Ok(ret as usize)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let ret = syscall!(readv(self.fd,
                        bufs.as_ptr() as *const libc::iovec,
                        cmp::min(bufs.len(), c_int::max_value() as usize) as c_int)
        )?;
        Ok(ret as usize)
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut me = self;
        (&mut me).read_to_end(buf)
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        syscall!(pread64(self.fd,
                        buf.as_mut_ptr() as *mut c_void,
                        cmp::min(buf.len(), max_len()),
                        offset as i64))
            .map(|n| n as usize)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let ret = syscall!(write(self.fd,
                        buf.as_ptr() as *const c_void,
                        cmp::min(buf.len(), max_len()))
        )?;
        Ok(ret as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let ret = syscall!(writev(self.fd,
                         bufs.as_ptr() as *const libc::iovec,
                         cmp::min(bufs.len(), c_int::max_value() as usize) as c_int)
        )?;
        Ok(ret as usize)
    }

    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        syscall!(pwrite64(self.fd,
                         buf.as_ptr() as *const c_void,
                         cmp::min(buf.len(), max_len()),
                         offset as i64))
                .map(|n| n as usize)
    }

    pub fn get_cloexec(&self) -> io::Result<bool> {
        Ok((syscall!(fcntl(self.fd, libc::F_GETFD))? & libc::FD_CLOEXEC) != 0)
    }

    pub fn set_cloexec(&self) -> io::Result<()> {
        let previous = syscall!(fcntl(self.fd, libc::F_GETFD))?;
        let new = previous | libc::FD_CLOEXEC;
        if new != previous {
            syscall!(fcntl(self.fd, libc::F_SETFD, new))?;
        }
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let v = nonblocking as c_int;
        syscall!(ioctl(self.fd, libc::FIONBIO, &v))?;
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<FileDesc> {
        // We want to atomically duplicate this file descriptor and set the
        // CLOEXEC flag, and currently that's done via F_DUPFD_CLOEXEC. This
        // flag, however, isn't supported on older Linux kernels (earlier than
        // 2.6.24).
        //
        // To detect this and ensure that CLOEXEC is still set, we
        // follow a strategy similar to musl [1] where if passing
        // F_DUPFD_CLOEXEC causes `fcntl` to return EINVAL it means it's not
        // supported (the third parameter, 0, is always valid), so we stop
        // trying that.
        //
        // Also note that Android doesn't have F_DUPFD_CLOEXEC, but get it to
        // resolve so we at least compile this.
        //
        // [1]: http://comments.gmane.org/gmane.linux.lib.musl.general/2963
        use libc::F_DUPFD_CLOEXEC;

        let make_filedesc = |fd| {
            let fd = FileDesc::new(fd);
            fd.set_cloexec()?;
            Ok(fd)
        };
        static TRY_CLOEXEC: AtomicBool = AtomicBool::new(true);
        let fd = self.raw();
        if TRY_CLOEXEC.load(Ordering::Relaxed) {
            match syscall!(fcntl(fd, F_DUPFD_CLOEXEC, 0)) {
                // We *still* call the `set_cloexec` method as apparently some
                // linux kernel at some point stopped setting CLOEXEC even
                // though it reported doing so on F_DUPFD_CLOEXEC.
                Ok(fd) => {
                    return Ok(make_filedesc(fd)?)
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    TRY_CLOEXEC.store(false, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        syscall!(fcntl(fd, libc::F_DUPFD, 0)).and_then(make_filedesc)
    }
}

impl<'a> Read for &'a FileDesc {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read(buf)
    }

    // #[inline]
    // unsafe fn initializer(&self) -> Initializer {
    //     Initializer::nop()
    // }
}

impl<'a> Write for &'a FileDesc {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (**self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsInner<c_int> for FileDesc {
    fn as_inner(&self) -> &c_int { &self.fd }
}

impl Drop for FileDesc {
    fn drop(&mut self) {
        // Note that errors are ignored when closing a file descriptor. The
        // reason for this is that if an error occurs we don't actually know if
        // the file descriptor was closed or not, and if we retried (for
        // something like EINTR), we might close another valid file descriptor
        // opened after we closed ours.
        let _ = syscall!(close(self.fd));
    }
}
