//! Server audio played to the caller (bidirectional audio): the socket task turns inbound binary
//! frames into samples ([`Prebuffer`]), the media thread mixes them into outgoing frames ([`Playout`]).
//!
//! The thresholds and chunking follow the C module's formula: a release needs `320 * factor * 4`
//! samples at the server's rate the first time and `320 * factor * 2` after that, where `factor`
//! is how many server samples make one codec sample.

use std::collections::VecDeque;

use parking_lot::Mutex;

/// Inbound L16 bytes collected until there is enough to release.
#[derive(Debug)]
pub struct Prebuffer {
    held: Vec<i16>,
    /// A frame can end in the middle of a sample; its first byte waits for the next frame.
    set_aside: Option<u8>,
    threshold: usize,
    factor: usize,
}

impl Prebuffer {
    pub fn new(server_rate: u32, codec_rate: u32) -> Self {
        let factor = if server_rate > codec_rate {
            (server_rate / codec_rate) as usize
        } else {
            1
        };
        Prebuffer {
            held: Vec::new(),
            set_aside: None,
            threshold: 320 * factor * 4,
            factor,
        }
    }

    /// Drops what is held for a killed stream, so none of it is played in front of the next one.
    pub fn reset(&mut self) {
        self.held.clear();
        self.set_aside = None;
    }

    /// Adds a frame; returns the samples to play once enough are held, a whole multiple of `factor`
    /// (the rest stays for the next release).
    pub fn push(&mut self, bytes: &[u8]) -> Option<Vec<i16>> {
        let mut bytes = bytes.iter().copied();
        if let Some(first) = self.set_aside.take() {
            match bytes.next() {
                Some(second) => self.held.push(i16::from_le_bytes([first, second])),
                None => self.set_aside = Some(first),
            }
        }
        let rest: Vec<u8> = bytes.collect();
        let pairs = rest.as_chunks::<2>();
        self.held
            .extend(pairs.0.iter().map(|p| i16::from_le_bytes(*p)));
        if let [odd] = pairs.1 {
            self.set_aside = Some(*odd);
        }
        if self.held.len() < self.threshold {
            return None;
        }
        self.threshold = 320 * self.factor * 2;
        let whole = self.held.len() / self.factor * self.factor;
        let rest = self.held.split_off(whole);
        Some(std::mem::replace(&mut self.held, rest))
    }
}

/// Samples waiting to be played, shared by the socket task (appends) and the media thread (takes).
/// Deliberately unbounded: long streamed playouts arrive faster than real time.
#[derive(Debug, Default)]
pub struct Playout {
    buffer: Mutex<VecDeque<i16>>,
}

impl Playout {
    pub fn append(&self, samples: &[i16]) {
        self.buffer.lock().extend(samples);
    }

    /// Done by the task right away: audio it appends after a `killAudio` belongs to the next stream.
    pub fn clear(&self) {
        self.buffer.lock().clear();
    }

    /// Mixes the next samples into an outgoing frame. Never blocks: when the task holds the buffer,
    /// the frame passes untouched and the samples wait for the next one. Returns whether the frame
    /// was mixed.
    pub fn mix_into(&self, frame: &mut [i16]) -> bool {
        let Some(mut buffer) = self.buffer.try_lock() else {
            return false;
        };
        let n = frame.len().min(buffer.len());
        for (out, sample) in frame.iter_mut().zip(buffer.drain(..n)) {
            *out = out.saturating_add(sample);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    proptest! {
        /// However the stream is split into frames, the same samples come out in order.
        #[test]
        fn any_split_yields_the_same_samples(
            samples in prop::collection::vec(any::<i16>(), 0..6000),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..40),
        ) {
            let stream = bytes(&samples);
            let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(stream.len() + 1)).collect();
            cuts.extend([0, stream.len()]);
            cuts.sort();
            let mut prebuffer = Prebuffer::new(8000, 8000);
            let mut out = Vec::new();
            for w in cuts.windows(2) {
                out.extend(prebuffer.push(&stream[w[0]..w[1]]).unwrap_or_default());
            }
            out.extend(&prebuffer.held);
            prop_assert_eq!(out, samples);
        }
    }

    #[test]
    fn first_release_waits_for_four_frames_later_ones_for_two() {
        // 16 kHz from the server to an 8 kHz codec: factor 2
        let mut prebuffer = Prebuffer::new(16000, 8000);
        assert_eq!(prebuffer.push(&bytes(&[1; 2559])), None);
        // 2560 + 1 held: the odd one waits as it is not a whole multiple of the factor
        assert_eq!(prebuffer.push(&bytes(&[1; 2])).map(|r| r.len()), Some(2560));
        assert_eq!(prebuffer.push(&bytes(&[1; 1278])), None);
        assert_eq!(prebuffer.push(&bytes(&[1; 1])).map(|r| r.len()), Some(1280));
    }

    #[test]
    fn mixing_saturates() {
        let playout = Playout::default();
        playout.append(&[30000, -30000, 5]);
        let mut frame = [10000, -10000, 5, 7];
        assert!(playout.mix_into(&mut frame));
        assert_eq!(frame, [i16::MAX, i16::MIN, 10, 7]);
    }

    #[test]
    fn a_busy_buffer_passes_the_frame_and_keeps_the_samples() {
        let playout = Playout::default();
        playout.append(&[1, 2]);
        let mut frame = [0; 2];
        {
            let _task = playout.buffer.lock();
            assert!(!playout.mix_into(&mut frame));
        }
        assert_eq!(frame, [0, 0]);
        assert!(playout.mix_into(&mut frame));
        assert_eq!(frame, [1, 2]);
    }
}
