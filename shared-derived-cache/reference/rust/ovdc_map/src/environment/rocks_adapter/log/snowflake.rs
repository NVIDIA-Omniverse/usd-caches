// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Mutex;

/// Seconds between the unix epoch and this generator's own, counted as 56 years
/// of 365 days -- so it is a fixed offset rather than a calendar date. Works out
/// at 2025-12-18T00:00:00Z.
///
/// Moving it forward buys room in the timestamp half of a tag, and costs
/// tolerance of a host clock that is behind. [`now_ms`] reads zero for any clock
/// earlier than this instant rather than failing, so on a machine with an unset
/// or badly wrong RTC every tag is issued in "millisecond zero" and [`age`]
/// reports every blob as brand new. The abandoned-upload grace in
/// `rollup::stale_blobs` is measured with `age`, so nothing it guards is ever
/// reclaimed on such a host. Raising this constant widens the range of clocks
/// that behave that way.
const EPOCH_SECS: u64 = 31536000 * 56;

struct IdGen {
    sequence: u64,
    last: u64,
}

impl IdGen {
    /// Produces the next tag for a clock reading of `now_ms`.
    ///
    /// Tags are the log's ordering key -- `rollup` decides what a record
    /// supersedes purely by comparing them -- so they have to increase no matter
    /// what the clock does. `last` is therefore a high-water mark, not the last
    /// reading: a clock that jumps backwards keeps the old millisecond and
    /// advances the sequence instead, so tags run slightly ahead of wall time
    /// rather than repeating. Nothing reads the time back out of a tag, so
    /// running ahead costs nothing.
    fn next(&mut self, now_ms: u64) -> u128 {
        let (time, seq) = if now_ms > self.last {
            self.last = now_ms;
            self.sequence = 0;
            (now_ms, 0)
        } else {
            // The same millisecond, or a clock that went backwards. Both are
            // counted within the mark already reached.
            self.sequence += 1;
            (self.last, self.sequence)
        };

        ((time as u128) << 64) | seq as u128
    }
}

/// Milliseconds since this generator's epoch.
///
/// A clock behind the epoch reads as zero rather than aborting the process. A
/// node that boots before its clock is set stays correct -- its tags start low
/// and climb -- and picks up real time once the clock is right, instead of
/// dying on every write until someone notices.
fn now_ms() -> u64 {
    let epoch = std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(EPOCH_SECS))
        .unwrap();

    std::time::SystemTime::now()
        .duration_since(epoch)
        .map(|y| y.as_millis() as u64)
        .unwrap_or(0)
}

pub fn next() -> u128 {
    static SEQUENCE: Mutex<IdGen> = Mutex::new(IdGen {
        sequence: 0,
        last: 0,
    });

    SEQUENCE.lock().unwrap().next(now_ms())
}

/// How long ago `tag` was issued, by wall clock.
///
/// Saturates at zero. Tags run ahead of the clock after a backwards step, and
/// reading that as "very old" would let a caller retire something still live.
pub fn age(tag: u128) -> std::time::Duration {
    let issued_ms = (tag >> 64) as u64;
    std::time::Duration::from_millis(now_ms().saturating_sub(issued_ms))
}

#[cfg(test)]
mod tests {
    use super::IdGen;

    fn gen() -> IdGen {
        IdGen {
            sequence: 0,
            last: 0,
        }
    }

    #[test]
    fn test_tags_increase_as_the_clock_advances() {
        let mut gen = gen();
        let mut last = gen.next(1);
        for ms in 2..64 {
            let next = gen.next(ms);
            assert!(next > last, "tag went backwards across milliseconds");
            last = next;
        }
    }

    #[test]
    fn test_tags_increase_within_one_millisecond() {
        let mut gen = gen();
        let mut last = gen.next(100);
        for _ in 0..64 {
            let next = gen.next(100);
            assert!(next > last, "tag repeated within a millisecond");
            last = next;
        }
    }

    /// The one that matters. An NTP step backwards used to produce tags below
    /// ones already written, and `rollup` skips any record whose tag is under
    /// the drop watermark -- so a put would be accepted, reported as `PutOk`,
    /// and then be invisible until wall time caught back up.
    #[test]
    fn test_tags_increase_when_the_clock_steps_backwards() {
        let mut gen = gen();
        let mut last = gen.next(10_000);

        for ms in [9_000, 1, 0, 8_500, 9_999] {
            let next = gen.next(ms);
            assert!(
                next > last,
                "tag went backwards after the clock stepped back to {ms}"
            );
            last = next;
        }

        // And it resumes counting from real time once the clock passes the mark.
        let next = gen.next(10_001);
        assert!(next > last);
        assert_eq!(next >> 64, 10_001, "did not resume tracking the clock");
    }

    /// A clock behind the generator's epoch reads as zero, which must still
    /// yield usable increasing tags rather than a panic.
    #[test]
    fn test_tags_increase_from_a_clock_before_the_epoch() {
        let mut gen = gen();
        let mut last = gen.next(0);
        for _ in 0..8 {
            let next = gen.next(0);
            assert!(next > last, "tag repeated while the clock read zero");
            last = next;
        }

        let next = gen.next(5);
        assert!(next > last, "tag went backwards once the clock was set");
    }
}
