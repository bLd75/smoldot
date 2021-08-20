// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use core::{cmp, mem};
use futures::future::{self, BoxFuture, Future, FutureExt as _};

// TODO: documentation

#[must_use]
pub struct ReadWrite<'a, TNow> {
    pub now: TNow,

    /// Pointer to a buffer of socket data ready to be processed.
    ///
    /// Contains `None` if the remote has closed their writing side of the socket.
    pub incoming_buffer: Option<&'a [u8]>,

    /// Pointer to two consecutive buffers of uninitialized data. Can be written to in order to
    /// write data out towards the socket.
    ///
    /// Contains `None` if the writing side of the socket has been closed or must be closed.
    pub outgoing_buffer: Option<(&'a mut [u8], &'a mut [u8])>,

    /// Total number of bytes that have been read from [`ReadWrite::incoming_buffer`].
    ///
    /// [`ReadWrite::incoming_buffer`] must have been advanced after these bytes.
    pub read_bytes: usize,

    /// Total number of bytes that have been written to [`ReadWrite::outgoing_buffer`].
    ///
    /// [`ReadWrite::outgoing_buffer`] must have been advanced after these bytes.
    pub written_bytes: usize,

    /// If `Some`, the socket must be waken up after the given `TNow` is reached.
    pub wake_up_after: Option<TNow>,

    /// If `Some`, the socket must be waken up after the given future is ready.
    pub wake_up_future: Option<BoxFuture<'static, ()>>,
}

impl<'a, TNow> ReadWrite<'a, TNow> {
    /// Returns true if the connection should be considered dead. That is, both
    /// [`ReadWrite::incoming_buffer`] and [`ReadWrite::outgoing_buffer`] are `None`.
    pub fn is_dead(&self) -> bool {
        self.incoming_buffer.is_none() && self.outgoing_buffer.is_none()
    }

    /// Discards the first `num` bytes of [`ReadWrite::incoming_buffer`] and adds them to
    /// [`ReadWrite::read_bytes`].
    ///
    /// # Panic
    ///
    /// Panics if `num` is superior to the size of the available buffer.
    ///
    pub fn advance_read(&mut self, num: usize) {
        if let Some(ref mut incoming_buffer) = self.incoming_buffer {
            self.read_bytes += num;
            *incoming_buffer = &incoming_buffer[num..];
        } else {
            assert_eq!(num, 0);
        }
    }

    /// Discards the first `num` bytes of [`ReadWrite::outgoing_buffer`] and adds them to
    /// [`ReadWrite::written_bytes`].
    ///
    /// # Panic
    ///
    /// Panics if `num` is superior to the size of the available buffer.
    ///
    pub fn advance_write(&mut self, num: usize) {
        if let Some(ref mut outgoing_buffer) = self.outgoing_buffer {
            self.written_bytes += num;

            let out_buf_0_len = outgoing_buffer.0.len();
            advance_buf(&mut outgoing_buffer.0, cmp::min(num, out_buf_0_len));
            advance_buf(&mut outgoing_buffer.1, num.saturating_sub(out_buf_0_len));
            if outgoing_buffer.0.is_empty() {
                mem::swap::<&mut [u8]>(&mut outgoing_buffer.0, &mut outgoing_buffer.1);
            }
        } else {
            assert_eq!(num, 0);
        }
    }

    /// Sets the writing side of the connection to closed.
    ///
    /// This is simply a shortcut for setting [`ReadWrite::outgoing_buffer`] to `None`.
    pub fn close_write(&mut self) {
        self.outgoing_buffer = None;
    }

    /// Returns the size of the data available in the incoming buffer.
    pub fn incoming_buffer_available(&self) -> usize {
        self.incoming_buffer
            .as_ref()
            .map(|buf| buf.len())
            .unwrap_or(0)
    }

