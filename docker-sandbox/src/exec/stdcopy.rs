//! Decoder of the Docker *stdcopy* framing.
//!
//! Without a terminal, the daemon multiplexes the process output: an 8-byte
//! header (`[stream, 0, 0, 0, size: u32 big-endian]`) precedes each chunk, with
//! stream 1 for stdout, 2 for stderr and 3 for errors of the daemon itself.
//! With a terminal the output is raw. The decoder also falls back to raw mode
//! when the first bytes do not look like a frame header.

use bytes::BytesMut;

use super::{OutputChunk, OutputStream};

/// Length of a frame header.
const HEADER_LEN: usize = 8;

/// Decoder of the Docker *stdcopy* framing.
#[derive(Debug)]
pub(crate) struct StdcopyDecoder {
    /// Current decoding mode.
    mode: Mode,
    /// Bytes received and not decoded yet.
    buffer: BytesMut,
}

/// Decoding mode of a [`StdcopyDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Waiting for enough bytes to tell framed from raw output.
    Detect,
    /// Framed output.
    Multiplexed,
    /// Raw output, attributed to stdout.
    Raw,
}

impl StdcopyDecoder {
    /// A decoder for an execution with (`tty`) or without a terminal.
    pub(crate) fn new(tty: bool) -> Self {
        Self {
            mode: if tty { Mode::Raw } else { Mode::Detect },
            buffer: BytesMut::new(),
        }
    }

    /// Appends received bytes.
    pub(crate) fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Next complete chunk, if one is buffered.
    pub(crate) fn next_chunk(&mut self) -> Option<OutputChunk> {
        loop {
            match self.mode {
                Mode::Raw => {
                    return (!self.buffer.is_empty()).then(|| OutputChunk {
                        stream: OutputStream::Stdout,
                        data: self.buffer.split().freeze(),
                    });
                }
                Mode::Detect => {
                    if self.buffer.len() < HEADER_LEN {
                        return None;
                    }
                    self.mode = if is_header(&self.buffer) {
                        Mode::Multiplexed
                    } else {
                        Mode::Raw
                    };
                }
                Mode::Multiplexed => {
                    if self.buffer.len() < HEADER_LEN {
                        return None;
                    }
                    if !is_header(&self.buffer) {
                        tracing::warn!(
                            "unexpected bytes in a multiplexed stream; switching to raw mode"
                        );
                        self.mode = Mode::Raw;
                        continue;
                    }
                    let size = u32::from_be_bytes([
                        self.buffer[4],
                        self.buffer[5],
                        self.buffer[6],
                        self.buffer[7],
                    ]) as usize;
                    if self.buffer.len() < HEADER_LEN + size {
                        return None;
                    }
                    let tag = self.buffer[0];
                    let mut frame = self.buffer.split_to(HEADER_LEN + size);
                    let data = frame.split_off(HEADER_LEN).freeze();
                    let stream = match tag {
                        1 => OutputStream::Stdout,
                        2 | 3 => OutputStream::Stderr,
                        // Stream 0 echoes the standard input: it is not output.
                        _ => continue,
                    };
                    return Some(OutputChunk { stream, data });
                }
            }
        }
    }

    /// Flushes what remains at the end of the stream.
    pub(crate) fn finish(&mut self) -> Option<OutputChunk> {
        if self.mode == Mode::Multiplexed && !self.buffer.is_empty() {
            let bytes = self.buffer.len();
            tracing::warn!(bytes, "multiplexed stream ended in the middle of a frame");
            self.buffer.clear();
            return None;
        }
        self.mode = Mode::Raw;
        self.next_chunk()
    }
}

/// Whether `bytes` starts with a plausible stdcopy header.
fn is_header(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[0] <= 3 && bytes[1..4] == [0, 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of `data` on `stream`.
    fn frame(stream: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![stream, 0, 0, 0];
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    /// Every chunk the decoder can produce from what it holds.
    fn drain(decoder: &mut StdcopyDecoder) -> Vec<(OutputStream, Vec<u8>)> {
        let mut chunks = Vec::new();
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push((chunk.stream, chunk.data.to_vec()));
        }
        chunks
    }

    #[test]
    fn multiplexed_frames_are_split_by_stream_even_across_reads() {
        let mut bytes = frame(1, b"out\n");
        bytes.extend(frame(2, b"err\n"));
        bytes.extend(frame(3, b"system\n"));

        let mut decoder = StdcopyDecoder::new(false);
        let (head, tail) = bytes.split_at(5);
        decoder.feed(head);
        assert!(
            drain(&mut decoder).is_empty(),
            "incomplete header must not be decoded"
        );
        decoder.feed(tail);

        assert_eq!(
            drain(&mut decoder),
            [
                (OutputStream::Stdout, b"out\n".to_vec()),
                (OutputStream::Stderr, b"err\n".to_vec()),
                (OutputStream::Stderr, b"system\n".to_vec()),
            ]
        );
        assert!(decoder.finish().is_none());
    }

    #[test]
    fn terminal_output_is_raw() {
        let mut decoder = StdcopyDecoder::new(true);
        decoder.feed(&frame(1, b"looks like a frame"));
        let chunks = drain(&mut decoder);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1.len(), HEADER_LEN + 18);
    }

    #[test]
    fn short_or_non_framed_output_falls_back_to_raw() {
        let mut decoder = StdcopyDecoder::new(false);
        decoder.feed(b"ok\n");
        assert!(
            drain(&mut decoder).is_empty(),
            "waits for a full header before deciding"
        );
        assert_eq!(
            decoder.finish().map(|c| c.data.to_vec()),
            Some(b"ok\n".to_vec())
        );

        let mut decoder = StdcopyDecoder::new(false);
        decoder.feed(b"plain text output\n");
        assert_eq!(
            drain(&mut decoder),
            [(OutputStream::Stdout, b"plain text output\n".to_vec())]
        );
    }

    #[test]
    fn partial_frames_wait_and_input_echoes_are_dropped() {
        let mut bytes = frame(0, b"typed\n");
        bytes.extend(frame(1, b"out\n"));
        let (head, tail) = bytes.split_at(bytes.len() - 2);

        let mut decoder = StdcopyDecoder::new(false);
        decoder.feed(head);
        assert!(
            drain(&mut decoder).is_empty(),
            "the echo is dropped and the frame is incomplete"
        );
        decoder.feed(tail);
        assert_eq!(
            drain(&mut decoder),
            [(OutputStream::Stdout, b"out\n".to_vec())]
        );
    }

    #[test]
    fn bytes_that_break_the_framing_switch_to_raw() {
        let mut bytes = frame(1, b"out\n");
        bytes.extend(b"no header here\n");

        let mut decoder = StdcopyDecoder::new(false);
        decoder.feed(&bytes);
        assert_eq!(
            drain(&mut decoder),
            [
                (OutputStream::Stdout, b"out\n".to_vec()),
                (OutputStream::Stdout, b"no header here\n".to_vec()),
            ]
        );
    }

    #[test]
    fn a_stream_cut_inside_a_frame_drops_the_partial_frame() {
        let bytes = frame(1, b"cut short");

        let mut decoder = StdcopyDecoder::new(false);
        decoder.feed(&bytes[..12]);
        assert!(drain(&mut decoder).is_empty());
        assert!(decoder.finish().is_none());
        assert!(drain(&mut decoder).is_empty(), "the partial frame is gone");
    }
}