    /// Returns an iterator that pops bytes from [`ReadWrite::incoming_buffer`]. Whenever the
    /// iterator advances, [`ReadWrite::read_bytes`] is increased by 1.
    pub fn incoming_bytes_iter<'b>(&'b mut self) -> IncomingBytes<'a, 'b, TNow> {
        IncomingBytes { me: self }
    }

    /// Extracts a certain number of bytes from [`ReadWrite::incoming_buffer`] and updates
    /// [`ReadWrite::read_bytes`].
    ///
    /// # Panic
    ///
    /// Panics if `N` is super to the number of bytes available.
    ///
    pub fn read_bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out: [u8; N] = [0; N];
        match self.incoming_buffer {
            Some(buf) => {
                assert!(buf.len() >= N);
                out.copy_from_slice(&buf[..N]);
                self.advance_read(N);
            }
            None => assert_eq!(N, 0),
        };
        out
    }

    /// Returns the size of the available outgoing buffer.
    pub fn outgoing_buffer_available(&self) -> usize {
        self.outgoing_buffer
            .as_ref()
            .map(|(a, b)| a.len() + b.len())
            .unwrap_or(0)
    }

    /// Copies the content of `data` to [`ReadWrite::outgoing_buffer`] and increases
    /// [`ReadWrite::written_bytes`].
    ///
    /// # Panic
    ///
    /// Panics if `data.len() > self.outgoing_buffer_available()`.
    ///
    pub fn write_out(&mut self, data: &[u8]) {
        let outgoing_buffer = match &mut self.outgoing_buffer {
            Some(b) => b,
            None => {
                assert!(data.is_empty());
                return;
            }
        };

        assert!(data.len() <= outgoing_buffer.0.len() + outgoing_buffer.1.len());

        let to_copy_buf1 = cmp::min(outgoing_buffer.0.len(), data.len());
        let to_copy_buf2 = data.len() - to_copy_buf1;
        debug_assert_eq!(to_copy_buf1 + to_copy_buf2, data.len());

        outgoing_buffer.0[..to_copy_buf1].copy_from_slice(&data[..to_copy_buf1]);
        outgoing_buffer.1[..to_copy_buf2].copy_from_slice(&data[to_copy_buf1..][..to_copy_buf2]);

        self.advance_write(data.len());
    }

    /// Sets [`ReadWrite::wake_up_after`] to `min(wake_up_after, after)`.
    pub fn wake_up_after(&mut self, after: &TNow)
    where
        TNow: Clone + Ord,
    {
        match self.wake_up_after {
            Some(ref mut t) if *t < *after => {}
            Some(ref mut t) => *t = after.clone(),
            ref mut t @ None => *t = Some(after.clone()),
        }
    }

    /// Sets [`ReadWrite::wake_up_future`] to `select(wake_up_future, when)`.
    pub fn wake_up_when(&mut self, when: impl Future<Output = ()> + Send + 'static) {
        let current = match self.wake_up_future.take() {
            Some(f) => f,
            None => {
                self.wake_up_future = Some(when.boxed());
                return;
            }
        };

        self.wake_up_future = Some(
            async move {
                futures::pin_mut!(when);
                future::select(current, when).await;
            }
            .boxed(),
        );
    }
}

fn advance_buf(buf: &mut &mut [u8], n: usize) {
    let tmp = mem::take(buf);
    *buf = &mut tmp[n..];
}

/// See [`ReadWrite::incoming_bytes_iter`].
pub struct IncomingBytes<'a, 'b, TNow> {
    me: &'b mut ReadWrite<'a, TNow>,
}

impl<'a, 'b, TNow> Iterator for IncomingBytes<'a, 'b, TNow> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        match &mut self.me.incoming_buffer {
            Some(ref mut buf) => {
                if buf.is_empty() {
                    return None;
                }

                let byte = buf[0];
                *buf = &buf[1..];
                self.me.read_bytes += 1;
                Some(byte)
            }
            None => return None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.me.incoming_buffer {
            Some(b) => (b.len(), Some(b.len())),
            None => (0, Some(0)),
        }
    }
}

impl<'a, 'b, TNow> ExactSizeIterator for IncomingBytes<'a, 'b, TNow> {}
